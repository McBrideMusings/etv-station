use std::borrow::Cow;
use std::collections::VecDeque;
use std::fmt::Formatter;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ersatztv_channel::config::ChannelConfig;
use ersatztv_channel::error::ChannelError;
use ersatztv_core::{
    HEARTBEAT_FILE_NAME, OVERLAY_WANTED_FILE_NAME, OVERLAY_WRITER_POLL_INTERVAL,
    OVERLAY_WRITER_TIMEOUT, READY_FILE_NAME, empty_folder,
};
use ersatztv_playout::playout::{
    AudioHint, PeriodicClock, PlayoutItem, PlayoutItemSource, PlayoutItemTracks, ProbeHint,
    TrackSelection, VideoHint, WatermarkLocation, WatermarkTiming,
};
use ersatztv_playout::template::expand_template;
use ffpipeline::ffmpeg_info::FfmpegInfo;
use ffpipeline::frame_rate::FrameRate;
use ffpipeline::frame_size::FrameSize;
use ffpipeline::input::{
    FfmpegInputArgs, HttpInputOptions, HttpInputSource, InputSettings, InputSource,
    LavfiInputSource, LocalInputSource, OverlayInput, ProbedInput, RtspInputOptions,
    RtspInputSource, WatermarkInput,
};
use ffpipeline::output_settings::{AudioOutputSettings, OutputSettings, SubtitleMode};
use ffpipeline::pipeline::{
    AudioFormat, Hz, Kbps, PtsOffset, ReadRate, SEGMENT_SECONDS, VideoFormat,
};
use ffpipeline::probe::{
    CodecType, ProbeResult, ProbeResultAudioStream, ProbeResultColorParams, ProbeResultStream,
    ProbeResultVideoStream, Probeable,
};
use ffpipeline::web_vtt::Cue;
use ffpipeline::{pipeline, probe};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use time::OffsetDateTime;
use tokio::io::AsyncBufReadExt;
use tokio::sync::Mutex;

use crate::dossier::DossierBuilder;
use crate::local_proxy::{LocalProxyServer, ScriptCommand};
use crate::playlist_manager::{
    PlaylistManager, PlaylistManagerOutputFiles, SessionAbort, SubtitleSource,
};
use crate::playout_loader::PlayoutLoader;
use crate::pts_scanner::{PtsScanner, PtsTime};

const STDERR_RING_LINES: usize = 2_000;
const STALL_THRESHOLD: Duration = Duration::from_secs(60);

/// How far ahead of wall clock the session keeps its transcode. A player that
/// tunes in has to be handed several segments at once or it has nothing to play,
/// so ffmpeg reads this much unthrottled before slowing to wall-clock speed.
/// Also what a mid-playback rebuild (after [`REBURST_AT_LEAD`] fires) tops the
/// buffer back up to — see [`reburst_decision`] — which is why this stays large:
/// [`REBURST_ARM_AT_LEAD`] arms almost immediately once a fresh buffer is built
/// (this constant already exceeds it), so the real protection against firing
/// again soon after is the gap down to [`REBURST_AT_LEAD`], not the arm point.
/// A thin gap there means a single ordinary dip in encode speed can trigger a
/// second visible restart shortly after the first — a real regression, not a
/// hypothetical one, so this is sized for that margin rather than for tune-in
/// speed. See [`INITIAL_BURST`] for the constant that actually governs how long
/// a first-time viewer waits.
const TARGET_BUFFER: time::Duration = time::Duration::seconds(SEGMENT_SECONDS as i64 * 11);

/// How much a *first* tune-in bursts before handing the viewer anything —
/// deliberately smaller than [`TARGET_BUFFER`], and used only at the one call
/// site building the very first buffer, before there is a viewer to glitch.
///
/// "Unthrottled" assumes the encode outruns wall clock; on GPU-bound hardware
/// (deep seek + hwaccel decode + overlay composite) it may not, in which case
/// this many seconds of buffer costs close to that many real seconds before the
/// viewer gets anything — confirmed against a real tune-in via the
/// `ffmpeg_progress` log target, which sat at ~1.0x for this exact pipeline
/// shape rather than bursting. This used to be the same constant as
/// `TARGET_BUFFER`, cut from 11 segments to 7 to shorten that wait — which
/// also, as an unreviewed side effect, cut the margin a rebuilt buffer has
/// against [`REBURST_AT_LEAD`] before it can fire again. Splitting the two
/// keeps the tune-in win without carrying that cost into every mid-playback
/// rebuild, which needs the larger number.
const INITIAL_BURST: time::Duration = time::Duration::seconds(SEGMENT_SECONDS as i64 * 7);

/// Lead at which a still-playing item is restarted to rebuild its buffer.
///
/// `-readrate 1.0` asks ffmpeg for exactly wall-clock speed, and ffmpeg delivers
/// a few percent under whatever it is asked for — measured at 0.958x on an idle
/// machine and 0.69x on a busy one. Because readrate only ever slows the read
/// down, a shortfall is never repaid, so the lead from [`TARGET_BUFFER`] drains
/// at the deficit rate and the session eventually cannot produce a segment
/// before the viewer needs it.
///
/// The lead used to be topped up only between items, which is fine for a
/// half-hour episode and useless for a feature film: one item can run for hours
/// with no opportunity to recover. Restarting the item mid-way costs a seek and
/// a few seconds of respawn, and buys back the whole buffer.
///
/// Set above the 4-10s a respawn takes, so the new process is producing again
/// before the remaining lead runs out.
const REBURST_AT_LEAD: time::Duration = time::Duration::seconds(20);

/// Lead the item must reach before [`REBURST_AT_LEAD`] is allowed to fire.
///
/// Without this the check would trip immediately: at the start of an item the
/// lead is still being built, so it is legitimately below the threshold and
/// would restart the item forever without ever playing any of it.
const REBURST_ARM_AT_LEAD: time::Duration = time::Duration::seconds(SEGMENT_SECONDS as i64 * 6);

/// Read end of the overlay rawvideo fifo, held open by this process and handed
/// to ffmpeg as an inherited fd. There are no fifos off unix, so the type is
/// uninhabited there and the overlay path fails instead of being constructed.
#[cfg(unix)]
type OverlayFifoReader = std::fs::File;
#[cfg(not(unix))]
type OverlayFifoReader = std::convert::Infallible;

/// The `-i` argument naming an already-open overlay fifo. ffmpeg reads the
/// descriptor this process opened rather than opening the fifo itself — see
/// [`open_overlay_fifo`] for why. The descriptor keeps its own number across
/// the fork, so name that number rather than moving it onto a fixed one.
#[cfg(unix)]
fn overlay_fifo_input_path(fifo: &OverlayFifoReader) -> String {
    use std::os::unix::io::AsRawFd;

    format!("pipe:{}", fifo.as_raw_fd())
}

#[cfg(not(unix))]
fn overlay_fifo_input_path(fifo: &OverlayFifoReader) -> String {
    match *fifo {}
}

/// Where in the item this session's next ffmpeg starts reading.
///
/// Only the first item of a session starts partway in — the viewer joined while
/// it was already playing. Every item after it is played from its beginning.
#[derive(Copy, Clone, PartialEq)]
enum ChannelSessionState {
    Seek,
    Zero,
}

impl std::fmt::Display for ChannelSessionState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelSessionState::Seek => write!(f, "Seek"),
            ChannelSessionState::Zero => write!(f, "Zero"),
        }
    }
}

struct TimingResult {
    in_point: Duration,
    out_point: Duration,
    finish: OffsetDateTime,
}

pub struct ChannelSession {
    channel_config: ChannelConfig,
    playout_loader: PlayoutLoader,
    pts_scanner: PtsScanner,
    playlist_manager: Arc<Mutex<PlaylistManager>>,
    local_proxy_server: LocalProxyServer,

    ffmpeg_path: PathBuf,
    ffprobe_path: PathBuf,
    ffmpeg_info: FfmpegInfo,
    hw_accel: Option<ffpipeline::hw_accel::HardwareAccel>,

    transcoded_until: OffsetDateTime,
    ready_file: PathBuf,

    output_file: String,
    output_segment_template: String,

    start_time_offset: time::Duration,
    state: ChannelSessionState,

    timeout_notify: Arc<tokio::sync::Notify>,

    cached_subtitles: Option<(String, Arc<Vec<Cue>>)>,
    dynamic_http_client: reqwest::Client,
}

impl ChannelSession {
    pub async fn new(channel_config: ChannelConfig) -> Result<ChannelSession, ChannelError> {
        let now = OffsetDateTime::now_local()?;

        let start_time_offset = if let Some(virtual_start) = channel_config.playout.virtual_start {
            virtual_start - now
        } else {
            time::Duration::ZERO
        };

        let output_folder = channel_config.expanded_output_folder().to_owned();
        let generated_output_file = output_folder
            .join("live.m3u8")
            .into_os_string()
            .into_string()
            .map_err(|_| ChannelError::ChannelConfigOutputFolderRequired)?;

        // Burned-in subtitles live inside the video picture, so the channel has
        // no separate subtitle playlist to serve.
        let generated_subtitle_output_file = match channel_config.normalization.subtitle.mode {
            ersatztv_channel::config::SubtitleMode::Convert => Some(
                output_folder
                    .join("live_sub.m3u8")
                    .into_os_string()
                    .into_string()
                    .map_err(|_| ChannelError::ChannelConfigOutputFolderRequired)?,
            ),
            ersatztv_channel::config::SubtitleMode::Burn => None,
        };

        let ffmpeg_output_file = output_folder
            .join("ffmpeg.m3u8")
            .into_os_string()
            .into_string()
            .map_err(|_| ChannelError::ChannelConfigOutputFolderRequired)?;

        let output_segment_template = output_folder
            .join("live%06d.ts")
            .into_os_string()
            .into_string()
            .map_err(|_| ChannelError::ChannelConfigOutputFolderRequired)?;

        let ready_file = output_folder.join(READY_FILE_NAME);

        let playout_loader = PlayoutLoader::new(&channel_config);
        let pts_scanner = PtsScanner::new(&channel_config);
        let playlist_manager = PlaylistManager::new(
            now,
            SEGMENT_SECONDS,
            output_folder.to_owned(),
            ready_file.to_owned(),
            PlaylistManagerOutputFiles {
                generated_playlist_file: generated_output_file,
                generated_subtitle_playlist_file: generated_subtitle_output_file,
                ffmpeg_playlist_file: ffmpeg_output_file.to_owned(),
            },
        )
        .await;

        let playlist_manager = Arc::new(Mutex::new(playlist_manager));

        let default_ffprobe_path = Path::new("ffprobe").to_path_buf();
        let default_ffmpeg_path = Path::new("ffmpeg").to_path_buf();

        let ffprobe_path = channel_config
            .ffmpeg
            .ffprobe_path
            .clone()
            .unwrap_or(default_ffprobe_path);
        let ffmpeg_path = channel_config
            .ffmpeg
            .ffmpeg_path
            .clone()
            .unwrap_or(default_ffmpeg_path);

        let local_proxy_server = LocalProxyServer::start().await?;

        let dynamic_http_client = reqwest::Client::builder()
            .build()
            .map_err(|e| ChannelError::ChannelStartup(format!("http client: {e}")))?;

        Ok(ChannelSession {
            channel_config,
            playout_loader,
            pts_scanner,
            playlist_manager,
            local_proxy_server,
            ffmpeg_path: ffmpeg_path.to_owned(),
            ffprobe_path: ffprobe_path.to_owned(),
            ffmpeg_info: FfmpegInfo::default(),
            hw_accel: None,
            transcoded_until: now + start_time_offset,
            ready_file,
            output_file: ffmpeg_output_file,
            output_segment_template,
            start_time_offset,
            state: ChannelSessionState::Seek,
            timeout_notify: Arc::new(tokio::sync::Notify::new()),
            cached_subtitles: None,
            dynamic_http_client,
        })
    }

    pub async fn run(&mut self, troubleshoot: bool) -> Result<(), ChannelError> {
        self.prep_output_folder(troubleshoot).await?;

        self.ffmpeg_info = FfmpegInfo::load(
            &self.ffmpeg_path,
            &self.channel_config.ffmpeg.disabled_filters,
            &self.channel_config.ffmpeg.preferred_filters,
        )
        .await?;

        log::debug!("ffmpeg info: {:?}", self.ffmpeg_info);

        self.hw_accel = self
            .channel_config
            .normalization
            .video
            .accel
            .as_ref()
            .and_then(|a| a.to_pipeline(&self.channel_config));

        let pm = self.playlist_manager.clone();
        let tn = self.timeout_notify.clone();

        tokio::spawn(async move {
            loop {
                let mut playlist_manager = pm.lock().await;
                let _ = playlist_manager.update().await;
                if playlist_manager.abort().is_some() {
                    tn.notify_one();
                    break;
                }
                drop(playlist_manager);
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });

        // nothing is on disk yet, so the first process bursts INITIAL_BURST —
        // deliberately smaller than TARGET_BUFFER; see that constant's doc.
        self.transcode(INITIAL_BURST, troubleshoot).await?;

        if troubleshoot {
            log::debug!("troubleshooting complete; terminating.");
            return Ok(());
        }

        let pm = self.playlist_manager.clone();
        let tn = self.timeout_notify.clone();

        loop {
            let abort = pm.lock().await.abort();
            if let Some(abort) = abort {
                tn.notify_one();
                return Err(self.abort_error(abort));
            }

            let now = OffsetDateTime::now_local()? + self.start_time_offset;
            let transcoded_buffer =
                std::cmp::max(time::Duration::ZERO, self.transcoded_until - now);
            log::debug!(
                "transcoded buffer: {}m {}s",
                transcoded_buffer.whole_minutes(),
                transcoded_buffer.whole_seconds() % 60
            );
            if transcoded_buffer <= time::Duration::minutes(1) {
                // top the buffer back up to TARGET_BUFFER, no more; bursting a
                // full buffer every item would push the lead up by that much
                // again each time
                self.transcode(
                    TARGET_BUFFER.saturating_sub(transcoded_buffer),
                    troubleshoot,
                )
                .await?;
            } else {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                    _ = tn.notified() => {
                        let abort = pm.lock().await.abort().unwrap_or(SessionAbort::IdleTimeout);
                        return Err(self.abort_error(abort));
                    }
                }
            }
        }
    }

    async fn prep_output_folder(&self, troubleshoot: bool) -> Result<(), ChannelError> {
        let output_folder = self.channel_config.expanded_output_folder();

        if self.ready_file.exists() {
            tokio::fs::remove_file(&self.ready_file).await?;
        }

        if output_folder.exists() {
            if !troubleshoot {
                empty_folder(output_folder)
                    .await
                    .map_err(|_| ChannelError::ChannelConfigOutputFolderRequired)?;
            }
        } else {
            tokio::fs::create_dir(output_folder)
                .await
                .map_err(|_| ChannelError::ChannelConfigOutputFolderRequired)?;
        }

        // Stamp the heartbeat here, after the wipe above, because both timers
        // that can end this worker read it and BOTH are inert while it is
        // absent: PlaylistManager evaluates the idle timeout and the stall
        // watchdog only inside `if heartbeat_file.exists()`, so with no file it
        // never times out and never sees a viewer it could be stalled on.
        //
        // Until now only the server's session route wrote it, and a viewer
        // reaches that route only after this channel is ready. A worker that
        // wedged before its first segment therefore never got a heartbeat at
        // all and ran forever, holding its ffmpeg, with nothing able to reap it.
        //
        // It belongs here rather than in the server before spawn: the wipe above
        // would delete anything written beforehand. Stamping it is honest, not a
        // convenient lie — a worker only ever starts because a request arrived,
        // so at this instant someone is genuinely waiting. If they leave, nobody
        // refreshes it and the ordinary idle timeout collects this worker.
        let heartbeat_file = output_folder.join(HEARTBEAT_FILE_NAME);
        if let Err(err) = tokio::fs::write(&heartbeat_file, b"").await {
            log::warn!("failed to stamp initial heartbeat: {err}");
        }

        Ok(())
    }

    /// Signal the overlay producer (the separate etv-station daemon) that this
    /// channel is now being watched — so it spawns the channel's overlay
    /// process — then open the rawvideo fifo and wait, bounded, for a writer to
    /// attach. Returns the open read end, which is handed to ffmpeg as an
    /// inherited fd so ffmpeg never opens the fifo itself.
    ///
    /// Every failure here returns `Err`, including a slow or absent overlay:
    /// the caller's item then fails and is replaced with black/silence. That is
    /// deliberate. The old best-effort posture let every failure path fall
    /// through to ffmpeg's own blocking `open()`, which parks forever when no
    /// writer is attached — producing zero segments, a structurally valid but
    /// permanently empty HLS playlist, and no error at any layer.
    async fn signal_overlay_wanted_and_wait_ready(
        &self,
        fifo_path: &str,
    ) -> Result<OverlayFifoReader, ChannelError> {
        // The overlay markers and fifo live in the PLAYOUT folder — the folder
        // shared with the etv-station daemon (where it writes playout JSON and
        // the overlay fifo) — not the HLS output folder.
        let playout_folder = self.channel_config.expanded_playout_folder();
        touch_overlay_wanted_and_open_fifo(playout_folder, fifo_path, OVERLAY_WRITER_TIMEOUT).await
    }

    async fn transcode(
        &mut self,
        initial_burst: time::Duration,
        troubleshoot: bool,
    ) -> Result<(), ChannelError> {
        log::debug!(
            "channel session state: {}, bursting {}s before wall-clock speed",
            self.state,
            initial_burst.whole_seconds()
        );

        // get last pts offset
        let mut pts_time: Option<PtsTime> = None;
        match self.pts_scanner.get_last_pts().await {
            Ok(scanned_pts_time) => pts_time = Some(scanned_pts_time),
            Err(e) => log::debug!("failed to scan pts time: {e}"),
        }

        let mut current_item_result = self
            .playout_loader
            .get_current_item(&self.transcoded_until)
            .await;

        if let Ok(
            item @ PlayoutItem {
                source: Some(PlayoutItemSource::Dynamic { .. }),
                ..
            },
        ) = current_item_result
        {
            current_item_result = self
                .resolve_dynamic_item(&self.transcoded_until, &item)
                .await;
        }

        let current_item = match current_item_result {
            Ok(playout_item) => playout_item,
            Err(ChannelError::PlayoutJsonNoItem { next_start }) => {
                self.fake_playout_item(next_start)
            }
            Err(err) => {
                log::error!("{}", err);
                self.fake_playout_item(None)
            }
        };

        let pts_duration = pts_time.map(|p| p.duration);

        let result = self
            .transcode_item(&current_item, initial_burst, troubleshoot, pts_duration)
            .await;

        let finish = match result {
            Ok(ok) => ok,
            Err(
                e @ (ChannelError::IdleTimeout(_)
                | ChannelError::SegmentStall(_)
                | ChannelError::Stalled(_)),
            ) => {
                return Err(e);
            }
            // The item is fine; the transcode had merely fallen behind. Pick it
            // up where its segments actually reached and leave the state at
            // `Seek` so it resumes mid-item rather than restarting the film.
            // This must be handled ahead of the catch-all below — routing it
            // there would blank the rest of the item, which is a far worse
            // outcome than the few seconds of respawn it costs here.
            Err(ChannelError::LeadExhausted) => {
                let resumed_at = *self.playlist_manager.lock().await.last_segment_end();
                self.transcoded_until = resumed_at;
                self.state = ChannelSessionState::Seek;
                log::debug!("resuming the same item from {}", resumed_at);
                return Ok(());
            }
            Err(e) if troubleshoot => return Err(e),
            Err(e) => {
                log::error!("item failed, replacing with black/silence: {e}");
                let fake_item = self.fake_playout_item(Some(current_item.finish));
                self.transcode_item(&fake_item, initial_burst, troubleshoot, pts_duration)
                    .await?
            }
        };

        self.transcoded_until = finish;
        log::debug!("transcoded until: {}", self.transcoded_until);

        // this item was played to its end, so the next one starts at its own
        self.state = ChannelSessionState::Zero;

        Ok(())
    }

    async fn transcode_item(
        &mut self,
        current_item: &PlayoutItem,
        initial_burst: time::Duration,
        troubleshoot: bool,
        pts_duration: Option<Duration>,
    ) -> Result<OffsetDateTime, ChannelError> {
        // prioritize source from audio tracks, then default source
        let audio_source = Self::resolve_source(current_item, |t| t.audio.as_ref())
            .ok_or(ChannelError::PlayoutJsonAudioSourceRequired)?;

        // prioritize source from video tracks, then default source
        let video_source = Self::resolve_source(current_item, |t| t.video.as_ref())
            .ok_or(ChannelError::PlayoutJsonVideoSourceRequired)?;

        let subtitle_source = Self::resolve_subtitle_source(current_item);

        let audio_source_is_video_source = audio_source == video_source;
        let subtitle_source_is_video_source =
            subtitle_source.as_ref().is_some_and(|s| s == &video_source);

        let audio_input_source = self.playout_source_to_input_source(audio_source.clone())?;
        let video_input_source = if audio_source_is_video_source {
            audio_input_source.clone()
        } else {
            self.playout_source_to_input_source(video_source.clone())?
        };
        let subtitle_input_source = if subtitle_source_is_video_source {
            Some(video_input_source.clone())
        } else {
            subtitle_source
                .clone()
                .and_then(|s| self.playout_source_to_input_source(s.clone()).ok())
        };

        let audio_fut = self.resolve_probe(&audio_source, &audio_input_source);
        let video_fut = async {
            if audio_source_is_video_source {
                Ok::<_, ChannelError>(None)
            } else {
                self.resolve_probe(&video_source, &video_input_source)
                    .await
                    .map(Some)
            }
        };
        let subtitle_fut = async {
            if subtitle_source_is_video_source {
                Ok::<_, ChannelError>(None)
            } else if let (Some(src), Some(s)) =
                (subtitle_source.as_ref(), subtitle_input_source.as_ref())
            {
                self.resolve_probe(src, s).await.map(Some)
            } else {
                Ok(None)
            }
        };

        let watermark_fut = async {
            if let Some(w) = current_item.watermark.as_ref() {
                let input_source = self.playout_source_to_input_source(w.source.clone())?;
                let location = playout_location_to_pipeline(&w.location);
                let timing = playout_timing_to_pipeline(w.timing.as_ref());

                let probe_result = self.resolve_probe(&w.source, &input_source).await?;
                Ok(Some(WatermarkInput {
                    input_source,
                    probe_result,
                    stream_index: w.stream_index,
                    location,
                    width_percent: w.width_percent,
                    within_source_content: w.within_source_content,
                    horizontal_margin_percent: w.horizontal_margin_percent,
                    vertical_margin_percent: w.vertical_margin_percent,
                    opacity_percent: w.opacity_percent,
                    timing,
                }))
            } else {
                Ok(None)
            }
        };

        let (audio_probe_result, video_probe_opt, subtitle_probe_opt, watermark_input) =
            tokio::try_join!(audio_fut, video_fut, subtitle_fut, watermark_fut)?;

        let video_probe_result = video_probe_opt.unwrap_or_else(|| audio_probe_result.clone());
        let subtitle_probe_result = if subtitle_source_is_video_source {
            Some(video_probe_result.clone())
        } else {
            subtitle_probe_opt
        };

        let audio_norm = &self.channel_config.normalization.audio;
        let video_norm = &self.channel_config.normalization.video;

        let video_size = match (video_norm.width, video_norm.height) {
            (Some(width), Some(height)) => Some(FrameSize { width, height }),
            _ => None,
        };

        // consider an item to be live if any of its sources are live;
        // live sources can never seek or work ahead
        let is_live = source_is_live(&video_source) || source_is_live(&audio_source);

        // generate pipeline
        let output_settings = OutputSettings {
            audio: AudioOutputSettings {
                format: audio_norm.format.clone().map(AudioFormat::from),
                bitrate: audio_norm.bitrate_kbps.map(Kbps),
                buffer: audio_norm.buffer_kbps.map(Kbps),
                channels: audio_norm.channels,
                sample_rate: audio_norm.sample_rate_hz.map(Hz),
                loudness: if audio_norm.normalize_loudness {
                    Some(
                        audio_norm
                            .loudness
                            .as_ref()
                            .map(|l| l.into())
                            .unwrap_or_default(),
                    )
                } else {
                    None
                },
            },
            video_format: video_norm.format.clone().map(VideoFormat::from),
            bit_depth: video_norm.bit_depth,
            video_bitrate: video_norm.bitrate_kbps.map(Kbps),
            video_buffer: video_norm.buffer_kbps.map(Kbps),
            video_size,
            scaling_mode: video_norm.scaling_mode.into(),
            filter_options: video_norm.filters.clone().into(),
            deinterlace: video_norm.deinterlace,
            accel: self.hw_accel.clone(),
            format: ffpipeline::output_format::OutputFormat::Hls {
                playlist: self.output_file.clone(),
                segment_template: self.output_segment_template.clone(),
                troubleshoot,
            },
            pts_offset: pts_duration.map(|duration| PtsOffset { duration }),
            read_rate: if is_live {
                // a live source arrives at its own speed; throttling it would
                // fall behind and never catch up
                ReadRate::Unthrottled
            } else {
                ReadRate::Realtime {
                    initial_burst: Duration::from_secs(initial_burst.whole_seconds().max(0) as u64),
                }
            },
            frame_rate: if video_probe_result.is_still_image() {
                Some(FrameRate::default())
            } else {
                None
            },
            subtitle_mode: self.channel_config.normalization.subtitle.mode.into(),
            fonts_folder: self
                .channel_config
                .normalization
                .subtitle
                .fonts_folder
                .clone(),
            subtitle_force_style: self
                .channel_config
                .normalization
                .subtitle
                .force_style
                .clone(),
            reports_folder: self.channel_config.ffmpeg.reports_folder.clone(),
            report_id: Some(self.channel_config.number().to_owned()),
        };

        let start_at_zero = matches!(self.state, ChannelSessionState::Zero);

        let audio_timing = self.input_timing(current_item, &audio_source, start_at_zero, is_live);
        let video_timing = self.input_timing(current_item, &video_source, start_at_zero, is_live);
        let subtitle_timing = subtitle_source
            .as_ref()
            .map(|s| self.input_timing(current_item, s, start_at_zero, is_live));

        let video_index = current_item
            .tracks
            .as_ref()
            .and_then(|t| t.video.as_ref())
            .and_then(|v| v.stream_index);

        let audio_index = current_item
            .tracks
            .as_ref()
            .and_then(|t| t.audio.as_ref())
            .and_then(|a| a.stream_index);

        let subtitle_index = current_item
            .tracks
            .as_ref()
            .and_then(|t| t.subtitle.as_ref())
            .and_then(|s| s.stream_index);

        let subtitle_input = match (
            subtitle_probe_result.clone(),
            subtitle_input_source,
            subtitle_timing,
        ) {
            (Some(s_probe), Some(s_in), Some(s_time)) => Some(ProbedInput {
                input_source: s_in,
                in_point: s_time.in_point,
                out_point: s_time.out_point,
                probe_result: s_probe,
                stream_index: subtitle_index,
            }),
            _ => None,
        };

        let (overlay_input, mut overlay_fifo) = match self.channel_config.overlay.as_ref() {
            // Tell the overlay producer (the etv-station daemon) this channel is
            // now watched so it spawns the overlay process, then open the fifo
            // ourselves and wait (bounded) for that producer to attach.
            //
            // A failure here is NOT an item failure. The overlay is decoration;
            // the program is the point. Propagating the error replaced the whole
            // item with black and silence, so a two-hour film became two hours of
            // black because a watermark writer was late — and it *is* routinely
            // late: the producer despawns its overlay when a channel goes cold,
            // and a viewer arriving in that same instant loses the race between
            // the producer's 5s respawn poll and this side's 20s writer timeout.
            // Observed in production 2026-08-16 on 011-madison, where the writer
            // attached 6 seconds after this side had already given up.
            //
            // So: log it and play the item bare. A missing watermark is a defect
            // a viewer can watch through; a black screen is not.
            Some(spec) => match self
                .signal_overlay_wanted_and_wait_ready(&spec.fifo_path)
                .await
            {
                Ok(fifo) => (
                    Some(OverlayInput {
                        // ffmpeg reads the descriptor we already opened, never the
                        // fifo path — an open it performed itself could not be
                        // bounded, and an unbounded one wedges the channel.
                        fifo_path: overlay_fifo_input_path(&fifo),
                        pixel_format: spec.pixel_format.clone(),
                        width: spec.width,
                        height: spec.height,
                        framerate: spec.framerate,
                        x: spec.x,
                        y: spec.y,
                    }),
                    Some(fifo),
                ),
                Err(e) => {
                    log::warn!("overlay unavailable for this item, playing without it: {e}");
                    (None, None)
                }
            },
            None => (None, None),
        };

        let mut input_settings = InputSettings {
            start: current_item.start,
            audio_input: ProbedInput {
                input_source: audio_input_source,
                in_point: audio_timing.in_point,
                out_point: audio_timing.out_point,
                probe_result: audio_probe_result.clone(),
                stream_index: audio_index,
            },
            video_input: ProbedInput {
                input_source: video_input_source,
                in_point: if video_probe_result.is_still_image() {
                    Duration::ZERO
                } else {
                    video_timing.in_point
                },
                out_point: video_timing.out_point,
                probe_result: video_probe_result.clone(),
                stream_index: video_index,
            },
            subtitle_input,
            watermark_input,
            overlay_input,
            subtitle_language_tag: self
                .channel_config
                .normalization
                .subtitle
                .language
                .tag
                .clone(),
        };

        let mut subtitle_source: Option<SubtitleSource> = None;
        if output_settings.subtitle_mode == SubtitleMode::Convert
            && let Some(subtitle_stream) = input_settings.select_subtitle_stream()
            && !subtitle_stream.is_subtitle_image()
            && let Some(input) = input_settings.subtitle_input.as_ref()
            && let Some(cues) = match &self.cached_subtitles {
                Some((id, c)) if id == &current_item.id => Some(Arc::clone(c)),
                _ => {
                    self.extract_and_convert_subs(input, subtitle_stream, current_item)
                        .await
                }
            }
        {
            subtitle_source = Some(SubtitleSource {
                cues,
                cursor: 0,
                next_segment_source_offset: input.in_point,
            });
        }

        let skip_embedded_text_subtitles = output_settings.subtitle_mode == SubtitleMode::Burn
            && input_settings
                .subtitle_input
                .as_ref()
                .is_some_and(|i| i.probe_result.streams.len() > 1)
            && input_settings
                .select_subtitle_stream()
                .is_some_and(|s| !s.is_subtitle_image());

        if skip_embedded_text_subtitles {
            log::warn!(
                "skipping embedded text subtitles for item {}; scheduler must extract to a sidecar file",
                current_item.id
            );
            input_settings.subtitle_input = None;
        }

        let pts_offset = output_settings.pts_offset;
        let mut pipeline_result =
            pipeline::generate_pipeline(&self.ffmpeg_info, input_settings, output_settings)?;
        pipeline_result.optimize();
        let mut args = pipeline_result.args();
        let envs = pipeline_result.envs();
        log::debug!("optimized pipeline: {}", args.join(" "));

        // Have ffmpeg report its own progress on stdout; see
        // [`log_ffmpeg_progress`]. A global option, so it must land before the
        // first output url or ffmpeg reads it as a trailing, unconsumed
        // argument.
        args.splice(0..0, [Cow::Borrowed("-progress"), Cow::Borrowed("pipe:1")]);

        self.playlist_manager
            .lock()
            .await
            .before_new_pipeline(pts_offset, subtitle_source)
            .await?;

        // stream current item
        let mut command = tokio::process::Command::new(&self.ffmpeg_path);
        command
            .args(args.iter().map(Cow::as_ref))
            .envs(
                envs.iter()
                    .map(|env| (env.key.as_str(), env.value.as_str())),
            )
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        #[cfg(unix)]
        if let Some(fifo) = overlay_fifo.as_ref() {
            use std::os::unix::io::AsRawFd;

            let fd = fifo.as_raw_fd();
            // SAFETY: the closure calls only fcntl, which is async-signal-safe —
            // the whole requirement on a pre_exec body. `fd` stays open in the
            // parent until after spawn returns, and fork duplicates it into the
            // child under the same number, so it is valid when this runs.
            unsafe {
                command.pre_exec(move || {
                    // Rust opens files close-on-exec, so the descriptor would
                    // vanish at exec. Clear the flag in place rather than
                    // dup2-ing onto a fixed number like 3: that number may
                    // already hold something std set up for this very spawn —
                    // a duplicate of the child's stdio, or the pipe it reports
                    // exec failures on — and clobbering it corrupts the spawn.
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }

        let mut ffmpeg_child = command
            .spawn()
            .map_err(|_| ChannelError::StreamFailure(String::from("failed to spawn ffmpeg")))?;

        // Drop our copy of the fifo read end now that ffmpeg holds its own.
        // Holding it would hide ffmpeg's exit from the producer, whose
        // reopen-on-BrokenPipe is what hands each new reader a fresh frame
        // boundary — without that it would render a torn, tiled overlay.
        drop(overlay_fifo.take());

        let stderr = ffmpeg_child
            .stderr
            .take()
            .ok_or(ChannelError::CaptureFFmpegStderrFailure)?;
        let stdout = ffmpeg_child
            .stdout
            .take()
            .ok_or(ChannelError::CaptureFFmpegStdoutFailure)?;
        let ring = Arc::new(std::sync::Mutex::new(VecDeque::<String>::with_capacity(
            STDERR_RING_LINES,
        )));

        let reader_ring = Arc::clone(&ring);
        let channel_number = self.channel_config.number().to_owned();
        let reader_handle = tokio::spawn(async move {
            tokio::join!(
                pump_ffmpeg_stderr(stderr, reader_ring),
                log_ffmpeg_progress(stdout, channel_number),
            );
        });

        log::debug!("waiting for ffmpeg to terminate...");

        tokio::select! {
            status = ffmpeg_child.wait() => {
                let status = status.map_err(|e| ChannelError::StreamFailure(e.to_string()))?;
                let _ = reader_handle.await;
                if !status.success() {
                    self.write_dossier(                        current_item,
                        &video_probe_result,                        &audio_probe_result,
                        subtitle_probe_result.as_ref(),
                        &ring,
                        format!("ffmpeg exited with code {status}")).await;
                    return Err(ChannelError::StreamFailure(format!(
                        "ffmpeg exited {status}"
                    )));
                } else if troubleshoot {
                    self.write_dossier(current_item, &video_probe_result,
                        &audio_probe_result, subtitle_probe_result.as_ref(),
                        &ring, "ffmpeg exited successfully".to_string()).await;
                } else {
                    self.cleanup_old_report().await;
                }
            }
            _ = self.timeout_notify.notified() => {
                ffmpeg_child.kill().await.ok();
                let _ = reader_handle.await;
                self.cleanup_old_report().await;
                let abort = self.playlist_manager.lock().await.abort().unwrap_or(SessionAbort::IdleTimeout);
                return Err(self.abort_error(abort));
            }
            _ = async {
                    loop {
                        let playlist_manager = self.playlist_manager.lock().await;
                        if OffsetDateTime::now_utc() - *playlist_manager.last_progress() > STALL_THRESHOLD {
                            break;
                        }
                        drop(playlist_manager);
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                } => {
                ffmpeg_child.kill().await.ok();
                let _ = reader_handle.await;
                self.write_dossier(current_item, &video_probe_result, &audio_probe_result,
                    subtitle_probe_result.as_ref(), &ring, "ffmpeg stalled".to_string()).await;
                return Err(ChannelError::Stalled(self.channel_config.number().to_owned()));
            }
            // Losing ground, but still producing. Distinct from the stall arm
            // above: ffmpeg is healthy and emitting segments, just slower than
            // they are being consumed, so waiting for it to stop producing
            // entirely means waiting until after the viewer has already run out.
            lead = async {
                    let mut armed = false;
                    loop {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        let playlist_manager = self.playlist_manager.lock().await;
                        let segment_frontier = *playlist_manager.last_segment_end();
                        drop(playlist_manager);
                        let lead = segment_frontier - OffsetDateTime::now_utc();
                        // What a restart would actually be assigned: the item
                        // from the frontier it would resume at to its finish.
                        let remaining = current_item.finish - segment_frontier;
                        let fire;
                        (armed, fire) = reburst_decision(armed, lead, remaining);
                        if fire {
                            break lead;
                        }
                    }
                } => {
                log::info!(
                    "lead down to {}s while still playing; restarting the item to rebuild the buffer",
                    lead.whole_seconds()
                );
                ffmpeg_child.kill().await.ok();
                let _ = reader_handle.await;
                self.cleanup_old_report().await;
                return Err(ChannelError::LeadExhausted);
            }
        }

        Ok(std::cmp::min(audio_timing.finish, video_timing.finish))
    }

    fn abort_error(&self, abort: SessionAbort) -> ChannelError {
        let number = self.channel_config.number().to_owned();
        match abort {
            SessionAbort::IdleTimeout => ChannelError::IdleTimeout(number),
            SessionAbort::SegmentStall => ChannelError::SegmentStall(number),
        }
    }

    async fn resolve_probe(
        &self,
        src: &PlayoutItemSource,
        input: &InputSource,
    ) -> Result<ProbeResult, ChannelError> {
        match src.probe_hint() {
            Some(hint) => {
                let path = input.input_path().ok_or(ChannelError::ProbeHintFailure)?;
                Ok(probe_hint_to_result(hint, path))
            }
            None => self.probe_source(input).await,
        }
    }

    async fn probe_source(&self, source: &InputSource) -> Result<ProbeResult, ChannelError> {
        let probe_deps = probe::ProbeDeps {
            ffprobe_path: &self.ffprobe_path,
            ffmpeg_path: &self.ffmpeg_path,
        };

        Ok(source.probe(&probe_deps).await?)
    }

    fn playout_source_to_input_source(
        &self,
        source: PlayoutItemSource,
    ) -> Result<InputSource, ChannelError> {
        match source {
            PlayoutItemSource::Local { path, .. } => {
                Ok(InputSource::Local(LocalInputSource { path }))
            }
            PlayoutItemSource::Lavfi { params, .. } => {
                Ok(InputSource::Lavfi(LavfiInputSource { params }))
            }
            PlayoutItemSource::Http {
                uri,
                headers,
                user_agent,
                timeout_us,
                reconnect,
                reconnect_delay_max,
                keep_alive,
                ..
            } => {
                let expanded_uri = expand_template(&uri)?;
                let expanded_headers: Vec<String> = headers
                    .unwrap_or_default()
                    .iter()
                    .map(|h| expand_template(h))
                    .collect::<Result<Vec<_>, _>>()?;
                let expanded_ua = user_agent.as_deref().map(expand_template).transpose()?;

                Ok(InputSource::Http(HttpInputSource {
                    uri: expanded_uri,
                    options: HttpInputOptions {
                        headers: expanded_headers,
                        user_agent: expanded_ua,
                        timeout_us,
                        reconnect: reconnect.unwrap_or(true),
                        reconnect_delay_max,
                        keep_alive,
                    },
                }))
            }
            PlayoutItemSource::Rtsp {
                uri, timeout_us, ..
            } => {
                let expanded_uri = expand_template(&uri)?;

                Ok(InputSource::Rtsp(RtspInputSource {
                    uri: expanded_uri,
                    options: RtspInputOptions { timeout_us },
                }))
            }
            PlayoutItemSource::Script { command, args, .. } => {
                let url = self.local_proxy_server.register_script(ScriptCommand {
                    command: expand_template(&command)?,
                    args: args
                        .iter()
                        .map(|a| expand_template(a))
                        .collect::<Result<_, _>>()?,
                })?;
                Ok(InputSource::Http(HttpInputSource {
                    uri: url,
                    options: HttpInputOptions {
                        reconnect: false,
                        ..Default::default()
                    },
                }))
            }
            PlayoutItemSource::Dynamic { .. } => {
                Err(ChannelError::DynamicSourceCannotBePlayedDirectly)
            }
        }
    }

    fn input_timing(
        &self,
        current_item: &PlayoutItem,
        source: &PlayoutItemSource,
        start_at_zero: bool,
        is_live: bool,
    ) -> TimingResult {
        let item_start = current_item.start;
        let item_finish = current_item.finish;
        let item_duration = current_item.finish - current_item.start;
        let item_in_point_base_ms = match source {
            PlayoutItemSource::Local { in_point_ms, .. }
            | PlayoutItemSource::Http { in_point_ms, .. } => in_point_ms.unwrap_or(0),
            _ => 0,
        };
        let item_out_point_ms = match source {
            PlayoutItemSource::Local { out_point_ms, .. }
            | PlayoutItemSource::Http { out_point_ms, .. } => out_point_ms
                .unwrap_or(item_in_point_base_ms + item_duration.whole_milliseconds() as u64),
            _ => item_in_point_base_ms + item_duration.whole_milliseconds() as u64,
        };

        // live content never seeks
        if is_live {
            return TimingResult {
                in_point: Duration::ZERO,
                out_point: Duration::from_millis(item_duration.whole_milliseconds() as u64),
                finish: item_finish,
            };
        }

        let effective_now = if start_at_zero {
            item_start
        } else {
            self.transcoded_until
        };

        let progress_ms = if start_at_zero {
            0
        } else {
            (effective_now - item_start).whole_milliseconds().max(0) as u64
        };
        let effective_in_point = Duration::from_millis(item_in_point_base_ms + progress_ms);

        // One process plays the item all the way out. Cutting it short here and
        // letting the next process pick up the remainder is what used to stamp
        // an EXT-X-DISCONTINUITY into the middle of an item.
        TimingResult {
            in_point: effective_in_point,
            out_point: Duration::from_millis(item_out_point_ms),
            finish: item_finish,
        }
    }

    fn fake_playout_item(&self, next_start: Option<OffsetDateTime>) -> PlayoutItem {
        let width = self
            .channel_config
            .normalization
            .video
            .width
            .unwrap_or(1920);

        let height = self
            .channel_config
            .normalization
            .video
            .height
            .unwrap_or(1080);

        let duration = Duration::from_mins(1);

        PlayoutItem {
            id: uuid::Uuid::new_v4().to_string(),
            start: self.transcoded_until,
            finish: next_start.unwrap_or(self.transcoded_until + duration),
            source: None,
            tracks: Some(PlayoutItemTracks {
                audio: Some(TrackSelection {
                    source: Some(PlayoutItemSource::Lavfi {
                        params: String::from("anullsrc=channel_layout=stereo:sample_rate=48000"),
                        probe_hint: Some(ProbeHint {
                            video: Vec::new(),
                            audio: vec![AudioHint {
                                stream_index: 0,
                                codec: String::from("pcm_s16le"),
                                channels: 2,
                            }],
                            subtitle: Vec::new(),
                            format_name: Some(String::from("mpegts")),
                            duration_ms: Some(duration.as_millis() as u64),
                        }),
                    }),
                    stream_index: None,
                }),
                video: Some(TrackSelection {
                    source: Some(PlayoutItemSource::Lavfi {
                        params: format!("color=c=black:s={}x{}", width, height),
                        probe_hint: Some(ProbeHint {
                            video: vec![VideoHint::new(
                                String::from("rawvideo"),
                                width,
                                height,
                                String::from("yuv420p"),
                            )],
                            audio: Vec::new(),
                            subtitle: Vec::new(),
                            format_name: Some(String::from("mpegts")),
                            duration_ms: Some(duration.as_millis() as u64),
                        }),
                    }),
                    stream_index: None,
                }),
                subtitle: None,
            }),
            watermark: None,
            program: None,
            metadata: None,
        }
    }

    async fn resolve_dynamic_item(
        &self,
        start: &OffsetDateTime,
        dynamic_item: &PlayoutItem,
    ) -> Result<PlayoutItem, ChannelError> {
        let Some(PlayoutItemSource::Dynamic {
            uri,
            headers,
            user_agent,
            timeout_us,
        }) = &dynamic_item.source
        else {
            return Err(ChannelError::DynamicSourceRequired);
        };

        let expanded_uri = expand_template(uri)?;
        let expanded_headers: Vec<String> = headers
            .iter()
            .flatten()
            .map(|h| expand_template(h))
            .collect::<Result<Vec<_>, _>>()?;
        let expanded_ua = user_agent.as_deref().map(expand_template).transpose()?;

        let mut header_map = HeaderMap::new();
        for h in &expanded_headers {
            let Some((name, value)) = h.split_once(':') else {
                continue;
            };
            let name = HeaderName::from_bytes(name.trim().as_bytes())
                .map_err(|e| ChannelError::DynamicSourceFailure(format!("bad header name: {e}")))?;
            let value = HeaderValue::from_str(value.trim()).map_err(|e| {
                ChannelError::DynamicSourceFailure(format!("bad header value: {e}"))
            })?;
            header_map.insert(name, value);
        }
        if let Some(ua) = expanded_ua.as_deref() {
            header_map.insert(
                USER_AGENT,
                HeaderValue::from_str(ua).map_err(|e| {
                    ChannelError::DynamicSourceFailure(format!("bad user agent: {e}"))
                })?,
            );
        }

        header_map.insert(
            HeaderName::from_static("x-etv-dynamic-id"),
            HeaderValue::from_str(&dynamic_item.id).map_err(|e| {
                ChannelError::DynamicSourceFailure(format!("bad dynamic source id {e}"))
            })?,
        );

        header_map.insert(
            HeaderName::from_static("x-etv-channel"),
            HeaderValue::from_str(self.channel_config.number()).map_err(|e| {
                ChannelError::DynamicSourceFailure(format!("bad channel number {e}"))
            })?,
        );

        header_map.insert(
            HeaderName::from_static("x-etv-now"),
            HeaderValue::from_str(&start.format(&time::format_description::well_known::Rfc3339)?)
                .map_err(|e| ChannelError::DynamicSourceFailure(format!("bad time value: {e}")))?,
        );

        header_map.insert(
            HeaderName::from_static("x-etv-until"),
            HeaderValue::from_str(
                &dynamic_item
                    .finish
                    .format(&time::format_description::well_known::Rfc3339)?,
            )
            .map_err(|e| ChannelError::DynamicSourceFailure(format!("bad time value: {e}")))?,
        );

        let timeout = timeout_us
            .map(Duration::from_micros)
            .unwrap_or_else(|| Duration::from_secs(10));

        let mut item: PlayoutItem = self
            .dynamic_http_client
            .get(&expanded_uri)
            .headers(header_map)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| ChannelError::DynamicSourceFailure(e.to_string()))?
            .error_for_status()
            .map_err(|e| ChannelError::DynamicSourceFailure(e.to_string()))?
            .json()
            .await
            .map_err(|e| ChannelError::DynamicSourceFailure(e.to_string()))?;

        // always start at the requested time
        if item.start != *start {
            let duration = item.finish - item.start;
            item.start = *start;
            item.finish = *start + duration;
        }

        // always clamp the finish time
        if item.finish > dynamic_item.finish {
            item.finish = dynamic_item.finish;
        }

        if item.finish <= item.start {
            return Err(ChannelError::DynamicSourceNoRemainingTime);
        }

        if let Some(PlayoutItemSource::Dynamic { .. }) = item.source {
            return Err(ChannelError::DynamicSourceCannotRecurse);
        }

        if let Some(tracks) = &item.tracks {
            if let Some(audio) = &tracks.audio
                && let Some(PlayoutItemSource::Dynamic { .. }) = audio.source
            {
                return Err(ChannelError::DynamicSourceCannotRecurse);
            }

            if let Some(video) = &tracks.video
                && let Some(PlayoutItemSource::Dynamic { .. }) = video.source
            {
                return Err(ChannelError::DynamicSourceCannotRecurse);
            }

            if let Some(subtitle) = &tracks.subtitle
                && let Some(PlayoutItemSource::Dynamic { .. }) = subtitle.source
            {
                return Err(ChannelError::DynamicSourceCannotRecurse);
            }
        }

        if let Some(watermark) = &item.watermark
            && let PlayoutItemSource::Dynamic { .. } = watermark.source
        {
            return Err(ChannelError::DynamicSourceCannotRecurse);
        }

        Ok(item)
    }

    fn resolve_source<F>(item: &PlayoutItem, pick: F) -> Option<PlayoutItemSource>
    where
        F: FnOnce(&PlayoutItemTracks) -> Option<&TrackSelection>,
    {
        item.tracks
            .as_ref()
            .and_then(pick)
            .and_then(|sel| sel.source.clone())
            .or_else(|| item.source.clone())
    }

    /// Subtitles are opt-in, which is why this does not go through
    /// [`Self::resolve_source`].
    ///
    /// Audio and video fall back to the item's own source because every item
    /// must have both — an item that names neither still has to play.
    /// Subtitles are different: most items carry no `tracks.subtitle` at all,
    /// and the same fallback there would make "this item names no subtitle
    /// track" mean exactly the same thing as "burn in whatever subtitle the
    /// file happens to contain". A Blu-ray rip whose only subtitle tracks are
    /// PGS then gets a picture subtitle composited onto every frame that
    /// nobody asked for and no viewer can switch off — while the channel's own
    /// selectable WebVTT rendition goes out carrying nothing.
    ///
    /// So an absent selection means absent. A selection that names a
    /// `stream_index` but no `source` still means "that track of this item's
    /// own file", which is how an item asks for subtitles it does carry.
    fn resolve_subtitle_source(item: &PlayoutItem) -> Option<PlayoutItemSource> {
        item.tracks
            .as_ref()
            .and_then(|t| t.subtitle.as_ref())
            .and_then(|sel| sel.source.clone().or_else(|| item.source.clone()))
    }

    async fn extract_and_convert_subs(
        &mut self,
        input: &ProbedInput,
        subtitle_stream: &ProbeResultVideoStream,
        current_item: &PlayoutItem,
    ) -> Option<Arc<Vec<Cue>>> {
        {
            match ffpipeline::web_vtt::convert_to_vtt(&self.ffmpeg_path, input, subtitle_stream)
                .await
            {
                Ok(temp_file) => match ffpipeline::web_vtt::parse_file(temp_file.path()).await {
                    Ok(extracted_cues) => {
                        let arc = Arc::new(extracted_cues);
                        self.cached_subtitles = Some((current_item.id.clone(), Arc::clone(&arc)));
                        Some(arc)
                    }
                    Err(err) => {
                        log::warn!("error parsing converted vtt: {err}");
                        None
                    }
                },
                Err(err) => {
                    log::warn!("error converting subtitle to vtt: {err}");
                    None
                }
            }
        }
    }

    async fn cleanup_old_report(&self) {
        if let Some(reports_folder) = &self.channel_config.ffmpeg.reports_folder {
            let report_file = PathBuf::from(reports_folder)
                .join(format!(".in-flight-{}.log", self.channel_config.number()));
            if report_file.exists() {
                let _ = tokio::fs::remove_file(report_file).await;
            }
        }
    }

    async fn write_dossier(
        &self,
        current_item: &PlayoutItem,
        video_probe_result: &ProbeResult,
        audio_probe_result: &ProbeResult,
        subtitle_probe_result: Option<&ProbeResult>,
        ring: &Arc<std::sync::Mutex<VecDeque<String>>>,
        outcome: String,
    ) {
        let stderr_tail: Vec<_> = ring
            .lock()
            .map(|r| r.iter().cloned().collect())
            .unwrap_or_default();

        let mut builder = DossierBuilder::new(&self.channel_config, &self.ffmpeg_info)
            .item(current_item)
            .stderr(stderr_tail)
            .video(video_probe_result)
            .audio(audio_probe_result)
            .outcome(outcome);

        if let Some(accel) = &self.hw_accel {
            builder = builder.accel(accel);
        }

        if let Some(subtitle_probe_result) = subtitle_probe_result {
            builder = builder.subtitle(subtitle_probe_result);
        }

        if let Some(report_source_file) =
            self.channel_config
                .ffmpeg
                .reports_folder
                .as_ref()
                .map(|folder| {
                    PathBuf::from(folder)
                        .join(format!(".in-flight-{}.log", self.channel_config.number()))
                })
        {
            builder = builder.report_source(report_source_file);
        }

        let dossier = builder.build();
        if let Err(err) = dossier.write().await {
            log::error!("failed to save dossier: {err}");
        }
    }
}

/// Mirror ffmpeg's stderr to this process's stderr, keeping the most recent
/// [`STDERR_RING_LINES`] lines in `ring` for the failure dossier.
async fn pump_ffmpeg_stderr(
    stderr: tokio::process::ChildStderr,
    ring: Arc<std::sync::Mutex<VecDeque<String>>>,
) {
    let mut lines = tokio::io::BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        eprintln!("{line}");
        if let Ok(mut buf) = ring.lock() {
            if buf.len() == STDERR_RING_LINES {
                buf.pop_front();
            }
            buf.push_back(line);
        }
    }
}

/// Accumulate one line of ffmpeg's `-progress` stream into `fields`. A
/// `progress=` line closes out a report: returns it formatted and clears
/// `fields` for the next one. Any other `key=value` line is folded in and
/// this returns `None`. A line with no `=` (never emitted by `-progress`,
/// but ffmpeg's output is not a contract) is ignored rather than panicking.
fn accumulate_progress_line(line: &str, fields: &mut Vec<String>) -> Option<String> {
    let (key, value) = line.split_once('=')?;
    let (key, value) = (key.trim(), value.trim());
    if key == "progress" {
        let report = format!("{} progress={value}", fields.join(" "));
        fields.clear();
        Some(report)
    } else {
        fields.push(format!("{key}={value}"));
        None
    }
}

/// Log the `-progress pipe:1` stream ffmpeg writes to stdout: one `key=value`
/// line per field (frame, fps, out_time, speed, ...), terminated by
/// `progress=continue` or `progress=end`, which closes one full report.
///
/// This is a category log, not a module-scoped one, so it toggles independently
/// of everything else in this crate: `RUST_LOG=debug,ffmpeg_progress=off`. On by
/// default for now — the tune-in stalls it exists to diagnose are otherwise
/// invisible without externally wrapping the ffmpeg binary.
async fn log_ffmpeg_progress(stdout: tokio::process::ChildStdout, channel_number: String) {
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let mut fields: Vec<String> = Vec::new();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(report) = accumulate_progress_line(&line, &mut fields) {
            log::debug!(target: "ffmpeg_progress", "channel {channel_number}: {report}");
        }
    }
}

/// Touch `.overlay-wanted` in `playout_folder`, signalling the overlay
/// producer that this channel is watched, then open `fifo_path` for reading —
/// bounded by `timeout`.
///
/// Deliberately no wait on `.overlay-ready` before that open: the overlay
/// only touches that marker once its own `open_for_writing()` returns
/// (crates/etv-overlay/src/bin/etv-overlay.rs), and that fifo rendezvous
/// blocks until a reader attaches — this function's caller is that reader.
/// Such a wait would be waiting on an effect of the open below, so it burned
/// its full 5s timeout on every tune-in and proceeded regardless. The fifo
/// open is the real, correctly-bounded rendezvous — see
/// `signal_overlay_wanted_does_not_wait_before_opening_the_fifo` below, which
/// pins the "no preceding wait" behavior directly.
async fn touch_overlay_wanted_and_open_fifo(
    playout_folder: &Path,
    fifo_path: &str,
    timeout: Duration,
) -> Result<OverlayFifoReader, ChannelError> {
    let wanted = playout_folder.join(OVERLAY_WANTED_FILE_NAME);
    // Write empty content: creates the marker, or refreshes its mtime so the
    // producer treats it as a fresh watch request.
    if let Err(e) = tokio::fs::write(&wanted, b"").await {
        // Not fatal on its own — the producer may already be running from an
        // earlier request. If it isn't, the bounded wait below reports it.
        log::warn!(
            "failed to touch overlay-wanted marker {}: {e}",
            wanted.display()
        );
    }
    let path = PathBuf::from(fifo_path);
    tokio::task::spawn_blocking(move || open_overlay_fifo(&path, timeout))
        .await
        .map_err(|e| ChannelError::StreamFailure(format!("overlay fifo open task failed: {e}")))?
}

/// Open the overlay rawvideo fifo for read and wait — bounded by `timeout` —
/// for the producer to attach and start writing.
///
/// This exists because a plain blocking `open()` on a fifo with no writer never
/// returns. Letting ffmpeg do that open is what wedges a channel permanently:
/// ffmpeg parks in `open()`, emits no segments, and nothing anywhere reports an
/// error. Owning the open here is the only way to put a deadline on it.
///
/// Note the asymmetry with the producer side (`FifoWriter::open_for_writing` in
/// etv-station): a *writer*'s nonblocking open returns `ENXIO` while no reader
/// is present, so that end can retry on the open call itself. A *reader*'s
/// nonblocking open always succeeds immediately, writer or not, so there is
/// nothing to retry on here. Writer presence is detected by polling the fd for
/// `POLLIN` instead: the producer writes its first frame as soon as its own
/// open succeeds, and polling — unlike reading — does not consume the bytes
/// ffmpeg needs in order to stay frame-aligned.
#[cfg(unix)]
fn open_overlay_fifo(path: &Path, timeout: Duration) -> Result<OverlayFifoReader, ChannelError> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| {
            ChannelError::StreamFailure(format!(
                "failed to open overlay fifo {}: {e}",
                path.display()
            ))
        })?;

    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(ChannelError::StreamFailure(format!(
                "no writer attached to overlay fifo {} within {timeout:?}",
                path.display()
            )));
        }

        let wait = remaining.min(OVERLAY_WRITER_POLL_INTERVAL);
        let mut fds = [libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        // SAFETY: `fds` is a valid, correctly-sized pollfd array and `file` owns
        // the descriptor it names for the whole call.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 1, wait.as_millis() as libc::c_int) };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(ChannelError::StreamFailure(format!(
                "poll on overlay fifo {}: {e}",
                path.display()
            )));
        }

        if fds[0].revents & libc::POLLIN != 0 {
            // A writer is attached and has produced data. Hand ffmpeg a
            // blocking fd so a slow producer applies back-pressure instead of
            // making ffmpeg spin on EAGAIN.
            clear_nonblocking(&file)?;
            return Ok(file);
        }

        if rc > 0 {
            // Revents without POLLIN means POLLHUP: no writer yet. poll returns
            // instantly in that state, so pace the retry rather than spin.
            std::thread::sleep(wait);
        }
    }
}

#[cfg(not(unix))]
fn open_overlay_fifo(path: &Path, _timeout: Duration) -> Result<OverlayFifoReader, ChannelError> {
    Err(ChannelError::StreamFailure(format!(
        "live overlay input {} requires a unix fifo",
        path.display()
    )))
}

/// Clear `O_NONBLOCK` on a descriptor opened nonblocking, so subsequent reads
/// block instead of returning `EAGAIN`.
#[cfg(unix)]
fn clear_nonblocking(file: &std::fs::File) -> Result<(), ChannelError> {
    use std::os::unix::io::AsRawFd;

    let fd = file.as_raw_fd();
    // SAFETY: `fd` is owned by `file` for the duration of both calls.
    let result = unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            -1
        } else {
            libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK)
        }
    };
    if result < 0 {
        return Err(ChannelError::StreamFailure(format!(
            "failed to clear O_NONBLOCK on overlay fifo: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// Whether a still-playing item should be restarted to rebuild its lead.
///
/// Returns the next `armed` state and whether to fire now. Split out from the
/// select arm that drives it because the arming half is the part that is easy to
/// get wrong and impossible to observe once it is inside a `tokio::select!`.
///
/// `remaining` is how much of the item is still untranscoded — the gap between
/// the item's finish and the segment frontier the restart would resume from.
/// A reburst rebuilds the buffer for the *rest of the item*, so when there is no
/// meaningful rest left it buys nothing and costs everything: it kills a healthy
/// ffmpeg and respawns one whose entire assignment is a fraction of a segment.
/// Measured in production on 2026-08-22: channel 2 fired at `20:30:59Z` with the
/// item finishing at `20:31:18.613Z`, and the replacement process was handed
/// `-t 153ms` against `-hls_time 4` — 3.8% of one segment. Channel 1 got
/// `-t 1336ms` the same way at `20:47:34Z`.
///
/// The threshold is [`REBURST_AT_LEAD`] because that constant is already sized
/// against the respawn — see its own docs: "set above the 4-10s a respawn takes".
/// What a restart buys is a *fresh* process reading unthrottled, so it needs
/// enough content left to burst through before the respawn cost eats the gain.
/// Under this threshold there is less item left than a respawn costs, so the
/// replacement cannot come out ahead however far behind the old one had fallen —
/// this holds fire even on a negative lead, where the restart would spend 4-10s
/// producing nothing and then face the same remaining work at the same speed.
/// The item ends within [`REBURST_AT_LEAD`] either way, and ending normally is
/// what drops the state to [`ChannelSessionState::Zero`] so the next item builds
/// a fresh [`INITIAL_BURST`] from its own beginning.
fn reburst_decision(armed: bool, lead: time::Duration, remaining: time::Duration) -> (bool, bool) {
    if armed {
        (true, lead <= REBURST_AT_LEAD && remaining > REBURST_AT_LEAD)
    } else {
        // Arming needs a lead that has actually been built. Firing on the way UP
        // would restart the item forever without ever playing any of it.
        (lead >= REBURST_ARM_AT_LEAD, false)
    }
}

fn playout_location_to_pipeline(value: &WatermarkLocation) -> ffpipeline::input::WatermarkLocation {
    match value {
        WatermarkLocation::TopLeft => ffpipeline::input::WatermarkLocation::TopLeft,
        WatermarkLocation::TopCenter => ffpipeline::input::WatermarkLocation::TopCenter,
        WatermarkLocation::TopRight => ffpipeline::input::WatermarkLocation::TopRight,
        WatermarkLocation::CenterLeft => ffpipeline::input::WatermarkLocation::CenterLeft,
        WatermarkLocation::Center => ffpipeline::input::WatermarkLocation::Center,
        WatermarkLocation::CenterRight => ffpipeline::input::WatermarkLocation::CenterRight,
        WatermarkLocation::BottomLeft => ffpipeline::input::WatermarkLocation::BottomLeft,
        WatermarkLocation::BottomCenter => ffpipeline::input::WatermarkLocation::BottomCenter,
        WatermarkLocation::BottomRight => ffpipeline::input::WatermarkLocation::BottomRight,
    }
}

fn playout_timing_to_pipeline(
    value: Option<&WatermarkTiming>,
) -> Option<ffpipeline::input::WatermarkTiming> {
    value.map(|timing| {
        let WatermarkTiming::Periodic {
            clock,
            frequency_ms,
            phase_offset_ms,
            disable_after_ms,
            fade_ms,
            hold_ms,
        } = timing;

        let clock = match clock {
            PeriodicClock::Content => ffpipeline::input::PeriodicClock::Content,
            PeriodicClock::Wall => ffpipeline::input::PeriodicClock::Wall,
        };

        let periodic_timing = ffpipeline::input::PeriodicTiming {
            clock,
            frequency_ms: *frequency_ms,
            phase_offset_ms: *phase_offset_ms,
            disable_after_ms: *disable_after_ms,
            fade_ms: *fade_ms,
            hold_ms: *hold_ms,
        };

        ffpipeline::input::WatermarkTiming::Periodic(periodic_timing)
    })
}

fn source_is_live(source: &PlayoutItemSource) -> bool {
    matches!(
        source,
        PlayoutItemSource::Http {
            is_live: Some(true),
            ..
        } | PlayoutItemSource::Script {
            is_live: Some(true),
            ..
        } | PlayoutItemSource::Rtsp { .. }
    )
}

fn probe_hint_to_result(hint: &ProbeHint, path: String) -> ProbeResult {
    let video = hint.video.iter().map(|v| {
        ProbeResultStream::Video(Box::new(ProbeResultVideoStream {
            stream_index: v.stream_index,
            codec: v.codec.to_lowercase(),
            codec_type: CodecType::Video,
            profile: v.profile.clone().unwrap_or_default().to_lowercase(),
            height: Some(v.height),
            width: Some(v.width),
            pix_fmt: v.pix_fmt.clone(),
            color_params: ProbeResultColorParams {
                color_range: v.color_range.clone(),
                color_space: v.color_space.clone(),
                color_transfer: v.color_transfer.clone(),
                color_primaries: v.color_primaries.clone(),
            },
            field_order: v.field_order.clone(),
            frame_rate: v
                .frame_rate
                .as_deref()
                .map(FrameRate::parse)
                .unwrap_or_default(),
            sample_aspect_ratio: v.sample_aspect_ratio.clone(),
            display_aspect_ratio: v.display_aspect_ratio.clone(),
            // The probe-hint schema (VideoHint) carries no language field;
            // this path is unused by etv-station, which always ffprobes
            // local files directly rather than supplying a hint.
            language: None,
        }))
    });

    let audio = hint.audio.iter().map(|a| {
        ProbeResultStream::Audio(ProbeResultAudioStream {
            stream_index: a.stream_index,
            codec: a.codec.to_lowercase(),
            channels: a.channels,
        })
    });

    let subtitle = hint.subtitle.iter().map(|s| {
        ProbeResultStream::Video(Box::new(ProbeResultVideoStream {
            stream_index: s.stream_index,
            codec: s.codec.to_lowercase(),
            codec_type: CodecType::Subtitle,
            profile: String::new(),
            height: None,
            width: None,
            pix_fmt: String::new(),
            color_params: ProbeResultColorParams::default(),
            field_order: None,
            frame_rate: FrameRate::default(),
            sample_aspect_ratio: None,
            display_aspect_ratio: None,
            // SubtitleHint carries no language field either; see the video
            // hint above for why this path never needs one today.
            language: None,
        }))
    });

    ProbeResult {
        path,
        streams: video.chain(audio).chain(subtitle).collect(),
        duration: hint.duration_ms.map(Duration::from_millis),
        format_name: hint.format_name.clone().or(Some(String::from("mpegts"))),
    }
}

#[cfg(test)]
mod reburst_tests {
    use super::*;

    /// Most of these cases are about the lead, not about how much item is left,
    /// so they pass a remaining that is comfortably long enough to restart.
    const PLENTY_REMAINING: time::Duration = time::Duration::minutes(20);

    /// A fresh item starts with no lead at all. Firing here would kill the
    /// transcode before it had produced anything, restart it, and do the same
    /// again — the film would never play.
    #[test]
    fn a_lead_still_being_built_does_not_fire() {
        let (armed, fire) = reburst_decision(false, time::Duration::seconds(0), PLENTY_REMAINING);
        assert!(!armed, "must not arm before the lead is built");
        assert!(!fire);

        let (armed, fire) = reburst_decision(
            false,
            REBURST_AT_LEAD - time::Duration::seconds(1),
            PLENTY_REMAINING,
        );
        assert!(!armed, "a lead below the arming level is still the way up");
        assert!(!fire, "firing on the way up would restart the item forever");
    }

    #[test]
    fn arming_needs_the_lead_to_reach_the_arming_level() {
        let (armed, fire) = reburst_decision(false, REBURST_ARM_AT_LEAD, PLENTY_REMAINING);
        assert!(armed);
        assert!(
            !fire,
            "arming is not firing — the lead is healthy at this point"
        );
    }

    /// The case this exists for: the lead was built, then drained by a
    /// transcode running under wall-clock speed.
    #[test]
    fn an_armed_item_fires_once_the_lead_drains() {
        let (armed, fire) = reburst_decision(true, REBURST_AT_LEAD, PLENTY_REMAINING);
        assert!(
            armed,
            "arming survives so a recovering lead cannot disarm it"
        );
        assert!(fire);

        let (_, fire) = reburst_decision(
            true,
            REBURST_AT_LEAD + time::Duration::seconds(1),
            PLENTY_REMAINING,
        );
        assert!(!fire, "still has lead to spare");
    }

    /// A lead that has gone negative means the viewer has already caught up with
    /// the encoder. It must fire, not be treated as an unreachable state.
    #[test]
    fn a_negative_lead_fires() {
        let (_, fire) = reburst_decision(true, time::Duration::seconds(-30), PLENTY_REMAINING);
        assert!(fire);
    }

    /// The real production case, replayed. Channel 2 fired at `20:30:59Z` with
    /// the item finishing at `20:31:18.613Z` and the segment frontier already at
    /// `20:31:18.460Z`, so the restart's whole assignment was 153ms — 3.8% of one
    /// 4s segment. Channel 1's was 1336ms the same way. Both must hold fire.
    #[test]
    fn a_sub_segment_remainder_does_not_fire() {
        for remaining in [
            time::Duration::milliseconds(153),
            time::Duration::milliseconds(1336),
            time::Duration::milliseconds(3619),
        ] {
            let (armed, fire) = reburst_decision(true, REBURST_AT_LEAD, remaining);
            assert!(armed, "holding fire must not disarm the check");
            assert!(
                !fire,
                "restarting to transcode {}ms buys nothing and kills a healthy ffmpeg",
                remaining.whole_milliseconds()
            );
        }
    }

    /// The boundary sits at [`REBURST_AT_LEAD`] because that constant is already
    /// sized above what a respawn costs. Below it, the item ends before a
    /// replacement process could burst through what is left.
    #[test]
    fn the_remaining_gate_sits_at_the_respawn_cost() {
        let (_, fire) = reburst_decision(true, REBURST_AT_LEAD, REBURST_AT_LEAD);
        assert!(!fire, "less item left than a respawn costs — let it finish");

        let (_, fire) = reburst_decision(
            true,
            REBURST_AT_LEAD,
            REBURST_AT_LEAD + time::Duration::seconds(1),
        );
        assert!(fire, "enough item left for a fresh burst to pay for itself");
    }

    /// A lead deep in the negative with the item nearly over is the case the
    /// respawn-cost argument has to carry on its own: there is no buffer in hand,
    /// and firing anyway would spend 4-10s producing nothing before facing the
    /// same remaining work at the same speed.
    #[test]
    fn a_negative_lead_does_not_fire_on_a_nearly_finished_item() {
        let (_, fire) = reburst_decision(
            true,
            time::Duration::seconds(-30),
            time::Duration::seconds(5),
        );
        assert!(!fire, "a respawn cannot outrun 5s of work it has 5s to do");
    }

    /// An item whose frontier has passed its finish is fully transcoded. A
    /// negative remaining must read as "nothing left", not wrap into firing.
    #[test]
    fn a_fully_transcoded_item_does_not_fire() {
        let (_, fire) = reburst_decision(true, time::Duration::seconds(-30), time::Duration::ZERO);
        assert!(!fire);

        let (_, fire) = reburst_decision(
            true,
            time::Duration::seconds(-30),
            time::Duration::seconds(-5),
        );
        assert!(!fire, "the frontier is already past the item's finish");
    }

    /// The threshold has to leave room for the respawn itself. A channel takes
    /// 4-10s to produce its first segment, so firing at a lead shorter than that
    /// would hand the viewer a gap rather than avoid one.
    #[test]
    fn the_threshold_leaves_room_for_a_respawn() {
        assert!(
            REBURST_AT_LEAD >= time::Duration::seconds(12),
            "too tight to respawn before the viewer runs dry"
        );
        assert!(
            REBURST_ARM_AT_LEAD > REBURST_AT_LEAD,
            "arming level must sit above the firing level or the two race"
        );
        assert!(
            TARGET_BUFFER > REBURST_ARM_AT_LEAD,
            "a full buffer must comfortably arm the check"
        );
    }
}

#[cfg(test)]
mod source_tests {
    use super::*;

    const MOVIE: &str = "/media/movies/bluray-rip.mkv";
    const SIDECAR: &str = "/media/movies/bluray-rip.en.srt";

    /// Build an item from JSON rather than by hand: a real playout file is
    /// JSON, and "the item names no subtitle track" is a shape that only
    /// exists once serde has turned an absent key into `None`.
    fn item(tracks: &str) -> PlayoutItem {
        let json = format!(
            r#"{{
                "id": "test",
                "start": "2026-08-12T21:00:00Z",
                "finish": "2026-08-12T23:00:00Z",
                "source": {{"source_type": "local", "path": "{MOVIE}"}}
                {tracks}
            }}"#
        );
        serde_json::from_str(&json).expect("valid playout item")
    }

    fn path_of(source: Option<PlayoutItemSource>) -> Option<String> {
        match source {
            Some(PlayoutItemSource::Local { path, .. }) => Some(path),
            _ => None,
        }
    }

    /// The regression this exists for: an item that says nothing about
    /// subtitles used to resolve to its own media file, so a Blu-ray rip got
    /// its first PGS track composited onto every frame. Nothing requested
    /// that, and burning it in costs more than the video itself.
    #[test]
    fn an_item_naming_no_subtitle_track_gets_no_subtitles() {
        assert_eq!(
            ChannelSession::resolve_subtitle_source(&item("")),
            None,
            "an absent subtitle selection must mean absent"
        );
    }

    /// An empty `tracks` object is still no selection.
    #[test]
    fn an_item_with_tracks_but_no_subtitle_selection_gets_no_subtitles() {
        assert_eq!(
            ChannelSession::resolve_subtitle_source(&item(r#", "tracks": {}"#)),
            None
        );
    }

    /// How an item asks for a subtitle it carries itself: name the track, not
    /// the file. This must keep working, or nothing can opt in.
    #[test]
    fn a_subtitle_selection_with_only_a_stream_index_uses_the_items_own_file() {
        let resolved = ChannelSession::resolve_subtitle_source(&item(
            r#", "tracks": {"subtitle": {"stream_index": 3}}"#,
        ));
        assert_eq!(path_of(resolved).as_deref(), Some(MOVIE));
    }

    /// A subtitle track living in a separate file still resolves to that file.
    #[test]
    fn a_subtitle_selection_naming_its_own_source_uses_that_source() {
        let resolved = ChannelSession::resolve_subtitle_source(&item(&format!(
            r#", "tracks": {{"subtitle": {{"source": {{"source_type": "local", "path": "{SIDECAR}"}}}}}}"#
        )));
        assert_eq!(path_of(resolved).as_deref(), Some(SIDECAR));
    }

    /// Audio and video keep the fallback subtitles just lost: every item must
    /// have both, so an item that names neither still has to play.
    #[test]
    fn audio_and_video_still_fall_back_to_the_items_own_source() {
        let bare = item("");
        assert_eq!(
            path_of(ChannelSession::resolve_source(&bare, |t| t.audio.as_ref())).as_deref(),
            Some(MOVIE)
        );
        assert_eq!(
            path_of(ChannelSession::resolve_source(&bare, |t| t.video.as_ref())).as_deref(),
            Some(MOVIE)
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::{Read, Write};

    use super::*;

    fn make_fifo(dir: &Path) -> PathBuf {
        let path = dir.join("overlay.fifo");
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `c_path` is a valid NUL-terminated string for the call.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo: {}", std::io::Error::last_os_error());
        path
    }

    /// The wedge this guards against: with no writer attached, the read side
    /// must give up on a deadline instead of parking forever the way ffmpeg's
    /// own blocking `open()` does.
    #[test]
    fn open_overlay_fifo_gives_up_when_no_writer_attaches() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_fifo(dir.path());

        let start = std::time::Instant::now();
        let result = open_overlay_fifo(&path, Duration::from_millis(300));

        assert!(result.is_err(), "expected a timeout, got an open fifo");
        assert!(start.elapsed() >= Duration::from_millis(300));
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "did not return promptly"
        );
    }

    /// Waiting for the writer must not eat the stream: the probe polls rather
    /// than reads, so every byte the producer wrote is still there for ffmpeg
    /// and the first frame it sees starts at a frame boundary.
    #[test]
    fn open_overlay_fifo_waits_for_a_writer_without_consuming_its_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_fifo(dir.path());

        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(&writer_path)
                .unwrap();
            file.write_all(b"frame").unwrap();
            file.flush().unwrap();
            // Hold the write end open until the reader has taken the bytes.
            std::thread::sleep(Duration::from_millis(500));
        });

        let mut file = open_overlay_fifo(&path, Duration::from_secs(5)).unwrap();
        let mut buf = [0u8; 5];
        file.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"frame");

        writer.join().unwrap();
    }

    /// The regression this guards: `signal_overlay_wanted_and_wait_ready` used
    /// to wait up to `OVERLAY_READY_TIMEOUT` (5s) on `.overlay-ready` before
    /// ever calling this — a wait that could never resolve early, since the
    /// overlay can't touch that marker until a reader (this function) attaches.
    /// With no writer ever attaching, total wall time must be bounded by the
    /// `timeout` passed in alone; a reintroduced pre-wait would show up here as
    /// several extra seconds.
    #[tokio::test]
    async fn signal_overlay_wanted_does_not_wait_before_opening_the_fifo() {
        let fifo_dir = tempfile::tempdir().unwrap();
        let path = make_fifo(fifo_dir.path());
        let playout_dir = tempfile::tempdir().unwrap();

        let start = std::time::Instant::now();
        let result = touch_overlay_wanted_and_open_fifo(
            playout_dir.path(),
            path.to_str().unwrap(),
            Duration::from_millis(300),
        )
        .await;
        let elapsed = start.elapsed();

        assert!(result.is_err(), "expected a timeout, got an open fifo");
        assert!(elapsed >= Duration::from_millis(300));
        assert!(
            elapsed < Duration::from_secs(2),
            "took {elapsed:?} — a wait longer than the fifo-open timeout regressed"
        );
        assert!(
            playout_dir.path().join(OVERLAY_WANTED_FILE_NAME).exists(),
            "did not touch the overlay-wanted marker"
        );
    }

    #[test]
    fn accumulate_progress_line_builds_one_report_per_progress_key() {
        let mut fields = Vec::new();

        assert_eq!(accumulate_progress_line("frame=17", &mut fields), None);
        assert_eq!(accumulate_progress_line("fps=0.00", &mut fields), None);
        assert_eq!(accumulate_progress_line("speed=1.33x", &mut fields), None);
        assert_eq!(
            accumulate_progress_line("progress=continue", &mut fields),
            Some("frame=17 fps=0.00 speed=1.33x progress=continue".to_owned())
        );
        assert!(fields.is_empty(), "fields must clear after a report closes");

        // A second block starts clean and does not see the first block's fields.
        assert_eq!(accumulate_progress_line("frame=29", &mut fields), None);
        assert_eq!(
            accumulate_progress_line("progress=end", &mut fields),
            Some("frame=29 progress=end".to_owned())
        );
    }

    #[test]
    fn accumulate_progress_line_ignores_a_line_with_no_equals_sign() {
        let mut fields = Vec::new();
        assert_eq!(
            accumulate_progress_line("not a key value line", &mut fields),
            None
        );
        assert!(fields.is_empty());
    }

    #[test]
    fn accumulate_progress_line_trims_whitespace_around_key_and_value() {
        let mut fields = Vec::new();
        accumulate_progress_line("  speed = 1x  ", &mut fields);
        assert_eq!(
            accumulate_progress_line("progress = continue", &mut fields),
            Some("speed=1x progress=continue".to_owned())
        );
    }
}

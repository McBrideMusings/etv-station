use std::time::Duration;

use enum_dispatch::enum_dispatch;
use simple_expand_tilde::expand_tilde;
use time::OffsetDateTime;

use crate::ArgVec;
use crate::error::FFPipelineError;
use crate::frame_size::FrameSize;
use crate::overlay_filter::FramePoint;
use crate::probe::{
    CodecType, ProbeResult, ProbeResultAudioStream, ProbeResultStream, ProbeResultVideoStream,
};

pub struct InputSettings {
    pub start: OffsetDateTime,
    pub audio_input: ProbedInput,
    pub video_input: ProbedInput,
    pub subtitle_input: Option<ProbedInput>,
    pub watermark_input: Option<WatermarkInput>,
    pub overlay_input: Option<OverlayInput>,
    /// The channel's declared subtitle language (BCP 47, e.g. `en`). HLS
    /// allows one language per subtitle rendition for the whole session, so
    /// this is the channel's fixed declaration, not read off the currently
    /// playing item. `select_subtitle_stream` prefers whichever probed
    /// stream's language tag matches this.
    pub subtitle_language_tag: String,
}

/// Live overlay source feeding the secondary input of an ffmpeg `overlay`
/// filter. The producer writes rawvideo bytes to `fifo_path`; ffmpeg reads
/// from that path like any other file.
#[derive(Clone, Debug)]
pub struct OverlayInput {
    pub fifo_path: String,
    pub pixel_format: String,
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub x: i32,
    pub y: i32,
}

impl InputSettings {
    pub fn select_video_stream(&self) -> Result<&ProbeResultVideoStream, FFPipelineError> {
        let mut all_video_streams: Vec<&Box<ProbeResultVideoStream>> = self
            .video_input
            .probe_result
            .streams
            .iter()
            .filter_map(|s| match s {
                ProbeResultStream::Video(video_stream)
                    if video_stream.codec_type == CodecType::Video =>
                {
                    Some(video_stream)
                }
                _ => None,
            })
            .collect();

        if let Some(video_index) = self.video_input.stream_index {
            let matched_stream = all_video_streams
                .iter()
                .find(|v| v.stream_index == video_index);

            match matched_stream {
                Some(video_stream) => {
                    return Ok(video_stream);
                }
                None => {
                    log::warn!(
                        "unable to locate requested video stream with index {}",
                        video_index
                    );
                }
            }
        }

        match all_video_streams.len() {
            0 => Err(FFPipelineError::VideoInputIsRequired),
            1 => Ok(all_video_streams[0]),
            _ => {
                log::warn!(
                    "content contains more than one video stream; selecting stream with lowest index"
                );
                all_video_streams.sort_by_key(|v| v.stream_index);
                Ok(all_video_streams[0])
            }
        }
    }

    pub fn select_audio_stream(&self) -> Result<&ProbeResultAudioStream, FFPipelineError> {
        let mut all_audio_streams: Vec<&ProbeResultAudioStream> = self
            .audio_input
            .probe_result
            .streams
            .iter()
            .filter_map(|s| match s {
                ProbeResultStream::Audio(audio_stream) => Some(audio_stream),
                _ => None,
            })
            .collect();

        if let Some(audio_index) = self.audio_input.stream_index {
            let matched_stream = all_audio_streams
                .iter()
                .find(|a| a.stream_index == audio_index);

            match matched_stream {
                Some(audio_stream) => {
                    return Ok(audio_stream);
                }
                None => {
                    log::warn!(
                        "unable to locate requested audio stream with index {}",
                        audio_index
                    );
                }
            }
        }

        match all_audio_streams.len() {
            0 => Err(FFPipelineError::AudioInputIsRequired),
            1 => Ok(all_audio_streams[0]),
            _ => {
                log::warn!(
                    "content contains more than one audio stream; selecting stream with greatest number of channels"
                );
                all_audio_streams.sort_by_key(|a| std::cmp::Reverse(a.channels));
                Ok(all_audio_streams[0])
            }
        }
    }

    pub fn select_subtitle_stream(&self) -> Option<&ProbeResultVideoStream> {
        let all_subtitle_streams: Vec<&Box<ProbeResultVideoStream>> =
            match self.subtitle_input.as_ref() {
                Some(input) => input
                    .probe_result
                    .streams
                    .iter()
                    .filter_map(|s| match s {
                        ProbeResultStream::Video(video_stream)
                            if video_stream.codec_type == CodecType::Subtitle =>
                        {
                            Some(video_stream)
                        }
                        _ => None,
                    })
                    .collect(),
                None => Vec::new(),
            };

        if let Some(subtitle_index) = self.subtitle_input.as_ref().and_then(|i| i.stream_index) {
            let matched_stream = all_subtitle_streams
                .iter()
                .find(|a| a.stream_index == subtitle_index);

            match matched_stream {
                Some(subtitle_stream) => return Some(subtitle_stream),
                None => {
                    log::warn!(
                        "unable to locate requested subtitle stream with index {}",
                        subtitle_index
                    );
                }
            }
        }

        // No track was named. Prefer a text stream whose probed language
        // matches what the channel declares — a viewer picking "English"
        // should get English, whatever order the file lists its tracks in.
        let language_match = all_subtitle_streams
            .iter()
            .copied()
            .find(|s| !s.is_subtitle_image() && s.matches_language(&self.subtitle_language_tag));

        if let Some(matched) = language_match {
            return Some(matched);
        }

        // Nothing matched the declared language (or the file carries no
        // language tags at all), so fall back to what the file actually
        // carries. Text beats image: text can either be burned in or
        // converted to WebVTT, while an image subtitle can only be burned,
        // so preferring text keeps both output modes open. Ordinary media
        // files reach this path — a playout item that names no subtitle
        // track is the common case, not an unusual one.
        all_subtitle_streams
            .iter()
            .copied()
            .find(|s| !s.is_subtitle_image())
            .or_else(|| all_subtitle_streams.first().copied())
            .map(|s| &**s)
    }

    pub fn select_watermark_stream(&self) -> Option<&ProbeResultVideoStream> {
        let mut all_watermark_streams: Vec<&Box<ProbeResultVideoStream>> =
            match self.watermark_input.as_ref() {
                Some(input) => input
                    .probe_result
                    .streams
                    .iter()
                    .filter_map(|s| match s {
                        ProbeResultStream::Video(video_stream)
                            if video_stream.codec_type == CodecType::Video =>
                        {
                            Some(video_stream)
                        }
                        _ => None,
                    })
                    .collect(),
                None => Vec::new(),
            };

        if let Some(watermark_index) = self.watermark_input.as_ref().and_then(|i| i.stream_index) {
            let matched_stream = all_watermark_streams
                .iter()
                .find(|a| a.stream_index == watermark_index);

            match matched_stream {
                Some(watermark_stream) => return Some(watermark_stream),
                None => {
                    log::warn!(
                        "unable to locate requested watermark stream with index {}",
                        watermark_index
                    );
                }
            }
        }

        match all_watermark_streams.len() {
            0 => None,
            1 => Some(all_watermark_streams[0]),
            _ => {
                log::warn!(
                    "content contains more than one watermark video stream; selecting stream with lowest index"
                );
                all_watermark_streams.sort_by_key(|v| v.stream_index);
                Some(all_watermark_streams[0])
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct HttpInputOptions {
    pub headers: Vec<String>,
    pub user_agent: Option<String>,
    pub timeout_us: Option<u64>,
    pub reconnect: bool,
    pub reconnect_delay_max: Option<u32>,
    pub keep_alive: Option<bool>,
}

#[derive(Clone, Debug, Default)]
pub struct RtspInputOptions {
    pub timeout_us: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct LocalInputSource {
    pub path: String,
}

impl LocalInputSource {
    pub fn expand_path(&self) -> Option<String> {
        let expanded_path_buf = expand_tilde(self.path.as_str()); //.ok_or(ChannelError::PlayoutJsonInvalidLocalSource)?;
        expanded_path_buf
            .map(|p| p.into_os_string())
            .and_then(|p| p.into_string().ok())
    }
}

#[derive(Debug, Clone)]
pub struct LavfiInputSource {
    pub params: String,
}

#[derive(Debug, Clone)]
pub struct HttpInputSource {
    pub uri: String,
    pub options: HttpInputOptions,
}

#[derive(Debug, Clone)]
pub struct RtspInputSource {
    pub uri: String,
    pub options: RtspInputOptions,
}

/// Raw RGBA-or-similar pixel stream fed via a regular file path (typically a
/// fifo). ffmpeg consumes it as `-f rawvideo -pixel_format X -video_size WxH
/// -framerate N -i {path}`.
#[derive(Debug, Clone)]
pub struct RawVideoInputSource {
    pub pixel_format: String,
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
}

#[derive(Debug, Clone)]
#[enum_dispatch(Probeable)]
#[enum_dispatch(FfmpegInputArgs)]
pub enum InputSource {
    Local(LocalInputSource),
    Lavfi(LavfiInputSource),
    Http(HttpInputSource),
    Rtsp(RtspInputSource),
    RawVideo(RawVideoInputSource),
}

#[enum_dispatch]
pub trait FfmpegInputArgs {
    fn args_for_input(&self) -> ArgVec;
    fn input_path(&self) -> Option<String>;
}

impl FfmpegInputArgs for LocalInputSource {
    fn args_for_input(&self) -> ArgVec {
        vec![]
    }
    fn input_path(&self) -> Option<String> {
        self.expand_path()
    }
}

impl FfmpegInputArgs for LavfiInputSource {
    fn args_for_input(&self) -> ArgVec {
        args!["-f", "lavfi"]
    }
    fn input_path(&self) -> Option<String> {
        Some(self.params.clone())
    }
}

impl FfmpegInputArgs for RawVideoInputSource {
    fn args_for_input(&self) -> ArgVec {
        args![
            "-f",
            "rawvideo",
            "-pixel_format",
            self.pixel_format.clone(),
            "-video_size",
            format!("{}x{}", self.width, self.height),
            "-framerate",
            self.framerate.to_string(),
        ]
    }

    fn input_path(&self) -> Option<String> {
        // The fifo path is supplied separately by the pipeline (as `-i {path}`),
        // not carried on the source itself.
        None
    }
}
impl FfmpegInputArgs for HttpInputSource {
    fn args_for_input(&self) -> ArgVec {
        let mut args: ArgVec = Vec::new();

        if self.options.reconnect {
            args.extend(args![
                "-reconnect",
                "1",
                "-reconnect_on_network_error",
                "1",
                "-reconnect_streamed",
                "1",
            ]);
            if let Some(max_delay) = self.options.reconnect_delay_max {
                args.extend(args!["-reconnect_delay_max", max_delay.to_string()]);
            }
        }

        if self.options.keep_alive.is_some_and(|ka| ka) {
            args.extend(args!["-multiple_requests", "1"])
        }

        if let Some(timeout) = self.options.timeout_us {
            args.extend(args!["-timeout", timeout.to_string()]);
        }

        if let Some(ua) = &self.options.user_agent {
            args.extend(args!["-user_agent", ua.clone()]);
        }

        if !self.options.headers.is_empty() {
            // FFmpeg expects headers separated by \r\n, with trailing \r\n
            let combined: String = self
                .options
                .headers
                .iter()
                .map(|h| format!("{}\r\n", h))
                .collect();
            args.extend(args!["-headers", combined]);
        }

        args.extend(args![
            "-protocol_whitelist",
            "file,http,https,tcp,tls,crypto",
        ]);

        args
    }

    fn input_path(&self) -> Option<String> {
        Some(self.uri.clone())
    }
}

impl FfmpegInputArgs for RtspInputSource {
    fn args_for_input(&self) -> ArgVec {
        let mut args: ArgVec = Vec::new();

        if let Some(timeout) = self.options.timeout_us {
            args.extend(args!["-timeout", timeout.to_string()]);
        }

        args.extend(args![
            "-protocol_whitelist",
            "file,rtp,rtsp,udp,tcp,tls,crypto"
        ]);

        args
    }

    fn input_path(&self) -> Option<String> {
        Some(self.uri.clone())
    }
}

pub struct ProbedInput {
    pub input_source: InputSource,
    pub probe_result: ProbeResult,
    pub in_point: Duration,
    pub out_point: Duration,
    pub stream_index: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct WatermarkInput {
    pub input_source: InputSource,
    pub probe_result: ProbeResult,
    pub stream_index: Option<u32>,
    pub location: WatermarkLocation,
    pub width_percent: Option<f32>,
    pub within_source_content: Option<bool>,
    pub horizontal_margin_percent: Option<f32>,
    pub vertical_margin_percent: Option<f32>,
    pub opacity_percent: Option<f32>,
    pub timing: Option<WatermarkTiming>,
}

#[derive(Debug, Clone)]
pub enum WatermarkTiming {
    Periodic(PeriodicTiming),
}

#[derive(Debug, Clone)]
pub struct PeriodicTiming {
    pub clock: PeriodicClock,
    pub frequency_ms: u64,
    pub phase_offset_ms: Option<u64>,
    pub disable_after_ms: Option<u64>,
    pub fade_ms: Option<u64>,
    pub hold_ms: u64,
}

#[derive(Debug, Clone)]
pub enum PeriodicClock {
    Wall,
    Content,
}

impl WatermarkInput {
    pub(crate) fn scaled_size(
        &self,
        watermark_size: FrameSize,
        video_size: Option<FrameSize>,
    ) -> FrameSize {
        if let Some(output_size) = video_size
            && let Some(width_percent) = self.width_percent
        {
            let mut scaled_width =
                f32::round((width_percent / 100f32) * output_size.width as f32) as u32;
            let aspect_ratio = watermark_size.height as f32 / watermark_size.width as f32;
            let mut scaled_height = f32::round(scaled_width as f32 * aspect_ratio) as u32;
            if scaled_width % 2 == 1 {
                scaled_width += 1;
            }
            if scaled_height % 2 == 1 {
                scaled_height += 1;
            }
            FrameSize {
                width: scaled_width,
                height: scaled_height,
            }
        } else {
            watermark_size
        }
    }

    pub(crate) fn frame_location(
        &self,
        source_content_size: &FrameSize,
        scaled_size: &FrameSize,
        video_size: &FrameSize,
    ) -> FramePoint {
        let (h_ref, v_ref, h_pad_offset, v_pad_offset) =
            if self.within_source_content.unwrap_or(false) {
                let h_pad = video_size.width.saturating_sub(source_content_size.width);
                let v_pad = video_size.height.saturating_sub(source_content_size.height);
                (
                    source_content_size.width,
                    source_content_size.height,
                    h_pad / 2,
                    v_pad / 2,
                )
            } else {
                (video_size.width, video_size.height, 0, 0)
            };

        let h_pct_margin =
            f32::round(self.horizontal_margin_percent.unwrap_or(0f32) / 100f32 * h_ref as f32)
                as u32;
        let v_pct_margin =
            f32::round(self.vertical_margin_percent.unwrap_or(0f32) / 100f32 * v_ref as f32) as u32;

        let center_x = video_size.width.saturating_sub(scaled_size.width) / 2;
        let center_y = video_size.height.saturating_sub(scaled_size.height) / 2;
        let right_anchor = video_size.width.saturating_sub(scaled_size.width);
        let bottom_anchor = video_size.height.saturating_sub(scaled_size.height);

        match self.location {
            WatermarkLocation::TopLeft => FramePoint {
                x: h_pct_margin + h_pad_offset,
                y: v_pct_margin + v_pad_offset,
            },
            WatermarkLocation::TopCenter => FramePoint {
                x: center_x + h_pct_margin,
                y: v_pct_margin + v_pad_offset,
            },
            WatermarkLocation::TopRight => FramePoint {
                x: right_anchor.saturating_sub(h_pct_margin + h_pad_offset),
                y: v_pct_margin + v_pad_offset,
            },
            WatermarkLocation::CenterLeft => FramePoint {
                x: h_pct_margin + h_pad_offset,
                y: center_y + v_pct_margin,
            },
            WatermarkLocation::Center => FramePoint {
                x: center_x + h_pct_margin,
                y: center_y + v_pct_margin,
            },
            WatermarkLocation::CenterRight => FramePoint {
                x: right_anchor.saturating_sub(h_pct_margin + h_pad_offset),
                y: center_y + v_pct_margin,
            },
            WatermarkLocation::BottomLeft => FramePoint {
                x: h_pct_margin + h_pad_offset,
                y: bottom_anchor.saturating_sub(v_pct_margin + v_pad_offset),
            },
            WatermarkLocation::BottomCenter => FramePoint {
                x: center_x + h_pct_margin,
                y: bottom_anchor.saturating_sub(v_pct_margin + v_pad_offset),
            },
            WatermarkLocation::BottomRight => FramePoint {
                x: right_anchor.saturating_sub(h_pct_margin + h_pad_offset),
                y: bottom_anchor.saturating_sub(v_pct_margin + v_pad_offset),
            },
        }
    }
}

#[derive(Debug, Clone)]
pub enum WatermarkLocation {
    TopLeft,
    TopCenter,
    TopRight,
    CenterLeft,
    Center,
    CenterRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;

    use super::*;
    use crate::frame_rate::FrameRate;
    use crate::probe::ProbeResultColorParams;

    fn stream(stream_index: u32, codec_type: CodecType, codec: &str) -> ProbeResultStream {
        stream_with_language(stream_index, codec_type, codec, None)
    }

    fn stream_with_language(
        stream_index: u32,
        codec_type: CodecType,
        codec: &str,
        language: Option<&str>,
    ) -> ProbeResultStream {
        ProbeResultStream::Video(Box::new(ProbeResultVideoStream {
            stream_index,
            codec: String::from(codec),
            codec_type,
            profile: String::new(),
            height: None,
            width: None,
            frame_rate: FrameRate::default(),
            sample_aspect_ratio: None,
            display_aspect_ratio: None,
            pix_fmt: String::new(),
            color_params: ProbeResultColorParams {
                color_range: None,
                color_space: None,
                color_transfer: None,
                color_primaries: None,
            },
            field_order: None,
            language: language.map(String::from),
        }))
    }

    fn probed(streams: Vec<ProbeResultStream>, stream_index: Option<u32>) -> ProbedInput {
        ProbedInput {
            input_source: InputSource::Local(LocalInputSource {
                path: String::from("/tmp/example.mkv"),
            }),
            probe_result: ProbeResult {
                path: String::from("/tmp/example.mkv"),
                streams,
                duration: None,
                format_name: None,
            },
            in_point: Duration::ZERO,
            out_point: Duration::from_secs(60),
            stream_index,
        }
    }

    fn settings(subtitle_input: Option<ProbedInput>) -> InputSettings {
        settings_with_language(subtitle_input, "en")
    }

    fn settings_with_language(
        subtitle_input: Option<ProbedInput>,
        subtitle_language_tag: &str,
    ) -> InputSettings {
        InputSettings {
            start: OffsetDateTime::UNIX_EPOCH,
            audio_input: probed(vec![stream(0, CodecType::Video, "h264")], None),
            video_input: probed(vec![stream(0, CodecType::Video, "h264")], None),
            subtitle_input,
            watermark_input: None,
            overlay_input: None,
            subtitle_language_tag: String::from(subtitle_language_tag),
        }
    }

    /// An ordinary media file: video, audio, and a subtitle track, with no
    /// track number named by the playout item. This is the common case, and
    /// before the fallback existed it selected nothing at all.
    #[test]
    fn picks_the_subtitle_track_out_of_an_ordinary_file() {
        let input = settings(Some(probed(
            vec![
                stream(0, CodecType::Video, "h264"),
                stream(1, CodecType::Audio, "aac"),
                stream(2, CodecType::Subtitle, "subrip"),
            ],
            None,
        )));

        let selected = input.select_subtitle_stream().expect("a subtitle stream");
        assert_eq!(selected.stream_index, 2);
    }

    /// Text can be burned in or converted to WebVTT; a picture can only be
    /// burned in. Preferring the text track keeps both output modes available.
    #[test]
    fn prefers_a_text_track_over_a_picture_one() {
        let input = settings(Some(probed(
            vec![
                stream(0, CodecType::Video, "h264"),
                stream(1, CodecType::Subtitle, "hdmv_pgs_subtitle"),
                stream(2, CodecType::Subtitle, "subrip"),
            ],
            None,
        )));

        let selected = input.select_subtitle_stream().expect("a subtitle stream");
        assert_eq!(selected.stream_index, 2);
    }

    /// A disc rip whose only subtitles are pictures still gets selected, so
    /// burn mode has something to draw.
    #[test]
    fn falls_back_to_a_picture_track_when_that_is_all_there_is() {
        let input = settings(Some(probed(
            vec![
                stream(0, CodecType::Video, "h264"),
                stream(1, CodecType::Subtitle, "hdmv_pgs_subtitle"),
            ],
            None,
        )));

        let selected = input.select_subtitle_stream().expect("a subtitle stream");
        assert_eq!(selected.stream_index, 1);
    }

    /// The playout item naming a track number still wins over the fallback.
    #[test]
    fn a_named_track_number_wins() {
        let input = settings(Some(probed(
            vec![
                stream(0, CodecType::Video, "h264"),
                stream(1, CodecType::Subtitle, "subrip"),
                stream(2, CodecType::Subtitle, "subrip"),
            ],
            Some(2),
        )));

        let selected = input.select_subtitle_stream().expect("a subtitle stream");
        assert_eq!(selected.stream_index, 2);
    }

    #[test]
    fn a_file_with_no_subtitles_selects_nothing() {
        let input = settings(Some(probed(
            vec![
                stream(0, CodecType::Video, "h264"),
                stream(1, CodecType::Audio, "aac"),
            ],
            None,
        )));

        assert!(input.select_subtitle_stream().is_none());
    }

    /// A file carries both a Spanish and an English subtitle stream, in that
    /// order. A channel declaring English must get the English stream even
    /// though it comes second.
    #[test]
    fn prefers_the_stream_matching_the_channels_declared_language_english() {
        let input = settings_with_language(
            Some(probed(
                vec![
                    stream(0, CodecType::Video, "h264"),
                    stream_with_language(1, CodecType::Subtitle, "subrip", Some("spa")),
                    stream_with_language(2, CodecType::Subtitle, "subrip", Some("eng")),
                ],
                None,
            )),
            "en",
        );

        let selected = input.select_subtitle_stream().expect("a subtitle stream");
        assert_eq!(selected.stream_index, 2);
    }

    /// Same file, channel declares Spanish instead: the Spanish stream wins,
    /// even though it comes first — the point is the language match, not
    /// file order.
    #[test]
    fn prefers_the_stream_matching_the_channels_declared_language_spanish() {
        let input = settings_with_language(
            Some(probed(
                vec![
                    stream(0, CodecType::Video, "h264"),
                    stream_with_language(1, CodecType::Subtitle, "subrip", Some("spa")),
                    stream_with_language(2, CodecType::Subtitle, "subrip", Some("eng")),
                ],
                None,
            )),
            "es",
        );

        let selected = input.select_subtitle_stream().expect("a subtitle stream");
        assert_eq!(selected.stream_index, 1);
    }

    /// Neither stream's language matches what the channel declares, so
    /// selection falls back to today's behavior: first non-image stream in
    /// file order.
    #[test]
    fn falls_back_to_file_order_when_nothing_matches_the_declared_language() {
        let input = settings_with_language(
            Some(probed(
                vec![
                    stream(0, CodecType::Video, "h264"),
                    stream_with_language(1, CodecType::Subtitle, "subrip", Some("spa")),
                    stream_with_language(2, CodecType::Subtitle, "subrip", Some("eng")),
                ],
                None,
            )),
            "de",
        );

        let selected = input.select_subtitle_stream().expect("a subtitle stream");
        assert_eq!(selected.stream_index, 1);
    }
}

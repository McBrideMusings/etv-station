use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ersatztv_channel::error::ChannelError;
use ersatztv_core::{HEARTBEAT_FILE_NAME, HEARTBEAT_FILE_TIMEOUT, SEQUENCE_FILE_NAME};
use ffpipeline::pipeline::PtsOffset;
use ffpipeline::web_vtt::{Cue, format_vtt_ts};
use time::OffsetDateTime;
use time::macros::format_description;

// 3 segments is the commonly cited floor for a live HLS playlist (e.g. AWS's
// Kinesis Video Streams HLS docs state a live media playlist should carry a
// minimum of 3 fragments) — Apple's own authoring spec doesn't appear to
// enumerate an exact minimum. Was 4; on GPU-bound hardware, each segment here
// costs close to SEGMENT_SECONDS of real tune-in wait (see TARGET_BUFFER in
// ersatztv-channel), so this is the smallest change consistent with a real,
// citable floor rather than an arbitrary cut.
//
// Deliberately sits exactly at that floor rather than above it — considered
// and rejected adding a cushion, since any number above the cited minimum is
// back to an arbitrary guess, the thing this constant is trying to avoid. Not
// a race: a segment only enters `self.segments` (below, in `update`) once its
// `.ts` file exists on disk AND ffmpeg's own playlist has published a known
// EXTINF duration for it — ffmpeg doesn't emit that duration until the
// segment is fully written, so `self.segments.len()` can never count a
// partial one. The floor is thin by design, not by oversight.
const MIN_SEGMENTS: usize = 3;

/// How far the newest segment's end may fall behind wall clock — in multiples of
/// the playlist's target duration — before a session that a viewer is actively
/// polling is declared stalled.
///
/// A healthy session keeps roughly a minute of transcoded content *ahead* of
/// wall clock, so this measure normally sits deeply negative and never
/// approaches zero, including during the work-ahead idle gap where no ffmpeg is
/// running at all. Sixty seconds of starvation is well past any legitimate
/// transient: the server gives a cold session only [`READY_FILE_TIMEOUT`] (30s)
/// to produce its first segments before it gives up on the client's behalf.
///
/// [`READY_FILE_TIMEOUT`]: ersatztv_core::READY_FILE_TIMEOUT
const STALL_TARGET_DURATIONS: u32 = 15;

/// Why a session should stop transcoding and let the process exit.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum SessionAbort {
    /// No viewer has requested anything for [`HEARTBEAT_FILE_TIMEOUT`].
    IdleTimeout,
    /// A viewer is still requesting, but the encoder has stopped producing
    /// segments — see [`STALL_TARGET_DURATIONS`].
    SegmentStall,
}

#[derive(Clone)]
pub struct SubtitleSource {
    pub cues: Arc<Vec<Cue>>,
    pub(crate) cursor: usize,
    pub next_segment_source_offset: Duration,
}

#[derive(Clone)]
pub struct PlaylistManager {
    output_folder: PathBuf,
    ready_file: PathBuf,
    heartbeat_file: PathBuf,
    sequence_file: PathBuf,
    generated_playlist_file: String,
    /// `None` on a channel that burns subtitles into the picture — there are no
    /// `.vtt` files for a subtitle playlist to point at, so none is written.
    generated_subtitle_playlist_file: Option<String>,
    ffmpeg_playlist_file: String,
    ready: bool,

    segments: VecDeque<Segment>,
    discontinuity_before: HashSet<String>,
    media_sequence: u64,
    last_served_media_sequence: u64,
    discontinuity_sequence: u64,
    target_duration: u32,
    target_duration_f64: f64,
    pending_discontinuity: bool,
    last_segment_end: OffsetDateTime,
    current_session_start: OffsetDateTime,

    pts_offset: Option<PtsOffset>,
    subtitle_source: Option<SubtitleSource>,

    timeout: bool,
    stalled: bool,

    last_progress: OffsetDateTime,
}

/// The sequence counters a session carries across worker restarts, persisted to
/// [`SEQUENCE_FILE_NAME`] in the output folder.
///
/// Both fields name what the *next* segment must be numbered, not what the last
/// one was, so seeding a fresh [`PlaylistManager`] is a plain assignment with no
/// off-by-one to get wrong at the call site.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SequenceState {
    /// `EXT-X-MEDIA-SEQUENCE` the next segment produced on this session takes.
    /// Strictly above every sequence number already published, so a client that
    /// discards anything at or below what it has already consumed still accepts
    /// the first segment of the new worker.
    next_media_sequence: u64,
    /// `EXT-X-DISCONTINUITY-SEQUENCE` in force once the segments held now have
    /// all been trimmed away.
    next_discontinuity_sequence: u64,
}

#[derive(Clone)]
struct Segment {
    path: String,
    duration: f64,
    program_date_time: OffsetDateTime,
}

pub struct PlaylistManagerOutputFiles {
    pub generated_playlist_file: String,
    pub ffmpeg_playlist_file: String,
    pub generated_subtitle_playlist_file: Option<String>,
}

impl PlaylistManager {
    /// Build a manager for `output_folder`, resuming the session's sequence
    /// numbering if a previous worker left its counters behind.
    ///
    /// Resuming is the normal case, not the exceptional one: a client holds one
    /// session URL across every worker restart under it, so the numbering it
    /// sees has to keep climbing even though the process producing it did not
    /// survive. A missing or unreadable counter file means a genuinely new
    /// session and starts at zero.
    pub async fn new(
        channel_start_time: OffsetDateTime,
        target_duration: u32,
        output_folder: PathBuf,
        ready_file: PathBuf,
        output_files: PlaylistManagerOutputFiles,
    ) -> PlaylistManager {
        let heartbeat_file = output_folder.join(HEARTBEAT_FILE_NAME);
        let sequence_file = output_folder.join(SEQUENCE_FILE_NAME);

        let resumed = read_sequence_state(&sequence_file).await;

        PlaylistManager {
            output_folder,
            ready_file,
            heartbeat_file,
            sequence_file,
            generated_playlist_file: output_files.generated_playlist_file,
            ffmpeg_playlist_file: output_files.ffmpeg_playlist_file,
            generated_subtitle_playlist_file: output_files.generated_subtitle_playlist_file,
            ready: false,

            segments: VecDeque::new(),
            discontinuity_before: HashSet::new(),
            media_sequence: resumed.next_media_sequence,
            last_served_media_sequence: resumed.next_media_sequence,
            discontinuity_sequence: resumed.next_discontinuity_sequence,
            target_duration,
            target_duration_f64: target_duration as f64,
            // A resumed session's first segment comes from a different encoder
            // run than the last one a client consumed — new PTS base, possibly
            // new codec parameters — so it needs an EXT-X-DISCONTINUITY ahead of
            // it. Continuing the sequence numbering without that tag would tell
            // the client the two spliced cleanly, which is the one claim a
            // restart cannot make.
            pending_discontinuity: resumed != SequenceState::default(),
            last_segment_end: channel_start_time,
            current_session_start: channel_start_time,

            pts_offset: None,
            subtitle_source: None,

            timeout: false,
            stalled: false,

            last_progress: OffsetDateTime::now_utc(),
        }
    }

    /// Whether this session should stop transcoding, and why. `None` while it is
    /// healthy.
    pub fn abort(&self) -> Option<SessionAbort> {
        if self.timeout {
            Some(SessionAbort::IdleTimeout)
        } else if self.stalled {
            Some(SessionAbort::SegmentStall)
        } else {
            None
        }
    }

    pub fn last_progress(&self) -> &OffsetDateTime {
        &self.last_progress
    }

    /// Wall-clock end of the newest segment produced so far.
    ///
    /// `last_segment_end - now` is the session's remaining lead: how much
    /// already-transcoded video a viewer has left before they catch up with the
    /// encoder. The session watches this to notice it is losing ground while an
    /// item is still playing, which is the only place that lead can be measured
    /// — `transcoded_until` is not updated until an item finishes, and a feature
    /// film is one item lasting hours.
    pub fn last_segment_end(&self) -> &OffsetDateTime {
        &self.last_segment_end
    }

    pub async fn before_new_pipeline(
        &mut self,
        new_pts_offset: Option<PtsOffset>,
        new_subtitle_source: Option<SubtitleSource>,
    ) -> Result<(), ChannelError> {
        self.update().await?;
        self.pts_offset = new_pts_offset;
        self.subtitle_source = new_subtitle_source;
        self.pending_discontinuity = true;
        self.current_session_start = self.last_segment_end;

        self.last_progress = OffsetDateTime::now_utc();

        // overwrite ffmpeg's playlist with a generated playlist (containing *all* segments)
        if Path::new(&self.generated_playlist_file).exists() {
            let generated_playlist = self.generate_playlist(|s| s.to_owned(), None)?;
            write_atomically(
                &self.output_folder,
                &self.ffmpeg_playlist_file,
                generated_playlist,
            )
            .await?;
        }

        Ok(())
    }

    pub async fn update(&mut self) -> Result<(), ChannelError> {
        // scan for segments on disk
        let mut new_segment_files: VecDeque<String> = VecDeque::new();
        let mut entries = tokio::fs::read_dir(&self.output_folder).await?;
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Some(file_name) = entry.file_name().to_str()
                && file_name.ends_with(".ts")
                && !self.segments.iter().any(|s| s.path == file_name)
            {
                new_segment_files.push_back(file_name.to_owned());
            }
        }

        // get all segment durations from extinf tags in ffmpeg playlist
        let new_segment_durations: HashMap<String, f64> = self.get_new_segment_durations().await?;

        // filter out segments without a known duration
        let mut sorted_new_segments: Vec<String> = Vec::new();
        for segment in new_segment_files {
            if new_segment_durations.contains_key(&segment) {
                sorted_new_segments.push(segment);
            }
        }
        sorted_new_segments.sort();

        // add new segments
        for file in sorted_new_segments {
            if self.pending_discontinuity {
                self.discontinuity_before.insert(file.to_owned());
                self.pending_discontinuity = false;
            }

            let duration = new_segment_durations
                .get(&file)
                .map(|f| f.to_owned())
                .unwrap_or(self.target_duration_f64);

            if duration > (self.target_duration as f64) {
                self.target_duration = duration.ceil() as u32;
            }

            let program_date_time = self.last_segment_end;

            self.segments.push_back(Segment {
                path: file.clone(),
                program_date_time,
                duration,
            });

            self.last_segment_end += Duration::from_secs_f64(duration);
            self.last_progress = OffsetDateTime::now_utc();

            let vtt_path = format!("{}.vtt", file.strip_suffix(".ts").unwrap_or(&file));
            let vtt_full = self.output_folder.join(&vtt_path);
            let mpegts_90khz = (((self.pts_offset.unwrap_or_default().duration.as_secs_f64()
                + (program_date_time - self.current_session_start).as_seconds_f64())
                * 90_000.0) as u64)
                % 8589934592;
            let body = match &mut self.subtitle_source {
                Some(src) => {
                    let body = render_subtitle_segment(
                        src,
                        src.next_segment_source_offset,
                        duration,
                        mpegts_90khz,
                    );
                    src.next_segment_source_offset += Duration::from_secs_f64(duration);
                    body
                }
                None => format!(
                    "WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:{}\n\n",
                    mpegts_90khz
                ),
            };
            write_atomically(&self.output_folder, &vtt_full, body).await?;
        }

        // trim old segments
        let cutoff = OffsetDateTime::now_utc() - Duration::from_mins(2);
        while !self.segments.is_empty() && self.segments[0].program_date_time < cutoff {
            if let Some(removed) = self.segments.remove(0) {
                self.media_sequence += 1;
                if self.discontinuity_before.contains(&removed.path) {
                    self.discontinuity_before.remove(&removed.path);
                    self.discontinuity_sequence += 1;
                }

                let path = self.output_folder.join(&removed.path);
                tokio::fs::remove_file(&path).await?;

                let vtt_path = self.output_folder.join(format!(
                    "{}.vtt",
                    removed.path.strip_suffix(".ts").unwrap_or(&removed.path)
                ));
                if vtt_path.exists() {
                    tokio::fs::remove_file(&vtt_path).await?;
                }
            }
        }

        // generate and atomically save playlist
        let generated_playlist = self.generate_playlist(|s| s.to_owned(), Some(10))?;
        write_atomically(
            &self.output_folder,
            &self.generated_playlist_file,
            generated_playlist,
        )
        .await?;

        // generate and atomically save subtitle playlist
        if let Some(subtitle_playlist_file) = self.generated_subtitle_playlist_file.clone() {
            let generated_subtitle_playlist = self.generate_playlist(
                |s| format!("{}.vtt", s.strip_suffix(".ts").unwrap_or(s)),
                Some(10),
            )?;
            write_atomically(
                &self.output_folder,
                &subtitle_playlist_file,
                generated_subtitle_playlist,
            )
            .await?;
        }

        // Persist the counters *after* the playlist naming them is on disk, so
        // the file can only ever promise numbering that has already been
        // published. Crashing between the two costs a restart the reuse of a few
        // sequence numbers no client was served — the harmless direction. The
        // reverse order would let a crash advertise numbers that never shipped,
        // leaving a permanent gap in the sequence.
        self.persist_sequence_state().await?;

        if !self.ready && self.segments.len() >= MIN_SEGMENTS {
            tokio::fs::write(&self.ready_file, b"").await?;
            self.ready = true;
        }

        let mut viewer_is_watching = false;
        if self.heartbeat_file.exists() {
            let metadata = tokio::fs::metadata(&self.heartbeat_file).await?;
            let modified = metadata.modified()?;
            self.timeout = modified.elapsed().unwrap_or(Duration::MAX) > HEARTBEAT_FILE_TIMEOUT;
            viewer_is_watching = !self.timeout;
        }

        // The heartbeat proves the worker process is alive and that a viewer is
        // still asking for the playlist. It does not prove the encoder emitted
        // any bytes, and those are the two things that can come apart: a session
        // wedged on a blocked ffmpeg keeps its heartbeat advancing while
        // producing nothing, and once the trim above drains the last segment the
        // viewer is served an empty playlist indefinitely.
        //
        // `last_segment_end` is the wall-clock end of the newest segment, so
        // `now - last_segment_end` is how long the viewer has been past the end
        // of the content we hold. It starts at the session start time and only
        // advances when a segment is added, so a session that has never produced
        // a single segment is measured the same way as one that stopped
        // mid-stream — no separate never-started case.
        if viewer_is_watching {
            let starved_for = OffsetDateTime::now_utc() - self.last_segment_end;
            let limit =
                time::Duration::seconds((self.target_duration * STALL_TARGET_DURATIONS) as i64);
            if starved_for > limit && !self.stalled {
                log::error!(
                    "no segments produced for {}s while a viewer is watching ({} segments held); \
                     tearing down the session so it can re-tune",
                    starved_for.whole_seconds(),
                    self.segments.len()
                );
                self.stalled = true;
            }
        }

        Ok(())
    }

    /// The counters a worker taking over this session folder must start from.
    ///
    /// `next_media_sequence` counts past every segment currently held, not just
    /// those a trim has already dropped: all of them have been published under
    /// this session's numbering, and a successor that reused any of those
    /// numbers for different content would be offering a client segments it has
    /// grounds to discard.
    fn sequence_state(&self) -> SequenceState {
        SequenceState {
            next_media_sequence: self.media_sequence + self.segments.len() as u64,
            next_discontinuity_sequence: self.discontinuity_sequence
                + self
                    .segments
                    .iter()
                    .filter(|s| self.discontinuity_before.contains(&s.path))
                    .count() as u64,
        }
    }

    async fn persist_sequence_state(&self) -> Result<(), ChannelError> {
        let state = serde_json::to_string(&self.sequence_state())
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        write_atomically(&self.output_folder, &self.sequence_file, state).await?;
        Ok(())
    }

    fn generate_playlist(
        &mut self,
        path_map: fn(&str) -> String,
        max_segments: Option<usize>,
    ) -> Result<String, ChannelError> {
        let mut playlist = String::new();
        playlist.push_str("#EXTM3U\n");
        playlist.push_str("#EXT-X-VERSION:7\n");
        playlist.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", self.target_duration));

        let (skip, limit) = match max_segments {
            Some(max) => {
                let anchor = OffsetDateTime::now_utc() - Duration::from_secs(1200u64);

                let candidate_skip = self
                    .segments
                    .iter()
                    .position(|s| s.program_date_time >= anchor)
                    .unwrap_or_else(|| self.segments.len().saturating_sub(max));

                // monotonic clamp
                let candidate_ms = self.media_sequence + candidate_skip as u64;
                let clamped_ms = candidate_ms.max(self.last_served_media_sequence);
                self.last_served_media_sequence = clamped_ms;

                let skip = (clamped_ms - self.media_sequence) as usize;
                let skip = skip.min(self.segments.len());
                (skip, max)
            }
            None => (0, self.segments.len()),
        };
        let effective_media_sequence = self.media_sequence + skip as u64;
        let effective_discontinuity_sequence = self.discontinuity_sequence
            + self
                .segments
                .iter()
                .take(skip)
                .filter(|s| self.discontinuity_before.contains(&s.path))
                .count() as u64;

        playlist.push_str(&format!(
            "#EXT-X-MEDIA-SEQUENCE:{}\n",
            effective_media_sequence
        ));
        if effective_discontinuity_sequence > 0 {
            playlist.push_str(&format!(
                "#EXT-X-DISCONTINUITY-SEQUENCE:{}\n",
                effective_discontinuity_sequence
            ));
        }
        playlist.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");

        let format = format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3][offset_hour sign:mandatory][offset_minute]"
        );

        for segment in self.segments.iter().skip(skip).take(limit) {
            if self.discontinuity_before.contains(&segment.path) {
                playlist.push_str("#EXT-X-DISCONTINUITY\n");
            }
            playlist.push_str(&format!("#EXTINF:{:.6},\n", segment.duration));
            playlist.push_str(&format!(
                "#EXT-X-PROGRAM-DATE-TIME:{}\n",
                segment.program_date_time.format(format)?
            ));
            playlist.push_str(&format!("{}\n", path_map(&segment.path)));
        }

        Ok(playlist)
    }

    async fn get_new_segment_durations(&self) -> Result<HashMap<String, f64>, ChannelError> {
        let mut result: HashMap<String, f64> = HashMap::new();

        let path = Path::new(&self.ffmpeg_playlist_file);
        if path.exists() {
            let contents = tokio::fs::read_to_string(&path).await?;
            let lines: Vec<&str> = contents.split('\n').collect();
            let mut i: usize = 0;
            while i < lines.len() {
                if lines[i].starts_with("#EXTINF:")
                    && i + 2 < lines.len()
                    && lines[i + 2].ends_with(".ts")
                {
                    let segment_name = lines[i + 2];
                    let inf_split: Vec<&str> =
                        lines[i].split(':').map(|s| s.trim_matches(',')).collect();
                    if let Ok(duration) = inf_split[1].parse::<f64>() {
                        result.insert(segment_name.to_owned(), duration);
                    }
                }

                i += 1;
            }
        }

        Ok(result)
    }
}

/// Read a session's persisted sequence counters, or [`SequenceState::default`]
/// when there are none to read.
///
/// Every failure route lands on the default deliberately. A missing file is the
/// ordinary first-run case; a truncated or malformed one is a file that was
/// being written when the process died. Neither is worth refusing to start a
/// channel over — the cost of falling back is that one already-tuned client
/// freezes until it re-tunes, which is exactly what happened before any of this
/// was persisted, and the cost of propagating the error is a channel that will
/// not start at all.
async fn read_sequence_state(path: &Path) -> SequenceState {
    let Ok(contents) = tokio::fs::read_to_string(path).await else {
        return SequenceState::default();
    };
    match serde_json::from_str(&contents) {
        Ok(state) => state,
        Err(e) => {
            log::warn!(
                "ignoring unreadable sequence file {}: {e}; this session's segment numbering \
                 restarts from zero and any client already tuned to it will freeze until it \
                 re-tunes",
                path.display()
            );
            SequenceState::default()
        }
    }
}

/// Write `contents` to `destination` in a single step no reader can catch
/// half-finished: fill a temp file in `folder` — the same folder the
/// destination lives in, so the rename cannot cross filesystems — then rename
/// it into place.
///
/// Widening the mode is part of that same step. `NamedTempFile` creates its
/// backing file `0600` on purpose, a scratch file being private to the process
/// that made it, and that mode survives the rename. Everything written here is
/// served over HTTP alongside ffmpeg's own files, which land `0644` from the
/// normal process umask, so the temp file is widened to match before it moves.
async fn write_atomically(
    folder: &Path,
    destination: impl AsRef<Path>,
    contents: String,
) -> std::io::Result<()> {
    let temp = tempfile::NamedTempFile::new_in(folder)?;
    tokio::fs::write(temp.path(), contents).await?;
    make_world_readable(&temp)?;
    tokio::fs::rename(temp.path(), destination).await?;

    Ok(())
}

#[cfg(unix)]
fn make_world_readable(temp: &tempfile::NamedTempFile) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    temp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o644))
}

/// Non-unix targets have no owner-only-vs-world-readable distinction to fix.
#[cfg(not(unix))]
fn make_world_readable(_temp: &tempfile::NamedTempFile) -> std::io::Result<()> {
    Ok(())
}

fn render_subtitle_segment(
    src: &mut SubtitleSource,
    seg_start_src: Duration,
    duration: f64,
    mpegts_90khz: u64,
) -> String {
    let seg_end_src = seg_start_src + Duration::from_secs_f64(duration);

    let mut out = format!(
        "WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:{}\n\n",
        mpegts_90khz
    );

    let mut segment_cursor = src.cursor;

    while let Some(cue) = src.cues.get(segment_cursor)
        && cue.start < seg_end_src
    {
        if cue.end > seg_start_src {
            let local_start = cue.start.saturating_sub(seg_start_src);
            let local_end = cue
                .end
                .saturating_sub(seg_start_src)
                .min(Duration::from_secs_f64(duration));
            out.push_str(&format!(
                "{} --> {}\n{}\n\n",
                format_vtt_ts(local_start),
                format_vtt_ts(local_end),
                cue.text
            ));
        }

        // walk persistent cursor if this cue will never display again
        if src.cursor == segment_cursor && cue.end <= seg_end_src {
            src.cursor += 1;
        }

        segment_cursor += 1;
    }

    out
}

#[cfg(test)]
mod tests {
    use ffpipeline::pipeline::SEGMENT_SECONDS;
    use tempfile::TempDir;

    use super::*;

    /// Seconds of starvation the watchdog tolerates at the default target
    /// duration.
    const THRESHOLD_SECONDS: i64 = (SEGMENT_SECONDS * STALL_TARGET_DURATIONS) as i64;

    /// A manager whose newest segment ends at `last_segment_end`. Passing that as
    /// the channel start time is how a session that has never produced a segment
    /// looks too — the field only ever advances when one is added.
    async fn manager(dir: &Path, last_segment_end: OffsetDateTime) -> PlaylistManager {
        let file = |name: &str| dir.join(name).to_string_lossy().into_owned();

        PlaylistManager::new(
            last_segment_end,
            SEGMENT_SECONDS,
            dir.to_path_buf(),
            dir.join(".ready"),
            PlaylistManagerOutputFiles {
                generated_playlist_file: file("live.m3u8"),
                generated_subtitle_playlist_file: Some(file("live_sub.m3u8")),
                ffmpeg_playlist_file: file("ffmpeg.m3u8"),
            },
        )
        .await
    }

    async fn watch(dir: &Path) {
        tokio::fs::write(dir.join(HEARTBEAT_FILE_NAME), b"")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn healthy_session_working_ahead_does_not_stall() {
        let dir = TempDir::new().unwrap();
        watch(dir.path()).await;

        // The run loop keeps roughly a minute of transcoded content ahead of wall
        // clock, and produces no segments at all while it waits for that buffer
        // to drain. That gap must never read as a stall.
        let mut pm = manager(
            dir.path(),
            OffsetDateTime::now_utc() + time::Duration::minutes(1),
        )
        .await;
        pm.update().await.unwrap();

        assert_eq!(pm.abort(), None);
    }

    #[tokio::test]
    async fn starvation_just_under_the_threshold_does_not_stall() {
        let dir = TempDir::new().unwrap();
        watch(dir.path()).await;

        let mut pm = manager(
            dir.path(),
            OffsetDateTime::now_utc() - time::Duration::seconds(THRESHOLD_SECONDS - 5),
        )
        .await;
        pm.update().await.unwrap();

        assert_eq!(pm.abort(), None);
    }

    #[tokio::test]
    async fn watched_session_past_the_threshold_stalls() {
        let dir = TempDir::new().unwrap();
        watch(dir.path()).await;

        let mut pm = manager(
            dir.path(),
            OffsetDateTime::now_utc() - time::Duration::seconds(THRESHOLD_SECONDS + 5),
        )
        .await;
        pm.update().await.unwrap();

        assert_eq!(pm.abort(), Some(SessionAbort::SegmentStall));
    }

    #[tokio::test]
    async fn unwatched_session_past_the_threshold_does_not_stall() {
        // No heartbeat file: nobody is asking for the playlist, so there is no
        // viewer to starve. The idle timeout owns this case.
        let dir = TempDir::new().unwrap();

        let mut pm = manager(
            dir.path(),
            OffsetDateTime::now_utc() - time::Duration::seconds(THRESHOLD_SECONDS + 5),
        )
        .await;
        pm.update().await.unwrap();

        assert_eq!(pm.abort(), None);
    }

    /// The bug this guards: `NamedTempFile` creates its backing file `0600`,
    /// and a bare rename into the output folder carried that mode onto files
    /// we serve over HTTP — while ffmpeg's own files in the same folder land
    /// `0644` from the normal process umask. Both playlists `update()` writes
    /// go out through [`write_atomically`], so both must come out `0644`.
    #[cfg(unix)]
    #[tokio::test]
    async fn generated_playlists_are_world_readable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        watch(dir.path()).await;

        let mut pm = manager(dir.path(), OffsetDateTime::now_utc()).await;
        pm.update().await.unwrap();

        for name in ["live.m3u8", "live_sub.m3u8"] {
            let mode = std::fs::metadata(dir.path().join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o644, "{name} should be 0644, was {mode:o}");
        }
    }

    /// Give `pm` `count` segments, as `update` would once ffmpeg had written
    /// them, without needing ffmpeg to have written anything.
    fn push_segments(pm: &mut PlaylistManager, count: usize) {
        for i in 0..count {
            let program_date_time = pm.last_segment_end;
            pm.segments.push_back(Segment {
                path: format!("live{i:06}.ts"),
                duration: pm.target_duration_f64,
                program_date_time,
            });
            pm.last_segment_end += Duration::from_secs_f64(pm.target_duration_f64);
        }
    }

    #[tokio::test]
    async fn a_fresh_session_numbers_from_zero() {
        let dir = TempDir::new().unwrap();

        let pm = manager(dir.path(), OffsetDateTime::now_utc()).await;

        assert_eq!(pm.media_sequence, 0);
        assert_eq!(pm.discontinuity_sequence, 0);
        assert!(
            !pm.pending_discontinuity,
            "nothing precedes the first segment of a new session, so there is \
             nothing for it to be discontinuous with"
        );
    }

    /// The freeze this whole mechanism exists to prevent: a worker exits, the
    /// server wipes the folder and respawns it, and the replacement must not
    /// hand a still-tuned client segment numbers it has already consumed.
    #[tokio::test]
    async fn a_restarted_worker_numbers_above_every_segment_the_last_one_published() {
        let dir = TempDir::new().unwrap();
        watch(dir.path()).await;

        let mut first = manager(dir.path(), OffsetDateTime::now_utc()).await;
        push_segments(&mut first, 12);
        first.media_sequence = 138;
        first.update().await.unwrap();

        // 138 already trimmed away plus the 12 still held: the client may have
        // consumed through 149, so 150 is the first number free to reuse.
        assert_eq!(first.sequence_state().next_media_sequence, 150);

        ersatztv_core::empty_folder(dir.path()).await.unwrap();
        let second = manager(dir.path(), OffsetDateTime::now_utc()).await;

        assert_eq!(second.media_sequence, 150);
        assert_eq!(second.last_served_media_sequence, 150);
        assert!(
            second.pending_discontinuity,
            "a resumed session's first segment comes from a different encoder run"
        );
    }

    #[tokio::test]
    async fn a_restarted_worker_carries_the_discontinuity_count_forward() {
        let dir = TempDir::new().unwrap();
        watch(dir.path()).await;

        let mut first = manager(dir.path(), OffsetDateTime::now_utc()).await;
        push_segments(&mut first, 4);
        first.discontinuity_sequence = 2;
        first
            .discontinuity_before
            .insert("live000002.ts".to_owned());
        first.update().await.unwrap();

        // Two already trimmed past, one still held ahead of a live segment.
        assert_eq!(first.sequence_state().next_discontinuity_sequence, 3);

        ersatztv_core::empty_folder(dir.path()).await.unwrap();
        let second = manager(dir.path(), OffsetDateTime::now_utc()).await;

        assert_eq!(second.discontinuity_sequence, 3);
    }

    /// A counter file caught half-written by a crash must not stop the channel
    /// from starting — the cost of ignoring it is one client re-tuning, and the
    /// cost of refusing is a channel nobody can watch.
    #[tokio::test]
    async fn a_corrupt_sequence_file_is_ignored_rather_than_fatal() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join(SEQUENCE_FILE_NAME), b"{\"next_media_seq")
            .await
            .unwrap();

        let pm = manager(dir.path(), OffsetDateTime::now_utc()).await;

        assert_eq!(pm.media_sequence, 0);
    }

    /// The published playlist is the promise; the counter file must never
    /// advertise numbering that playlist did not carry.
    #[tokio::test]
    async fn the_persisted_counter_never_runs_ahead_of_the_published_playlist() {
        let dir = TempDir::new().unwrap();
        watch(dir.path()).await;

        let mut pm = manager(dir.path(), OffsetDateTime::now_utc()).await;
        push_segments(&mut pm, 6);
        pm.update().await.unwrap();

        let playlist = tokio::fs::read_to_string(dir.path().join("live.m3u8"))
            .await
            .unwrap();
        let published: u64 = playlist
            .lines()
            .find_map(|l| l.strip_prefix("#EXT-X-MEDIA-SEQUENCE:"))
            .expect("playlist carries a media sequence")
            .parse()
            .unwrap();
        let segments_listed = playlist.lines().filter(|l| l.ends_with(".ts")).count() as u64;

        let persisted: SequenceState = serde_json::from_str(
            &tokio::fs::read_to_string(dir.path().join(SEQUENCE_FILE_NAME))
                .await
                .unwrap(),
        )
        .unwrap();

        assert_eq!(persisted.next_media_sequence, published + segments_listed);
    }
}

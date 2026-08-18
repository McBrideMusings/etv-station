//! The per-channel overlay timeline: which overlay config is on screen when.
//!
//! One file, `overlay.json`, in the same folder the station writes a channel's
//! playout chunks to and the overlay process already reads them from. The
//! station writes it; the overlay process polls it and swaps its script and
//! layers when the airing span changes (#48, ADR 0007).
//!
//! # Why a precomputed timeline rather than a "current config" file
//!
//! The station knows every block's wall-clock span at generation time — that is
//! what generating a schedule *is*. Writing the whole timeline means nothing has
//! to notice a block boundary as it passes: the overlay reads a clock and looks
//! up a span, exactly as it already does to answer "what is airing now" for
//! `title` and `item_elapsed`. A file holding only the current config would need
//! the station to be awake and correct at every boundary, and would be stale
//! after any restart — the same reasons the playout itself is a written timeline
//! rather than a live feed.
//!
//! # Shape
//!
//! ```json
//! {
//!   "base": { "width": 1280, "height": 720, "framerate": 30, "layers": [...] },
//!   "spans": [
//!     { "start": "...", "finish": "...", "spec": { ... } },
//!     { "start": "...", "finish": "...", "spec": null }
//!   ]
//! }
//! ```
//!
//! `base` is the channel's own resolved config — what sized the canvas and the
//! fifo at spawn, and what plays whenever no span covers the moment (before the
//! first generation, or in a gap the station has not filled yet). A `spec` of
//! `null` is a block that draws nothing; the process stays up and renders
//! transparent frames, because the fifo it owns is being read by an ffmpeg that
//! must not see it close.
//!
//! `spans` is empty on the common channel whose overlay never varies by block —
//! there is nothing to say that `base` does not already say.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::overlay_spec::OverlaySpec;

/// The timeline's filename inside a channel's playout folder.
///
/// It deliberately contains no `_`, which is what keeps it out of
/// [`crate::program_context`]'s chunk-file filter: chunks are
/// `{start}_{finish}.json`, and that underscore is the discriminator saying "a
/// schedule file, not a sidecar".
pub const OVERLAY_TIMELINE_FILE_NAME: &str = "overlay.json";

/// How often the file's mtime is checked. Matches
/// [`crate::program_context`]'s own poll: both answer "what is on screen now"
/// from the same folder, and two different staleness windows would let the
/// overlay swap its layers a second before or after the title it is drawing.
const MTIME_POLL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OverlayTimeline {
    /// The channel's own resolved config: the spawn config, and the fallback
    /// for any moment no span covers.
    pub base: OverlaySpec,
    /// Block spans in ascending, non-overlapping start order.
    #[serde(default)]
    pub spans: Vec<OverlaySpan>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OverlaySpan {
    #[serde(with = "time::serde::rfc3339")]
    pub start: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub finish: OffsetDateTime,
    /// `None` — written as `null` — is a span that draws nothing.
    pub spec: Option<OverlaySpec>,
}

impl OverlayTimeline {
    /// The config on screen at `now`: the covering span's, or `base` when no
    /// span covers it.
    ///
    /// `None` means nothing is drawn. That is only ever a span saying so
    /// explicitly — an uncovered moment falls back to `base`, never to blank,
    /// because a station that is behind on generating should leave the channel
    /// looking like itself rather than stripping its watermark.
    pub fn spec_at(&self, now: OffsetDateTime) -> Option<&OverlaySpec> {
        let after = self.spans.partition_point(|s| s.start <= now);
        if after > 0 {
            let span = &self.spans[after - 1];
            if now >= span.start && now < span.finish {
                return span.spec.as_ref();
            }
        }
        Some(&self.base)
    }
}

/// Loads and caches a channel's `overlay.json`, re-reading only when the file's
/// mtime moves.
///
/// Separate from [`crate::program_context::ProgramContextSource`] rather than
/// folded into it because the two answer questions with different lifetimes: a
/// program context is rebuilt every frame from an in-memory list, while a
/// timeline change costs a Rhai engine rebuild and so has to be reported as an
/// *edge*, once.
pub struct OverlayTimelineSource {
    path: PathBuf,
    timeline: Option<OverlayTimeline>,
    file_mtime: Option<SystemTime>,
    last_mtime_check: Option<Instant>,
}

impl OverlayTimelineSource {
    /// Watch the timeline in `folder`, a channel's playout folder.
    pub fn new(folder: &Path) -> Self {
        Self {
            path: folder.join(OVERLAY_TIMELINE_FILE_NAME),
            timeline: None,
            file_mtime: None,
            last_mtime_check: None,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Re-read the timeline if its mtime has moved since the last successful
    /// read. Rate-limited to one `stat` per [`MTIME_POLL`].
    ///
    /// Returns `true` when a new timeline was loaded. A parse failure is an
    /// error and leaves the previously loaded timeline in place: a half-written
    /// or malformed file must not blank a channel's overlay.
    pub fn refresh(&mut self) -> std::io::Result<bool> {
        let now = Instant::now();
        if let Some(prev) = self.last_mtime_check
            && now.duration_since(prev) < MTIME_POLL
        {
            return Ok(false);
        }
        self.last_mtime_check = Some(now);

        let meta = match std::fs::metadata(&self.path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        let mtime = meta.modified().ok();
        if mtime == self.file_mtime && self.timeline.is_some() {
            return Ok(false);
        }

        let raw = std::fs::read_to_string(&self.path)?;
        let timeline: OverlayTimeline = serde_json::from_str(&raw)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        self.file_mtime = mtime;
        self.timeline = Some(timeline);
        Ok(true)
    }

    /// The config on screen at `now`, or `None` when no timeline has loaded yet
    /// or the covering span draws nothing.
    pub fn spec_at(&self, now: OffsetDateTime) -> Option<&OverlaySpec> {
        self.timeline.as_ref()?.spec_at(now)
    }

    /// Whether a timeline has been loaded at all. Distinguishes "the file says
    /// draw nothing" from "there is no file", which [`Self::spec_at`] cannot.
    pub fn is_loaded(&self) -> bool {
        self.timeline.is_some()
    }

    /// The channel's spawn config — geometry and the fallback config for an
    /// uncovered moment. `None` only until the first successful [`Self::refresh`].
    pub fn base(&self) -> Option<&OverlaySpec> {
        self.timeline.as_ref().map(|t| &t.base)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay_spec::PixelFormat;
    use time::macros::datetime;

    fn spec(framerate: u32) -> OverlaySpec {
        OverlaySpec {
            width: 1280,
            height: 720,
            framerate,
            pixel_format: PixelFormat::Rgba8,
            script: None,
            config: None,
            layers: vec![],
        }
    }

    fn timeline() -> OverlayTimeline {
        OverlayTimeline {
            base: spec(30),
            spans: vec![
                OverlaySpan {
                    start: datetime!(2026-04-13 00:00 UTC),
                    finish: datetime!(2026-04-13 01:00 UTC),
                    spec: Some(spec(31)),
                },
                OverlaySpan {
                    start: datetime!(2026-04-13 01:00 UTC),
                    finish: datetime!(2026-04-13 02:00 UTC),
                    spec: None,
                },
            ],
        }
    }

    #[test]
    fn a_covered_moment_reads_its_spans_spec() {
        assert_eq!(
            timeline().spec_at(datetime!(2026-04-13 00:30 UTC)),
            Some(&spec(31))
        );
    }

    /// A span that draws nothing is not the same as an uncovered moment, and
    /// the difference is the whole reason `spec` is an `Option` rather than the
    /// span being left out: a block saying `overlay: clear` has to beat the
    /// channel's watermark, not fall back to it.
    #[test]
    fn a_span_that_draws_nothing_does_not_fall_back_to_base() {
        assert_eq!(timeline().spec_at(datetime!(2026-04-13 01:30 UTC)), None);
    }

    /// A station that has not generated this far ahead — or one restarted
    /// before its first generation — leaves the channel looking like itself.
    #[test]
    fn an_uncovered_moment_falls_back_to_base() {
        let t = timeline();
        assert_eq!(t.spec_at(datetime!(2026-04-12 23:00 UTC)), Some(&spec(30)));
        assert_eq!(t.spec_at(datetime!(2026-04-13 03:00 UTC)), Some(&spec(30)));
    }

    #[test]
    fn a_timeline_with_no_spans_is_base_everywhere() {
        let t = OverlayTimeline {
            base: spec(30),
            spans: vec![],
        };
        assert_eq!(t.spec_at(datetime!(2026-04-13 00:30 UTC)), Some(&spec(30)));
    }

    #[test]
    fn the_file_round_trips_through_json() {
        let written = serde_json::to_string(&timeline()).unwrap();
        let read: OverlayTimeline = serde_json::from_str(&written).unwrap();
        assert_eq!(read, timeline());
    }

    #[test]
    fn the_source_loads_and_reloads_on_mtime_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(OVERLAY_TIMELINE_FILE_NAME);
        std::fs::write(&path, serde_json::to_string(&timeline()).unwrap()).unwrap();

        let mut src = OverlayTimelineSource::new(dir.path());
        assert!(src.refresh().unwrap(), "the first refresh loads the file");
        assert_eq!(
            src.spec_at(datetime!(2026-04-13 00:30 UTC)),
            Some(&spec(31))
        );

        let mut replaced = timeline();
        replaced.spans[0].spec = Some(spec(32));
        std::fs::write(&path, serde_json::to_string(&replaced).unwrap()).unwrap();
        // Filesystem mtime resolution varies; set it rather than sleeping.
        let bumped =
            std::fs::metadata(&path).unwrap().modified().unwrap() + Duration::from_secs(30);
        std::fs::File::open(&path)
            .unwrap()
            .set_modified(bumped)
            .unwrap();
        src.last_mtime_check = None;

        assert!(src.refresh().unwrap(), "a bumped mtime reloads");
        assert_eq!(
            src.spec_at(datetime!(2026-04-13 00:30 UTC)),
            Some(&spec(32))
        );
    }

    /// A malformed file is an error, not a blank overlay: the station writes
    /// this atomically, so the only way to see one is real corruption, and
    /// dropping the last good timeline over it would strip the channel's
    /// watermark until someone noticed.
    #[test]
    fn a_malformed_file_keeps_the_last_good_timeline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(OVERLAY_TIMELINE_FILE_NAME);
        std::fs::write(&path, serde_json::to_string(&timeline()).unwrap()).unwrap();

        let mut src = OverlayTimelineSource::new(dir.path());
        src.refresh().unwrap();

        std::fs::write(&path, "{ not json").unwrap();
        let bumped =
            std::fs::metadata(&path).unwrap().modified().unwrap() + Duration::from_secs(30);
        std::fs::File::open(&path)
            .unwrap()
            .set_modified(bumped)
            .unwrap();
        src.last_mtime_check = None;

        assert!(src.refresh().is_err());
        assert_eq!(
            src.spec_at(datetime!(2026-04-13 00:30 UTC)),
            Some(&spec(31)),
            "the last good timeline must survive a corrupt write"
        );
    }

    #[test]
    fn an_absent_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut src = OverlayTimelineSource::new(dir.path());
        assert!(!src.refresh().unwrap());
        assert!(!src.is_loaded());
    }
}

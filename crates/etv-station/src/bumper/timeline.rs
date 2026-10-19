//! The bumper model and its flex solver.
//!
//! A [`Timeline`] is an ordered list of [`Segment`]s whose durations may be
//! flexible. [`fit`] resolves one onto a gap of `gap_ms` at a channel
//! framerate and returns a [`ResolvedTimeline`] whose segments are consecutive,
//! sit on the frame grid, and sum to the gap to the millisecond. It is pure:
//! no clock, no filesystem, no logging. What it did that a caller may want to
//! log (dropped segments, a black filler) is returned in the result.
//!
//! The solver follows the CSS flexbox algorithm:
//!
//! 1. `remaining = gap - sum(Fixed)`.
//! 2. While `remaining < sum(Flex.min)`, drop optional segments by ascending
//!    `priority`. Still short: drop every flex segment, keeping only the last
//!    `loop_` segment to fill.
//! 3. Spread `remaining - sum(min)` over the flex segments by `grow`, clamped
//!    to `[min, max]`; a clamped segment's overflow goes to the rest.
//! 4. Still short of the gap: extend the last `loop_` segment, else append a
//!    black filler.
//! 5. Snap every boundary to the frame grid; the final boundary is the gap
//!    itself, so the rounding residual lands on the last segment.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Name of the black segment [`fit`] appends when nothing can fill the gap.
pub const FILLER_NAME: &str = "filler";

/// Where a segment's picture (and, with [`SegmentAudio::Own`], sound) comes from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentSource {
    /// A slice of a media file, from a clip lookup or search.
    Clip {
        path: PathBuf,
        in_ms: u64,
        out_ms: u64,
    },
    /// An ffmpeg lavfi graph, e.g. `color=c=black`.
    Lavfi { params: String },
    /// An image held on screen.
    Still { image_path: PathBuf },
    /// A render-cache file for `profile`.
    Render { profile: String, path: PathBuf },
    /// A render that has not landed yet: `stand_in` airs now, and
    /// [`crate::reconcile::swap_pending_playout`] swaps `path` in once the
    /// file is readable and before the segment airs.
    Pending { path: PathBuf, stand_in: PathBuf },
    /// A slice of a remote file ETV-next fetches over HTTP(S). `url` is a
    /// playout URI template: a `{{VAR}}` in it is expanded from ETV-next's own
    /// environment at play time, so a token never enters the playout file.
    Http {
        url: String,
        in_ms: u64,
        out_ms: u64,
    },
}

impl SegmentSource {
    /// Black video, the solver's filler.
    pub fn black() -> Self {
        SegmentSource::Lavfi {
            params: "color=c=black".to_string(),
        }
    }
}

/// A segment's length: exact, or a range the solver picks from.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Duration {
    Fixed(u64),
    /// `grow` is the weight in the share of spare time; `0.0` stays at `min_ms`.
    Flex {
        min_ms: u64,
        max_ms: u64,
        grow: f32,
    },
}

/// Which sound a segment plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentAudio {
    /// The source's own audio.
    #[default]
    Own,
    /// The timeline's [`Timeline::bed`].
    Bed,
    /// Silence.
    Mute,
}

/// An overlay script and the variables it draws with; becomes the playout
/// item's metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverlayVars {
    pub script: String,
    #[serde(default)]
    pub vars: BTreeMap<String, serde_json::Value>,
}

/// A music or ambience file played under [`SegmentAudio::Bed`] segments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioSource {
    pub path: PathBuf,
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default)]
    pub fade_in_ms: u64,
    #[serde(default)]
    pub fade_out_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub name: String,
    pub source: SegmentSource,
    pub duration: Duration,
    /// May repeat to fill (lavfi, stills, short renders).
    #[serde(default, rename = "loop")]
    pub loop_: bool,
    /// Dropped first when the gap is too short.
    #[serde(default)]
    pub optional: bool,
    /// Drop order among optionals; lower drops first.
    #[serde(default)]
    pub priority: u8,
    #[serde(default)]
    pub overlay: Option<OverlayVars>,
    #[serde(default)]
    pub audio: SegmentAudio,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Timeline {
    pub segments: Vec<Segment>,
    #[serde(default)]
    pub bed: Option<AudioSource>,
}

/// One segment placed in the gap.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedSegment {
    pub segment: Segment,
    /// Offset from the start of the gap.
    pub start_ms: u64,
    pub duration_ms: u64,
    /// True for the black segment the solver appended; it has no entry in the
    /// input timeline.
    pub filler: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedTimeline {
    pub segments: Vec<ResolvedSegment>,
    pub bed: Option<AudioSource>,
    /// Names of the input segments the solver dropped, in drop order.
    pub dropped: Vec<String>,
}

impl ResolvedTimeline {
    pub fn total_ms(&self) -> u64 {
        self.segments.iter().map(|s| s.duration_ms).sum()
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum FitError {
    #[error("the timeline has no segments")]
    Empty,
    #[error("the gap is zero milliseconds")]
    ZeroGap,
    #[error("framerate {0} is not a positive finite number")]
    BadFps(f64),
    #[error("segment {segment:?}: flex min {min_ms}ms exceeds max {max_ms}ms")]
    BadFlex {
        segment: String,
        min_ms: u64,
        max_ms: u64,
    },
    #[error("segment {segment:?}: grow {grow} is not a finite number >= 0")]
    BadGrow { segment: String, grow: f32 },
    #[error("fixed segments need {fixed_ms}ms, more than the {gap_ms}ms gap")]
    FixedExceedsGap { fixed_ms: u64, gap_ms: u64 },
}

/// Resolve `timeline` onto `gap_ms` at `fps` frames per second.
pub fn fit(timeline: &Timeline, gap_ms: u64, fps: f64) -> Result<ResolvedTimeline, FitError> {
    if timeline.segments.is_empty() {
        return Err(FitError::Empty);
    }
    if gap_ms == 0 {
        return Err(FitError::ZeroGap);
    }
    if !fps.is_finite() || fps <= 0.0 {
        return Err(FitError::BadFps(fps));
    }
    for seg in &timeline.segments {
        if let Duration::Flex {
            min_ms,
            max_ms,
            grow,
        } = seg.duration
        {
            if min_ms > max_ms {
                return Err(FitError::BadFlex {
                    segment: seg.name.clone(),
                    min_ms,
                    max_ms,
                });
            }
            if !grow.is_finite() || grow < 0.0 {
                return Err(FitError::BadGrow {
                    segment: seg.name.clone(),
                    grow,
                });
            }
        }
    }

    let gap = gap_ms as i64;
    let mut live: Vec<usize> = (0..timeline.segments.len()).collect();
    let mut dropped: Vec<usize> = Vec::new();

    // Step 2: drop optionals, lowest priority first (the later one on a tie),
    // until the fixed segments and flex minimums fit.
    while !feasible(timeline, &live, gap) {
        let victim = live
            .iter()
            .copied()
            .filter(|&i| timeline.segments[i].optional)
            .min_by_key(|&i| (timeline.segments[i].priority, std::cmp::Reverse(i)));
        match victim {
            Some(i) => {
                live.retain(|&j| j != i);
                dropped.push(i);
            }
            None => break,
        }
    }

    // Still infeasible: drop every flex segment except the last loop_ one,
    // which stays to fill whatever the fixed segments leave.
    let fits = feasible(timeline, &live, gap);
    if !fits {
        let keep = live.iter().copied().rfind(|&i| timeline.segments[i].loop_);
        let (kept, gone): (Vec<usize>, Vec<usize>) = live.iter().copied().partition(|&i| {
            matches!(timeline.segments[i].duration, Duration::Fixed(_)) || Some(i) == keep
        });
        dropped.extend(gone);
        live = kept;
    }

    let fixed: i64 = live
        .iter()
        .map(|&i| match timeline.segments[i].duration {
            Duration::Fixed(ms) => ms as i64,
            Duration::Flex { .. } => 0,
        })
        .sum();
    if fixed > gap {
        return Err(FitError::FixedExceedsGap {
            fixed_ms: fixed as u64,
            gap_ms,
        });
    }

    // Step 3: durations in f64 milliseconds, in `live` order.
    let mut durs: Vec<f64> = live
        .iter()
        .map(|&i| match timeline.segments[i].duration {
            Duration::Fixed(ms) => ms as f64,
            Duration::Flex { min_ms, .. } => min_ms as f64,
        })
        .collect();
    if fits {
        grow_flex(timeline, &live, &mut durs, (gap - fixed) as f64);
    } else {
        // The one flex segment left is the kept loop_ one; it takes the room.
        for (d, &i) in durs.iter_mut().zip(&live) {
            if matches!(timeline.segments[i].duration, Duration::Flex { .. }) {
                *d = (gap - fixed) as f64;
            }
        }
    }

    // Step 4: still short of the gap.
    let mut filler = false;
    let short = gap as f64 - durs.iter().sum::<f64>();
    // Float shares can sum a hair under the gap; under half a millisecond is
    // that noise, and the residual rule below places it.
    if short >= 0.5 {
        match live.iter().rposition(|&i| timeline.segments[i].loop_) {
            Some(pos) => durs[pos] += short,
            None => filler = true,
        }
    }

    // Step 5: snap interior boundaries to the frame grid; the last boundary is
    // the gap, so the residual lands on the last segment. A segment that snaps
    // to zero length is omitted.
    let mut placed: Vec<(Segment, f64, bool)> = live
        .iter()
        .zip(&durs)
        .map(|(&i, &d)| (timeline.segments[i].clone(), d, false))
        .collect();
    if filler {
        let seg = Segment {
            name: FILLER_NAME.to_string(),
            source: SegmentSource::black(),
            duration: Duration::Fixed(0),
            loop_: false,
            optional: false,
            priority: 0,
            overlay: None,
            audio: SegmentAudio::Mute,
        };
        placed.push((seg, short, true));
    }
    let last = placed.len() - 1;
    let mut dropped_names: Vec<String> = dropped
        .into_iter()
        .map(|i| timeline.segments[i].name.clone())
        .collect();
    let mut resolved = Vec::with_capacity(placed.len());
    let mut cumulative = 0.0;
    let mut prev_boundary = 0u64;
    for (idx, (segment, dur, is_filler)) in placed.into_iter().enumerate() {
        cumulative += dur;
        let boundary = if idx == last {
            gap_ms
        } else {
            snap_to_frame(cumulative, fps).min(gap_ms)
        };
        if boundary > prev_boundary {
            resolved.push(ResolvedSegment {
                segment,
                start_ms: prev_boundary,
                duration_ms: boundary - prev_boundary,
                filler: is_filler,
            });
        } else {
            dropped_names.push(segment.name);
        }
        prev_boundary = boundary;
    }

    Ok(ResolvedTimeline {
        segments: resolved,
        bed: timeline.bed.clone(),
        dropped: dropped_names,
    })
}

/// Whether the fixed segments plus the flex minimums among `live` fit in `gap`.
fn feasible(timeline: &Timeline, live: &[usize], gap: i64) -> bool {
    let needed: i64 = live
        .iter()
        .map(|&i| match timeline.segments[i].duration {
            Duration::Fixed(ms) => ms as i64,
            Duration::Flex { min_ms, .. } => min_ms as i64,
        })
        .sum();
    needed <= gap
}

/// Step 3: share `remaining` (the gap minus the fixed segments) over the flex
/// segments, starting from their minimums, by `grow`. A segment that would
/// pass its max is frozen there and the rest are shared again; each round
/// freezes at least one, so the loop ends within `live.len()` rounds.
fn grow_flex(timeline: &Timeline, live: &[usize], durs: &mut [f64], remaining: f64) {
    // (position in `live`, min, max, grow)
    let flex: Vec<(usize, f64, f64, f64)> = live
        .iter()
        .enumerate()
        .filter_map(|(pos, &i)| match timeline.segments[i].duration {
            Duration::Flex {
                min_ms,
                max_ms,
                grow,
            } => Some((pos, min_ms as f64, max_ms as f64, grow as f64)),
            Duration::Fixed(_) => None,
        })
        .collect();
    let mut frozen = vec![false; flex.len()];
    let mut spare = remaining - flex.iter().map(|f| f.1).sum::<f64>();
    loop {
        let weight: f64 = flex
            .iter()
            .zip(&frozen)
            .filter(|(_, fz)| !**fz)
            .map(|(f, _)| f.3)
            .sum();
        if weight <= 0.0 || spare <= 0.0 {
            return;
        }
        let mut overflowed = false;
        for (k, f) in flex.iter().enumerate() {
            if !frozen[k] && f.1 + spare * f.3 / weight > f.2 {
                durs[f.0] = f.2;
                spare -= f.2 - f.1;
                frozen[k] = true;
                overflowed = true;
            }
        }
        if !overflowed {
            for (k, f) in flex.iter().enumerate() {
                if !frozen[k] {
                    durs[f.0] = f.1 + spare * f.3 / weight;
                }
            }
            return;
        }
    }
}

/// The millisecond of the frame boundary nearest `ms`.
fn snap_to_frame(ms: f64, fps: f64) -> u64 {
    let frames = (ms * fps / 1000.0).round();
    (frames * 1000.0 / fps).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(name: &str, duration: Duration) -> Segment {
        Segment {
            name: name.to_string(),
            source: SegmentSource::black(),
            duration,
            loop_: false,
            optional: false,
            priority: 0,
            overlay: None,
            audio: SegmentAudio::Own,
        }
    }

    fn flex(min_ms: u64, max_ms: u64, grow: f32) -> Duration {
        Duration::Flex {
            min_ms,
            max_ms,
            grow,
        }
    }

    fn tl(segments: Vec<Segment>) -> Timeline {
        Timeline {
            segments,
            bed: None,
        }
    }

    fn shape(r: &ResolvedTimeline) -> Vec<(&str, u64, u64)> {
        r.segments
            .iter()
            .map(|s| (s.segment.name.as_str(), s.start_ms, s.duration_ms))
            .collect()
    }

    #[test]
    fn fixed_only_fills_the_gap_with_a_black_filler() {
        let t = tl(vec![
            seg("a", Duration::Fixed(2000)),
            seg("b", Duration::Fixed(3000)),
        ]);
        let r = fit(&t, 6000, 25.0).unwrap();
        assert_eq!(
            shape(&r),
            [("a", 0, 2000), ("b", 2000, 3000), ("filler", 5000, 1000)]
        );
        assert!(r.segments[2].filler);
        assert_eq!(r.total_ms(), 6000);
    }

    #[test]
    fn fixed_only_extends_the_last_loop_segment_not_a_filler() {
        let mut looped = seg("b", Duration::Fixed(3000));
        looped.loop_ = true;
        let t = tl(vec![seg("a", Duration::Fixed(2000)), looped]);
        let r = fit(&t, 6000, 25.0).unwrap();
        assert_eq!(shape(&r), [("a", 0, 2000), ("b", 2000, 4000)]);
        assert!(r.segments.iter().all(|s| !s.filler));
    }

    #[test]
    fn flex_only_splits_by_grow() {
        let t = tl(vec![
            seg("a", flex(1000, 10_000, 1.0)),
            seg("b", flex(1000, 10_000, 3.0)),
        ]);
        let r = fit(&t, 10_000, 25.0).unwrap();
        // spare 8000 -> a 1000+2000, b 1000+6000.
        assert_eq!(shape(&r), [("a", 0, 3000), ("b", 3000, 7000)]);
    }

    #[test]
    fn infeasible_drops_optionals_lowest_priority_first() {
        let mut b = seg("b", flex(2000, 4000, 1.0));
        b.optional = true;
        b.priority = 1;
        let mut c = seg("c", flex(1000, 2000, 1.0));
        c.optional = true;
        c.priority = 2;
        let t = tl(vec![seg("a", Duration::Fixed(3000)), b, c]);
        let r = fit(&t, 5000, 25.0).unwrap();
        assert_eq!(r.dropped, ["b"]);
        assert_eq!(shape(&r), [("a", 0, 3000), ("c", 3000, 2000)]);
    }

    #[test]
    fn infeasible_drops_a_fixed_optional_too() {
        let mut a = seg("a", Duration::Fixed(4000));
        a.optional = true;
        let t = tl(vec![a, seg("b", flex(3000, 6000, 1.0))]);
        let r = fit(&t, 5000, 25.0).unwrap();
        assert_eq!(r.dropped, ["a"]);
        assert_eq!(shape(&r), [("b", 0, 5000)]);
    }

    #[test]
    fn infeasible_with_loop_keeps_the_loop_segment_and_drops_other_flex() {
        let mut looped = seg("loop", flex(2000, 4000, 1.0));
        looped.loop_ = true;
        let t = tl(vec![
            seg("a", Duration::Fixed(2000)),
            seg("other", flex(1000, 2000, 1.0)),
            looped,
        ]);
        let r = fit(&t, 3000, 25.0).unwrap();
        assert_eq!(r.dropped, ["other"]);
        assert_eq!(shape(&r), [("a", 0, 2000), ("loop", 2000, 1000)]);
    }

    #[test]
    fn infeasible_without_loop_ends_in_black() {
        let t = tl(vec![
            seg("a", Duration::Fixed(2000)),
            seg("flex", flex(2000, 4000, 1.0)),
        ]);
        let r = fit(&t, 3000, 25.0).unwrap();
        assert_eq!(r.dropped, ["flex"]);
        assert_eq!(shape(&r), [("a", 0, 2000), ("filler", 2000, 1000)]);
    }

    #[test]
    fn fixed_segments_longer_than_the_gap_are_an_error() {
        let t = tl(vec![seg("a", Duration::Fixed(5000))]);
        assert_eq!(
            fit(&t, 3000, 25.0),
            Err(FitError::FixedExceedsGap {
                fixed_ms: 5000,
                gap_ms: 3000
            })
        );
    }

    #[test]
    fn clamp_overflow_goes_to_the_unclamped_segment() {
        let t = tl(vec![
            seg("a", flex(1000, 2000, 1.0)),
            seg("b", flex(1000, 10_000, 1.0)),
        ]);
        let r = fit(&t, 10_000, 25.0).unwrap();
        // a would take 5000 but clamps at 2000; b takes the rest.
        assert_eq!(shape(&r), [("a", 0, 2000), ("b", 2000, 8000)]);
    }

    #[test]
    fn clamped_everywhere_extends_the_loop_segment() {
        let mut a = seg("a", flex(1000, 2000, 1.0));
        a.loop_ = true;
        let t = tl(vec![a, seg("b", flex(1000, 2000, 1.0))]);
        let r = fit(&t, 10_000, 25.0).unwrap();
        assert_eq!(shape(&r), [("a", 0, 8000), ("b", 8000, 2000)]);
    }

    #[test]
    fn zero_grow_stays_at_min() {
        let t = tl(vec![
            seg("a", flex(1000, 5000, 0.0)),
            seg("b", flex(1000, 9000, 1.0)),
        ]);
        let r = fit(&t, 6000, 25.0).unwrap();
        assert_eq!(shape(&r), [("a", 0, 1000), ("b", 1000, 5000)]);
    }

    #[test]
    fn boundaries_land_on_the_frame_grid_and_the_residual_on_the_last() {
        // 29.97fps: 1000ms is 29.97 frames -> 30 frames -> 1001ms.
        let t = tl(vec![
            seg("a", Duration::Fixed(1000)),
            seg("b", flex(1000, 20_000, 1.0)),
        ]);
        let r = fit(&t, 10_000, 29.97).unwrap();
        assert_eq!(shape(&r), [("a", 0, 1001), ("b", 1001, 8999)]);
        assert_eq!(r.total_ms(), 10_000);
    }

    #[test]
    fn sum_equals_the_gap_for_awkward_gaps() {
        let t = tl(vec![
            seg("a", flex(500, 9000, 1.0)),
            seg("b", flex(500, 9000, 2.0)),
            seg("c", flex(500, 9000, 3.0)),
        ]);
        for gap in [4_999u64, 7_001, 12_345, 20_017] {
            for fps in [24.0, 25.0, 29.97, 30.0, 59.94] {
                let r = fit(&t, gap, fps).unwrap();
                assert_eq!(r.total_ms(), gap, "gap {gap} fps {fps}");
                let mut at = 0;
                for s in &r.segments {
                    assert_eq!(s.start_ms, at);
                    at += s.duration_ms;
                }
            }
        }
    }

    #[test]
    fn a_segment_shorter_than_a_frame_is_reported_dropped() {
        let t = tl(vec![
            seg("blip", Duration::Fixed(5)),
            seg("b", flex(1000, 9000, 1.0)),
        ]);
        let r = fit(&t, 5000, 25.0).unwrap();
        assert_eq!(r.dropped, ["blip"]);
        assert_eq!(shape(&r), [("b", 0, 5000)]);
    }

    #[test]
    fn rejects_unusable_input() {
        let ok = tl(vec![seg("a", Duration::Fixed(1000))]);
        assert_eq!(fit(&Timeline::default(), 1000, 25.0), Err(FitError::Empty));
        assert_eq!(fit(&ok, 0, 25.0), Err(FitError::ZeroGap));
        assert_eq!(fit(&ok, 1000, 0.0), Err(FitError::BadFps(0.0)));
        let bad = tl(vec![seg("a", flex(5000, 1000, 1.0))]);
        assert!(matches!(
            fit(&bad, 9000, 25.0),
            Err(FitError::BadFlex { .. })
        ));
        let neg = tl(vec![seg("a", flex(1000, 2000, -1.0))]);
        assert!(matches!(
            fit(&neg, 9000, 25.0),
            Err(FitError::BadGrow { .. })
        ));
    }

    #[test]
    fn the_bed_carries_through() {
        let mut t = tl(vec![seg("a", Duration::Fixed(1000))]);
        t.bed = Some(AudioSource {
            path: "bed.flac".into(),
            gain_db: -6.0,
            fade_in_ms: 0,
            fade_out_ms: 500,
        });
        assert_eq!(fit(&t, 1000, 25.0).unwrap().bed, t.bed);
    }
}

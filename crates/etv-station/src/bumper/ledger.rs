//! The airings-ledger rows for a resolved bumper timeline: one row per
//! segment, so `ctx.airings` can answer "which clip did this channel last
//! play" for bumpers as well as programmes.

use time::OffsetDateTime;

use super::timeline::{ResolvedTimeline, SegmentSource};
use crate::history::{AiringKind, PlayRecord};

/// One bumper-kind [`PlayRecord`] per segment of `resolved`, placed at
/// `gap_start` plus the segment's offset. `produced_by` names the plugin
/// script that built the timeline; `written_at` is when the generation ran.
pub fn records(
    resolved: &ResolvedTimeline,
    gap_start: OffsetDateTime,
    written_at: OffsetDateTime,
    produced_by: &str,
) -> Vec<PlayRecord> {
    resolved
        .segments
        .iter()
        .map(|placed| {
            let (source, in_ms, out_ms) = match &placed.segment.source {
                SegmentSource::Clip {
                    path,
                    in_ms,
                    out_ms,
                } => (path.display().to_string(), Some(*in_ms), Some(*out_ms)),
                SegmentSource::Http { url, in_ms, out_ms } => {
                    (url.clone(), Some(*in_ms), Some(*out_ms))
                }
                SegmentSource::Lavfi { params } => (params.clone(), None, None),
                SegmentSource::Still { image_path } => {
                    (image_path.display().to_string(), None, None)
                }
                SegmentSource::Render { path, .. } => (path.display().to_string(), None, None),
                // The stand-in is what airs unless the swap runs first;
                // `reconcile::swap_pending_playout` then points the row at the
                // real file.
                SegmentSource::Pending { stand_in, .. } => {
                    (stand_in.display().to_string(), None, None)
                }
            };
            PlayRecord {
                entry_id: placed.segment.name.clone(),
                start: gap_start + time::Duration::milliseconds(placed.start_ms as i64),
                played_at: written_at,
                kind: AiringKind::Bumper,
                source: Some(source),
                in_ms,
                out_ms,
                produced_by: Some(produced_by.to_string()),
                ..Default::default()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bumper::timeline::{Duration, Segment, SegmentAudio, Timeline, fit};
    use time::macros::datetime;

    fn segment(name: &str, source: SegmentSource, ms: u64) -> Segment {
        Segment {
            name: name.to_string(),
            source,
            duration: Duration::Fixed(ms),
            loop_: false,
            optional: false,
            priority: 0,
            overlay: None,
            audio: SegmentAudio::Mute,
        }
    }

    #[test]
    fn a_row_per_segment_at_its_offset_with_its_source() {
        let timeline = Timeline {
            segments: vec![
                segment(
                    "tease",
                    SegmentSource::Clip {
                        path: "/m/film.mkv".into(),
                        in_ms: 4_000,
                        out_ms: 9_000,
                    },
                    5_000,
                ),
                segment("card", SegmentSource::black(), 2_000),
            ],
            bed: None,
        };
        let resolved = fit(&timeline, 7_000, 24.0).unwrap();
        let start = datetime!(2026-10-17 20:00 UTC);
        let rows = records(&resolved, start, start, "bumper.rhai");

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].entry_id, "tease");
        assert_eq!(rows[0].start, start);
        assert_eq!(rows[0].source.as_deref(), Some("/m/film.mkv"));
        assert_eq!((rows[0].in_ms, rows[0].out_ms), (Some(4_000), Some(9_000)));
        assert_eq!(rows[0].kind, AiringKind::Bumper);
        assert_eq!(rows[0].produced_by.as_deref(), Some("bumper.rhai"));
        assert_eq!(rows[1].start, start + time::Duration::milliseconds(5_000));
        assert_eq!(rows[1].source.as_deref(), Some("color=c=black"));
        assert_eq!(rows[1].in_ms, None);
    }

    #[test]
    fn a_pending_segment_is_recorded_under_its_stand_in() {
        let timeline = Timeline {
            segments: vec![segment(
                "title",
                SegmentSource::Pending {
                    path: "/r/real.mp4".into(),
                    stand_in: "/r/stand.mp4".into(),
                },
                3_000,
            )],
            bed: None,
        };
        let resolved = fit(&timeline, 3_000, 24.0).unwrap();
        let start = datetime!(2026-10-17 20:00 UTC);
        let rows = records(&resolved, start, start, "bumper.rhai");
        assert_eq!(rows[0].source.as_deref(), Some("/r/stand.mp4"));
    }
}

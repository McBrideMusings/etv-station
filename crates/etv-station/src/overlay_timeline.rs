//! Writing a channel's `overlay.json` — the station half of the timeline whose
//! shape and reader live in [`etv_overlay::overlay_timeline`].
//!
//! Two entry points, matching the two moments the station knows something the
//! running overlay does not:
//!
//! - [`reset`] at the start of a generation, when the channel's spawn config is
//!   known and no span has been generated against it yet.
//! - [`append`] after each emission, when the blocks covering the window just
//!   written are known.
//!
//! Both write the whole file atomically, because the overlay process reads it
//! with no lock and a torn read would either blank a channel or leave it on a
//! stale config until the next generation.

use std::path::Path;

use etv_overlay::overlay_spec::OverlaySpec;
use etv_overlay::overlay_timeline::{OVERLAY_TIMELINE_FILE_NAME, OverlaySpan, OverlayTimeline};
use time::OffsetDateTime;

use crate::atomic::atomic_write_json;
use crate::config::ChannelOverlays;
use crate::errors::StationError;
use crate::rule::Rule;

/// Start this channel's timeline over from its spawn config, dropping every
/// span a previous run left behind.
///
/// The spans describe a schedule generated against a config that may since have
/// been edited, and there is no way to tell from the file which. Rather than
/// carry them and risk a block wearing last week's overlay, the timeline is
/// rebuilt: an uncovered moment falls back to `base`, so the channel looks like
/// itself for the seconds before the first emission refills it.
pub async fn reset(output_folder: &Path, base: &OverlaySpec) -> Result<(), StationError> {
    write(
        output_folder,
        &OverlayTimeline {
            base: base.clone(),
            spans: Vec::new(),
        },
    )
    .await
}

/// Record which overlay config each block of the window `[from, to)` puts on
/// screen, replacing whatever this range previously said.
///
/// `retain_from` is the earliest instant still worth keeping: spans finishing
/// before it are dropped. The caller passes the start of the oldest playout
/// chunk still on disk, so the timeline is retained on exactly the terms the
/// schedule it describes is — one rule, not a second knob that could disagree
/// with `retention_days`.
///
/// A channel whose overlay does not vary by block writes no spans at all. There
/// is nothing they could say that `base` does not already say, and 64 channels
/// each rewriting a day of redundant spans every hour is a cost with no reader.
pub async fn append(
    output_folder: &Path,
    overlays: &ChannelOverlays,
    rule: &impl Rule,
    anchor_utc: OffsetDateTime,
    from: OffsetDateTime,
    to: OffsetDateTime,
    retain_from: OffsetDateTime,
) -> Result<(), StationError> {
    let Some(base) = overlays.base.as_ref() else {
        return Ok(());
    };
    if !overlays.varies_by_block() {
        return Ok(());
    }

    let mut spans = read(output_folder)
        .await
        .map(|t| t.spans)
        .unwrap_or_default();
    // Anything this emission covers is being restated, and anything older than
    // the retained schedule has no reader left.
    spans.retain(|s| s.finish > retain_from && s.finish <= from);

    spans.extend(
        rule.block_spans_covering(anchor_utc, from, to)
            .into_iter()
            .map(|span| OverlaySpan {
                start: span.start,
                finish: span.finish,
                // A block index with no entry is not reachable from a loaded
                // config — `resolve_channel` fills one per `rule.blocks` — so
                // the flattened `None` here can only mean "draws nothing".
                spec: overlays.blocks.get(span.block).cloned().flatten(),
            }),
    );
    spans.sort_by_key(|s| s.start);

    write(
        output_folder,
        &OverlayTimeline {
            base: base.clone(),
            spans,
        },
    )
    .await
}

async fn read(output_folder: &Path) -> Option<OverlayTimeline> {
    let path = output_folder.join(OVERLAY_TIMELINE_FILE_NAME);
    let bytes = tokio::fs::read(&path).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn write(output_folder: &Path, timeline: &OverlayTimeline) -> Result<(), StationError> {
    atomic_write_json(&output_folder.join(OVERLAY_TIMELINE_FILE_NAME), timeline).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SourceConfig;
    use crate::resolve::ResolvedItem;
    use crate::rule::Sequential;
    use etv_overlay::overlay_spec::PixelFormat;
    use std::time::Duration;
    use tempfile::tempdir;
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

    fn item(block: usize, secs: u64) -> ResolvedItem {
        ResolvedItem {
            block,
            id: format!("b{block}-{secs}"),
            source: SourceConfig::Lavfi {
                params: "testsrc".into(),
            },
            in_point: Some(Duration::ZERO),
            out_point: Some(Duration::from_secs(secs)),
            program: None,
            catalog_duration: None,
            error_card: false,
            metadata: None,
            guide: None,
            guide_fields: crate::guide::GuideFields::default(),
        }
    }

    /// Two blocks with different overlays, one hour each.
    fn two_block_channel() -> (Vec<ResolvedItem>, Vec<Duration>, ChannelOverlays) {
        let items = vec![item(0, 3600), item(1, 3600)];
        let durations = vec![Duration::from_secs(3600), Duration::from_secs(3600)];
        let overlays = ChannelOverlays {
            base: Some(spec(30)),
            blocks: vec![Some(spec(30)), Some(spec(31))],
        };
        (items, durations, overlays)
    }

    async fn read_written(dir: &Path) -> OverlayTimeline {
        read(dir)
            .await
            .expect("a timeline should have been written")
    }

    #[tokio::test]
    async fn spans_name_the_config_each_block_puts_on_screen() {
        let dir = tempdir().unwrap();
        let (items, durations, overlays) = two_block_channel();
        let rule = Sequential::new(&items, &durations);
        let start = datetime!(2026-04-13 00:00 UTC);

        append(
            dir.path(),
            &overlays,
            &rule,
            start,
            start,
            start + time::Duration::hours(2),
            start,
        )
        .await
        .unwrap();

        let timeline = read_written(dir.path()).await;
        assert_eq!(timeline.spans.len(), 2);
        assert_eq!(timeline.spans[0].start, start);
        assert_eq!(timeline.spans[0].finish, datetime!(2026-04-13 01:00 UTC));
        assert_eq!(timeline.spans[0].spec, Some(spec(30)));
        assert_eq!(timeline.spans[1].spec, Some(spec(31)));
    }

    /// A block that resolves to nothing writes a span saying so, not a missing
    /// span — a gap would fall back to `base` and put the channel's watermark
    /// back on a block that asked for none.
    #[tokio::test]
    async fn a_block_that_draws_nothing_writes_a_null_span() {
        let dir = tempdir().unwrap();
        let (items, durations, mut overlays) = two_block_channel();
        overlays.blocks[1] = None;
        let rule = Sequential::new(&items, &durations);
        let start = datetime!(2026-04-13 00:00 UTC);

        append(
            dir.path(),
            &overlays,
            &rule,
            start,
            start,
            start + time::Duration::hours(2),
            start,
        )
        .await
        .unwrap();

        let timeline = read_written(dir.path()).await;
        assert_eq!(timeline.spans[1].spec, None);
        assert_eq!(
            timeline.spec_at(datetime!(2026-04-13 01:30 UTC)),
            None,
            "the second block must draw nothing, not the channel's base"
        );
    }

    /// The common channel: one overlay everywhere. The file still records the
    /// spawn config, but nothing per block — there is nothing to say.
    #[tokio::test]
    async fn a_channel_whose_overlay_never_varies_writes_no_spans() {
        let dir = tempdir().unwrap();
        let (items, durations, mut overlays) = two_block_channel();
        overlays.blocks = vec![Some(spec(30)), Some(spec(30))];
        let rule = Sequential::new(&items, &durations);
        let start = datetime!(2026-04-13 00:00 UTC);

        reset(dir.path(), overlays.base.as_ref().unwrap())
            .await
            .unwrap();
        append(
            dir.path(),
            &overlays,
            &rule,
            start,
            start,
            start + time::Duration::hours(2),
            start,
        )
        .await
        .unwrap();

        let timeline = read_written(dir.path()).await;
        assert!(timeline.spans.is_empty());
        assert_eq!(
            timeline.spec_at(datetime!(2026-04-13 01:30 UTC)),
            Some(&spec(30))
        );
    }

    /// Successive generations extend one timeline rather than replacing it, so
    /// a block still airing from the last pass keeps its config.
    #[tokio::test]
    async fn a_second_generation_keeps_the_first_generations_spans() {
        let dir = tempdir().unwrap();
        let (items, durations, overlays) = two_block_channel();
        let rule = Sequential::new(&items, &durations);
        let first = datetime!(2026-04-13 00:00 UTC);
        let second = datetime!(2026-04-13 02:00 UTC);

        append(dir.path(), &overlays, &rule, first, first, second, first)
            .await
            .unwrap();
        append(
            dir.path(),
            &overlays,
            &rule,
            second,
            second,
            second + time::Duration::hours(2),
            first,
        )
        .await
        .unwrap();

        let timeline = read_written(dir.path()).await;
        assert_eq!(timeline.spans.len(), 4);
        assert_eq!(timeline.spans[0].start, first);
        assert_eq!(timeline.spans[3].finish, datetime!(2026-04-13 04:00 UTC));
    }

    /// The timeline is retained on the schedule's terms: a span whose playout
    /// chunk has been wiped is dropped with it, so a 24/7 channel's file does
    /// not grow without bound.
    #[tokio::test]
    async fn spans_older_than_the_retained_schedule_are_dropped() {
        let dir = tempdir().unwrap();
        let (items, durations, overlays) = two_block_channel();
        let rule = Sequential::new(&items, &durations);
        let first = datetime!(2026-04-13 00:00 UTC);
        let second = datetime!(2026-04-13 02:00 UTC);

        append(dir.path(), &overlays, &rule, first, first, second, first)
            .await
            .unwrap();
        append(
            dir.path(),
            &overlays,
            &rule,
            second,
            second,
            second + time::Duration::hours(2),
            // Retention has moved past the first generation entirely.
            second,
        )
        .await
        .unwrap();

        let timeline = read_written(dir.path()).await;
        assert_eq!(timeline.spans.len(), 2);
        assert_eq!(timeline.spans[0].start, second);
    }

    #[tokio::test]
    async fn a_channel_with_no_overlay_writes_no_file() {
        let dir = tempdir().unwrap();
        let (items, durations, _) = two_block_channel();
        let rule = Sequential::new(&items, &durations);
        let start = datetime!(2026-04-13 00:00 UTC);

        append(
            dir.path(),
            &ChannelOverlays::default(),
            &rule,
            start,
            start,
            start + time::Duration::hours(2),
            start,
        )
        .await
        .unwrap();

        assert!(!dir.path().join(OVERLAY_TIMELINE_FILE_NAME).exists());
    }

    /// A generation regenerating a range it already wrote must restate it, not
    /// double it — the same property `emit_window` has for chunk files.
    #[tokio::test]
    async fn regenerating_a_range_replaces_its_spans() {
        let dir = tempdir().unwrap();
        let (items, durations, overlays) = two_block_channel();
        let rule = Sequential::new(&items, &durations);
        let start = datetime!(2026-04-13 00:00 UTC);
        let to = start + time::Duration::hours(2);

        for _ in 0..3 {
            append(dir.path(), &overlays, &rule, start, start, to, start)
                .await
                .unwrap();
        }

        assert_eq!(read_written(dir.path()).await.spans.len(), 2);
    }
}

//! What a channel puts on screen when the loop that schedules it has died.
//!
//! [`crate::error_card`] covers the case where one tape is snapped. This module
//! covers the case where the projectionist has left the building: the channel's
//! own task returned an error and the supervisor in [`crate::daemon`] could not
//! get it running again. Nothing is left to schedule real programmes, so the
//! supervisor writes cards directly into the playout folder instead.
//!
//! The channel does not go dark the moment the loop dies — whatever was already
//! written keeps airing, for up to `window_days`. These cards take over from the
//! end of that written schedule, so the failure is visible on screen before the
//! channel would otherwise fall silent, and stays visible for as long as it is
//! broken.
//!
//! Cards are written **after** everything real, never over it, and
//! [`wipe_cards_from`] takes them back out item by item — so a channel that
//! recovers replaces its cards with the real schedule rather than airing behind
//! them.

use std::path::Path;
use std::time::Duration;

use ersatztv_playout::playout::{Playout, PlayoutItem};
use time::OffsetDateTime;
use time_tz::Tz;

use crate::atomic::atomic_write_json;
use crate::config::LoadedChannel;
use crate::emit;
use crate::errors::StationError;
use crate::resolve::ResolvedItem;
use crate::rule::Sequential;
use crate::scan;

/// How long one card item runs before the next identical one starts.
///
/// Consecutive cards are indistinguishable on screen, so this is only a
/// granularity: short enough that a recovering channel resumes real programmes
/// within a few minutes, long enough that covering a multi-day window is a few
/// hundred items rather than tens of thousands.
const CARD_SEGMENT: Duration = Duration::from_secs(300);

/// Marks an item as one of ours. Channel cards live in the same chunk files as
/// real programmes, so removing them again means recognising them; the prefix is
/// what makes that possible without a schema change.
const CARD_ID_PREFIX: &str = "etv-station-channel-card";

fn is_card(item: &PlayoutItem) -> bool {
    item.id.starts_with(CARD_ID_PREFIX)
}

/// Fill the span between the end of everything already written for `channel` and
/// `now + window_days` with channel-unavailable cards.
///
/// Returns the instant the card run begins, or `None` when the written schedule
/// already reaches past the window and there is nothing to cover yet. Calling
/// this again later extends the run: the previous cards are now part of "already
/// written", so the next call picks up after them.
pub async fn cover_after_written(
    channel: &LoadedChannel,
    tz: &'static Tz,
    reason: &str,
    now: OffsetDateTime,
) -> Result<Option<OffsetDateTime>, StationError> {
    let existing = scan::scan_output_folder(&channel.output_folder).await?;
    let from = scan::highest_finish(&existing)
        .await
        .unwrap_or(now)
        .max(now);
    let target = now + crate::daemon::window_duration(channel.config.window_days);
    if from >= target {
        return Ok(None);
    }

    let segment = time::Duration::seconds_f64(CARD_SEGMENT.as_secs_f64());
    let count = ((target - from).as_seconds_f64() / segment.as_seconds_f64()).ceil() as usize;
    let items: Vec<ResolvedItem> = (0..count)
        .map(|i| {
            crate::error_card::make_channel_card(
                format!("{CARD_ID_PREFIX}-{i}"),
                &channel.name,
                reason,
                CARD_SEGMENT,
            )
        })
        .collect();
    let durations = vec![CARD_SEGMENT; count];
    let rule = Sequential::new(&items, &durations);

    // Emit to the sequence's own end rather than to `target`: the last card
    // straddles the window edge and is written whole, exactly as a real
    // programme straddling it would be.
    emit::emit_window(
        &channel.output_folder,
        &rule,
        from,
        tz,
        channel.config.chunk_hours,
        from,
        from + rule.total_duration(),
    )
    .await?;

    Ok(Some(from))
}

/// Remove every card at or after `from`, leaving real programmes in place.
/// Returns how many card items were dropped.
///
/// Item-level rather than file-level, unlike `wipe_playout_from`: the card run
/// starts wherever the real schedule happened to end, which is mid-chunk, so the
/// first chunk file of the run holds real programmes and cards side by side.
/// Deleting that file would take real, still-airing content off the channel.
pub async fn wipe_cards_from(
    channel: &LoadedChannel,
    from: OffsetDateTime,
) -> Result<usize, StationError> {
    let files = scan::scan_output_folder(&channel.output_folder).await?;
    let mut dropped = 0;
    for f in files.iter().filter(|f| f.finish > from) {
        let bytes = tokio::fs::read(&f.path)
            .await
            .map_err(|source| StationError::Io {
                path: f.path.clone(),
                source,
            })?;
        let playout: Playout =
            serde_json::from_slice(&bytes).map_err(|source| StationError::PlayoutCorrupt {
                path: f.path.clone(),
                source,
            })?;
        let before = playout.items.len();
        let kept: Vec<PlayoutItem> = playout
            .items
            .into_iter()
            .filter(|i| !(is_card(i) && i.start >= from))
            .collect();
        if kept.len() == before {
            continue;
        }
        dropped += before - kept.len();
        rewrite_chunk(&f.path, f.start, kept).await?;
    }
    Ok(dropped)
}

/// Put `kept` back on disk under the name its content now deserves, dropping the
/// old file when the name changed — the same growing-chunk naming `emit` uses,
/// run backwards.
async fn rewrite_chunk(
    path: &Path,
    chunk_start: OffsetDateTime,
    kept: Vec<PlayoutItem>,
) -> Result<(), StationError> {
    if kept.is_empty() {
        return tokio::fs::remove_file(path)
            .await
            .or_else(|source| match source.kind() {
                std::io::ErrorKind::NotFound => Ok(()),
                _ => Err(StationError::Io {
                    path: path.to_path_buf(),
                    source,
                }),
            });
    }
    let finish = kept.last().map(|i| i.finish).unwrap_or(chunk_start);
    let folder = path.parent().unwrap_or_else(|| Path::new("."));
    let new_path = folder.join(emit::chunk_filename(chunk_start, finish)?);
    atomic_write_json(&new_path, &Playout::new(kept)).await?;
    if new_path != path {
        let _ = tokio::fs::remove_file(path).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChannelConfig, LoadedChannel};
    use std::path::PathBuf;
    use time::macros::datetime;

    fn channel(dir: &Path) -> LoadedChannel {
        let config: ChannelConfig = toml::from_str(
            "window_days = 1\nchunk_hours = 6\nroll_interval = \"1h\"\n\
             [rule]\nblocks = []\n",
        )
        .expect("fixture channel config parses");
        LoadedChannel {
            name: "testch".into(),
            config_path: PathBuf::from("testch.toml"),
            output_folder: dir.to_path_buf(),
            config,
        }
    }

    async fn items_on_disk(dir: &Path) -> Vec<PlayoutItem> {
        let mut all = Vec::new();
        for f in scan::scan_output_folder(dir).await.unwrap() {
            let bytes = tokio::fs::read(&f.path).await.unwrap();
            all.extend(serde_json::from_slice::<Playout>(&bytes).unwrap().items);
        }
        all.sort_by_key(|i| i.start);
        all
    }

    /// The point of the whole module: a channel with nothing written gets a
    /// gapless run of cards covering the window it would otherwise air black.
    #[tokio::test]
    async fn cards_cover_the_whole_window_without_a_gap() {
        let dir = tempfile::tempdir().unwrap();
        let ch = channel(dir.path());
        let tz = crate::tz::parse("UTC").unwrap();
        let now = datetime!(2026-04-13 00:00 UTC);

        let from = cover_after_written(&ch, tz, "resolve failed", now)
            .await
            .unwrap()
            .expect("a channel with nothing written has a window to cover");
        assert_eq!(from, now);

        let items = items_on_disk(dir.path()).await;
        assert!(!items.is_empty());
        assert_eq!(items[0].start, now);
        // Every card butts against the next; the run reaches past now + 1 day.
        for pair in items.windows(2) {
            assert_eq!(pair[0].finish, pair[1].start, "gap between cards");
        }
        assert!(items.last().unwrap().finish >= now + time::Duration::days(1));
        assert!(items.iter().all(is_card));
    }

    /// Cards start after the real schedule, never on top of it.
    #[tokio::test]
    async fn cards_begin_where_the_written_schedule_ends() {
        let dir = tempfile::tempdir().unwrap();
        let ch = channel(dir.path());
        let tz = crate::tz::parse("UTC").unwrap();
        let now = datetime!(2026-04-13 00:00 UTC);
        let real_end = datetime!(2026-04-13 02:00 UTC);
        write_real(dir.path(), now, real_end).await;

        let from = cover_after_written(&ch, tz, "emit failed", now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(from, real_end);
        let items = items_on_disk(dir.path()).await;
        assert_eq!(items[0].id, "real", "the real programme survives");
        assert!(items[1..].iter().all(is_card));
        assert_eq!(items[1].start, real_end);
    }

    /// Recovery: the cards come back out and the real programme they were
    /// written alongside — in the same chunk file — stays on air.
    #[tokio::test]
    async fn wiping_cards_leaves_the_real_programme_alone() {
        let dir = tempfile::tempdir().unwrap();
        let ch = channel(dir.path());
        let tz = crate::tz::parse("UTC").unwrap();
        let now = datetime!(2026-04-13 00:00 UTC);
        let real_end = datetime!(2026-04-13 02:00 UTC);
        write_real(dir.path(), now, real_end).await;

        let from = cover_after_written(&ch, tz, "emit failed", now)
            .await
            .unwrap()
            .unwrap();
        let dropped = wipe_cards_from(&ch, from).await.unwrap();
        assert!(dropped > 0);

        let items = items_on_disk(dir.path()).await;
        assert_eq!(items.len(), 1, "only the real programme is left");
        assert_eq!(items[0].id, "real");
        assert_eq!(items[0].finish, real_end);
        // And the frontier is back where the real schedule ended, so the
        // restarted loop generates from there rather than after the cards.
        let files = scan::scan_output_folder(dir.path()).await.unwrap();
        assert_eq!(scan::highest_finish(&files).await, Some(real_end));
    }

    /// A second failure while already carded extends the run instead of
    /// duplicating it.
    #[tokio::test]
    async fn a_refresh_extends_the_run_rather_than_overlapping_it() {
        let dir = tempfile::tempdir().unwrap();
        let ch = channel(dir.path());
        let tz = crate::tz::parse("UTC").unwrap();
        let now = datetime!(2026-04-13 00:00 UTC);

        cover_after_written(&ch, tz, "boom", now).await.unwrap();
        let later = now + time::Duration::hours(3);
        cover_after_written(&ch, tz, "boom", later).await.unwrap();

        let items = items_on_disk(dir.path()).await;
        for pair in items.windows(2) {
            assert_eq!(
                pair[0].finish, pair[1].start,
                "overlap or gap after refresh"
            );
        }
        assert!(items.last().unwrap().finish >= later + time::Duration::days(1));
    }

    /// Nothing to do when the written schedule already runs past the window.
    #[tokio::test]
    async fn a_fully_covered_window_writes_no_cards() {
        let dir = tempfile::tempdir().unwrap();
        let ch = channel(dir.path());
        let tz = crate::tz::parse("UTC").unwrap();
        let now = datetime!(2026-04-13 00:00 UTC);
        write_real(dir.path(), now, now + time::Duration::days(2)).await;

        assert_eq!(
            cover_after_written(&ch, tz, "boom", now).await.unwrap(),
            None
        );
    }

    /// One real programme spanning `[start, end)`, written the way `emit` names
    /// a still-filling chunk.
    async fn write_real(dir: &Path, start: OffsetDateTime, end: OffsetDateTime) {
        let source = crate::config::SourceConfig::Lavfi {
            params: "color=c=blue".into(),
        };
        let item = PlayoutItem::scheduled(
            "real".into(),
            start,
            end,
            source.to_playout_source(Some(Duration::ZERO), Some(Duration::from_secs(1))),
        );
        let path = dir.join(emit::chunk_filename(start, end).unwrap());
        atomic_write_json(&path, &Playout::new(vec![item]))
            .await
            .unwrap();
    }
}

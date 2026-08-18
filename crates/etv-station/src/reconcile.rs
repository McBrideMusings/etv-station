//! The **reconciliation sweep**: patch already-written playout JSON so it keeps
//! naming files that exist.
//!
//! # The failure it exists to stop
//!
//! A playout file bakes in an absolute `path` per item at the moment it is
//! written, and the daemon writes days ahead. Rename the file on disk after
//! that — a Radarr re-import adding `[EN]`/`2.0` to a filename is the case that
//! triggered this — and every already-materialized chunk still points at the old
//! name. ETV-next reads the item, ffmpeg cannot open the path, and the channel
//! airs black and silence for the whole slot. A two-hour film becomes two hours
//! of black.
//!
//! Nothing regenerated those files, because nothing was wrong with them by the
//! daemon's own reckoning: the schedule was laid, the window was covered, and
//! the retention sweep only removes what has aired.
//!
//! # What it does instead
//!
//! An item's `id` is the catalog `entry_id`, which is stable across a rename
//! (identity comes from GUID priority, not from the path — see
//! [`crate::catalog::identity`]). So the id is enough to re-ask the catalog
//! where the file is *now*. After every catalog refresh, this walks every
//! playout file that has not fully aired and, per item:
//!
//! - the entry resolves to a different path → rewrite the path in place;
//! - the entry is **missing** (ADR 0006) → replace the item with an error card,
//!   which is the same screen a file that fails to probe already gets;
//! - the path still matches, or the id is not a catalog entry at all (an inline
//!   item, an authored `lavfi` source, a card already placed) → leave it alone.
//!
//! Only the `source` field of an item changes. `start`, `finish`, and `program`
//! are untouched, so the timeline and the guide say exactly what they said
//! before — a viewer sees the title they expected, playing from the right file.
//!
//! # Why patching is enough
//!
//! ETV-next re-reads the playout JSON from disk on every item boundary
//! (`playout_loader.rs`'s `get_current_item`), so a patched file is picked up
//! with no restart and no signal. Writes go through
//! [`crate::atomic::atomic_write_json`], so a crash mid-sweep cannot leave a
//! half-written file for the channel worker to read.

use std::path::Path;

use ersatztv_playout::playout::{Playout, PlayoutItem, PlayoutItemSource};
use time::OffsetDateTime;

use crate::atomic::atomic_write_json;
use crate::catalog::Catalog;
use crate::config::LoadedChannel;
use crate::errors::StationError;
use crate::resolve::pick_playback_source;

/// What one sweep changed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileCounts {
    /// Playout files considered — those with a `finish` still in the future.
    pub files_examined: usize,
    /// Files actually rewritten. Always ≤ `files_examined`; a sweep that finds
    /// nothing stale writes nothing.
    pub files_rewritten: usize,
    /// Items whose `path` was re-pointed at where the catalog says the file is
    /// now.
    pub paths_patched: usize,
    /// Items replaced with an error card because their entry is missing.
    pub items_carded: usize,
}

impl ReconcileCounts {
    /// Whether anything changed — what the caller logs on.
    pub fn changed_anything(self) -> bool {
        self.files_rewritten > 0
    }
}

/// Sweep every channel's output folder. `catalog_path` is the station catalog,
/// opened read-only for the duration; `now` bounds which files are still worth
/// touching.
///
/// A channel whose folder cannot be scanned, or a file that cannot be read or
/// parsed, is logged and skipped rather than failing the sweep — one unreadable
/// file must not stop the other channels from being fixed. The `Err` return is
/// reserved for not being able to open the catalog at all, which is the same
/// condition the rest of the daemon already treats as fatal.
pub async fn reconcile_playout_paths(
    channels: &[LoadedChannel],
    catalog_path: &Path,
    now: OffsetDateTime,
) -> Result<ReconcileCounts, StationError> {
    let catalog = Catalog::open_readonly(catalog_path)?;
    let mut counts = ReconcileCounts::default();

    for channel in channels {
        let files = match crate::scan::scan_output_folder(&channel.output_folder).await {
            Ok(files) => files,
            Err(e) => {
                tracing::warn!(
                    event = "reconcile.scan_failed",
                    channel = %channel.name,
                    error = %e,
                    "could not scan this channel's output folder; skipping it this sweep",
                );
                continue;
            }
        };

        for file in files {
            // Already fully aired: nothing downstream will ever read it again,
            // and the retention sweep owns removing it.
            if file.finish <= now {
                continue;
            }
            counts.files_examined += 1;
            match reconcile_one_file(&catalog, &file.path, now).await {
                Ok(Some(file_counts)) => {
                    counts.files_rewritten += 1;
                    counts.paths_patched += file_counts.paths_patched;
                    counts.items_carded += file_counts.items_carded;
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(
                    event = "reconcile.file_failed",
                    channel = %channel.name,
                    file = %file.path.display(),
                    error = %e,
                    "could not reconcile this playout file; leaving it as it is",
                ),
            }
        }
    }

    Ok(counts)
}

/// Reconcile one playout file. `Ok(None)` means nothing needed changing and
/// nothing was written.
async fn reconcile_one_file(
    catalog: &Catalog,
    path: &Path,
    now: OffsetDateTime,
) -> Result<Option<ReconcileCounts>, String> {
    let bytes = tokio::fs::read(path).await.map_err(|e| e.to_string())?;
    let mut playout: Playout = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;

    let mut counts = ReconcileCounts::default();
    for item in &mut playout.items {
        // An item that has already finished is history even inside a file that
        // has not: rewriting it changes nothing on screen and only risks a
        // needless write.
        if item.finish <= now {
            continue;
        }
        match reconcile_item(catalog, item)? {
            ItemOutcome::Unchanged => {}
            ItemOutcome::PathPatched => counts.paths_patched += 1,
            ItemOutcome::Carded => counts.items_carded += 1,
        }
    }

    if counts.paths_patched == 0 && counts.items_carded == 0 {
        return Ok(None);
    }
    atomic_write_json(path, &playout)
        .await
        .map_err(|e| e.to_string())?;
    Ok(Some(counts))
}

/// What [`reconcile_item`] did to one item.
enum ItemOutcome {
    Unchanged,
    PathPatched,
    Carded,
}

/// Re-resolve one item against the catalog, mutating its `source` if the
/// catalog now disagrees with what the file says.
fn reconcile_item(catalog: &Catalog, item: &mut PlayoutItem) -> Result<ItemOutcome, String> {
    // Only a local path can go stale under us. An authored `lavfi` (the smoke
    // channel, an already-placed error card) or an `http` source names no file
    // this catalog owns, so there is nothing here to re-resolve — and skipping
    // `lavfi` is also what stops a card placed by a previous sweep from being
    // re-carded on every sweep after it.
    let Some(PlayoutItemSource::Local { path: current, .. }) = &item.source else {
        return Ok(ItemOutcome::Unchanged);
    };
    let current = current.clone();

    // An id the catalog does not know is an inline item, whose path came from
    // the channel config and is the config author's to keep correct — not
    // something to card off the air because the catalog never heard of it.
    let Some(entry) = catalog.entry(&item.id).map_err(|e| e.to_string())? else {
        return Ok(ItemOutcome::Unchanged);
    };

    if entry.missing_since.is_some() {
        let title = item
            .program
            .as_ref()
            .and_then(|p| p.title.clone())
            .unwrap_or_else(|| item.id.clone());
        item.source = Some(PlayoutItemSource::Lavfi {
            params: crate::error_card::playback_error_params(
                &title,
                "this title is no longer in the library",
            ),
            probe_hint: None,
        });
        tracing::info!(
            event = "reconcile.item_carded",
            item = %item.id,
            title = %title,
            was = %current,
            "catalog entry is missing; replacing the scheduled item with an error card",
        );
        return Ok(ItemOutcome::Carded);
    }

    let sources = catalog.sources_for(&item.id).map_err(|e| e.to_string())?;
    let Some(source) = pick_playback_source(&sources) else {
        // The entry is live but holds no provenance row at all (#137's hollow
        // row). `resolve` already logs and skips that case when scheduling; a
        // file that was scheduled before the row went hollow has a path that
        // may still be perfectly good, so leave it.
        return Ok(ItemOutcome::Unchanged);
    };
    if source.playback_path == current {
        return Ok(ItemOutcome::Unchanged);
    }

    tracing::info!(
        event = "reconcile.path_patched",
        item = %item.id,
        was = %current,
        now = %source.playback_path,
        "catalog resolves this item to a different file than the playout says; patching in place",
    );
    // The path alone is replaced, in place. `in_point_ms` / `out_point_ms`
    // describe the *item* — where in the film this slot starts and stops — not
    // the filename it lives under, and a rename changes neither. Rebuilding the
    // variant instead would silently drop them.
    if let Some(PlayoutItemSource::Local { path, .. }) = &mut item.source {
        *path = source.playback_path.clone();
    }
    Ok(ItemOutcome::PathPatched)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use ersatztv_playout::playout::{DATE_FORMAT, ProgramMetadata};
    use tempfile::TempDir;
    use time::macros::datetime;

    use crate::catalog::{Entry, EntrySource, Source};
    use crate::config::ChannelConfig;

    /// The instant every test treats as "now". Items before it have aired.
    fn now() -> OffsetDateTime {
        datetime!(2026-08-16 12:00:00 UTC)
    }

    fn channel(dir: &TempDir) -> LoadedChannel {
        let config: ChannelConfig = toml::from_str(
            "window_days = 1\nchunk_hours = 6\nroll_interval = \"1h\"\n\
             [rule]\nblocks = []\n",
        )
        .expect("fixture channel config parses");
        LoadedChannel {
            overlays: Default::default(),
            name: "testch".into(),
            config_path: PathBuf::from("testch.toml"),
            output_folder: dir.path().to_path_buf(),
            config,
        }
    }

    /// A catalog on disk holding one entry with one `local_fs` source at
    /// `path`, so the sweep's read-only handle has something to resolve.
    fn catalog_at(dir: &TempDir, entry_id: &str, path: &str) -> PathBuf {
        let db = dir.path().join("catalog.db");
        let cat = Catalog::open(&db).unwrap();
        cat.upsert_entry(&Entry::new(entry_id, "movie", "Die Hard", Source::LocalFs))
            .unwrap();
        cat.add_source(&EntrySource {
            source: Source::LocalFs,
            source_id: entry_id.into(),
            entry_id: entry_id.into(),
            playback_path: path.into(),
            last_seen: Some("2026-08-16T00:00:00Z".into()),
            missing_since: None,
        })
        .unwrap();
        db
    }

    fn item(id: &str, path: &str, start: OffsetDateTime, finish: OffsetDateTime) -> PlayoutItem {
        PlayoutItem {
            id: id.into(),
            start,
            finish,
            source: Some(PlayoutItemSource::Local {
                path: path.into(),
                in_point_ms: Some(0),
                out_point_ms: Some(7_200_000),
                probe_hint: None,
            }),
            tracks: None,
            watermark: None,
            program: Some(ProgramMetadata {
                title: Some("Die Hard".into()),
                ..Default::default()
            }),
            metadata: None,
        }
    }

    /// Write `items` as a playout file named for the span it covers, the way
    /// [`crate::emit`] does, so [`crate::scan::scan_output_folder`] finds it.
    async fn write_playout(dir: &TempDir, items: Vec<PlayoutItem>) -> PathBuf {
        let start = items.iter().map(|i| i.start).min().unwrap();
        let finish = items.iter().map(|i| i.finish).max().unwrap();
        let name = format!(
            "{}_{}.json",
            start.format(&DATE_FORMAT).unwrap(),
            finish.format(&DATE_FORMAT).unwrap(),
        );
        let path = dir.path().join(name);
        atomic_write_json(&path, &Playout::new(items))
            .await
            .unwrap();
        path
    }

    async fn items_at(path: &Path) -> Vec<PlayoutItem> {
        let bytes = tokio::fs::read(path).await.unwrap();
        serde_json::from_slice::<Playout>(&bytes).unwrap().items
    }

    fn source_path(item: &PlayoutItem) -> Option<&str> {
        match &item.source {
            Some(PlayoutItemSource::Local { path, .. }) => Some(path),
            _ => None,
        }
    }

    fn is_card(item: &PlayoutItem) -> bool {
        matches!(item.source, Some(PlayoutItemSource::Lavfi { .. }))
    }

    /// The exact production failure: playout written before a Radarr rename
    /// still names the old file. The entry is fine, the catalog knows the new
    /// name, and nothing regenerates the chunk — so the sweep re-points it.
    #[tokio::test]
    async fn a_renamed_file_is_patched_in_place() {
        let dir = TempDir::new().unwrap();
        let db = catalog_at(&dir, "imdb:tt1", "/media/Die Hard (1988) [EN] 2.0.mkv");
        let file = write_playout(
            &dir,
            vec![item(
                "imdb:tt1",
                "/media/Die Hard (1988).mkv",
                datetime!(2026-08-16 13:00:00 UTC),
                datetime!(2026-08-16 15:00:00 UTC),
            )],
        )
        .await;

        let counts = reconcile_playout_paths(&[channel(&dir)], &db, now())
            .await
            .unwrap();

        assert_eq!(counts.files_examined, 1);
        assert_eq!(counts.files_rewritten, 1);
        assert_eq!(counts.paths_patched, 1);
        assert_eq!(counts.items_carded, 0);
        let items = items_at(&file).await;
        assert_eq!(
            source_path(&items[0]),
            Some("/media/Die Hard (1988) [EN] 2.0.mkv"),
        );
        // The slot and the guide are untouched — a viewer sees the title they
        // expected, at the time they expected it, playing from the right file.
        assert_eq!(items[0].start, datetime!(2026-08-16 13:00:00 UTC));
        assert_eq!(items[0].finish, datetime!(2026-08-16 15:00:00 UTC));
        assert_eq!(
            items[0].program.as_ref().unwrap().title.as_deref(),
            Some("Die Hard"),
        );
        // Seek points describe the item, not the filename, and must survive.
        assert!(matches!(
            items[0].source,
            Some(PlayoutItemSource::Local {
                in_point_ms: Some(0),
                out_point_ms: Some(7_200_000),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_missing_entry_becomes_an_error_card() {
        let dir = TempDir::new().unwrap();
        let db = catalog_at(&dir, "imdb:tt1", "/media/Die Hard (1988).mkv");
        {
            let cat = Catalog::open(&db).unwrap();
            cat.mark_unseen_sources_missing(Source::LocalFs, "2027-01-01T00:00:00Z")
                .unwrap();
            cat.mark_entries_missing_without_live_sources("2027-01-01T00:00:00Z")
                .unwrap();
        }
        let file = write_playout(
            &dir,
            vec![item(
                "imdb:tt1",
                "/media/Die Hard (1988).mkv",
                datetime!(2026-08-16 13:00:00 UTC),
                datetime!(2026-08-16 15:00:00 UTC),
            )],
        )
        .await;

        let counts = reconcile_playout_paths(&[channel(&dir)], &db, now())
            .await
            .unwrap();

        assert_eq!(counts.items_carded, 1);
        assert_eq!(counts.paths_patched, 0);
        let items = items_at(&file).await;
        assert!(
            is_card(&items[0]),
            "a missing title must not stay scheduled"
        );
        assert_eq!(items[0].start, datetime!(2026-08-16 13:00:00 UTC));
        assert_eq!(items[0].finish, datetime!(2026-08-16 15:00:00 UTC));
        assert_eq!(
            items[0].program.as_ref().unwrap().title.as_deref(),
            Some("Die Hard"),
            "the guide still lists what was meant to air",
        );

        // Second sweep: the card is a `lavfi` source now, so there is nothing
        // left to re-resolve and nothing is rewritten.
        let counts = reconcile_playout_paths(&[channel(&dir)], &db, now())
            .await
            .unwrap();
        assert_eq!(counts.files_rewritten, 0);
    }

    #[tokio::test]
    async fn a_file_that_has_fully_aired_is_left_alone() {
        let dir = TempDir::new().unwrap();
        let db = catalog_at(&dir, "imdb:tt1", "/media/new.mkv");
        let file = write_playout(
            &dir,
            vec![item(
                "imdb:tt1",
                "/media/old.mkv",
                datetime!(2026-08-16 08:00:00 UTC),
                datetime!(2026-08-16 10:00:00 UTC),
            )],
        )
        .await;

        let counts = reconcile_playout_paths(&[channel(&dir)], &db, now())
            .await
            .unwrap();

        assert_eq!(counts.files_examined, 0);
        assert_eq!(counts.files_rewritten, 0);
        assert_eq!(
            source_path(&items_at(&file).await[0]),
            Some("/media/old.mkv")
        );
    }

    /// An item that already finished inside a file that has not: rewriting it
    /// changes nothing on screen, so it is skipped even while its neighbour in
    /// the same file gets patched.
    #[tokio::test]
    async fn an_already_aired_item_in_a_live_file_is_left_alone() {
        let dir = TempDir::new().unwrap();
        let db = catalog_at(&dir, "imdb:tt1", "/media/new.mkv");
        let file = write_playout(
            &dir,
            vec![
                item(
                    "imdb:tt1",
                    "/media/old.mkv",
                    datetime!(2026-08-16 08:00:00 UTC),
                    datetime!(2026-08-16 10:00:00 UTC),
                ),
                item(
                    "imdb:tt1",
                    "/media/old.mkv",
                    datetime!(2026-08-16 13:00:00 UTC),
                    datetime!(2026-08-16 15:00:00 UTC),
                ),
            ],
        )
        .await;

        let counts = reconcile_playout_paths(&[channel(&dir)], &db, now())
            .await
            .unwrap();

        assert_eq!(counts.paths_patched, 1);
        let items = items_at(&file).await;
        assert_eq!(source_path(&items[0]), Some("/media/old.mkv"));
        assert_eq!(source_path(&items[1]), Some("/media/new.mkv"));
    }

    /// An id the catalog never heard of is an inline item, whose path the
    /// config author owns. Carding it off the air because the catalog is silent
    /// would take down every `manual` channel on the first sweep.
    #[tokio::test]
    async fn an_item_the_catalog_does_not_know_is_untouched() {
        let dir = TempDir::new().unwrap();
        let db = catalog_at(&dir, "imdb:tt1", "/media/new.mkv");
        let file = write_playout(
            &dir,
            vec![item(
                "inline:bumper",
                "/media/bumpers/ident.mp4",
                datetime!(2026-08-16 13:00:00 UTC),
                datetime!(2026-08-16 13:00:30 UTC),
            )],
        )
        .await;

        let counts = reconcile_playout_paths(&[channel(&dir)], &db, now())
            .await
            .unwrap();

        assert_eq!(counts.files_examined, 1);
        assert_eq!(counts.files_rewritten, 0);
        assert_eq!(
            source_path(&items_at(&file).await[0]),
            Some("/media/bumpers/ident.mp4"),
        );
    }

    /// A rename under a `local_fs` root leaves two rows on one entry — the new
    /// path live, the old one marked — because the canonical path is the row's
    /// key. The sweep has to pick the live one; picking by source alone would
    /// re-point the item at the file that is gone.
    #[tokio::test]
    async fn a_stale_source_row_never_wins_over_a_live_one() {
        let dir = TempDir::new().unwrap();
        let db = catalog_at(&dir, "imdb:tt1", "/media/a-old.mkv");
        {
            let cat = Catalog::open(&db).unwrap();
            // The old row goes missing; a new row for the renamed file arrives.
            // `a-old` sorts before `b-new`, so a source-only pick takes the
            // wrong one.
            cat.mark_unseen_sources_missing(Source::LocalFs, "2027-01-01T00:00:00Z")
                .unwrap();
            cat.add_source(&EntrySource {
                source: Source::LocalFs,
                source_id: "b-new".into(),
                entry_id: "imdb:tt1".into(),
                playback_path: "/media/b-new.mkv".into(),
                last_seen: Some("2027-01-01T00:00:00Z".into()),
                missing_since: None,
            })
            .unwrap();
        }
        let file = write_playout(
            &dir,
            vec![item(
                "imdb:tt1",
                "/media/a-old.mkv",
                datetime!(2026-08-16 13:00:00 UTC),
                datetime!(2026-08-16 15:00:00 UTC),
            )],
        )
        .await;

        reconcile_playout_paths(&[channel(&dir)], &db, now())
            .await
            .unwrap();

        assert_eq!(
            source_path(&items_at(&file).await[0]),
            Some("/media/b-new.mkv"),
        );
    }

    /// A sweep that finds nothing stale writes nothing at all — the common case,
    /// running every `catalog_refresh_secs` over every channel's whole forward
    /// window.
    #[tokio::test]
    async fn a_clean_sweep_rewrites_no_files() {
        let dir = TempDir::new().unwrap();
        let db = catalog_at(&dir, "imdb:tt1", "/media/Die Hard (1988).mkv");
        let file = write_playout(
            &dir,
            vec![item(
                "imdb:tt1",
                "/media/Die Hard (1988).mkv",
                datetime!(2026-08-16 13:00:00 UTC),
                datetime!(2026-08-16 15:00:00 UTC),
            )],
        )
        .await;
        let before = tokio::fs::metadata(&file)
            .await
            .unwrap()
            .modified()
            .unwrap();

        let counts = reconcile_playout_paths(&[channel(&dir)], &db, now())
            .await
            .unwrap();

        assert_eq!(counts.files_examined, 1);
        assert_eq!(counts.files_rewritten, 0);
        assert!(!counts.changed_anything());
        assert_eq!(
            tokio::fs::metadata(&file)
                .await
                .unwrap()
                .modified()
                .unwrap(),
            before,
        );
    }
}

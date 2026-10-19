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
//! - the path still matches the catalog's, but is gone from disk → logged and
//!   counted, **not repaired**. Plex does not bump an item's `updatedAt` when
//!   the file behind it is renamed, so a delta ingest never learns the catalog
//!   itself is stale — the catalog and the playout can agree on a dead path.
//!   Fixing that needs a targeted single-item Plex re-resolve, which does not
//!   exist yet; that is the deliberately deferred repair half of #300;
//! - the path still matches and is present on disk, or the id is not a catalog
//!   entry at all (an inline item, an authored `lavfi` source, a card already
//!   placed) → leave it alone.
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
use std::sync::Arc;

use ersatztv_playout::playout::{Playout, PlayoutItem, PlayoutItemSource};
use time::OffsetDateTime;

use crate::atomic::atomic_write_json;
use crate::bumper::emit::{Media, PENDING_KEY, push_local_span};
use crate::catalog::Catalog;
use crate::config::LoadedChannel;
use crate::errors::StationError;
use crate::history::HistoryDb;
use crate::resolve::pick_playback_source;

/// Every item in `channel`'s output folder that has not started, as
/// `(catalog entry id, start)`.
///
/// Split from [`first_unplayable`] because `Catalog` is not `Sync`: a reference
/// to it cannot be held across the awaits here and still be spawned.
pub async fn unaired_items(
    channel: &LoadedChannel,
    now: OffsetDateTime,
) -> Result<Vec<(String, OffsetDateTime)>, StationError> {
    let mut out = Vec::new();
    for file in crate::scan::scan_output_folder(&channel.output_folder).await? {
        if file.finish <= now {
            continue;
        }
        let bytes = tokio::fs::read(&file.path)
            .await
            .map_err(|source| StationError::Io {
                path: file.path.clone(),
                source,
            })?;
        let playout: Playout =
            serde_json::from_slice(&bytes).map_err(|source| StationError::PlayoutCorrupt {
                path: file.path.clone(),
                source,
            })?;
        out.extend(
            playout
                .items
                .into_iter()
                .filter(|i| i.start > now)
                .map(|i| (i.id, i.start)),
        );
    }
    Ok(out)
}

/// The `start` of the earliest of `unaired` whose catalog entry has gone
/// missing, or `None` when every one can still play.
///
/// Reads the same `missing_since` the sweep reads, and tests the **entry**, not
/// whether the item is already an error card: a slot carded by an earlier sweep
/// and one not yet swept have the same missing entry, and both need a real title
/// back. An id the catalog does not know is an inline item and is not this
/// function's business (see [`reconcile_item`]).
pub fn first_unplayable(
    catalog: &Catalog,
    unaired: &[(String, OffsetDateTime)],
) -> Result<Option<OffsetDateTime>, StationError> {
    let mut earliest: Option<OffsetDateTime> = None;
    for (id, start) in unaired {
        if earliest.is_some_and(|e| e <= *start) {
            continue;
        }
        if catalog
            .entry(id)?
            .is_some_and(|e| e.missing_since.is_some())
        {
            earliest = Some(*start);
        }
    }
    Ok(earliest)
}

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
    /// Items whose file is gone from disk and which the sweep cannot repair —
    /// the catalog and the playout agree on a path, and that path does not
    /// exist. This is the count that used to be invisible: a delta ingest
    /// never notices a rename, so both sides can be stale together and the
    /// old comparison-only sweep reported `Unchanged`.
    pub items_path_missing: usize,
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
                Ok(file_counts) => {
                    counts.files_rewritten += file_counts.files_rewritten;
                    counts.paths_patched += file_counts.paths_patched;
                    counts.items_carded += file_counts.items_carded;
                    counts.items_path_missing += file_counts.items_path_missing;
                }
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

/// Reconcile one playout file. Always returns counts, even when nothing was
/// written — a missing-path finding mutates no item, so it must be countable
/// without a rewrite happening.
async fn reconcile_one_file(
    catalog: &Catalog,
    path: &Path,
    now: OffsetDateTime,
) -> Result<ReconcileCounts, String> {
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
        match reconcile_item(catalog, item).await? {
            ItemOutcome::Unchanged => {}
            ItemOutcome::PathPatched => counts.paths_patched += 1,
            ItemOutcome::Carded => counts.items_carded += 1,
            ItemOutcome::PathMissing => counts.items_path_missing += 1,
        }
    }

    if counts.paths_patched == 0 && counts.items_carded == 0 {
        return Ok(counts);
    }
    atomic_write_json(path, &playout)
        .await
        .map_err(|e| e.to_string())?;
    counts.files_rewritten = 1;
    Ok(counts)
}

/// What [`reconcile_item`] did to one item.
enum ItemOutcome {
    Unchanged,
    PathPatched,
    Carded,
    /// The catalog and the playout agree on a path, and that path is not on
    /// disk. Nothing is repaired here — see the module doc's note on #300's
    /// deferred repair half.
    PathMissing,
}

/// The title to log an item under: its program title, falling back to the
/// catalog id when the playout carries none.
fn item_title(item: &PlayoutItem) -> String {
    item.program
        .as_ref()
        .and_then(|p| p.title.clone())
        .unwrap_or_else(|| item.id.clone())
}

/// Re-resolve one item against the catalog, mutating its `source` if the
/// catalog now disagrees with what the file says.
async fn reconcile_item(catalog: &Catalog, item: &mut PlayoutItem) -> Result<ItemOutcome, String> {
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
        let title = item_title(item);
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
        // The catalog and the playout agree — but Plex does not bump an
        // item's `updatedAt` when the file behind it is renamed, so a delta
        // ingest never learns the catalog's own path went stale. Both sides
        // can agree on a dead path. Treat an errored stat (EACCES, a briefly
        // unmounted media root) as present: only a definitive "no" counts,
        // so a whole root going offline does not produce a false-positive
        // storm.
        if !tokio::fs::try_exists(&current).await.unwrap_or(true) {
            let title = item_title(item);
            tracing::warn!(
                event = "reconcile.path_missing",
                item = %item.id,
                title = %title,
                path = %current,
                "the catalog and the playout agree on a path that is not on disk; \
                 this item will air black until the next full ingest re-resolves it",
            );
            return Ok(ItemOutcome::PathMissing);
        }
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

/// A pending segment whose real file lands less than this long before it airs
/// keeps its stand-in: ETV-next may already be preparing the item.
const PENDING_SWAP_LEAD: time::Duration = time::Duration::seconds(30);

/// One pending segment [`swap_pending_items`] replaced with its real file: the
/// ledger row it was recorded under needs to name that file, not the stand-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Swapped {
    /// The segment's name, which is the ledger row's `entry_id`.
    pub segment: String,
    /// When the segment starts.
    pub start: OffsetDateTime,
    /// The real file now airing in the stand-in's place.
    pub path: String,
}

/// What the previous tick saw of each pending file: its size and mtime. A file
/// whose stamp differs this tick is still being written, so it is not swapped.
pub type PendingSeen = std::collections::HashMap<std::path::PathBuf, (u64, std::time::SystemTime)>;

/// Record `real`'s stamp in `next`; true when `prev`, the map the previous
/// tick built, holds the same one. A file that cannot be stat'd is never
/// stable.
fn stable_since_last_tick(prev: &PendingSeen, next: &mut PendingSeen, real: &Path) -> bool {
    let Some(stamp) = std::fs::metadata(real)
        .ok()
        .and_then(|m| Some((m.len(), m.modified().ok()?)))
    else {
        return false;
    };
    next.insert(real.to_path_buf(), stamp);
    prev.get(real) == Some(&stamp)
}

/// A pending item's `(real file, loop)`, from its `metadata.pending`.
fn pending_of(item: &PlayoutItem) -> Option<(String, bool)> {
    let pending = item.metadata.as_ref()?.get(PENDING_KEY)?;
    Some((
        pending.get("path")?.as_str()?.to_string(),
        pending
            .get("loop")
            .and_then(|l| l.as_bool())
            .unwrap_or(false),
    ))
}

/// A copy of `item`; the vendored playout types are not `Clone`. A round trip
/// through JSON cannot fail for an item that was itself read from JSON.
fn copy_item(item: &PlayoutItem) -> Option<PlayoutItem> {
    serde_json::from_value(serde_json::to_value(item).ok()?).ok()
}

/// Swap every pending segment in `items` whose real file is readable and that
/// starts more than [`PENDING_SWAP_LEAD`] after `now`. One segment is a run of
/// consecutive items naming the same pending path (a stand-in shorter than its
/// slot is followed by black); the run is replaced by the real file's items, so
/// a file shorter than the slot is looped when the segment loops and padded
/// with black otherwise. `record_swap` is asked before each swap to point the
/// segment's ledger row at the real file; a `false` leaves the stand-in in
/// place for the next tick, so the file and the ledger never disagree.
/// A file whose size or mtime differs from what `prev` recorded on the
/// previous tick is still being written and keeps its stand-in until it holds
/// still for one full tick; this tick's stamps go into `next`.
/// Returns the segments swapped.
pub fn swap_pending_items(
    items: &mut Vec<PlayoutItem>,
    channel: &str,
    now: OffsetDateTime,
    media_len_ms: fn(&Path) -> Option<u64>,
    prev: &PendingSeen,
    next: &mut PendingSeen,
    record_swap: &mut dyn FnMut(&Swapped) -> bool,
) -> Vec<Swapped> {
    let mut old: Vec<Option<PlayoutItem>> = std::mem::take(items).into_iter().map(Some).collect();
    let mut swapped = Vec::new();
    let mut i = 0;
    while i < old.len() {
        let Some((path, loop_)) = old[i].as_ref().and_then(pending_of) else {
            items.extend(old[i].take());
            i += 1;
            continue;
        };
        let base_of = |item: &PlayoutItem| item.id.split('#').next().map(str::to_string);
        let group_base = old[i].as_ref().and_then(base_of);
        let mut end = i + 1;
        while end < old.len()
            && old[end].as_ref().is_some_and(|it| {
                base_of(it) == group_base && pending_of(it).is_some_and(|(p, _)| p == path)
            })
        {
            end += 1;
        }
        let group: Vec<PlayoutItem> = old[i..end].iter_mut().filter_map(Option::take).collect();
        i = end;
        let (start, finish) = (group[0].start, group[group.len() - 1].finish);
        let real = Path::new(&path);
        let len = if real.is_file()
            && stable_since_last_tick(prev, next, real)
            && start > now + PENDING_SWAP_LEAD
        {
            media_len_ms(real).filter(|l| *l > 0)
        } else {
            None
        };
        let Some(len) = len else {
            items.extend(group);
            continue;
        };
        let Some(segment) = group[0]
            .metadata
            .as_ref()
            .and_then(|m| m.get("segment"))
            .and_then(|s| s.as_str())
            .map(str::to_string)
        else {
            items.extend(group);
            continue;
        };
        let record = Swapped {
            segment,
            start,
            path: path.clone(),
        };
        let slot_ms = (finish - start).whole_milliseconds().max(0) as u64;
        let base = group[0].id.split('#').next().unwrap_or(&group[0].id);
        let mut fresh = Vec::new();
        push_local_span(
            &mut fresh,
            &Media {
                channel,
                len_ms: &media_len_ms,
            },
            base,
            real,
            0,
            len,
            loop_,
            start,
            slot_ms,
        );
        let Some(mut template) = copy_item(&group[0]) else {
            items.extend(group);
            continue;
        };
        if let Some(meta) = template.metadata.as_mut().and_then(|m| m.as_object_mut()) {
            meta.remove(PENDING_KEY);
        }
        let copies: Option<Vec<PlayoutItem>> = fresh
            .into_iter()
            .map(|new| {
                let mut item = copy_item(&template)?;
                item.id = new.id;
                item.start = new.start;
                item.finish = new.finish;
                item.source = new.source;
                Some(item)
            })
            .collect();
        let Some(copies) = copies else {
            items.extend(group);
            continue;
        };
        if !record_swap(&record) {
            items.extend(group);
            continue;
        }
        tracing::info!(
            event = "bumper.pending_swapped",
            channel,
            item = base,
            path = %path,
            slot_ms,
            file_ms = len,
            looped = loop_ && len < slot_ms,
            padded = !loop_ && len < slot_ms,
            items = copies.len(),
            "a pending bumper's real file landed before airtime; swapping it in for the stand-in",
        );
        items.extend(copies);
        swapped.push(record);
    }
    swapped
}

/// Swap landed renders into every channel's unaired playout, atomically
/// (temp file + rename). Aired and airing items are never touched. Returns
/// the number of segments swapped. A swap first points the segment's ledger
/// row at the real file, so recency and no-repeat count what aired; a segment
/// whose row is not in the ledger yet (its generation is still writing it)
/// keeps its stand-in until a later tick.
///
/// A file or channel that cannot be read is logged and skipped, as in
/// [`reconcile_playout_paths`]. `seen` carries each pending file's size and
/// mtime from one tick to the next; the caller keeps it across ticks. Each call
/// replaces it with the stamps this tick saw, so a path no longer pending drops
/// out, and a stamp taken earlier in this tick never counts as a previous one.
pub async fn swap_pending_playout(
    channels: &[LoadedChannel],
    history_db: &Arc<HistoryDb>,
    now: OffsetDateTime,
    media_len_ms: fn(&Path) -> Option<u64>,
    seen: &mut PendingSeen,
) -> usize {
    let prev = std::mem::take(seen);
    let mut total = 0;
    for channel in channels {
        let files = match crate::scan::scan_output_folder(&channel.output_folder).await {
            Ok(files) => files,
            Err(e) => {
                tracing::warn!(
                    event = "bumper.pending_scan_failed",
                    channel = %channel.name,
                    error = %e,
                    "could not scan this channel's output folder for pending bumpers",
                );
                continue;
            }
        };
        for file in files.into_iter().filter(|f| f.finish > now) {
            match swap_pending_file(
                &file.path,
                &channel.name,
                history_db,
                now,
                media_len_ms,
                &prev,
                seen,
            )
            .await
            {
                Ok(n) => total += n,
                Err(e) => tracing::warn!(
                    event = "bumper.pending_file_failed",
                    channel = %channel.name,
                    file = %file.path.display(),
                    error = %e,
                    "could not swap pending bumpers in this playout file; leaving it as it is",
                ),
            }
        }
    }
    total
}

async fn swap_pending_file(
    path: &Path,
    channel: &str,
    history_db: &Arc<HistoryDb>,
    now: OffsetDateTime,
    media_len_ms: fn(&Path) -> Option<u64>,
    prev: &PendingSeen,
    next: &mut PendingSeen,
) -> Result<usize, String> {
    let bytes = tokio::fs::read(path).await.map_err(|e| e.to_string())?;
    // Almost no file holds a pending item; skip parsing the ones that cannot.
    if !bytes
        .windows(PENDING_NEEDLE.len())
        .any(|w| w == PENDING_NEEDLE)
    {
        return Ok(0);
    }
    let mut playout: Playout = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let channel = channel.to_string();
    let history_db = Arc::clone(history_db);
    let prev_owned = prev.clone();
    let (playout, swapped, stamps) = tokio::task::spawn_blocking(move || {
        let mut record_swap = |s: &Swapped| {
            match history_db.set_bumper_source(&channel, &s.segment, s.start, &s.path) {
                Ok(rows) if rows > 0 => true,
                Ok(_) => {
                    tracing::debug!(
                        event = "bumper.pending_not_in_ledger",
                        channel = %channel,
                        segment = %s.segment,
                        "no ledger row for this pending bumper yet; keeping its stand-in this tick",
                    );
                    false
                }
                Err(e) => {
                    tracing::warn!(
                        event = "bumper.pending_ledger_failed",
                        channel = %channel,
                        segment = %s.segment,
                        error = %e,
                        "could not record the swapped-in file in the airings ledger; keeping the stand-in",
                    );
                    false
                }
            }
        };
        let mut stamps = PendingSeen::new();
        let swapped = swap_pending_items(
            &mut playout.items,
            &channel,
            now,
            media_len_ms,
            &prev_owned,
            &mut stamps,
            &mut record_swap,
        );
        (playout, swapped, stamps)
    })
    .await
    .map_err(|e| e.to_string())?;
    next.extend(stamps);
    if !swapped.is_empty() {
        atomic_write_json(path, &playout)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(swapped.len())
}

const PENDING_NEEDLE: &[u8] = b"\"pending\"";

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
            "number = 1\nname = \"x\"\nwindow_days = 1\nchunk_hours = 6\nroll_interval = \"1h\"\n\
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
            overlay: None,
            normalize_loudness: None,
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

    /// A deleted title two items ahead is found at its own start, not the
    /// first unaired item's; with nothing missing the answer is `None`.
    #[tokio::test]
    async fn the_first_unplayable_unaired_item_is_found_at_its_own_start() {
        let dir = TempDir::new().unwrap();
        let db = catalog_at(&dir, "imdb:tt1", "/media/a.mkv");
        let cat = Catalog::open(&db).unwrap();
        cat.upsert_entry(&Entry::new("imdb:tt2", "movie", "Gone", Source::LocalFs))
            .unwrap();
        write_playout(
            &dir,
            vec![
                item(
                    "imdb:tt0",
                    "/media/aired.mkv",
                    datetime!(2026-08-16 10:00:00 UTC),
                    datetime!(2026-08-16 12:00:00 UTC),
                ),
                item(
                    "imdb:tt1",
                    "/media/a.mkv",
                    datetime!(2026-08-16 13:00:00 UTC),
                    datetime!(2026-08-16 15:00:00 UTC),
                ),
                item(
                    "imdb:tt2",
                    "/media/b.mkv",
                    datetime!(2026-08-16 15:00:00 UTC),
                    datetime!(2026-08-16 17:00:00 UTC),
                ),
                item(
                    "inline-x",
                    "/media/c.mkv",
                    datetime!(2026-08-16 17:00:00 UTC),
                    datetime!(2026-08-16 19:00:00 UTC),
                ),
            ],
        )
        .await;
        let ch = channel(&dir);

        assert_eq!(
            first_unplayable(&cat, &unaired_items(&ch, now()).await.unwrap()).unwrap(),
            None,
            "nothing missing, so nothing to repair",
        );

        cat.mark_entries_missing_without_live_sources("2027-01-01T00:00:00Z")
            .unwrap();
        assert_eq!(
            first_unplayable(&cat, &unaired_items(&ch, now()).await.unwrap()).unwrap(),
            Some(datetime!(2026-08-16 15:00:00 UTC)),
            "tt1 still has a live source, tt2 has none; the inline id is ignored",
        );
    }

    /// A sweep that finds nothing stale writes nothing at all — the common case,
    /// running every `catalog_refresh_secs` over every channel's whole forward
    /// window.
    #[tokio::test]
    async fn a_clean_sweep_rewrites_no_files() {
        let dir = TempDir::new().unwrap();
        let media_path = dir.path().join("Die Hard (1988).mkv");
        tokio::fs::write(&media_path, b"fixture").await.unwrap();
        let db = catalog_at(&dir, "imdb:tt1", media_path.to_str().unwrap());
        let file = write_playout(
            &dir,
            vec![item(
                "imdb:tt1",
                media_path.to_str().unwrap(),
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
        assert_eq!(
            counts.items_path_missing, 0,
            "the file exists on disk, so this must not be flagged missing",
        );
        assert_eq!(
            tokio::fs::metadata(&file)
                .await
                .unwrap()
                .modified()
                .unwrap(),
            before,
        );
    }

    /// The exact incident shape from #300: a Radarr rename changes the file on
    /// disk but never touches the catalog row (Plex does not bump `updatedAt`
    /// on a rename), so the catalog and the playout still agree with each
    /// other — on a path that no longer exists. The old comparison-only sweep
    /// reported this `Unchanged`; detection must not.
    #[tokio::test]
    async fn a_path_that_is_gone_from_disk_is_not_unchanged() {
        let dir = TempDir::new().unwrap();
        let dead_path = dir.path().join("Die Hard (1988).mkv");
        // Deliberately never created.
        let db = catalog_at(&dir, "imdb:tt1", dead_path.to_str().unwrap());
        let file = write_playout(
            &dir,
            vec![item(
                "imdb:tt1",
                dead_path.to_str().unwrap(),
                datetime!(2026-08-16 13:00:00 UTC),
                datetime!(2026-08-16 15:00:00 UTC),
            )],
        )
        .await;

        let counts = reconcile_playout_paths(&[channel(&dir)], &db, now())
            .await
            .unwrap();

        assert_eq!(counts.items_path_missing, 1);
        assert_eq!(counts.paths_patched, 0);
        assert_eq!(counts.items_carded, 0);
        assert_eq!(
            counts.files_rewritten, 0,
            "detection only — the item is not mutated, so nothing is written",
        );
        // The item's source is untouched: same `Local` path, still there.
        assert_eq!(
            source_path(&items_at(&file).await[0]),
            Some(dead_path.to_str().unwrap()),
        );
    }

    /// Sibling of the above with the file present: the same path, on disk,
    /// must still report `Unchanged` and write nothing.
    #[tokio::test]
    async fn a_path_that_exists_is_still_unchanged() {
        let dir = TempDir::new().unwrap();
        let live_path = dir.path().join("Die Hard (1988).mkv");
        tokio::fs::write(&live_path, b"fixture").await.unwrap();
        let db = catalog_at(&dir, "imdb:tt1", live_path.to_str().unwrap());
        write_playout(
            &dir,
            vec![item(
                "imdb:tt1",
                live_path.to_str().unwrap(),
                datetime!(2026-08-16 13:00:00 UTC),
                datetime!(2026-08-16 15:00:00 UTC),
            )],
        )
        .await;

        let counts = reconcile_playout_paths(&[channel(&dir)], &db, now())
            .await
            .unwrap();

        assert_eq!(counts.items_path_missing, 0);
        assert_eq!(counts.files_rewritten, 0);
    }

    fn pending_item(
        id: &str,
        path: &str,
        loop_: bool,
        start: OffsetDateTime,
        secs: i64,
    ) -> PlayoutItem {
        let mut it = item(
            id,
            "/stand/in.mp4",
            start,
            start + time::Duration::seconds(secs),
        );
        it.metadata = Some(serde_json::json!({
            "bumper": true,
            "segment": id,
            PENDING_KEY: { "path": path, "loop": loop_ },
        }));
        it
    }

    fn len_4s(_: &Path) -> Option<u64> {
        Some(4_000)
    }

    fn len_20s(_: &Path) -> Option<u64> {
        Some(20_000)
    }

    fn spans(items: &[PlayoutItem]) -> Vec<(String, i64, Option<u64>)> {
        items
            .iter()
            .map(|i| {
                let out = match &i.source {
                    Some(PlayoutItemSource::Local { out_point_ms, .. }) => *out_point_ms,
                    _ => None,
                };
                (
                    source_path(i).unwrap_or("lavfi").to_string(),
                    (i.finish - i.start).whole_seconds(),
                    out,
                )
            })
            .collect()
    }

    /// `swap_pending_items` as the second tick of a stable file: the first call
    /// only records what it saw, the second is the one whose result counts.
    fn swap_items(
        items: &mut Vec<PlayoutItem>,
        channel: &str,
        now: OffsetDateTime,
        len: fn(&Path) -> Option<u64>,
        record: &mut dyn FnMut(&Swapped) -> bool,
    ) -> Vec<Swapped> {
        let mut first = PendingSeen::new();
        swap_pending_items(
            items,
            channel,
            now,
            len,
            &PendingSeen::new(),
            &mut first,
            record,
        );
        swap_pending_items(
            items,
            channel,
            now,
            len,
            &first,
            &mut PendingSeen::new(),
            record,
        )
    }

    /// One daemon tick over `items`: the stamps `seen` holds are the previous
    /// tick's, and it is replaced with this tick's.
    fn item_tick(
        items: &mut Vec<PlayoutItem>,
        now: OffsetDateTime,
        seen: &mut PendingSeen,
    ) -> usize {
        let prev = std::mem::take(seen);
        swap_pending_items(items, "c", now, len_20s, &prev, seen, &mut |_| true).len()
    }

    /// One daemon tick over a single playout file.
    async fn file_tick(
        file: &Path,
        db: &Arc<HistoryDb>,
        now: OffsetDateTime,
        seen: &mut PendingSeen,
    ) -> usize {
        let prev = std::mem::take(seen);
        swap_pending_file(file, "c", db, now, len_20s, &prev, seen)
            .await
            .unwrap()
    }

    fn real_file(dir: &TempDir) -> String {
        let real = dir.path().join("real.mp4");
        std::fs::write(&real, b"x").unwrap();
        real.to_str().unwrap().to_string()
    }

    #[test]
    fn a_landed_file_replaces_the_unaired_stand_in() {
        let dir = TempDir::new().unwrap();
        let real = real_file(&dir);
        let now = datetime!(2026-08-16 12:00:00 UTC);
        let mut items = vec![pending_item(
            "bumper:c:1:0",
            &real,
            false,
            datetime!(2026-08-16 13:00:00 UTC),
            10,
        )];
        assert_eq!(
            swap_items(&mut items, "c", now, len_20s, &mut |_| true).len(),
            1
        );
        assert_eq!(spans(&items), vec![(real, 10, Some(10_000))]);
        assert_eq!(items[0].id, "bumper:c:1:0");
        assert!(
            items[0]
                .metadata
                .as_ref()
                .unwrap()
                .get(PENDING_KEY)
                .is_none()
        );
        assert_eq!(items[0].metadata.as_ref().unwrap()["bumper"], true);
    }

    #[test]
    fn a_short_file_loops_when_the_segment_loops_and_pads_black_otherwise() {
        let dir = TempDir::new().unwrap();
        let real = real_file(&dir);
        let now = datetime!(2026-08-16 12:00:00 UTC);
        let at = datetime!(2026-08-16 13:00:00 UTC);

        let mut looped = vec![pending_item("bumper:c:1:0", &real, true, at, 10)];
        swap_items(&mut looped, "c", now, len_4s, &mut |_| true);
        assert_eq!(
            spans(&looped),
            vec![
                (real.clone(), 4, Some(4_000)),
                (real.clone(), 4, Some(4_000)),
                (real.clone(), 2, Some(2_000)),
            ]
        );
        assert_eq!(looped[2].id, "bumper:c:1:0#2");

        let mut padded = vec![pending_item("bumper:c:1:0", &real, false, at, 10)];
        swap_items(&mut padded, "c", now, len_4s, &mut |_| true);
        assert_eq!(
            spans(&padded),
            vec![(real, 4, Some(4_000)), ("lavfi".into(), 6, None)]
        );
        assert_eq!(padded[1].id, "bumper:c:1:0#pad");
    }

    #[test]
    fn a_stand_in_and_its_black_pad_are_swapped_as_one_segment() {
        let dir = TempDir::new().unwrap();
        let real = real_file(&dir);
        let at = datetime!(2026-08-16 13:00:00 UTC);
        let mut items = vec![
            pending_item("bumper:c:1:0", &real, false, at, 4),
            pending_item(
                "bumper:c:1:0#pad",
                &real,
                false,
                at + time::Duration::seconds(4),
                6,
            ),
        ];
        let n = swap_items(
            &mut items,
            "c",
            datetime!(2026-08-16 12:00:00 UTC),
            len_20s,
            &mut |_| true,
        )
        .len();
        assert_eq!(n, 1);
        assert_eq!(spans(&items), vec![(real, 10, Some(10_000))]);
    }

    #[test]
    fn an_airing_or_imminent_item_and_an_unlanded_file_keep_their_stand_in() {
        let dir = TempDir::new().unwrap();
        let real = real_file(&dir);
        let now = datetime!(2026-08-16 13:00:00 UTC);
        let mut items = vec![
            pending_item(
                "bumper:c:1:0",
                &real,
                false,
                now - time::Duration::seconds(5),
                10,
            ),
            pending_item(
                "bumper:c:2:0",
                &real,
                false,
                now + time::Duration::seconds(10),
                10,
            ),
            pending_item(
                "bumper:c:3:0",
                "/does/not/exist.mp4",
                false,
                now + time::Duration::minutes(5),
                10,
            ),
        ];
        assert_eq!(
            swap_items(&mut items, "c", now, len_20s, &mut |_| true).len(),
            0
        );
        assert!(
            items
                .iter()
                .all(|i| source_path(i) == Some("/stand/in.mp4"))
        );
        assert!(
            items
                .iter()
                .all(|i| i.metadata.as_ref().unwrap().get(PENDING_KEY).is_some())
        );
    }

    #[tokio::test]
    async fn the_playout_file_is_rewritten_only_when_a_swap_happens() {
        let dir = TempDir::new().unwrap();
        let real = real_file(&dir);
        let out = TempDir::new().unwrap();
        let file = write_playout(
            &out,
            vec![pending_item(
                "bumper:c:1:0",
                &real,
                false,
                datetime!(2026-08-16 13:00:00 UTC),
                10,
            )],
        )
        .await;
        let now = datetime!(2026-08-16 12:00:00 UTC);
        let db = Arc::new(HistoryDb::open_in_memory().unwrap());
        let mut seen = PendingSeen::new();

        // The generation has not recorded the segment yet: it keeps its stand-in.
        let n = file_tick(&file, &db, now, &mut seen).await;
        assert_eq!(n, 0);
        assert_eq!(
            source_path(&items_at(&file).await[0]),
            Some("/stand/in.mp4")
        );

        db.record(
            "c",
            &[crate::history::PlayRecord {
                entry_id: "bumper:c:1:0".into(),
                start: datetime!(2026-08-16 13:00:00 UTC),
                played_at: now,
                kind: crate::history::AiringKind::Bumper,
                source: Some("/stand/in.mp4".into()),
                ..Default::default()
            }],
        )
        .unwrap();
        let n = file_tick(&file, &db, now, &mut seen).await;
        assert_eq!(n, 1);
        assert_eq!(source_path(&items_at(&file).await[0]), Some(real.as_str()));
        let n = file_tick(&file, &db, now, &mut seen).await;
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn the_ledger_names_the_file_that_aired() {
        use crate::history::{AiringFilter, AiringKind, PlayRecord};

        let dir = TempDir::new().unwrap();
        let real = real_file(&dir);
        let ch = channel(&dir);
        let swapped_at = datetime!(2026-08-16 13:00:00 UTC);
        let unswapped_at = now() + time::Duration::seconds(10);
        let mut swapped = pending_item("bumper:c:1:0", &real, false, swapped_at, 10);
        let mut unswapped = pending_item("bumper:c:2:0", &real, false, unswapped_at, 10);
        for (item, name) in [(&mut swapped, "swapped"), (&mut unswapped, "unswapped")] {
            item.metadata.as_mut().unwrap()["segment"] = name.into();
        }
        write_playout(&dir, vec![unswapped, swapped]).await;

        let db = Arc::new(HistoryDb::open_in_memory().unwrap());
        let row = |name: &str, start| PlayRecord {
            entry_id: name.into(),
            start,
            played_at: now(),
            kind: AiringKind::Bumper,
            source: Some("/stand/in.mp4".into()),
            ..Default::default()
        };
        db.record(
            &ch.name,
            &[row("unswapped", unswapped_at), row("swapped", swapped_at)],
        )
        .unwrap();

        let mut seen = PendingSeen::new();
        let chs = [ch];
        assert_eq!(
            swap_pending_playout(&chs, &db, now(), len_20s, &mut seen).await,
            0
        );
        let n = swap_pending_playout(&chs, &db, now(), len_20s, &mut seen).await;
        assert_eq!(n, 1);

        let source_of = |name: &str| {
            db.airings(&AiringFilter {
                entry_id: Some(name.into()),
                ..Default::default()
            })
            .unwrap()
            .remove(0)
            .record
            .source
        };
        assert_eq!(source_of("swapped").as_deref(), Some(real.as_str()));
        assert_eq!(source_of("unswapped").as_deref(), Some("/stand/in.mp4"));
    }

    #[test]
    fn a_growing_file_keeps_its_stand_in_until_it_holds_still() {
        let dir = TempDir::new().unwrap();
        let real = real_file(&dir);
        let now = datetime!(2026-08-16 12:00:00 UTC);
        let at = datetime!(2026-08-16 13:00:00 UTC);
        let mut items = vec![pending_item("bumper:c:1:0", &real, false, at, 10)];
        let mut seen = PendingSeen::new();

        assert_eq!(item_tick(&mut items, now, &mut seen), 0, "first sight");
        std::fs::write(&real, b"xxxxxxxx").unwrap();
        assert_eq!(
            item_tick(&mut items, now, &mut seen),
            0,
            "grew since the last tick"
        );
        assert!(
            items[0]
                .metadata
                .as_ref()
                .unwrap()
                .get(PENDING_KEY)
                .is_some()
        );
        assert_eq!(
            item_tick(&mut items, now, &mut seen),
            1,
            "held still one tick"
        );
        assert!(
            items[0]
                .metadata
                .as_ref()
                .unwrap()
                .get(PENDING_KEY)
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_path_in_two_playout_files_is_not_stable_within_one_tick() {
        let dir = TempDir::new().unwrap();
        let real = real_file(&dir);
        let (out, out2) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let at = datetime!(2026-08-16 13:00:00 UTC);
        let a = write_playout(
            &out,
            vec![pending_item("bumper:c:1:0", &real, false, at, 10)],
        )
        .await;
        let b = write_playout(
            &out2,
            vec![pending_item("bumper:c:2:0", &real, false, at, 10)],
        )
        .await;
        let now = datetime!(2026-08-16 12:00:00 UTC);
        let db = Arc::new(HistoryDb::open_in_memory().unwrap());
        let mut seen = PendingSeen::new();
        let prev = PendingSeen::new();
        for f in [&a, &b] {
            let n = swap_pending_file(f, "c", &db, now, len_20s, &prev, &mut seen)
                .await
                .unwrap();
            assert_eq!(n, 0, "no earlier tick has seen this file");
        }
    }

    #[test]
    fn two_back_to_back_pending_segments_naming_one_file_stay_two_segments() {
        let dir = TempDir::new().unwrap();
        let real = real_file(&dir);
        let at = datetime!(2026-08-16 13:00:00 UTC);
        let mut items = vec![
            pending_item("bumper:c:1:0", &real, false, at, 10),
            pending_item(
                "bumper:c:1:1",
                &real,
                false,
                at + time::Duration::seconds(10),
                10,
            ),
        ];
        let n = swap_items(
            &mut items,
            "c",
            datetime!(2026-08-16 12:00:00 UTC),
            len_20s,
            &mut |_| true,
        )
        .len();
        assert_eq!(n, 2);
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["bumper:c:1:0", "bumper:c:1:1"]);
    }
}

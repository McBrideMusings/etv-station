//! Local-filesystem catalog ingester (#92, third slice of #47).
//!
//! Walks configured media roots, probes each file's duration with `ffprobe`, and
//! writes `entries` + `entry_sources` rows into the [`Catalog`]. Identity is
//! derived with ingest-time **path-match inherit**: a file whose canonical path
//! already resolves to an entry (e.g. a Plex-indexed file) reuses that
//! `entry_id` and only adds a `local_fs` provenance row; a file the catalog has
//! never seen gets the deterministic `fs:<fnv1a>` fallback and a fresh entry.
//!
//! The pure catalog-writing core, [`ingest_files`], takes already-probed
//! `(path, duration)` pairs so it is unit-testable without `ffprobe` or real
//! media; [`ingest_roots`] is the filesystem front door that globs + probes and
//! then calls it.
//!
//! A full pass also reconciles **deletions** (#144): every row it writes carries
//! the same `last_seen` stamp, and rows left holding an older one are files that
//! moved, were renamed, or were deleted — they get `missing_since` set, along
//! with any entry whose last live provenance row went with them. Without that, a
//! row for a file that is gone keeps telling a channel to play a path that no
//! longer resolves.
//!
//! Marked, not deleted (ADR 0006): `entry_id` is a durable join key for the
//! play-history ledger and the enrichment graph, and "gone forever" is
//! indistinguishable at this point from "gone until the next pass re-matches
//! it". The scheduler stops picking a missing entry ([`crate::catalog::Catalog::resolve_query`]);
//! everything joined on its id keeps resolving.
//!
//! It writes **no tags at all**. The directory a file sits in used to be stored
//! as an `fs_dir` tag, which only ever accumulated: move a bumper from
//! `bumpers/` to `commercials/` and the entry answered to both folders forever.
//! `item.fs_dir` now reads the directory off the `entry_sources` row at query
//! time (#123), so the answer moves when the file does — and an entry with rows
//! in two folders still matches both, which a per-entry tag reset could not have
//! got right.

use std::path::{Path, PathBuf};

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::catalog::identity::{canonical_path, derive_entry_id};
use crate::catalog::model::{Entry, EntrySource, Source};
use crate::catalog::{Catalog, CatalogError};
use crate::duration::FileProbe;

/// Video container extensions the walker considers media.
const MEDIA_EXTS: [&str; 5] = ["mp4", "mkv", "mov", "m4v", "webm"];

#[derive(Debug, thiserror::Error)]
pub enum FsIngestError {
    #[error("catalog: {0}")]
    Catalog(#[from] CatalogError),
    #[error("glob pattern: {0}")]
    Glob(String),
}

/// What one ingest pass touched.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FsIngestStats {
    /// Entries created or refreshed (only `fs:`-owned entries; a file inheriting
    /// a foreign entry_id leaves that entry's metadata untouched).
    pub entries_written: usize,
    /// `local_fs` provenance rows upserted (one per file seen).
    pub sources_written: usize,
    /// Files that inherited an existing entry_id via path-match (cross-source
    /// dedup or a prior scan).
    pub inherited: usize,
    /// `local_fs` provenance rows newly marked missing because the file behind
    /// them is no longer under a scanned root (moved, renamed, or deleted).
    /// Always 0 unless the pass was a full one. The rows are kept, not deleted
    /// (ADR 0006), so this counts rows that went from live to missing — a row
    /// already missing before this pass is not counted again.
    pub sources_marked_missing: usize,
    /// Entries newly marked missing because every one of their provenance rows
    /// is now missing, leaving nothing to play.
    pub entries_marked_missing: usize,
    /// Files whose probe found them unairable this pass (no video stream, no
    /// length, or a length far off the catalog's runtime). Pools stop drawing
    /// their entries.
    pub files_unairable: usize,
}

/// Walk `roots`, probe durations, and ingest into `catalog`. `identity_roots`
/// are the media mount roots used to canonicalise paths for identity (see
/// [`canonical_path`]) — a separate setting from `roots` (#243): this function
/// scans exactly the directories it is given, whatever `identity_roots` says.
/// Files that fail to probe are still ingested with a `None` duration — a
/// missing runtime is a metadata gap, not a reason to drop the file. A file
/// whose probe answers but describes something that cannot air is ingested too,
/// and marked unairable ([`record_airability`]) so pools stop drawing it.
pub async fn ingest_roots(
    catalog: &Catalog,
    roots: &[PathBuf],
    identity_roots: &[String],
) -> Result<FsIngestStats, FsIngestError> {
    // Case-insensitive so `.MKV` matches, and the root prefix is escaped so a
    // real directory name containing glob metacharacters (`Show [1080p]`,
    // `S01 [BluRay]`) is matched literally instead of as a character class — an
    // unescaped `[…]` would silently drop every file under it.
    let opts = glob::MatchOptions {
        case_sensitive: false,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    };
    let mut files: Vec<PathBuf> = Vec::new();
    for root in roots {
        let escaped_root = glob::Pattern::escape(&root.to_string_lossy());
        for ext in MEDIA_EXTS {
            let pattern = format!("{escaped_root}/**/*.{ext}");
            let matches =
                glob::glob_with(&pattern, opts).map_err(|e| FsIngestError::Glob(e.to_string()))?;
            files.extend(matches.filter_map(Result::ok));
        }
    }
    files.sort();
    files.dedup();

    let probes = probe_concurrently(files, PROBE_CONCURRENCY, |path| async move {
        crate::duration::probe_file(&path).await
    })
    .await;
    let probed: Vec<(PathBuf, Option<f64>)> = probes
        .iter()
        .map(|(path, probe)| (path.clone(), probe_seconds(probe)))
        .collect();
    // All writes in one transaction: a failure part-way rolls back, never
    // leaving a half-scanned catalog.
    //
    // `prune_absent: true` — this walked every configured root exhaustively, so a
    // stored row the walk did not reach is a file that is gone, not one that was
    // skipped. Anything that ever scans a subset of the roots must pass `false`.
    catalog.in_transaction(|c| {
        let mut stats = ingest_files(c, &probed, identity_roots, true)?;
        stats.files_unairable = record_airability(c, &probes, identity_roots)?;
        Ok(stats)
    })
}

/// The length a probe measured, for the catalog's `duration_ms`. `None` when the
/// probe learned nothing usable: the row keeps the length an earlier scan wrote.
fn probe_seconds(probe: &FileProbe) -> Option<f64> {
    match probe {
        FileProbe::Answered { duration, .. } => Some(duration.as_secs_f64()),
        FileProbe::NoAnswer | FileProbe::Garbled(_) => None,
    }
}

/// Write each probed file's verdict onto its `local_fs` provenance row, and
/// return how many were found unairable. Run after [`ingest_files`], which
/// creates the rows. A file whose probe did not answer is left alone, so a
/// stale mount cannot clear or set a verdict. The length compared against is the
/// catalog's own runtime, which exists only for an entry some other source
/// (Plex) authored; an `fs:` entry's runtime *is* the probe.
pub(crate) fn record_airability(
    catalog: &Catalog,
    probes: &[(PathBuf, FileProbe)],
    identity_roots: &[String],
) -> Result<usize, FsIngestError> {
    let roots: Vec<&str> = identity_roots.iter().map(String::as_str).collect();
    let mut unairable = 0;
    for (path, probe) in probes {
        let source_id = canonical_path(&path.to_string_lossy(), &roots);
        let reason = match probe {
            FileProbe::NoAnswer => continue,
            FileProbe::Garbled(reason) => Some(reason.clone()),
            FileProbe::Answered { duration, streams } => {
                let catalog_runtime = catalog
                    .entry_id_for_source(Source::LocalFs, &source_id)?
                    .filter(|entry_id| !entry_id.starts_with("fs:"))
                    .map(|entry_id| catalog.entry(&entry_id))
                    .transpose()?
                    .flatten()
                    .and_then(|e| e.duration_ms)
                    .filter(|&ms| ms > 0)
                    .map(|ms| std::time::Duration::from_millis(ms as u64));
                crate::duration::unairable_reason(*duration, streams, catalog_runtime)
            }
        };
        if let Some(reason) = &reason {
            unairable += 1;
            tracing::warn!(
                event = "catalog.file_unairable",
                path = %path.display(),
                reason = %reason,
                "file cannot air; pools stop drawing its entry"
            );
        }
        catalog.set_source_unairable(Source::LocalFs, &source_id, reason.as_deref())?;
        // A Plex row naming this same file is no escape from its verdict: the
        // entry has a second source, not a second file. Only a row naming a
        // different file counts as an airable alternative.
        if let Some(entry_id) = catalog.entry_id_for_source(Source::LocalFs, &source_id)? {
            for other in catalog.sources_for(&entry_id)? {
                if other.source != Source::LocalFs
                    && canonical_path(&other.playback_path, &roots) == source_id
                {
                    catalog.set_source_unairable(
                        other.source,
                        &other.source_id,
                        reason.as_deref(),
                    )?;
                }
            }
        }
    }
    Ok(unairable)
}

/// Write catalog rows for already-probed files. Pure over the catalog (no
/// filesystem or process access), so tests exercise identity, inherit, and
/// idempotency directly. Re-running with the same inputs is a no-op beyond
/// refreshing `last_seen`: entry ids are deterministic and every write is an
/// upsert keyed on a stable canonical path.
///
/// `prune_absent` reconciles *deletions*, the same contract as
/// [`super::plex::ingest_collections`]: when true, any `local_fs` provenance row
/// this pass did not stamp is marked missing, and any entry left with no live
/// provenance row at all is marked too. It must be true only when `files` is the
/// **complete** content of every root the caller scanned — on a partial pass,
/// absence means "not looked at", and marking would take files that are still on
/// disk out of the scheduling pool.
pub fn ingest_files(
    catalog: &Catalog,
    files: &[(PathBuf, Option<f64>)],
    identity_roots: &[String],
    prune_absent: bool,
) -> Result<FsIngestStats, FsIngestError> {
    let roots: Vec<&str> = identity_roots.iter().map(String::as_str).collect();
    // Canonical-path → entry_id over everything already in the catalog (Plex rows
    // from a prior ingest, or FS rows from a prior scan). Built once; within this
    // pass, deterministic `fs:` derivation keeps same-file duplicates coherent
    // even though they aren't in this map yet.
    let index = super::canonical_index(catalog, &roots)?;
    let now = OffsetDateTime::now_utc().format(&Rfc3339).ok();

    let mut stats = FsIngestStats::default();
    for (path, duration_secs) in files {
        let raw = path.to_string_lossy().into_owned();
        let canonical = canonical_path(&raw, &roots);

        let (entry_id, inherited) = match index.get(&canonical) {
            Some(existing) => (existing.clone(), true),
            None => (derive_entry_id(&[], &canonical), false),
        };
        if inherited {
            stats.inherited += 1;
        }

        // FS owns `fs:` entries — (re)write their metadata. Never clobber a
        // foreign entry (Plex/GUID-derived id): inheriting it means Plex has the
        // richer record; we only attach local provenance below.
        if entry_id.starts_with("fs:") {
            let mut entry = Entry::new(
                &entry_id,
                type_from_path(path),
                file_title(path),
                Source::LocalFs,
            );
            // `None` means this pass learned nothing (timeout on a stale mount,
            // unreadable file), not that the file has no length: keep the length
            // an earlier scan recorded rather than wiping it.
            entry.duration_ms = match duration_secs {
                Some(s) => Some((s * 1000.0) as i64),
                None => catalog.entry(&entry_id)?.and_then(|e| e.duration_ms),
            };
            catalog.upsert_entry(&entry)?;
            stats.entries_written += 1;
        }

        catalog.add_source(&EntrySource {
            source: Source::LocalFs,
            // Canonical path is the stable provenance key, so a re-scan upserts
            // the same row (PK is (source, source_id)) rather than duplicating.
            source_id: canonical,
            entry_id: entry_id.clone(),
            playback_path: raw,
            last_seen: now.clone(),
            // Ignored by `add_source`, which always writes NULL here: a row
            // this pass wrote is a row this pass saw (ADR 0006).
            missing_since: None,
            unairable_reason: None,
        })?;

        stats.sources_written += 1;
    }

    // Reconcile deletions last, so every row this pass touched already carries
    // `now` and only genuinely-absent files are left holding an older stamp.
    // `now` is `None` only if formatting the clock failed, in which case nothing
    // written above carries a stamp either — sweeping on it would mark the
    // whole source missing. Leaving the rows live is the safe failure.
    if let (true, Some(stamp)) = (prune_absent, now.as_deref()) {
        stats.sources_marked_missing =
            catalog.mark_unseen_sources_missing(Source::LocalFs, stamp)?;
        // Unconditional, not gated on `sources_marked_missing > 0`: an entry can
        // also be left with only missing sources by an earlier pass that marked
        // nothing, and one pass that leaves it behind leaves it behind forever.
        stats.entries_marked_missing = catalog.mark_entries_missing_without_live_sources(stamp)?;
    }
    Ok(stats)
}

/// Probes in flight at once. A file on a stale mount costs the full
/// [`crate::ffprobe::PROBE_TIMEOUT`]; run one at a time, N such files cost
/// N × 10s of scan time.
const PROBE_CONCURRENCY: usize = 8;

/// Run `probe` over every file with at most `limit` in flight, returning the
/// results in the order of `files` so the write pass stays deterministic.
async fn probe_concurrently<T, F, Fut>(
    files: Vec<PathBuf>,
    limit: usize,
    probe: F,
) -> Vec<(PathBuf, T)>
where
    T: Default + Send + 'static,
    F: Fn(PathBuf) -> Fut,
    Fut: std::future::Future<Output = T> + Send + 'static,
{
    let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(limit));
    let mut set = tokio::task::JoinSet::new();
    for (idx, path) in files.iter().cloned().enumerate() {
        let gate = gate.clone();
        let fut = probe(path);
        set.spawn(async move {
            let _permit = gate
                .acquire_owned()
                .await
                .expect("semaphore is never closed");
            (idx, fut.await)
        });
    }
    let mut results: Vec<T> = files.iter().map(|_| T::default()).collect();
    while let Some(done) = set.join_next().await {
        // A panicked probe task leaves its file's duration unknown, the same
        // as a failed probe.
        match done {
            Ok((idx, r)) => results[idx] = r,
            Err(e) => tracing::warn!(
                event = "catalog.probe_task_failed",
                error = %e,
                "probe task panicked; its file is treated as unprobed"
            ),
        }
    }
    files.into_iter().zip(results).collect()
}

/// The file's stem as its title (`station-bumper-01.mp4` → `station-bumper-01`).
fn file_title(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Derive a semantic `type` from the file's parent directory name.
/// `bumpers/` → `bumper`, `musicvideos/` → `music_video`, etc.; anything else is
/// a plain `video`.
pub fn type_from_path(path: &Path) -> String {
    let dir = path
        .parent()
        .and_then(Path::file_name)
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match dir.as_str() {
        "bumpers" | "bumper" => "bumper",
        "musicvideos" | "music_videos" | "music-videos" => "music_video",
        "concerts" | "concert" => "concert",
        "power_hours" | "power-hours" | "powerhours" => "power_hour",
        "commercials" | "commercial" => "commercial",
        "idents" | "ident" => "ident",
        "promos" | "promo" => "promo",
        _ => "video",
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::model::ExternalNs;

    fn file(path: &str, secs: f64) -> (PathBuf, Option<f64>) {
        (PathBuf::from(path), Some(secs))
    }

    /// Push every `local_fs` row's `last_seen` back to a fixed old instant, so
    /// the next pass's stamp is unambiguously different. Two `ingest_files`
    /// calls microseconds apart can format to the same RFC3339 string, and the
    /// sweep then (correctly, by design) declines to delete anything — this
    /// stands in for the real gap between two scans.
    fn age_fs_rows(cat: &Catalog) {
        cat.conn
            .execute(
                "UPDATE entry_sources SET last_seen = '2000-01-01T00:00:00Z'
                 WHERE source = 'local_fs'",
                [],
            )
            .unwrap();
    }

    #[test]
    fn a_failed_probe_keeps_the_recorded_duration_and_the_row_live() {
        let cat = Catalog::open_in_memory().unwrap();
        let roots = ["/data/media".to_string()];
        let path = "/data/media/a.mp4";
        ingest_files(&cat, &[file(path, 90.0)], &roots, true).unwrap();
        age_fs_rows(&cat);
        let id = cat.all_sources().unwrap()[0].entry_id.clone();

        ingest_files(&cat, &[(PathBuf::from(path), None)], &roots, true).unwrap();

        assert_eq!(cat.entry(&id).unwrap().unwrap().duration_ms, Some(90_000));
        assert_eq!(fs_paths(&cat), vec![path.to_string()]);
    }

    /// Every `local_fs` path the catalog would still play — missing rows are
    /// kept on disk (ADR 0006) but are not a file anything should open, so they
    /// are excluded here the same way [`crate::resolve::pick_playback_source`]
    /// excludes them.
    fn fs_paths(cat: &Catalog) -> Vec<String> {
        let mut paths: Vec<String> = cat
            .all_sources()
            .unwrap()
            .into_iter()
            .filter(|s| s.source == Source::LocalFs && s.missing_since.is_none())
            .map(|s| s.playback_path)
            .collect();
        paths.sort();
        paths
    }

    /// `missing_since` for one `local_fs` row, by playback path. `None` = live;
    /// the outer `None` means no such row at all.
    fn fs_missing_since(cat: &Catalog, playback_path: &str) -> Option<Option<String>> {
        cat.all_sources()
            .unwrap()
            .into_iter()
            .find(|s| s.source == Source::LocalFs && s.playback_path == playback_path)
            .map(|s| s.missing_since)
    }

    #[test]
    fn fs_only_content_gets_fs_ids_and_entries() {
        let cat = Catalog::open_in_memory().unwrap();
        let files = [
            file("/data/media/bumpers/station-01.mkv", 12.0),
            file("/data/media/commercials/cola.mp4", 30.0),
        ];
        let stats = ingest_files(&cat, &files, &["/data/media".into()], true).unwrap();
        assert_eq!(stats.entries_written, 2);
        assert_eq!(stats.sources_written, 2);
        assert_eq!(stats.inherited, 0);

        let ids = cat.all_entry_ids().unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.iter().all(|id| id.starts_with("fs:")));

        // Metadata: type from dir, title from stem, duration in ms.
        let sources = cat.all_sources().unwrap();
        let bumper = sources
            .iter()
            .find(|s| s.playback_path.ends_with("station-01.mkv"))
            .unwrap();
        let bumper_id = &bumper.entry_id;
        let e = cat.entry(bumper_id).unwrap().unwrap();
        assert_eq!(e.kind, "bumper");
        assert_eq!(e.title, "station-01");
        assert_eq!(e.duration_ms, Some(12_000));
        assert_eq!(
            cat.resolve_query(r#"item.fs_dir == "bumpers""#).unwrap(),
            vec![bumper_id.clone()]
        );
    }

    #[test]
    fn file_indexed_by_plex_dedupes_onto_plex_entry() {
        let cat = Catalog::open_in_memory().unwrap();
        // Seed a Plex-style entry with a provenance row for a real file path.
        cat.upsert_entry(&Entry::new(
            "imdb:tt0095016",
            "movie",
            "Die Hard",
            Source::Plex,
        ))
        .unwrap();
        cat.add_source(&EntrySource {
            source: Source::Plex,
            source_id: "plex-12345".into(),
            entry_id: "imdb:tt0095016".into(),
            playback_path: "/data/media/movies/Die Hard (1988)/Die.Hard.mkv".into(),
            last_seen: None,
            missing_since: None,
            unairable_reason: None,
        })
        .unwrap();

        // FS scan reaches the same file under a *different* mount root.
        let files = [file(
            "/mnt/media/movies/Die Hard (1988)/Die.Hard.mkv",
            132.0 * 60.0,
        )];
        let stats = ingest_files(
            &cat,
            &files,
            &["/data/media".into(), "/mnt/media".into()],
            true,
        )
        .unwrap();

        // Inherited the Plex entry_id, wrote no new entry, added a second row.
        assert_eq!(stats.inherited, 1);
        assert_eq!(stats.entries_written, 0);
        assert_eq!(
            cat.all_entry_ids().unwrap(),
            vec!["imdb:tt0095016".to_string()]
        );
        let sources = cat.sources_for("imdb:tt0095016").unwrap();
        assert_eq!(sources.len(), 2);
        assert!(sources.iter().any(|s| s.source == Source::LocalFs));
        // Plex metadata untouched — FS did not clobber the title.
        assert_eq!(
            cat.entry("imdb:tt0095016").unwrap().unwrap().title,
            "Die Hard"
        );
    }

    #[test]
    fn inherit_prefers_a_foreign_id_over_a_stale_fs_id() {
        // A canonical path already resolves to BOTH a stale fs: entry (an earlier
        // FS scan) and a Plex entry. A new scan must inherit the Plex id so the
        // file merges onto the richer record, not the fs: one.
        let cat = Catalog::open_in_memory().unwrap();
        let path = "/data/media/movies/x.mkv";
        cat.upsert_entry(&Entry::new("fs:deadbeef", "video", "x", Source::LocalFs))
            .unwrap();
        cat.add_source(&EntrySource {
            source: Source::LocalFs,
            source_id: "movies/x.mkv".into(),
            entry_id: "fs:deadbeef".into(),
            playback_path: path.into(),
            last_seen: None,
            missing_since: None,
            unairable_reason: None,
        })
        .unwrap();
        cat.upsert_entry(&Entry::new("imdb:tt1", "movie", "X", Source::Plex))
            .unwrap();
        cat.add_source(&EntrySource {
            source: Source::Plex,
            source_id: "plex-1".into(),
            entry_id: "imdb:tt1".into(),
            playback_path: path.into(),
            last_seen: None,
            missing_since: None,
            unairable_reason: None,
        })
        .unwrap();

        let stats = ingest_files(&cat, &[file(path, 90.0)], &["/data/media".into()], true).unwrap();
        assert_eq!(stats.inherited, 1);
        assert_eq!(stats.entries_written, 0);
        // The local_fs provenance row now points at the Plex entry.
        let local = cat
            .all_sources()
            .unwrap()
            .into_iter()
            .find(|s| s.source == Source::LocalFs)
            .unwrap();
        assert_eq!(local.entry_id, "imdb:tt1");
    }

    #[test]
    fn rescans_are_idempotent() {
        let cat = Catalog::open_in_memory().unwrap();
        let files = [
            file("/data/media/bumpers/a.mkv", 5.0),
            file("/data/media/bumpers/b.mkv", 6.0),
        ];
        let roots = ["/data/media".to_string()];
        ingest_files(&cat, &files, &roots, true).unwrap();
        let first_ids = cat.all_entry_ids().unwrap();
        let first_sources: usize = first_ids
            .iter()
            .map(|id| cat.sources_for(id).unwrap().len())
            .sum();

        // Second pass: same files → same rows, now all inherited, no duplication.
        let stats = ingest_files(&cat, &files, &roots, true).unwrap();
        assert_eq!(stats.inherited, 2);
        assert_eq!(stats.sources_marked_missing, 0);
        assert_eq!(stats.entries_marked_missing, 0);
        assert_eq!(cat.all_entry_ids().unwrap(), first_ids);
        let second_sources: usize = first_ids
            .iter()
            .map(|id| cat.sources_for(id).unwrap().len())
            .sum();
        assert_eq!(first_sources, second_sources);
        assert_eq!(cat.all_sources().unwrap().len(), 2);
    }

    #[test]
    fn same_file_under_two_roots_in_one_pass_is_one_entry() {
        let cat = Catalog::open_in_memory().unwrap();
        let files = [
            file("/data/media/bumpers/x.mkv", 5.0),
            file("/mnt/media/bumpers/x.mkv", 5.0),
        ];
        ingest_files(
            &cat,
            &files,
            &["/data/media".into(), "/mnt/media".into()],
            true,
        )
        .unwrap();
        // Both canonicalise to `bumpers/x.mkv` → one deterministic fs: entry, one
        // provenance row (same canonical source_id upserts in place).
        assert_eq!(cat.all_entry_ids().unwrap().len(), 1);
        assert_eq!(cat.all_sources().unwrap().len(), 1);
    }

    /// A file deleted off disk stops being playable, but its row and its entry
    /// stay (ADR 0006) — with `missing_since` set, which is what takes it out of
    /// the scheduling pool. The old behaviour deleted both, which silently
    /// orphaned every ledger row joined on the `entry_id`.
    #[test]
    fn full_scan_marks_a_deleted_file_missing_and_keeps_its_row() {
        let cat = Catalog::open_in_memory().unwrap();
        let roots = ["/data/media".to_string()];
        ingest_files(
            &cat,
            &[
                file("/data/media/bumpers/keep.mkv", 5.0),
                file("/data/media/bumpers/gone.mkv", 6.0),
            ],
            &roots,
            true,
        )
        .unwrap();
        assert_eq!(fs_paths(&cat).len(), 2);
        age_fs_rows(&cat);

        // `gone.mkv` was deleted off disk, so the next full walk never sees it.
        let stats = ingest_files(
            &cat,
            &[file("/data/media/bumpers/keep.mkv", 5.0)],
            &roots,
            true,
        )
        .unwrap();

        assert_eq!(stats.sources_marked_missing, 1);
        assert_eq!(stats.entries_marked_missing, 1);
        assert_eq!(fs_paths(&cat), vec!["/data/media/bumpers/keep.mkv"]);
        // Both rows are still there; only one of them is still live.
        assert_eq!(cat.all_sources().unwrap().len(), 2);
        assert_eq!(cat.all_entry_ids().unwrap().len(), 2);
        assert!(
            fs_missing_since(&cat, "/data/media/bumpers/gone.mkv")
                .expect("the deleted file's row must be kept, not deleted")
                .is_some(),
            "the deleted file's provenance row must carry missing_since",
        );
        assert!(
            fs_missing_since(&cat, "/data/media/bumpers/keep.mkv")
                .unwrap()
                .is_none(),
            "the surviving file's row must stay live",
        );
        let gone_id = cat
            .all_entry_ids()
            .unwrap()
            .into_iter()
            .find(|id| fs_paths_for(&cat, id) == vec!["/data/media/bumpers/gone.mkv"])
            .expect("the deleted file's entry must be kept");
        assert!(
            cat.entry(&gone_id)
                .unwrap()
                .unwrap()
                .missing_since
                .is_some(),
            "an entry with no live source left must be marked missing",
        );
        // And the scheduler stops picking it, which is the whole point.
        assert_eq!(
            cat.resolve_query(r#"item.fs_dir == "bumpers""#)
                .unwrap()
                .len(),
            1,
        );
    }

    /// Every `local_fs` playback path attached to one entry, live or missing.
    fn fs_paths_for(cat: &Catalog, entry_id: &str) -> Vec<String> {
        let mut paths: Vec<String> = cat
            .sources_for(entry_id)
            .unwrap()
            .into_iter()
            .filter(|s| s.source == Source::LocalFs)
            .map(|s| s.playback_path)
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn full_scan_leaves_one_row_for_a_moved_file() {
        let cat = Catalog::open_in_memory().unwrap();
        let roots = ["/data/media".to_string()];
        ingest_files(
            &cat,
            &[file("/data/media/bumpers/x.mkv", 5.0)],
            &roots,
            true,
        )
        .unwrap();
        age_fs_rows(&cat);

        // Same file, dragged from bumpers/ to commercials/.
        ingest_files(
            &cat,
            &[file("/data/media/commercials/x.mkv", 5.0)],
            &roots,
            true,
        )
        .unwrap();

        assert_eq!(fs_paths(&cat), vec!["/data/media/commercials/x.mkv"]);
        // Two entries now, because the `fs:` id is derived from the path and the
        // path changed — but only the new one is live. The old one is kept and
        // marked missing (ADR 0006) rather than deleted, so anything joined on
        // its id still resolves.
        let ids: Vec<String> = cat
            .all_entry_ids()
            .unwrap()
            .into_iter()
            .filter(|id| cat.entry(id).unwrap().unwrap().missing_since.is_none())
            .collect();
        assert_eq!(ids.len(), 1, "only the moved-to entry is still live");
        assert_eq!(cat.all_entry_ids().unwrap().len(), 2);
        // The move re-typed it, and `item.fs_dir` follows the file: it answers to
        // the folder the file is in now and stops answering to the one it left
        // (#123). Here the `fs:` id is derived from the path, so the move also
        // replaces the entry; the folder still has to be read off the surviving
        // row rather than assumed from the id, which is what this checks.
        assert_eq!(cat.entry(&ids[0]).unwrap().unwrap().kind, "commercial");
        assert_eq!(
            cat.resolve_query(r#"item.fs_dir == "commercials""#)
                .unwrap(),
            ids
        );
        assert!(
            cat.resolve_query(r#"item.fs_dir == "bumpers""#)
                .unwrap()
                .is_empty(),
            "the folder the file left must stop matching"
        );
    }

    /// The filesystem scan writes no tag rows at all now — the folder lives only
    /// in `entry_sources`, where it cannot be a second, staler copy of anything.
    #[test]
    fn a_scan_writes_no_tag_rows() {
        let cat = Catalog::open_in_memory().unwrap();
        ingest_files(
            &cat,
            &[file("/data/media/bumpers/x.mkv", 5.0)],
            &["/data/media".to_string()],
            true,
        )
        .unwrap();
        let tag_rows: i64 = cat
            .conn
            .query_row("SELECT COUNT(*) FROM tags", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tag_rows, 0);
    }

    /// Marking the last source missing takes the entry out of the scheduling
    /// pool, but leaves the row and everything hanging off it — external ids,
    /// tags, collection membership — exactly where they were. That retention is
    /// the point of ADR 0006: `entry_id` is a join key the play-history ledger
    /// and the enrichment graph depend on.
    #[test]
    fn marking_the_last_source_keeps_the_entry_and_its_children() {
        let cat = Catalog::open_in_memory().unwrap();
        let roots = ["/data/media".to_string()];
        ingest_files(
            &cat,
            &[file("/data/media/bumpers/x.mkv", 5.0)],
            &roots,
            true,
        )
        .unwrap();
        let id = cat.all_entry_ids().unwrap()[0].clone();
        cat.add_external_id(ExternalNs::Imdb, "tt-swept", "video", &id)
            .unwrap();
        assert_eq!(
            cat.resolve_query(r#"item.fs_dir == "bumpers""#).unwrap(),
            vec![id.clone()]
        );
        age_fs_rows(&cat);

        // The root is now empty: nothing left to attach provenance to.
        let stats = ingest_files(&cat, &[], &roots, true).unwrap();

        assert_eq!(stats.sources_marked_missing, 1);
        assert_eq!(stats.entries_marked_missing, 1);
        assert!(
            cat.entry(&id).unwrap().unwrap().missing_since.is_some(),
            "the entry is kept, marked missing",
        );
        assert!(
            cat.resolve_query(r#"item.fs_dir == "bumpers""#)
                .unwrap()
                .is_empty(),
            "a missing entry must not be schedulable",
        );
        assert_eq!(
            cat.entry_id_for_external_id(ExternalNs::Imdb, "tt-swept", "video")
                .unwrap(),
            Some(id),
            "the external id must keep resolving — that is what makes the file resurfacing a re-match rather than a fresh entry",
        );
    }

    #[test]
    fn a_partial_scan_marks_nothing() {
        let cat = Catalog::open_in_memory().unwrap();
        let roots = ["/data/media".to_string()];
        ingest_files(
            &cat,
            &[
                file("/data/media/bumpers/a.mkv", 5.0),
                file("/data/media/commercials/b.mkv", 6.0),
            ],
            &roots,
            true,
        )
        .unwrap();
        age_fs_rows(&cat);

        // Only `bumpers/` was walked this time. `commercials/b.mkv` is absent
        // because nobody looked, not because it is gone.
        let stats = ingest_files(
            &cat,
            &[file("/data/media/bumpers/a.mkv", 5.0)],
            &roots,
            false,
        )
        .unwrap();

        assert_eq!(stats.sources_marked_missing, 0);
        assert_eq!(stats.entries_marked_missing, 0);
        assert_eq!(
            fs_paths(&cat),
            vec![
                "/data/media/bumpers/a.mkv".to_string(),
                "/data/media/commercials/b.mkv".to_string()
            ]
        );
        assert_eq!(cat.all_entry_ids().unwrap().len(), 2);
    }

    #[test]
    fn a_swept_fs_row_leaves_a_plex_backed_entry_alone() {
        let cat = Catalog::open_in_memory().unwrap();
        let roots = ["/data/media".to_string()];
        let path = "/data/media/movies/Die Hard.mkv";
        cat.upsert_entry(&Entry::new("imdb:tt1", "movie", "Die Hard", Source::Plex))
            .unwrap();
        cat.add_source(&EntrySource {
            source: Source::Plex,
            source_id: "plex-1".into(),
            entry_id: "imdb:tt1".into(),
            playback_path: path.into(),
            last_seen: Some("2000-01-01T00:00:00Z".into()),
            missing_since: None,
            unairable_reason: None,
        })
        .unwrap();
        ingest_files(&cat, &[file(path, 120.0)], &roots, true).unwrap();
        age_fs_rows(&cat);

        // The local copy vanished; Plex still serves the same title.
        let stats = ingest_files(&cat, &[], &roots, true).unwrap();

        assert_eq!(stats.sources_marked_missing, 1);
        assert_eq!(
            stats.entries_marked_missing, 0,
            "the Plex row is still live, so the entry is not missing",
        );
        assert!(fs_paths(&cat).is_empty());
        // The Plex row's own `last_seen` is stale too, and the fs sweep must not
        // have touched it — nor may the entry have been marked.
        let plex_row = cat
            .sources_for("imdb:tt1")
            .unwrap()
            .into_iter()
            .find(|s| s.source == Source::Plex)
            .unwrap();
        assert!(plex_row.missing_since.is_none());
        let entry = cat.entry("imdb:tt1").unwrap().unwrap();
        assert_eq!(entry.title, "Die Hard");
        assert!(entry.missing_since.is_none());
    }

    /// The daemon's actual front door, driven over a real directory: scan, move
    /// a file on disk, scan again. Everything above tests the pure core with a
    /// hand-built file list; this proves the glob walk and the sweep agree about
    /// what "still there" means. Files are empty, so `ffprobe` fails and every
    /// duration is `None` — the sweep does not depend on probing.
    #[tokio::test]
    async fn ingest_roots_over_a_real_directory_marks_a_moved_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("bumpers")).unwrap();
        std::fs::create_dir_all(root.join("commercials")).unwrap();
        std::fs::write(root.join("bumpers/station-bumper-01.mp4"), b"").unwrap();

        let cat = Catalog::open_in_memory().unwrap();
        let roots = vec![root.clone()];
        let identity_roots = vec![root.to_string_lossy().into_owned()];
        let stats = ingest_roots(&cat, &roots, &identity_roots).await.unwrap();
        assert_eq!(stats.sources_written, 1);
        assert_eq!(stats.sources_marked_missing, 0);
        age_fs_rows(&cat);

        std::fs::rename(
            root.join("bumpers/station-bumper-01.mp4"),
            root.join("commercials/station-bumper-01.mp4"),
        )
        .unwrap();

        let stats = ingest_roots(&cat, &roots, &identity_roots).await.unwrap();
        assert_eq!(stats.sources_written, 1);
        assert_eq!(stats.sources_marked_missing, 1);
        assert_eq!(stats.entries_marked_missing, 1);
        assert_eq!(
            fs_paths(&cat),
            vec![
                root.join("commercials/station-bumper-01.mp4")
                    .to_string_lossy()
                    .into_owned()
            ]
        );
        // Two rows on the books, one of them missing — the moved-from path is
        // kept so the file moving back is a re-match, not a fresh entry.
        assert_eq!(cat.all_entry_ids().unwrap().len(), 2);
    }

    fn answered(secs: u64, kinds: &[crate::tracks::StreamKind]) -> FileProbe {
        FileProbe::Answered {
            duration: std::time::Duration::from_secs(secs),
            streams: kinds
                .iter()
                .enumerate()
                .map(|(i, k)| crate::tracks::StreamInfo {
                    index: i as u32,
                    kind: *k,
                    codec: "x".into(),
                    lang: None,
                    title: None,
                    channels: None,
                    default: false,
                    forced: false,
                })
                .collect(),
        }
    }

    fn fs_id(path: &str) -> String {
        derive_entry_id(&[], &canonical_path(path, &["/data/media"]))
    }

    fn pooled(cat: &Catalog) -> Vec<String> {
        cat.resolve_query(r#"item.fs_dir == "movies""#).unwrap()
    }

    #[test]
    fn a_file_with_no_video_stream_leaves_the_pool_and_a_good_rescan_returns_it() {
        use crate::tracks::StreamKind::{Audio, Video};
        let cat = Catalog::open_in_memory().unwrap();
        let roots = ["/data/media".to_string()];
        let good = "/data/media/movies/good.mkv";
        let bad = "/data/media/movies/bad.mkv";
        let files = [file(good, 5400.0), file(bad, 5400.0)];
        ingest_files(&cat, &files, &roots, true).unwrap();
        assert_eq!(pooled(&cat).len(), 2);

        let probes = vec![
            (PathBuf::from(good), answered(5400, &[Video, Audio])),
            (PathBuf::from(bad), answered(5400, &[Audio])),
        ];
        assert_eq!(record_airability(&cat, &probes, &roots).unwrap(), 1);
        let left = pooled(&cat);
        assert_eq!(left, vec![fs_id(good)]);
        assert_eq!(
            cat.source_unairable_reason(Source::LocalFs, &canonical_path(bad, &["/data/media"]))
                .unwrap(),
            Some("no video stream".to_string())
        );

        // A timeout learns nothing: the verdict stands.
        let probes = vec![(PathBuf::from(bad), FileProbe::NoAnswer)];
        record_airability(&cat, &probes, &roots).unwrap();
        assert_eq!(pooled(&cat).len(), 1);

        // The file is replaced with a good one; the next answer clears it.
        let probes = vec![(PathBuf::from(bad), answered(5400, &[Video, Audio]))];
        assert_eq!(record_airability(&cat, &probes, &roots).unwrap(), 0);
        assert_eq!(pooled(&cat).len(), 2);
    }

    #[test]
    fn cover_art_zero_length_and_garbled_output_are_unairable() {
        use crate::tracks::StreamKind::{Other, Video};
        let cat = Catalog::open_in_memory().unwrap();
        let roots = ["/data/media".to_string()];
        let paths: Vec<String> = ["art", "zero", "garbled"]
            .iter()
            .map(|n| format!("/data/media/movies/{n}.mkv"))
            .collect();
        let files: Vec<_> = paths.iter().map(|p| file(p, 60.0)).collect();
        ingest_files(&cat, &files, &roots, true).unwrap();
        let probes = vec![
            (PathBuf::from(&paths[0]), answered(60, &[Other])),
            (PathBuf::from(&paths[1]), answered(0, &[Video])),
            (
                PathBuf::from(&paths[2]),
                FileProbe::Garbled("format.duration missing".into()),
            ),
        ];
        assert_eq!(record_airability(&cat, &probes, &roots).unwrap(), 3);
        assert!(pooled(&cat).is_empty());
    }

    #[test]
    fn a_probe_far_off_a_plex_runtime_is_unairable_but_an_fs_runtime_is_the_probe() {
        use crate::tracks::StreamKind::Video;
        let cat = Catalog::open_in_memory().unwrap();
        let roots = ["/data/media".to_string()];
        // A Plex-authored entry whose runtime is 5,040s, matched by path.
        let trunc = "/data/media/movies/trunc.mkv";
        let other = "/data/media/movies/other.mkv";
        let mut plex = Entry::new("plex:1", "movie", "Truncated", Source::Plex);
        plex.duration_ms = Some(5_040_000);
        cat.upsert_entry(&plex).unwrap();
        cat.add_source(&EntrySource {
            source: Source::Plex,
            source_id: trunc.into(),
            entry_id: "plex:1".into(),
            playback_path: trunc.into(),
            last_seen: None,
            missing_since: None,
            unairable_reason: None,
        })
        .unwrap();
        ingest_files(&cat, &[file(trunc, 5.0), file(other, 5.0)], &roots, true).unwrap();
        let probes = vec![
            (PathBuf::from(trunc), answered(5, &[Video])),
            (PathBuf::from(other), answered(5, &[Video])),
        ];
        assert_eq!(record_airability(&cat, &probes, &roots).unwrap(), 1);
        assert_eq!(pooled(&cat), vec![fs_id(other)]);
    }

    /// A Plex row naming the probed file shares its verdict; a Plex row naming a
    /// different file does not, so the entry still airs from it. A later airable
    /// probe clears both.
    #[test]
    fn a_verdict_reaches_only_the_plex_row_naming_the_same_file() {
        use crate::tracks::StreamKind::Video;
        let cat = Catalog::open_in_memory().unwrap();
        let roots = ["/data/media".to_string()];
        let bad = "/data/media/movies/bad.mkv";
        let good = "/data/media/movies/good.mkv";
        let plex_row = |id: &str, path: &str| EntrySource {
            source: Source::Plex,
            source_id: id.into(),
            entry_id: "plex:1".into(),
            playback_path: path.into(),
            last_seen: None,
            missing_since: None,
            unairable_reason: None,
        };
        let mut plex = Entry::new("plex:1", "movie", "Both", Source::Plex);
        plex.duration_ms = Some(5_000_000);
        cat.upsert_entry(&plex).unwrap();
        cat.add_source(&plex_row("p-bad", bad)).unwrap();
        cat.add_source(&plex_row("p-good", good)).unwrap();
        ingest_files(&cat, &[file(bad, 5.0)], &roots, true).unwrap();
        // The bad file has no video stream; the good one is never fs-probed.
        let probes = vec![(PathBuf::from(bad), answered(5, &[]))];
        assert_eq!(record_airability(&cat, &probes, &roots).unwrap(), 1);
        assert!(
            cat.source_unairable_reason(Source::Plex, "p-bad")
                .unwrap()
                .is_some()
        );
        assert_eq!(
            cat.source_unairable_reason(Source::Plex, "p-good").unwrap(),
            None
        );
        assert_eq!(
            cat.resolve_query("item.title != ''").unwrap(),
            vec!["plex:1"]
        );

        let probes = vec![(PathBuf::from(bad), answered(5000, &[Video]))];
        assert_eq!(record_airability(&cat, &probes, &roots).unwrap(), 0);
        assert_eq!(
            cat.source_unairable_reason(Source::Plex, "p-bad").unwrap(),
            None
        );
    }

    /// A verdict is about one file: when Plex repoints the row to a different
    /// path the mark goes, and re-adding the same path keeps it.
    #[test]
    fn repointing_a_marked_plex_row_clears_its_mark() {
        let cat = Catalog::open_in_memory().unwrap();
        let roots = ["/data/media".to_string()];
        let bad = "/data/media/movies/bad.mkv";
        let fixed = "/data/media/movies/fixed.mkv";
        let plex_row = |path: &str| EntrySource {
            source: Source::Plex,
            source_id: "p-1".into(),
            entry_id: "plex:1".into(),
            playback_path: path.into(),
            last_seen: None,
            missing_since: None,
            unairable_reason: None,
        };
        let mut plex = Entry::new("plex:1", "movie", "Both", Source::Plex);
        plex.duration_ms = Some(5_000_000);
        cat.upsert_entry(&plex).unwrap();
        cat.add_source(&plex_row(bad)).unwrap();
        ingest_files(&cat, &[file(bad, 5.0)], &roots, true).unwrap();
        let probes = vec![(PathBuf::from(bad), answered(5, &[]))];
        assert_eq!(record_airability(&cat, &probes, &roots).unwrap(), 1);
        assert!(cat.resolve_query("item.title != ''").unwrap().is_empty());

        cat.add_source(&plex_row(bad)).unwrap();
        assert!(
            cat.source_unairable_reason(Source::Plex, "p-1")
                .unwrap()
                .is_some()
        );

        cat.add_source(&plex_row(fixed)).unwrap();
        assert_eq!(
            cat.source_unairable_reason(Source::Plex, "p-1").unwrap(),
            None
        );
        assert_eq!(
            cat.resolve_query("item.title != ''").unwrap(),
            vec!["plex:1"]
        );
    }

    /// 16 files that each take 200ms to probe finish in about two waves at a
    /// limit of 8, not 16 serial waits, and results keep the input order.
    #[tokio::test]
    async fn probes_overlap_up_to_the_limit_and_keep_input_order() {
        let files: Vec<PathBuf> = (0..16)
            .map(|i| PathBuf::from(format!("/m/{i}.mp4")))
            .collect();
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let out = probe_concurrently(files.clone(), 8, |path| {
            let (in_flight, peak) = (in_flight.clone(), peak.clone());
            async move {
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                let n: f64 = path.file_stem().unwrap().to_str().unwrap().parse().unwrap();
                Some(n)
            }
        })
        .await;
        assert_eq!(
            peak.load(Ordering::SeqCst),
            8,
            "probes must overlap up to the limit and never beyond it"
        );
        assert_eq!(out.len(), 16);
        for (i, (path, secs)) in out.iter().enumerate() {
            assert_eq!(path, &files[i]);
            assert_eq!(*secs, Some(i as f64));
        }
    }

    #[test]
    fn type_from_path_maps_known_dirs() {
        assert_eq!(type_from_path(Path::new("/m/bumpers/a.mp4")), "bumper");
        assert_eq!(
            type_from_path(Path::new("/m/musicvideos/a.mp4")),
            "music_video"
        );
        assert_eq!(type_from_path(Path::new("/m/concerts/a.mkv")), "concert");
        assert_eq!(type_from_path(Path::new("/m/whatever/a.mp4")), "video");
    }
}

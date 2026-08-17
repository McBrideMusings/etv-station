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
}

/// Walk `roots`, probe durations, and ingest into `catalog`. `identity_roots`
/// are the media mount roots used to canonicalise paths for identity (see
/// [`canonical_path`]) — a separate setting from `roots` (#243): this function
/// scans exactly the directories it is given, whatever `identity_roots` says.
/// Files that fail to probe are still ingested with a `None` duration — a
/// missing runtime is a metadata gap, not a reason to drop the file.
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

    let mut probed: Vec<(PathBuf, Option<f64>)> = Vec::with_capacity(files.len());
    for path in files {
        let secs = ffprobe_seconds(&path).await;
        probed.push((path, secs));
    }
    // All writes in one transaction: a failure part-way rolls back, never
    // leaving a half-scanned catalog.
    //
    // `prune_absent: true` — this walked every configured root exhaustively, so a
    // stored row the walk did not reach is a file that is gone, not one that was
    // skipped. Anything that ever scans a subset of the roots must pass `false`.
    catalog.in_transaction(|c| ingest_files(c, &probed, identity_roots, true))
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
            entry.duration_ms = duration_secs.map(|s| (s * 1000.0) as i64);
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

/// Probe a file's duration in seconds. Returns `None` on any failure (spawn
/// error, non-zero exit, unparseable output) — a scan tolerates individual bad
/// files rather than aborting the whole pass.
async fn ffprobe_seconds(path: &Path) -> Option<f64> {
    let output = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    // `parse::<f64>` accepts "nan"/"inf"; reject non-finite so a corrupt probe
    // records an unknown duration (None) rather than a garbage 0 / i64::MAX ms.
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|s| s.is_finite())
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
        cat.add_external_id(ExternalNs::Imdb, "tt-swept", &id)
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
            cat.entry_id_for_external_id(ExternalNs::Imdb, "tt-swept")
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

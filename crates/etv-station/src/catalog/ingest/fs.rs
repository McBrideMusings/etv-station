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

use std::path::{Path, PathBuf};

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::catalog::identity::{canonical_path, derive_entry_id};
use crate::catalog::model::{Entry, EntrySource, Source, TagNs};
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
}

/// Walk `roots`, probe durations, and ingest into `catalog`. `source_roots` are
/// the media mount roots used to canonicalise paths for identity (see
/// [`canonical_path`]). Files that fail to probe are still ingested with a `None`
/// duration — a missing runtime is a metadata gap, not a reason to drop the file.
pub async fn ingest_roots(
    catalog: &Catalog,
    roots: &[PathBuf],
    source_roots: &[String],
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
            let matches = glob::glob_with(&pattern, opts)
                .map_err(|e| FsIngestError::Glob(e.to_string()))?;
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
    catalog.in_transaction(|c| ingest_files(c, &probed, source_roots))
}

/// Write catalog rows for already-probed files. Pure over the catalog (no
/// filesystem or process access), so tests exercise identity, inherit, and
/// idempotency directly. Re-running with the same inputs is a no-op beyond
/// refreshing `last_seen`: entry ids are deterministic and every write is an
/// upsert keyed on a stable canonical path.
pub fn ingest_files(
    catalog: &Catalog,
    files: &[(PathBuf, Option<f64>)],
    source_roots: &[String],
) -> Result<FsIngestStats, FsIngestError> {
    let roots: Vec<&str> = source_roots.iter().map(String::as_str).collect();
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
            let mut entry = Entry::new(&entry_id, type_from_path(path), file_title(path), Source::LocalFs);
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
        })?;

        if let Some(dir) = parent_dir_name(path) {
            catalog.add_tag(&entry_id, TagNs::FsDir, &dir)?;
        }
        stats.sources_written += 1;
    }
    Ok(stats)
}

/// The file's stem as its title (`station-bumper-01.mp4` → `station-bumper-01`).
fn file_title(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The immediate parent directory name, used as the `fs_dir` tag value.
fn parent_dir_name(path: &Path) -> Option<String> {
    path.parent()
        .and_then(Path::file_name)
        .map(|s| s.to_string_lossy().into_owned())
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

    fn file(path: &str, secs: f64) -> (PathBuf, Option<f64>) {
        (PathBuf::from(path), Some(secs))
    }

    #[test]
    fn fs_only_content_gets_fs_ids_and_entries() {
        let cat = Catalog::open_in_memory().unwrap();
        let files = [
            file("/data/media/bumpers/station-01.mkv", 12.0),
            file("/data/media/commercials/cola.mp4", 30.0),
        ];
        let stats = ingest_files(&cat, &files, &["/data/media".into()]).unwrap();
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
            cat.tags_for(bumper_id, TagNs::FsDir).unwrap(),
            vec!["bumpers".to_string()]
        );
    }

    #[test]
    fn file_indexed_by_plex_dedupes_onto_plex_entry() {
        let cat = Catalog::open_in_memory().unwrap();
        // Seed a Plex-style entry with a provenance row for a real file path.
        cat.upsert_entry(&Entry::new("imdb:tt0095016", "movie", "Die Hard", Source::Plex))
            .unwrap();
        cat.add_source(&EntrySource {
            source: Source::Plex,
            source_id: "plex-12345".into(),
            entry_id: "imdb:tt0095016".into(),
            playback_path: "/data/media/movies/Die Hard (1988)/Die.Hard.mkv".into(),
            last_seen: None,
        })
        .unwrap();

        // FS scan reaches the same file under a *different* mount root.
        let files = [file("/mnt/media/movies/Die Hard (1988)/Die.Hard.mkv", 132.0 * 60.0)];
        let stats = ingest_files(&cat, &files, &["/data/media".into(), "/mnt/media".into()]).unwrap();

        // Inherited the Plex entry_id, wrote no new entry, added a second row.
        assert_eq!(stats.inherited, 1);
        assert_eq!(stats.entries_written, 0);
        assert_eq!(cat.all_entry_ids().unwrap(), vec!["imdb:tt0095016".to_string()]);
        let sources = cat.sources_for("imdb:tt0095016").unwrap();
        assert_eq!(sources.len(), 2);
        assert!(sources.iter().any(|s| s.source == Source::LocalFs));
        // Plex metadata untouched — FS did not clobber the title.
        assert_eq!(cat.entry("imdb:tt0095016").unwrap().unwrap().title, "Die Hard");
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
        })
        .unwrap();

        let stats = ingest_files(&cat, &[file(path, 90.0)], &["/data/media".into()]).unwrap();
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
        ingest_files(&cat, &files, &roots).unwrap();
        let first_ids = cat.all_entry_ids().unwrap();
        let first_sources: usize = first_ids.iter().map(|id| cat.sources_for(id).unwrap().len()).sum();

        // Second pass: same files → same rows, now all inherited, no duplication.
        let stats = ingest_files(&cat, &files, &roots).unwrap();
        assert_eq!(stats.inherited, 2);
        assert_eq!(cat.all_entry_ids().unwrap(), first_ids);
        let second_sources: usize = first_ids.iter().map(|id| cat.sources_for(id).unwrap().len()).sum();
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
        ingest_files(&cat, &files, &["/data/media".into(), "/mnt/media".into()]).unwrap();
        // Both canonicalise to `bumpers/x.mkv` → one deterministic fs: entry, one
        // provenance row (same canonical source_id upserts in place).
        assert_eq!(cat.all_entry_ids().unwrap().len(), 1);
        assert_eq!(cat.all_sources().unwrap().len(), 1);
    }

    #[test]
    fn type_from_path_maps_known_dirs() {
        assert_eq!(type_from_path(Path::new("/m/bumpers/a.mp4")), "bumper");
        assert_eq!(type_from_path(Path::new("/m/musicvideos/a.mp4")), "music_video");
        assert_eq!(type_from_path(Path::new("/m/concerts/a.mkv")), "concert");
        assert_eq!(type_from_path(Path::new("/m/whatever/a.mp4")), "video");
    }
}

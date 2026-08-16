//! Catalog ingesters — the units that *populate* the [`super::Catalog`] store
//! (the store itself is persistence-only). Each ingester walks a source (the
//! local filesystem, the Plex API), derives a deterministic `entry_id` via
//! [`super::identity`] with ingest-time **path-match inherit**, and writes
//! `entries` + `entry_sources` (+ external ids / tags) rows.

pub mod fs;
pub mod plex;

use std::collections::HashMap;
use std::path::Path;

use super::identity::canonical_path;
use super::{Catalog, CatalogError};

/// Build a canonical-path → `entry_id` index over every existing provenance row,
/// canonicalising each stored `playback_path` the way an incoming file is. Both
/// ingesters consult it for path-match inherit: a file whose canonical path is
/// already in the catalog reuses that `entry_id` instead of minting a new one.
///
/// When one canonical path resolves to multiple entries — a stale `fs:` entry
/// from an early FS scan plus a Plex/GUID entry — the **stronger** (non-`fs:`)
/// id wins, so the file merges onto the richer record rather than staying split.
pub(crate) fn canonical_index(
    catalog: &Catalog,
    roots: &[&str],
) -> Result<HashMap<String, String>, CatalogError> {
    let mut index: HashMap<String, String> = HashMap::new();
    for source in catalog.all_sources()? {
        let canonical = canonical_path(&source.playback_path, roots);
        match index.get(&canonical) {
            Some(existing) if !existing.starts_with("fs:") => {}
            _ => {
                index.insert(canonical, source.entry_id);
            }
        }
    }
    Ok(index)
}

/// Delete every file directly under `dir` that no current entry's
/// `artwork_cache_path` names (#187) — what keeps the cache bounded by the
/// catalog rather than growing forever. Since ADR 0006 no entry is ever
/// removed, so in practice this only collects images an entry stopped naming
/// (a changed Plex `thumb`); nothing else ever removes a cached file.
///
/// Run once per ingest pass (see `daemon::open_and_ingest_catalog`), after
/// both ingesters have written. A missing `dir` (artwork caching never ran)
/// is not an error — nothing to reconcile. Returns the number of files
/// removed.
pub fn reconcile_artwork_cache(catalog: &Catalog, dir: &Path) -> std::io::Result<usize> {
    let keep = match catalog.all_artwork_cache_paths() {
        Ok(paths) => paths,
        Err(e) => {
            tracing::error!(
                event = "catalog.artwork.reconcile_failed",
                error = %e,
                "could not read artwork_cache_path from the catalog; skipping reconcile",
            );
            return Ok(0);
        }
    };

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };

    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if keep.contains(&name) {
            continue;
        }
        if let Err(e) = std::fs::remove_file(entry.path()) {
            tracing::warn!(
                event = "catalog.artwork.reconcile_remove_failed",
                file = %name,
                error = %e,
                "failed to remove orphaned cached artwork file",
            );
            continue;
        }
        removed += 1;
    }
    Ok(removed)
}

#[cfg(test)]
mod artwork_reconcile_tests {
    use super::*;
    use crate::catalog::model::{Entry, Source};

    #[test]
    fn removes_files_no_entry_references_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.jpg"), b"keep").unwrap();
        std::fs::write(dir.path().join("orphan.jpg"), b"orphan").unwrap();

        let catalog = Catalog::open_in_memory().unwrap();
        let mut entry = Entry::new("imdb:tt1", "movie", "Kept", Source::Plex);
        entry.artwork_cache_path = Some("keep.jpg".into());
        catalog.upsert_entry(&entry).unwrap();
        catalog
            .set_artwork("imdb:tt1", "keep.jpg", "/library/metadata/1/thumb/1")
            .unwrap();

        let removed = reconcile_artwork_cache(&catalog, dir.path()).unwrap();
        assert_eq!(removed, 1);
        assert!(dir.path().join("keep.jpg").exists());
        assert!(!dir.path().join("orphan.jpg").exists());
    }

    #[test]
    fn a_missing_cache_dir_is_not_an_error() {
        let catalog = Catalog::open_in_memory().unwrap();
        let missing = std::path::Path::new("/nonexistent/etv-station-artwork-test-dir");
        assert_eq!(reconcile_artwork_cache(&catalog, missing).unwrap(), 0);
    }
}

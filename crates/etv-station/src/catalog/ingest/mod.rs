//! Catalog ingesters — the units that *populate* the [`super::Catalog`] store
//! (the store itself is persistence-only). Each ingester walks a source (the
//! local filesystem, the Plex API), derives a deterministic `entry_id` via
//! [`super::identity`] with ingest-time **path-match inherit**, and writes
//! `entries` + `entry_sources` (+ external ids / tags) rows.

pub mod fs;
pub mod plex;

use std::collections::HashMap;

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
pub(super) fn canonical_index(
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

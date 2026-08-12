//! Schema-version gate.
//!
//! `plexdb-reader`'s typed accessors are written against one exact schema
//! shape. Rather than let a renamed or missing column surface as a
//! confusing runtime query failure deep inside an accessor, [`check`] reads
//! the store's own `schema_version` row at open time and refuses anything
//! that isn't [`SUPPORTED_SCHEMA_VERSION`] — older *or* newer — naming both
//! versions in the error.
//!
//! Mirrors `plexdb/schema.py::SCHEMA_VERSION`. Bump this constant, and the
//! accessors it backs, in the same change that adds a new migration there.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension};

use crate::error::ReaderError;

/// The schema version this crate's accessors are written against.
///
/// Currently version 7 — `items`, `external_ids`, `plex_items`, `enrichment`,
/// `enrichment_cursor`, `plays`, `plays_ingest_cursor`, `edges`, `collection`,
/// `collection_membership` — see `plexdb/schema.py`.
///
/// Versions 4, 5 and 6 added things this crate does not read: Tautulli's
/// `seconds_watched`/`tautulli_id` on `plays` (issue #9), then `kind` on
/// `external_ids` and `rating_key` on `plays` (issue #23), then the two
/// collection tables (issue #34), whose accessor is issue #29.
///
/// **Version 7 is different — it changed what this crate reads.** Writers'
/// fetch cursors moved out of `enrichment` into `enrichment_cursor`, so
/// `attributes_by_item` dropped the key-prefix filter that had been hiding
/// them (issue #41, ADR-0013). Against a v6 store that query would now return
/// the sentinels as real attributes, which is exactly why the gate demands an
/// exact match rather than a minimum.
pub const SUPPORTED_SCHEMA_VERSION: i64 = 7;

/// Confirm `conn` is a plexdb store at exactly [`SUPPORTED_SCHEMA_VERSION`].
pub(crate) fn check(conn: &Connection, path: &Path) -> Result<(), ReaderError> {
    let has_version_table: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_version'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_version_table {
        return Err(ReaderError::NotAStore {
            path: path.to_path_buf(),
        });
    }

    let store_version: Option<i64> = conn
        .query_row("SELECT version FROM schema_version LIMIT 1", [], |row| {
            row.get(0)
        })
        .optional()?;
    let store_version = store_version.ok_or_else(|| ReaderError::DamagedStore {
        path: path.to_path_buf(),
    })?;

    if store_version != SUPPORTED_SCHEMA_VERSION {
        return Err(ReaderError::UnsupportedSchemaVersion {
            path: path.to_path_buf(),
            store_version,
            supported_version: SUPPORTED_SCHEMA_VERSION,
        });
    }

    Ok(())
}

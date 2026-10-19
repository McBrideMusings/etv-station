//! Read-shape gate.
//!
//! `plexdb-reader`'s typed accessors are written against one read shape: the
//! tables, columns and row meanings they select. Rather than let a renamed or
//! missing column surface as a confusing runtime query failure deep inside an
//! accessor, [`check_store`] reads the store's own `reader_shape` row at open
//! time and refuses anything that isn't [`SUPPORTED_READER_SHAPE`] — older
//! *or* newer — naming both in the error.
//!
//! The gate is on `reader_shape`, not `schema_version`. Most migrations only
//! add tables or columns no accessor reads; those leave `reader_shape` alone,
//! so a reader built against an older schema keeps reading a newer store. A
//! migration that changes what an accessor reads bumps `reader_shape`, and
//! every reader built before it refuses the store rather than misreading it.
//!
//! Mirrors `plexdb/schema.py::READER_SHAPE`. Bump this constant, and the
//! accessors it backs, in the same change that bumps that one.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension};

use crate::error::ReaderError;

/// The read shape this crate's accessors are written against.
///
/// Shape 10 is the store as schema version 10 left it (ADR-0016):
/// `enrichment` and `enrichment_cursor` carry `source` in their primary key,
/// keywords live in the `keywords` namespace, and `keyword_forms` maps raw
/// spellings to normalized forms. Writers' fetch cursors live in
/// `enrichment_cursor`, not `enrichment` (ADR-0013).
///
/// `tests/test_schema.py` fails whenever this constant differs from
/// `plexdb/schema.py::READER_SHAPE`.
pub const SUPPORTED_READER_SHAPE: i64 = 10;

/// Confirm `conn` is a plexdb store whose `reader_shape` is exactly
/// [`SUPPORTED_READER_SHAPE`].
pub fn check_store(conn: &Connection, path: &Path) -> Result<(), ReaderError> {
    if !has_table(conn, "schema_version")? {
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

    if !has_table(conn, "reader_shape")? {
        return Err(ReaderError::NoReaderShape {
            path: path.to_path_buf(),
            store_version,
        });
    }

    let store_shape: Option<i64> = conn
        .query_row("SELECT version FROM reader_shape LIMIT 1", [], |row| {
            row.get(0)
        })
        .optional()?;
    let store_shape = store_shape.ok_or_else(|| ReaderError::DamagedStore {
        path: path.to_path_buf(),
    })?;

    if store_shape != SUPPORTED_READER_SHAPE {
        return Err(ReaderError::UnsupportedReaderShape {
            path: path.to_path_buf(),
            store_shape,
            supported_shape: SUPPORTED_READER_SHAPE,
        });
    }

    Ok(())
}

/// The `frames.db` schema version this crate's search was written against
/// (`plexdb/frames.py::SCHEMA_VERSION`). A frames.db at any other version is
/// refused at open, older or newer.
pub const SUPPORTED_FRAMES_VERSION: i64 = 4;

/// Confirm the attached `frames` schema is a frames.db at exactly
/// [`SUPPORTED_FRAMES_VERSION`].
pub fn check_frames(conn: &Connection, path: &Path) -> Result<(), ReaderError> {
    let has: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM frames.sqlite_master WHERE type = 'table' AND name = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if has.is_none() {
        return Err(ReaderError::NotAFramesFile {
            path: path.to_path_buf(),
        });
    }
    let found: Option<i64> = conn
        .query_row(
            "SELECT version FROM frames.schema_version LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let found = found.ok_or_else(|| ReaderError::NotAFramesFile {
        path: path.to_path_buf(),
    })?;
    if found != SUPPORTED_FRAMES_VERSION {
        return Err(ReaderError::UnsupportedFramesVersion {
            path: path.to_path_buf(),
            found,
            supported: SUPPORTED_FRAMES_VERSION,
        });
    }
    Ok(())
}

fn has_table(conn: &Connection, name: &str) -> Result<bool, ReaderError> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

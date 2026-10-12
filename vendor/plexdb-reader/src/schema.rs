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

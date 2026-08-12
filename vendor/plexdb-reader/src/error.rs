//! `plexdb-reader`'s error type.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ReaderError {
    #[error("failed to open store at {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("{path} has no schema_version table — it is not a plexdb store")]
    NotAStore { path: PathBuf },

    #[error(
        "{path} has a schema_version table but no version row — the store is damaged, not empty"
    )]
    DamagedStore { path: PathBuf },

    #[error(
        "store at {path} is schema version {store_version}, but plexdb-reader only understands \
         version {supported_version} — rebuild plexdb-reader against a matching plex-db-ex, or \
         point it at a store of the version it understands"
    )]
    UnsupportedSchemaVersion {
        path: PathBuf,
        store_version: i64,
        supported_version: i64,
    },

    #[error("store query failed: {0}")]
    Query(#[from] rusqlite::Error),
}

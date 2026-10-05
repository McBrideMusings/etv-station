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
        "{path} has a schema_version or reader_shape table with no row — the store is damaged, \
         not empty"
    )]
    DamagedStore { path: PathBuf },

    #[error(
        "store at {path} is schema version {store_version}, which has no reader_shape table — \
         migrate it with a plex-db-ex at schema version 15 or later"
    )]
    NoReaderShape { path: PathBuf, store_version: i64 },

    #[error(
        "store at {path} has reader shape {store_shape}, but plexdb-reader only understands \
         reader shape {supported_shape} — rebuild plexdb-reader against a matching plex-db-ex, \
         or point it at a store of the shape it understands"
    )]
    UnsupportedReaderShape {
        path: PathBuf,
        store_shape: i64,
        supported_shape: i64,
    },

    #[error("store query failed: {0}")]
    Query(#[from] rusqlite::Error),
}

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

    #[error("failed to attach the Frame index at {path}: {source}")]
    FramesOpen {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("{path} has no schema_version row — it is not a frames.db")]
    NotAFramesFile { path: PathBuf },

    #[error(
        "frames.db at {path} is schema version {found}, but plexdb-reader reads version \
         {supported} — rebuild plexdb-reader against a matching plex-db-ex, or migrate the \
         Frame index with `plexdb frames migrate`"
    )]
    UnsupportedFramesVersion {
        path: PathBuf,
        found: i64,
        supported: i64,
    },

    #[error("the Frame index is not attached — open the Reader with a frames.db path")]
    FramesNotAttached,

    #[error("no Phrase cache was given — open the Reader with a PhraseCache to search")]
    NoPhraseCache,

    #[error("the Phrase cache at {path} failed: {source}")]
    PhraseCache {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("{0}")]
    EncoderUnavailable(String),

    #[error("{0}")]
    EncoderFailed(String),

    #[error(
        "the pool holds titles indexed by more than one model ({0}); a search compares vectors of one model"
    )]
    MixedModels(String),

    #[error("the Frame index is inconsistent for {item_id}: {detail}")]
    InconsistentIndex { item_id: String, detail: String },

    #[error("invalid search: {0}")]
    InvalidSearch(String),
}

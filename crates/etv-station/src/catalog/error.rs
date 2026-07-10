//! Catalog error type.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("failed to open catalog at {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("catalog sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("catalog row has invalid {field}: {message}")]
    BadRow {
        field: &'static str,
        message: String,
    },
}

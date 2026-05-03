use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("invalid config at {path}: {message}")]
    Validation { path: PathBuf, message: String },
}

#[derive(Debug, Error)]
pub enum AtomicWriteError {
    #[error("failed to serialize value to JSON: {0}")]
    Serialize(#[from] serde_json::Error),

    #[error("io error writing {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Error)]
pub enum StationError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(transparent)]
    AtomicWrite(#[from] AtomicWriteError),

    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("ffprobe failed for {path}: {reason}")]
    Ffprobe { path: PathBuf, reason: String },

    #[error("sidecar {path} corrupt: {source}")]
    SidecarCorrupt {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("invalid playout filename {name}: {reason}")]
    BadFilename { name: String, reason: String },

    #[error("invalid timezone {tz}: {reason}")]
    Tz { tz: String, reason: String },

    #[error("item {id} requires {field} but it is missing")]
    MissingField { id: String, field: &'static str },

    #[error("local file not found for item {id}: {path}")]
    MissingLocalFile { id: String, path: PathBuf },

    #[error("task panicked: {0}")]
    Task(String),
}

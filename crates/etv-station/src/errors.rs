use std::path::{Path, PathBuf};

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

    #[error("failed to parse {path}: {source}")]
    ParseYaml {
        path: PathBuf,
        #[source]
        source: serde_norway::Error,
    },

    #[error("invalid config at {path}: {message}")]
    Validation { path: PathBuf, message: String },

    #[error("unsupported config at {path}: {message}")]
    Unsupported { path: PathBuf, message: String },
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

    #[error("catalog error: {0}")]
    Catalog(#[from] crate::catalog::CatalogError),

    #[error("history db error: {0}")]
    History(#[from] crate::history::HistoryError),

    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("ffprobe failed for {path}: {reason}")]
    Ffprobe { path: PathBuf, reason: String },

    /// ffprobe itself could not be run — missing binary, not a missing file.
    /// Deliberately NOT [`StationError::unreadable_media`]: that would read a
    /// broken install as "every file in the library is corrupt" and quietly turn
    /// every channel into wall-to-wall error cards.
    #[error("could not run ffprobe: {reason}")]
    FfprobeUnavailable { reason: String },

    #[error("sidecar {path} corrupt: {source}")]
    SidecarCorrupt {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("playout chunk {path} corrupt: {source}")]
    PlayoutCorrupt {
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

    #[error("local file unreadable for item {id}: {path}: {reason}")]
    MissingLocalFile {
        id: String,
        path: PathBuf,
        /// Why the file could not be stat'd. A deleted file and a dropped SMB
        /// mount are different problems and this is what tells them apart —
        /// both on screen and in the log.
        reason: String,
    },

    #[error(
        "channel {channel} failed earlier in this run and was already reported as channel.failed; the daemon itself ran to shutdown: {reason}"
    )]
    ChannelFailed { channel: String, reason: String },

    #[error("task panicked: {0}")]
    Task(String),

    /// No channel named `name` in the loaded station config — a
    /// `check-determinism` request naming a channel that isn't one of them.
    #[error("no channel named {name:?} in this station config")]
    UnknownChannel { name: String },
}

impl StationError {
    /// One file the player cannot open — a truncated download, a file Plex still
    /// lists after it was deleted — as opposed to a channel that is configured
    /// wrong.
    ///
    /// The distinction is the whole point: a misconfigured channel should stop
    /// and be fixed, but a library of 12,000 films with one bad file in it is a
    /// working channel with one bad slot. Callers use this to keep going.
    /// Returns the offending path and a short human-readable reason.
    pub fn unreadable_media(&self) -> Option<(&Path, &str)> {
        match self {
            StationError::Ffprobe { path, reason } => Some((path, reason.as_str())),
            StationError::MissingLocalFile { path, reason, .. } => Some((path, reason.as_str())),
            // FfprobeUnavailable is deliberately absent: a tool that cannot run
            // says nothing about any particular file.
            _ => None,
        }
    }
}

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StationConfig {
    #[serde(default = "default_tz")]
    pub tz: String,

    /// Base directory every channel's output folder is derived under. A
    /// channel writing playout JSON to `{output_base}/{identity}`, where
    /// `identity` is the channel's `name` override or its config file stem.
    /// Used verbatim relative to the process CWD (the same way the daemon
    /// writes), not resolved against the station config's directory.
    pub output_base: PathBuf,

    /// Channel config references. Each entry is either a literal path or a
    /// glob pattern (e.g. `channels/*.yaml`), resolved relative to the station
    /// config's directory. Globs expand to every matching file; a literal path
    /// that doesn't exist is an error, and a glob matching nothing is an error.
    pub channels: Vec<String>,

    /// Media mount roots, in the daemon's filesystem view. Used to canonicalise
    /// a local item's path when deriving its identity (see
    /// [`crate::catalog::identity::canonical_path`]) so the same file collapses
    /// to one identity regardless of which mount root it is reached under. May
    /// be empty — an empty list only skips root-stripping, leaving identity as a
    /// hash of the separator-normalised full path.
    #[serde(default)]
    pub source_roots: Vec<String>,
}

fn default_tz() -> String {
    "UTC".to_string()
}

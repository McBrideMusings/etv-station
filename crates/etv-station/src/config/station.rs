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
    /// hash of the separator-normalised full path. The daemon also scans these
    /// roots to populate the catalog when `catalog_path` is set.
    #[serde(default)]
    pub source_roots: Vec<String>,

    /// Path to the sqlite catalog the daemon opens and ingests at startup, so
    /// `query` entries and non-`manual` order resolve and manual items path-match
    /// onto catalog identities. Unset (or blank) keeps the catalog-free behavior:
    /// only inline-item, `manual`-order channels resolve. `ETV_STATION_CATALOG`
    /// overrides at runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_path: Option<String>,

    /// How long a freshly ingested catalog is trusted without contacting Plex at
    /// all, in seconds. A restart inside this window reuses the sqlite file as
    /// it stands — the common case when iterating on channel configs, where the
    /// daemon may be restarted many times an hour and the library has not
    /// changed. `0` disables the skip and re-checks Plex on every start.
    #[serde(default = "default_catalog_refresh_secs")]
    pub catalog_refresh_secs: u64,

    /// How long before a delta ingest is escalated to a full re-read, in
    /// seconds. A delta asks Plex only for records touched since the last pass,
    /// which cannot express a deletion — an item removed from the library simply
    /// stops being mentioned. Only a full pass notices those, so one is forced
    /// this often. `0` disables delta ingest entirely: every pass is full.
    #[serde(default = "default_full_sweep_after_secs")]
    pub full_sweep_after_secs: u64,
}

/// 15 minutes: long enough that a restart-heavy editing session pays the ingest
/// once, short enough that a library change shows up without thinking about it.
fn default_catalog_refresh_secs() -> u64 {
    900
}

/// 24 hours. Deletions are the only thing a delta misses, and they are rare and
/// rarely urgent; a daily full pass costs one slow startup.
fn default_full_sweep_after_secs() -> u64 {
    86_400
}

fn default_tz() -> String {
    "UTC".to_string()
}

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct StationConfig {
    #[serde(default = "default_tz")]
    pub tz: String,

    /// How Plex tells this tuner apart from every other one it has seen. Plex
    /// keys a DVR's entire channel mapping on it, so a value that changes
    /// between restarts reads as a brand-new tuner and all of the mapping has
    /// to be redone by hand.
    ///
    /// Leave it unset and one is minted on first run and kept in
    /// `{output_base}/.device_id`, which lives on the data volume and so
    /// survives container recreates and image rebuilds. Set it here and that
    /// value wins — the file is not consulted.
    ///
    /// The generated id cannot live in the rendered `lineup.json`: the
    /// container regenerates that file from this config at every start, so a
    /// value written there would be discarded on the next boot.
    #[serde(default)]
    pub device_id: Option<String>,

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

    /// Filesystem directories the daemon walks to populate the catalog when
    /// `catalog_path` is set — an operational choice about which parts of this
    /// deployment's disk the daemon is allowed to read. Empty (the default)
    /// skips the filesystem scan entirely; a Plex-only station, or one that
    /// hasn't set `catalog_path` at all, leaves it empty and the scan costs
    /// nothing at startup.
    ///
    /// **Not used for identity.** See [`Self::identity_roots`] for the setting
    /// that canonicalises a local item's path — the two used to be one field,
    /// and setting it to fix identity started an unwanted filesystem walk
    /// (#243).
    #[serde(default)]
    pub source_roots: Vec<String>,

    /// Media mount roots, in the daemon's own filesystem view. Used to
    /// canonicalise a local item's path when deriving its identity (see
    /// [`crate::catalog::identity::canonical_path`]) so the same file collapses
    /// to one identity regardless of which mount root it is reached under. May
    /// be empty — an empty list only skips root-stripping, leaving identity as a
    /// hash of the separator-normalised full path, which is portable only while
    /// every consumer of that identity mounts the disk at the same path.
    ///
    /// **Pure string manipulation — touches no disk.** Unlike
    /// [`Self::source_roots`], setting this triggers no filesystem access, so it
    /// is safe to fill in with the real media root even when a full-library scan
    /// is unaffordable at startup. `ETV_STATION_IDENTITY_ROOTS` (colon-separated)
    /// overrides at runtime, matching `source_roots`' own override convention.
    #[serde(default)]
    pub identity_roots: Vec<String>,

    /// Path to the sqlite catalog the daemon opens and ingests at startup, so
    /// `query` entries and non-`manual` order resolve and manual items path-match
    /// onto catalog identities. Unset (or blank) keeps the catalog-free behavior:
    /// only inline-item, `manual`-order channels resolve. `ETV_STATION_CATALOG`
    /// overrides at runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_path: Option<String>,

    /// Directory Plex artwork is cached to at ingest time and ETV-next serves
    /// from over its own HTTP surface (#187) — the only acceptable source for
    /// `<icon src>` in the generated guide, since a Plex artwork URL carries
    /// `X-Plex-Token` as a working credential. Unset (the default) disables
    /// artwork caching entirely: no fetch happens and `<icon>` is omitted from
    /// every programme, the same safe-by-default behavior as before this
    /// existed. `ETV_STATION_ARTWORK_CACHE` overrides at runtime, matching
    /// `catalog_path`'s own override convention.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork_cache_dir: Option<String>,

    /// How long a freshly ingested catalog is trusted without contacting Plex at
    /// all, in seconds. Two things read it, both meaning the same thing:
    ///
    /// - **At startup**, a restart inside this window reuses the sqlite file as
    ///   it stands — the common case when iterating on channel configs, where
    ///   the daemon may be restarted many times an hour and the library has not
    ///   changed.
    /// - **While running**, it is the interval of the catalog refresh: the
    ///   daemon re-ingests this often, so a long-lived process cannot fall out
    ///   of the freshness window and stay there. A rename or a new title shows
    ///   up within one interval instead of at the next restart. The
    ///   reconciliation sweep runs on the same tick, patching playout JSON that
    ///   was written before the change.
    ///
    /// `0` disables both: no skip at startup, and no periodic refresh.
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

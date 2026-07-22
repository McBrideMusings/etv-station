//! The `.resume` sidecar — the only scheduling state a pattern channel persists
//! (#72, consuming the resume-map half of the generation model #70).
//!
//! Generation is a pure function of `(catalog, config, resume_in)`; the resume
//! map is what carries progression across a window seam with **no live cursor**.
//! It records, per pool, where each series left off and which series is up next.
//! Everything else about a window — the ordering, the interleave, the timings —
//! is recomputed, so the map stays tiny and a corrupt or missing one costs at
//! most a restart from the top.
//!
//! Series positions are stored as the **last-played `entry_id`**, never an
//! index: the resolved set churns (a trending list gains and loses shows, new
//! episodes are ingested) and an index silently means something different after
//! any such change, which is exactly the corruption #70 rules out. Resolution
//! looks the id up in the freshly-ordered series and continues after it; an id
//! that has vanished from the catalog restarts that series, and only that one.
//!
//! Pool names are unique per channel (enforced in config validation), so the
//! map keys on the pool name alone and survives blocks being reordered.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::atomic::atomic_write_json;
use crate::errors::StationError;

const SIDECAR_NAME: &str = ".resume";

/// Bumped only if the on-disk shape changes incompatibly. A file whose version
/// this binary doesn't know is discarded, not guessed at — see [`load`].
const CURRENT_VERSION: u32 = 1;

/// Where every pool in a channel picks up next. `BTreeMap` throughout so the
/// serialized bytes are stable for a given state, which keeps the regeneration
/// tests byte-comparable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeMap {
    #[serde(default = "current_version")]
    pub version: u32,
    #[serde(default)]
    pub pools: BTreeMap<String, PoolResume>,
}

/// One pool's resume state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolResume {
    /// The series key whose turn is next in the rotation. `None` starts at the
    /// first series of the freshly-resolved set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,

    /// series key → last-played `entry_id`. The series key is the catalog
    /// `show_id` for an episode; for an item with no `show_id` (a movie) it is
    /// the item's own `entry_id`, so a movie pool is a rotation of one-item
    /// series and needs no special case.
    #[serde(default)]
    pub cursor: BTreeMap<String, String>,

    /// Series that have run out under `wrap = "drop"`. They stay out of the
    /// rotation until new content puts them back in the resolved set — a key
    /// listed here that no longer resolves is simply ignored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped: Vec<String>,
}

fn current_version() -> u32 {
    CURRENT_VERSION
}

impl ResumeMap {
    pub fn new() -> Self {
        Self {
            version: CURRENT_VERSION,
            pools: BTreeMap::new(),
        }
    }

    pub fn pool(&self, name: &str) -> Option<&PoolResume> {
        self.pools.get(name)
    }

    pub fn is_empty(&self) -> bool {
        self.pools.is_empty()
    }
}

pub fn sidecar_path(output_folder: &Path) -> PathBuf {
    output_folder.join(SIDECAR_NAME)
}

/// Read the sidecar, or an empty map if there is none.
///
/// A file that is missing, unparseable, or written by a future version yields
/// an empty map rather than an error: resume state is an optimisation over
/// "start from the top", and refusing to start a channel because a progress
/// note went bad would trade a cosmetic restart for dead air. Both recoveries
/// are logged by the caller.
pub async fn load(output_folder: &Path) -> Result<(ResumeMap, ResumeLoad), StationError> {
    let path = sidecar_path(output_folder);
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((ResumeMap::new(), ResumeLoad::Fresh));
        }
        Err(source) => return Err(StationError::Io { path, source }),
    };

    match serde_json::from_slice::<ResumeMap>(&bytes) {
        Ok(map) if map.version == CURRENT_VERSION => Ok((map, ResumeLoad::Loaded)),
        Ok(map) => Ok((
            ResumeMap::new(),
            ResumeLoad::Discarded(format!(
                "sidecar version {} is not {CURRENT_VERSION}",
                map.version
            )),
        )),
        Err(e) => Ok((ResumeMap::new(), ResumeLoad::Discarded(e.to_string()))),
    }
}

/// How [`load`] arrived at the map it returned, so the daemon can log the
/// difference between a first run and a recovered-from-garbage one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeLoad {
    /// No sidecar yet — first generation for this channel.
    Fresh,
    /// Read from disk.
    Loaded,
    /// Present but unusable; starting over. Carries the reason for the log.
    Discarded(String),
}

/// Write the map at the window seam. Atomic, so a crash mid-write leaves the
/// previous map intact rather than a truncated one.
pub async fn save(output_folder: &Path, map: &ResumeMap) -> Result<(), StationError> {
    tokio::fs::create_dir_all(output_folder)
        .await
        .map_err(|source| StationError::Io {
            path: output_folder.to_path_buf(),
            source,
        })?;
    atomic_write_json(&sidecar_path(output_folder), map).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample() -> ResumeMap {
        let mut map = ResumeMap::new();
        let mut shows = PoolResume {
            next: Some("show:invincible".into()),
            ..Default::default()
        };
        shows.cursor.insert("show:got".into(), "plex:ep-1-3".into());
        shows
            .cursor
            .insert("show:invincible".into(), "plex:ep-2-6".into());
        map.pools.insert("shows".into(), shows);
        map
    }

    #[tokio::test]
    async fn missing_sidecar_reads_as_fresh() {
        let dir = tempdir().unwrap();
        let (map, how) = load(dir.path()).await.unwrap();
        assert!(map.is_empty());
        assert_eq!(how, ResumeLoad::Fresh);
    }

    #[tokio::test]
    async fn round_trips_through_disk() {
        let dir = tempdir().unwrap();
        save(dir.path(), &sample()).await.unwrap();
        let (map, how) = load(dir.path()).await.unwrap();
        assert_eq!(how, ResumeLoad::Loaded);
        assert_eq!(map, sample());
        let pool = map.pool("shows").unwrap();
        assert_eq!(pool.next.as_deref(), Some("show:invincible"));
        assert_eq!(pool.cursor.get("show:got").unwrap(), "plex:ep-1-3");
    }

    #[tokio::test]
    async fn corrupt_sidecar_starts_over_instead_of_failing() {
        let dir = tempdir().unwrap();
        tokio::fs::write(sidecar_path(dir.path()), b"{not json")
            .await
            .unwrap();
        let (map, how) = load(dir.path()).await.unwrap();
        assert!(map.is_empty());
        assert!(matches!(how, ResumeLoad::Discarded(_)));
    }

    #[tokio::test]
    async fn future_version_is_discarded_not_misread() {
        let dir = tempdir().unwrap();
        tokio::fs::write(
            sidecar_path(dir.path()),
            br#"{"version":99,"pools":{"shows":{"cursor":{}}}}"#,
        )
        .await
        .unwrap();
        let (map, how) = load(dir.path()).await.unwrap();
        assert!(map.is_empty());
        assert!(matches!(how, ResumeLoad::Discarded(_)));
    }

    #[tokio::test]
    async fn serialized_bytes_are_stable_for_a_given_state() {
        let dir1 = tempdir().unwrap();
        let dir2 = tempdir().unwrap();
        save(dir1.path(), &sample()).await.unwrap();
        save(dir2.path(), &sample()).await.unwrap();
        let a = tokio::fs::read(sidecar_path(dir1.path())).await.unwrap();
        let b = tokio::fs::read(sidecar_path(dir2.path())).await.unwrap();
        assert_eq!(a, b);
    }
}

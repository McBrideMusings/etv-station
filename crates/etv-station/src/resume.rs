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
//! **Where each series left off is not here** — that is the play-history
//! ledger's job (#70, [`crate::history`]), and the cursor is a projection of
//! it. This sidecar holds only what the ledger cannot express: whose turn the
//! rotation is on, and the checkpoints that make a rewind possible. Keeping the position in exactly one place is the point;
//! two stores of "where are we" is the drift #70 exists to prevent.
//!
//! Pool names are unique per channel (enforced in config validation), so the
//! map keys on the pool name alone and survives blocks being reordered.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

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

    /// Where each not-yet-aired generation *started* from, newest last.
    ///
    /// Forward materialization otherwise makes a pattern channel's emitted
    /// future permanent: nothing rewrites it, so a config or overlay edit would
    /// only take effect once the already-written window had fully aired (the
    /// #53 sharp edge, made worse by never wiping). These checkpoints are the
    /// way back — each records the pool state immediately *before* the
    /// generation that begins at `start`, so a channel can throw away its
    /// unaired chunks, rewind to the matching pool state, and regenerate from
    /// the current config without losing or repeating a single item.
    ///
    /// Only future entries are worth keeping; [`prune_elapsed`] drops the rest,
    /// which bounds the list to the generations covering one window.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checkpoints: Vec<Checkpoint>,
}

/// The pool state immediately before the generation that starts at `start`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    #[serde(with = "time::serde::rfc3339")]
    pub start: OffsetDateTime,
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
}

fn current_version() -> u32 {
    CURRENT_VERSION
}

impl ResumeMap {
    pub fn new() -> Self {
        Self {
            version: CURRENT_VERSION,
            pools: BTreeMap::new(),
            checkpoints: Vec::new(),
        }
    }

    pub fn pool(&self, name: &str) -> Option<&PoolResume> {
        self.pools.get(name)
    }

    pub fn is_empty(&self) -> bool {
        self.pools.is_empty()
    }

    /// Record the state entering a generation that begins at `start`.
    pub fn checkpoint(&mut self, start: OffsetDateTime) {
        self.checkpoints.push(Checkpoint {
            start,
            pools: self.pools.clone(),
        });
    }

    /// Drop checkpoints for generations that have already begun airing — their
    /// content is a record now, not something to regenerate.
    pub fn prune_elapsed(&mut self, now: OffsetDateTime) {
        self.checkpoints.retain(|c| c.start > now);
    }

    /// Rewind to the earliest generation that has not started airing: returns
    /// the instant to re-emit from, having restored the pool state as it was
    /// before that generation ran. `None` when nothing is regenerable, in which
    /// case the map is untouched.
    ///
    /// This is what makes a config or overlay edit take effect on a pattern
    /// channel: the caller deletes the emitted files at or after the returned
    /// instant and generates the same span again from the current config.
    pub fn rewind_to_unaired(&mut self, now: OffsetDateTime) -> Option<OffsetDateTime> {
        self.prune_elapsed(now);
        let earliest = self.checkpoints.first()?.clone();
        self.pools = earliest.pools;
        // Everything from here forward is about to be regenerated, so its
        // checkpoints are re-recorded as it goes.
        self.checkpoints.clear();
        Some(earliest.start)
    }
}

/// Everything one generation is handed about where the channel stands.
///
/// Two inputs from two places, deliberately: `resume` is this sidecar (rotation
/// and drop state), and `cursor` is projected from the play-history ledger
/// (`series_key -> last-played entry_id`). Bundling them keeps the resolver's
/// signature honest about needing both without implying they are one store.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GenerationState {
    pub resume: ResumeMap,
    pub cursor: BTreeMap<String, String>,
}

impl GenerationState {
    /// The empty state — every pool starts from the top.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Whether anything has ever played on this channel. Used to tell a pattern
    /// channel that has run out of content apart from one that never had any.
    pub fn is_fresh(&self) -> bool {
        self.resume.is_empty() && self.cursor.is_empty()
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
    use time::macros::datetime;

    fn sample() -> ResumeMap {
        let mut map = ResumeMap::new();
        let shows = PoolResume {
            next: Some("show:invincible".into()),
        };
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

    // ---- checkpoints -------------------------------------------------------

    fn at(hour: u8) -> OffsetDateTime {
        datetime!(2026-04-13 00:00 UTC) + time::Duration::hours(hour as i64)
    }

    /// A distinguishable pool state — the rotation position is what this
    /// sidecar actually stores, so checkpoints are told apart by it.
    fn pools_with(next_show: &str) -> BTreeMap<String, PoolResume> {
        let pool = PoolResume {
            next: Some(next_show.into()),
        };
        BTreeMap::from([("shows".to_string(), pool)])
    }

    #[test]
    fn rewind_restores_the_state_before_the_earliest_unaired_generation() {
        let mut map = ResumeMap::new();
        // Three generations recorded, the first already airing by `now`.
        map.pools = pools_with("e0");
        map.checkpoint(at(0));
        map.pools = pools_with("e1");
        map.checkpoint(at(6));
        map.pools = pools_with("e2");
        map.checkpoint(at(12));
        map.pools = pools_with("e3");

        // At hour 8, the 06:00 generation has started — the 12:00 one has not.
        let regen = map.rewind_to_unaired(at(8)).unwrap();
        assert_eq!(regen, at(12));
        assert_eq!(
            map.pools,
            pools_with("e2"),
            "pools must be exactly what they were entering the 12:00 generation"
        );
        assert!(
            map.checkpoints.is_empty(),
            "regenerated spans re-record their own checkpoints"
        );
    }

    #[test]
    fn rewind_is_a_no_op_when_nothing_is_unaired() {
        let mut map = ResumeMap::new();
        map.pools = pools_with("e0");
        map.checkpoint(at(0));
        map.pools = pools_with("e1");

        // Everything already airing: nothing to regenerate, state untouched.
        assert!(map.rewind_to_unaired(at(9)).is_none());
        assert_eq!(map.pools, pools_with("e1"));
    }

    #[test]
    fn rewind_on_a_fresh_map_does_nothing() {
        let mut map = ResumeMap::new();
        assert!(map.rewind_to_unaired(at(1)).is_none());
        assert!(map.is_empty());
    }

    #[test]
    fn prune_keeps_only_future_checkpoints() {
        let mut map = ResumeMap::new();
        map.checkpoint(at(0));
        map.checkpoint(at(6));
        map.checkpoint(at(12));
        map.prune_elapsed(at(7));
        assert_eq!(map.checkpoints.len(), 1);
        assert_eq!(map.checkpoints[0].start, at(12));
    }

    #[tokio::test]
    async fn checkpoints_survive_the_sidecar_round_trip() {
        let dir = tempdir().unwrap();
        let mut map = sample();
        map.checkpoint(at(12));
        save(dir.path(), &map).await.unwrap();
        let (loaded, _) = load(dir.path()).await.unwrap();
        assert_eq!(loaded, map);
        assert_eq!(loaded.checkpoints[0].start, at(12));
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

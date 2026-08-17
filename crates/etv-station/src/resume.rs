//! The `.resume` sidecar — the only scheduling state a channel persists
//! (#72, consuming the resume-map half of the generation model #70).
//!
//! Generation is a pure function of `(catalog, config, resume_in)`; the resume
//! map is what carries progression across a window seam with **no live cursor**.
//! It records, per pool, which series is up next — and, for a channel that is a
//! flat authored list rather than a pattern, how far into that list the next
//! generation starts (#118). Everything else about a window — the ordering, the
//! interleave, the timings — is recomputed, so the map stays tiny and a corrupt
//! or missing one costs at most a restart from the top.
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

    /// How far into a channel's entries list the next generation starts
    /// (#118). For a channel with no pattern block, that list is every block
    /// concatenated in config order. A channel that mixes a pattern block
    /// with exactly one entries block (#146) instead counts over that one
    /// block's own list — its pattern block(s) bound themselves separately,
    /// through the pool resume state above, not through this field. Mixing a
    /// pattern block with *several* entries blocks is unsupported rather than
    /// guessing what a fused cursor across them should mean (#190).
    ///
    /// A channel with no pattern block has no pools and no rotation, so before
    /// this it persisted nothing at all: every generation re-resolved the same
    /// authored list and laid it from the top. That was invisible only because
    /// one generation emitted the whole list — a 950-item channel booked a month
    /// of playout in one pass, and an edit to its config took a month to reach
    /// the screen. A generation now stops at the window, and this is what the
    /// next one continues from.
    ///
    /// It is a position into an **authored** list, not a second copy of a
    /// derived one: the entries are written in the config file, so "item 37"
    /// means the same thing next tick. Nothing about where a *series* left off
    /// lives here — that is still the ledger's, exactly as the header says.
    #[serde(default, skip_serializing_if = "is_start")]
    pub position: usize,

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

    /// A hash of the inputs that produced the unaired window currently on
    /// disk — the channel's resolved candidate entry-id list, its config and
    /// overlay config bytes, and the resume state entering the earliest
    /// unaired generation (#182). `None` on a sidecar written before this
    /// field existed, or whenever it could not be computed; both decode the
    /// same as a mismatch, so a missing fingerprint always regenerates
    /// rather than skipping on a guess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

/// The scheduling state immediately before the generation that starts at
/// `start` — the pool rotation, and a flat channel's list position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    #[serde(with = "time::serde::rfc3339")]
    pub start: OffsetDateTime,
    #[serde(default)]
    pub pools: BTreeMap<String, PoolResume>,
    /// Where the authored list stood entering that generation. Restored by the
    /// rewinds below, so a config edit re-emits the same items rather than
    /// jumping the cursor past a span it just threw away.
    #[serde(default, skip_serializing_if = "is_start")]
    pub position: usize,
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

/// A list position of zero is the top, which is also what an absent field
/// means, so it is left out of the written bytes.
fn is_start(position: &usize) -> bool {
    *position == 0
}

impl ResumeMap {
    pub fn new() -> Self {
        Self {
            version: CURRENT_VERSION,
            pools: BTreeMap::new(),
            position: 0,
            checkpoints: Vec::new(),
            fingerprint: None,
        }
    }

    pub fn pool(&self, name: &str) -> Option<&PoolResume> {
        self.pools.get(name)
    }

    pub fn is_empty(&self) -> bool {
        self.pools.is_empty() && self.position == 0
    }

    /// Record the state entering a generation that begins at `start`.
    pub fn checkpoint(&mut self, start: OffsetDateTime) {
        self.checkpoints.push(Checkpoint {
            start,
            pools: self.pools.clone(),
            position: self.position,
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
        self.position = earliest.position;
        // Everything from here forward is about to be regenerated, so its
        // checkpoints are re-recorded as it goes.
        self.checkpoints.clear();
        Some(earliest.start)
    }

    /// Non-mutating counterpart to [`rewind_to_unaired`]: the start, pool
    /// state, and list position of the earliest unaired checkpoint, without
    /// restoring or clearing anything (#182). `rewind_to_unaired` always
    /// commits to its rewind, so this is what lets a caller compute a
    /// candidate fingerprint against the state a rewind *would* restore
    /// before deciding whether to actually run one.
    ///
    /// Elapsed checkpoints are skipped exactly as [`prune_elapsed`] would drop
    /// them, but nothing is removed — the checkpoints list a caller sees
    /// after this call is identical to the one before it. Correct without
    /// pruning first because [`checkpoint`] always pushes in chronological
    /// order, so the first checkpoint whose `start` is still in the future
    /// is the same one pruning-then-`.first()` would find.
    pub fn peek_unaired(
        &self,
        now: OffsetDateTime,
    ) -> Option<(OffsetDateTime, BTreeMap<String, PoolResume>, usize)> {
        self.checkpoints
            .iter()
            .find(|c| c.start > now)
            .map(|c| (c.start, c.pools.clone(), c.position))
    }

    /// Rewind to the generation that was airing at `instant`: restore the pool
    /// state recorded before it and drop it (and every later checkpoint), so the
    /// span from `instant` on can be regenerated. Returns that generation's start
    /// instant, or `None` when no checkpoint covers `instant` — its record was
    /// pruned after it aired, so its pool state is gone and the caller must
    /// regenerate from the current pools instead (a possible seam glitch, never
    /// black).
    ///
    /// Distinct from [`rewind_to_unaired`], which always rewinds to the *earliest*
    /// unaired generation to apply a config edit. This targets a specific
    /// instant — the start of an observed coverage hole — and rewinds only as far
    /// as that hole, leaving healthy earlier chunks in place.
    pub fn rewind_to(&mut self, instant: OffsetDateTime) -> Option<OffsetDateTime> {
        let idx = self.checkpoints.iter().rposition(|c| c.start <= instant)?;
        let cp = self.checkpoints[idx].clone();
        self.pools = cp.pools;
        self.position = cp.position;
        self.checkpoints.truncate(idx);
        Some(cp.start)
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
    /// The most recently aired entry ids, oldest first — the adjacency seam the
    /// `no_repeat_within` pass reads so it does not repeat across a generation
    /// boundary (#73). Projected from the same ledger as `cursor`.
    pub tail: Vec<String>,
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
    fn rewind_to_targets_the_generation_covering_an_instant() {
        let mut map = ResumeMap::new();
        map.pools = pools_with("e0");
        map.checkpoint(at(0));
        map.pools = pools_with("e1");
        map.checkpoint(at(6));
        map.pools = pools_with("e2");
        map.checkpoint(at(12));
        map.pools = pools_with("e3");

        // A hole at hour 8 lives inside the 06:00 generation: rewind to it,
        // restoring the pools recorded before it, and drop it and everything
        // later so they regenerate. The 00:00 checkpoint (healthy, earlier) stays.
        let regen = map.rewind_to(at(8)).unwrap();
        assert_eq!(regen, at(6));
        assert_eq!(map.pools, pools_with("e1"));
        assert_eq!(map.checkpoints.len(), 1);
        assert_eq!(map.checkpoints[0].start, at(0));
    }

    #[test]
    fn rewind_to_is_none_when_the_instant_predates_every_checkpoint() {
        let mut map = ResumeMap::new();
        map.pools = pools_with("e1");
        map.checkpoint(at(6));
        map.pools = pools_with("e2");

        // The covering checkpoint was pruned after airing — its pool state is
        // gone, so the caller must regenerate from the current pools.
        assert!(map.rewind_to(at(3)).is_none());
        assert_eq!(map.pools, pools_with("e2"), "state left untouched");
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

    // ---- a flat channel's list position (#118) -----------------------------

    #[tokio::test]
    async fn the_list_position_round_trips_through_disk() {
        let dir = tempdir().unwrap();
        let mut map = ResumeMap::new();
        map.position = 37;
        save(dir.path(), &map).await.unwrap();
        let (loaded, how) = load(dir.path()).await.unwrap();
        assert_eq!(how, ResumeLoad::Loaded);
        assert_eq!(loaded.position, 37);
        assert!(!loaded.is_empty(), "a channel 37 items in is not fresh");
    }

    #[tokio::test]
    async fn a_missing_sidecar_puts_a_flat_channel_at_the_top() {
        let dir = tempdir().unwrap();
        let (map, _) = load(dir.path()).await.unwrap();
        assert_eq!(map.position, 0);
    }

    /// A config edit rewinds to the earliest unaired generation and re-emits it.
    /// The list position has to come back with the pools, or the regenerated
    /// span would start where the thrown-away one *finished* and skip a day.
    #[test]
    fn rewinding_restores_the_list_position_with_the_pools() {
        let mut map = ResumeMap::new();
        map.position = 10;
        map.checkpoint(at(0));
        map.position = 40;
        map.checkpoint(at(6));
        map.position = 75;

        assert_eq!(map.rewind_to_unaired(at(3)).unwrap(), at(6));
        assert_eq!(map.position, 40);
    }

    #[test]
    fn rewinding_to_an_instant_restores_the_list_position() {
        let mut map = ResumeMap::new();
        map.position = 10;
        map.checkpoint(at(0));
        map.position = 40;
        map.checkpoint(at(6));
        map.position = 75;

        assert_eq!(map.rewind_to(at(8)).unwrap(), at(6));
        assert_eq!(map.position, 40);
    }

    // ---- fingerprint (#182) -------------------------------------------------

    #[test]
    fn peek_unaired_returns_the_earliest_unaired_checkpoint_without_mutating() {
        let mut map = ResumeMap::new();
        map.pools = pools_with("e0");
        map.checkpoint(at(0));
        map.pools = pools_with("e1");
        map.checkpoint(at(6));
        map.pools = pools_with("e2");
        map.checkpoint(at(12));
        map.pools = pools_with("e3");
        let before = map.clone();

        let (start, pools, position) = map.peek_unaired(at(8)).unwrap();
        assert_eq!(start, at(12));
        assert_eq!(pools, pools_with("e2"));
        assert_eq!(position, 0);
        assert_eq!(map, before, "peeking must not mutate the map");
    }

    #[test]
    fn peek_unaired_is_none_when_nothing_is_unaired() {
        let mut map = ResumeMap::new();
        map.pools = pools_with("e0");
        map.checkpoint(at(0));
        assert!(map.peek_unaired(at(9)).is_none());
    }

    #[tokio::test]
    async fn a_missing_fingerprint_decodes_as_none() {
        let dir = tempdir().unwrap();
        save(dir.path(), &sample()).await.unwrap();
        let (map, _) = load(dir.path()).await.unwrap();
        assert_eq!(map.fingerprint, None);
    }

    #[tokio::test]
    async fn the_fingerprint_round_trips_through_disk() {
        let dir = tempdir().unwrap();
        let mut map = sample();
        map.fingerprint = Some("deadbeef".to_string());
        save(dir.path(), &map).await.unwrap();
        let (loaded, _) = load(dir.path()).await.unwrap();
        assert_eq!(loaded.fingerprint.as_deref(), Some("deadbeef"));
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

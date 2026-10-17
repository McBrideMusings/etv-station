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

    /// The same position for each entries block in `rule.slots`, by slot
    /// name (ADR 0004). A slot airs only while a schedule places it, so its
    /// list continues from here the next time it comes on rather than
    /// sharing the any-time list's `position`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub slot_positions: BTreeMap<String, usize>,

    /// The scheduling state at each not-yet-aired item boundary, oldest first.
    ///
    /// Forward materialization otherwise makes a pattern channel's emitted
    /// future permanent: nothing rewrites it, so a config or overlay edit would
    /// only take effect once the already-written window had fully aired (the
    /// #53 sharp edge, made worse by never wiping). These checkpoints are the
    /// way back — each records the state that holds if everything before
    /// `start` airs and nothing after it does, so a channel can throw away its
    /// unaired schedule, rewind to the matching state, and regenerate from the
    /// current config without losing or repeating a single item.
    ///
    /// One per emitted item, not one per generation. A wipe cuts at an item
    /// boundary that can sit anywhere inside a generation — it spares the chunk
    /// holding a boundary-straddling item, and a refresh cuts after the item on
    /// screen — so a per-generation record rewound past content still on disk,
    /// and the next generation re-aired it (#378).
    ///
    /// Only future entries are worth keeping; [`prune_elapsed`] drops the rest,
    /// which bounds the list to the items covering one window.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checkpoints: Vec<Checkpoint>,

    /// The channel configuration the written future was generated under — a
    /// hash of the channel's config and its resolved overlay, and nothing else.
    ///
    /// This is what decides whether an edit reaches the screen. When it differs
    /// from the channel as it stands now, the daemon throws away the schedule
    /// from the next item boundary forward and regenerates it under the new
    /// rules; when it matches, the written future is left alone.
    ///
    /// **Deliberately not the catalog.** The candidate entry-id list moves every
    /// time Plex gains or loses a title, and rewriting the future on that would
    /// mean the taste-scored channels reshuffle continuously — for a reason that
    /// has nothing to do with what anybody configured. A new title reaches the
    /// screen as the window extends, which is soon enough.
    ///
    /// **Nor the watch history or the clock.** Generation is a pure function of
    /// its inputs, but two of those inputs move on their own: a scorer plugin
    /// reads `ctx.history` and `ctx.now`. So "nothing changed, therefore
    /// regenerating is a no-op" holds for a plain query channel and does not
    /// hold for a taste-scored one, which is exactly why this gate is narrow.
    ///
    /// `None` on a channel that has never generated, which regenerates rather
    /// than guessing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_id: Option<String>,

    /// Each rotating plugin pool's ranked lists, keyed by pool name — the
    /// list a pool walks for a whole period, ranked once when the period
    /// began (see [`FrozenList`]). A list stays while the current pool state
    /// or any surviving checkpoint names its key, so a rewind regenerates
    /// from the list it walked; [`Self::prune_lists`] drops the rest.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub lists: BTreeMap<String, Vec<FrozenList>>,
}

/// A plugin pool's ranked list for one period: what `pick()` returned when
/// the period began, held so every later generation in the period walks the
/// same list instead of a re-ranking that has moved under it.
///
/// It has to be stored rather than recomputed. A plugin's ranking reads inputs
/// that move as the channel plays — `ctx.recent`, the watch history — so asking
/// again mid-period returns a different list, and a walk resumed by position in
/// a list that reordered itself skips titles and replays others.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrozenList {
    /// [`crate::variety::Dice::list_key`] — the seed, temperature, both
    /// periods and the pool's own config this list was ranked under.
    pub key: String,
    /// The entry ids `pick()` returned, in its order, plus anything a refill
    /// appended later in the period.
    pub ids: Vec<String>,
    /// Each id's `metadata` / take override, as `pick()` attached them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extras: BTreeMap<String, FrozenExtra>,
}

/// One id's [`crate::score::PickedExtra`], in a shape the sidecar can hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrozenExtra {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub take: Option<usize>,
}

/// The scheduling state at the item boundary `start` — the pool rotation, and
/// a flat channel's list position.
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub slot_positions: BTreeMap<String, usize>,
}

/// The scheduling state a generation would leave behind had it stopped after
/// some prefix of its items — what the resolver reports per item so the daemon
/// can checkpoint every item boundary rather than only the generation's start.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    pub pools: BTreeMap<String, PoolResume>,
    pub position: usize,
    pub slot_positions: BTreeMap<String, usize>,
}

/// One pool's resume state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolResume {
    /// The series key whose turn is next in the rotation. `None` starts at the
    /// first series of the freshly-resolved set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// How many pattern cycles this pool's block has walked before this point,
    /// across every generation. The pattern walk keys its random rolls on it,
    /// so a pinned `seed` draws a fresh sequence each generation instead of
    /// replaying the first cycle's draws every time (etv-station-pwzv).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cycle: u64,
    /// The pattern step of `cycle` the next generation opens on. Zero except
    /// where a generation stopped at a slot or window boundary partway through
    /// a cycle: the cycle's earlier steps already aired and the rotation has
    /// moved past them, so the next generation carries on from this step
    /// rather than replaying them.
    #[serde(default, skip_serializing_if = "is_zero_step")]
    pub step: usize,
    /// [`crate::pattern::pattern_id`] of the pattern `step` was recorded
    /// under. `step` indexes into that pattern as written, so a generation
    /// honours it only when the pattern it walks has the same id; any edit to
    /// the pattern opens the cycle at step 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// [`crate::variety::Dice::list_key`] of the list `next` points into, or
    /// `None` when nothing about this pool rotates. A different key next time
    /// means a rollover: `next` belongs to a list that no longer exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<String>,
    /// The series this pool has visited since its walk last covered every
    /// series in its list — the current lap. At a rollover the walk opens on
    /// the first series of the new list that is not in here, so a list that
    /// barely changed carries on where it was instead of replaying its top,
    /// and a list that changed completely opens on its top.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lap: Vec<String>,
    /// The ids already drawn from the list named by `list`, across every
    /// generation of its period. A generation that opens in the same period
    /// starts from these, so a frozen list that has been walked to its end
    /// asks its source for more rather than wrapping to its top. Empty for a
    /// pool whose list does not rotate, and ignored when `list` differs from
    /// the list the next generation holds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drawn: Vec<String>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

fn is_zero_step(n: &usize) -> bool {
    *n == 0
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
            slot_positions: BTreeMap::new(),
            checkpoints: Vec::new(),
            config_id: None,
            lists: BTreeMap::new(),
        }
    }

    pub fn pool(&self, name: &str) -> Option<&PoolResume> {
        self.pools.get(name)
    }

    pub fn is_empty(&self) -> bool {
        self.pools.is_empty() && self.position == 0 && self.slot_positions.is_empty()
    }

    /// Record the state entering a generation that begins at `start`, along
    /// with the input fingerprint that produced it (#182) — `None` when it
    /// could not be computed, which always regenerates rather than skipping
    /// on a guess.
    pub fn checkpoint(&mut self, start: OffsetDateTime) {
        self.checkpoints.push(Checkpoint {
            start,
            pools: self.pools.clone(),
            position: self.position,
            slot_positions: self.slot_positions.clone(),
        });
    }

    /// Record the state at the item boundary `start`, inside a generation that
    /// already checkpointed its own start. Skipped when it matches the latest
    /// checkpoint: a rewind landing here would restore the same state from that
    /// one, so the duplicate only grows the sidecar.
    pub fn checkpoint_at(&mut self, start: OffsetDateTime, progress: &Progress) {
        if let Some(last) = self.checkpoints.last()
            && (last.start >= start
                || (last.pools == progress.pools
                    && last.position == progress.position
                    && last.slot_positions == progress.slot_positions))
        {
            return;
        }
        self.checkpoints.push(Checkpoint {
            start,
            pools: progress.pools.clone(),
            position: progress.position,
            slot_positions: progress.slot_positions.clone(),
        });
    }

    /// Drop every stored list nothing can walk again: keep a pool's list only
    /// while the current pool state or a surviving checkpoint names its key.
    /// Run after [`Self::prune_elapsed`], so a list lives exactly as long as a
    /// rewind could land on it.
    pub fn prune_lists(&mut self) {
        let mut wanted: std::collections::BTreeSet<(&str, &str)> = Default::default();
        let states = std::iter::once(&self.pools).chain(self.checkpoints.iter().map(|c| &c.pools));
        for pools in states {
            for (name, pool) in pools {
                if let Some(key) = &pool.list {
                    wanted.insert((name.as_str(), key.as_str()));
                }
            }
        }
        let wanted: std::collections::BTreeSet<(String, String)> = wanted
            .into_iter()
            .map(|(n, k)| (n.to_string(), k.to_string()))
            .collect();
        for (name, kept) in &mut self.lists {
            kept.retain(|l| wanted.contains(&(name.clone(), l.key.clone())));
        }
        self.lists.retain(|_, kept| !kept.is_empty());
    }

    /// Drop checkpoints for item boundaries that have already aired — their
    /// content is a record now, not something to regenerate.
    pub fn prune_elapsed(&mut self, now: OffsetDateTime) {
        self.checkpoints.retain(|c| c.start > now);
    }

    /// Rewind to the latest checkpoint at or before `instant`: restore the
    /// state recorded there and drop it (and every later checkpoint), so the
    /// span from `instant` on can be regenerated. Returns that checkpoint's
    /// instant, or `None` when none covers `instant` — its record was pruned
    /// after it aired, so its state is gone and the caller must regenerate
    /// from the current pools instead (a possible seam glitch, never black).
    ///
    /// `instant` must be where the surviving schedule actually ends, not where
    /// a wipe began: a wipe spares whole items past its own cut, and rewinding
    /// to the cut re-airs them.
    pub fn rewind_to(&mut self, instant: OffsetDateTime) -> Option<OffsetDateTime> {
        let idx = self.checkpoints.iter().rposition(|c| c.start <= instant)?;
        let cp = self.checkpoints[idx].clone();
        self.pools = cp.pools;
        self.position = cp.position;
        self.slot_positions = cp.slot_positions;
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
    /// `show_id -> last-played entry_id` across every channel, read by pools
    /// declaring `cursor_scope: global`. Projected from the same ledger as
    /// `cursor`, just not narrowed to this channel (#160).
    pub global_cursor: BTreeMap<String, String>,
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
            ..Default::default()
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
            ..Default::default()
        };
        BTreeMap::from([("shows".to_string(), pool)])
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

    /// Item checkpoints inside a generation land where a partial wipe can cut,
    /// and one equal to the latest adds nothing a rewind would not already
    /// restore from that one.
    #[test]
    fn item_checkpoints_rewind_to_the_boundary_a_wipe_left() {
        let mut map = ResumeMap::new();
        map.pools = pools_with("e0");
        map.checkpoint(at(0));
        let step = |next: &str, position| Progress {
            pools: pools_with(next),
            position,
            slot_positions: BTreeMap::new(),
        };
        map.checkpoint_at(at(1), &step("e1", 1));
        map.checkpoint_at(at(2), &step("e1", 1));
        map.checkpoint_at(at(3), &step("e2", 2));
        map.checkpoint_at(at(3), &step("e3", 3));
        assert_eq!(
            map.checkpoints.iter().map(|c| c.start).collect::<Vec<_>>(),
            vec![at(0), at(1), at(3)],
            "an unchanged state and a non-advancing instant add no checkpoint",
        );

        // The surviving schedule ends at hour 2: the state that held there is
        // the one recorded at hour 1, not the generation's start.
        assert_eq!(map.rewind_to(at(2)), Some(at(1)));
        assert_eq!(map.pools, pools_with("e1"));
        assert_eq!(map.position, 1);
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

    /// A stored list lives exactly as long as something can walk it again:
    /// the current pool state or a surviving checkpoint naming its key.
    #[test]
    fn prune_lists_keeps_only_lists_a_rewind_can_reach() {
        let list = |key: &str| FrozenList {
            key: key.into(),
            ids: vec![],
            extras: BTreeMap::new(),
        };
        let on = |key: &str| {
            BTreeMap::from([(
                "films".to_string(),
                PoolResume {
                    list: Some(key.into()),
                    ..Default::default()
                },
            )])
        };
        let mut map = ResumeMap::new();
        map.lists.insert(
            "films".into(),
            vec![list("p1"), list("p2"), list("p3"), list("p4")],
        );
        map.lists.insert("gone".into(), vec![list("p1")]);
        map.pools = on("p2");
        map.checkpoint(at(6));
        map.pools = on("p4");
        map.prune_lists();
        let kept: Vec<&str> = map.lists["films"].iter().map(|l| l.key.as_str()).collect();
        assert_eq!(
            kept,
            ["p2", "p4"],
            "the checkpoint's list and the current one"
        );
        assert!(
            !map.lists.contains_key("gone"),
            "a pool nothing names is dropped"
        );
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

    /// The config identity is what decides whether an edit reaches the screen,
    /// so it has to survive the sidecar round trip. A channel that has never
    /// generated has none, and that decodes as "regenerate" rather than as a
    /// match against nothing.
    #[tokio::test]
    async fn the_config_id_round_trips_through_disk() {
        let dir = tempdir().unwrap();
        let mut map = sample();
        assert_eq!(map.config_id, None, "a fresh map claims no configuration");
        map.config_id = Some("deadbeef".into());
        save(dir.path(), &map).await.unwrap();
        let (loaded, _) = load(dir.path()).await.unwrap();
        assert_eq!(loaded.config_id.as_deref(), Some("deadbeef"));
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

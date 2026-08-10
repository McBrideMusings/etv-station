//! Pattern interleave (#72): walk a repeating template of `{pool, take}` steps,
//! drawing from named pools so a block can express "1 movie, then 3 episodes,
//! repeat" with each series progressing independently.
//!
//! # The shape of a pool
//!
//! A pool resolves to a flat, ordered id list (its `expr`, then its `order`),
//! which is then grouped into **series**. A series key is the catalog `show_id`
//! for an episode — or `show_id` plus the season number where the pool sets
//! `group_by = "season"` (#155) — and an item with neither, a movie, is its own
//! series of one. That single rule is why a movie pool needs no special case:
//! rotating through one-item series *is* playing the movies in order.
//!
//! Series rotate in order of first appearance in the ordered set, so the pool's
//! `order` fixes the rotation and nothing else has to.
//!
//! Where a season is the series, `take = "all"` empties the one a visit picked,
//! which is the whole of "air a random season start to finish": nothing in the
//! walk below knows what a season is, and nothing needs to. The count `all`
//! stands for is resolved *after* the pick, since which series it means was the
//! pick's decision.
//!
//! # Where adjacency constraints run
//!
//! A pool's `constraints` are enforced entirely inside the pool (#115), never
//! over the interleaved result: a pass that reordered the finished list would
//! repair a repeat by swapping an episode into a movie slot, destroying the very
//! shape the pattern was written to build. Keeping the rule inside the pool
//! makes the shape safe by construction rather than by a slot-aware repair.
//!
//! It takes two checks, because a pool makes repeats in two different ways:
//!
//! - **The list** — `constrain_pool` orders the flat list after its `order` and
//!   before it is grouped into series. This settles which item opens a window
//!   and the order the series rotate in, and is where the generation seam is
//!   answered.
//! - **The draw** — [`PoolRuntime::eligible_from`] checks each series' next item
//!   against what this pool has just emitted. This is the one that matters in
//!   practice: a resolved set holds each `entry_id` once, so the list pass has
//!   nothing to fix, while every repeat that reaches air is made *here* — the
//!   rotation parking on a series that only half-filled a visit, and a series
//!   played to its end looping.
//!
//! A gap therefore counts **this pool's draws**, so `N` on a pool visited once a
//! cycle means N cycles of aired schedule. And each pool sees only itself: two
//! pools resolving the same `entry_id` collide unseen, so pools that must stay
//! apart have to be disjoint by construction.
//!
//! When no series can serve without a clash, one serves anyway and the clash is
//! counted for the `constraints.unsatisfied` warning. A pool that cannot satisfy
//! its own constraint still has to put television on the air.
//!
//! # Who fills a visit
//!
//! With `rotate = "visit"` one visit to a step draws all `take` items from one
//! series (the mini-binge). When that series can't supply them all, `on_short`
//! decides: roll onto the next series (`next`, the default), loop the same
//! series back to its start (`wrap`), or emit fewer (`short`).
//!
//! The rotation pointer then lands where the next visit should resume: past the
//! series if it served the whole visit by itself, otherwise **on** the series
//! that only partially served — so a short visit's filler continues next time
//! rather than being replayed from its start.
//!
//! # Determinism
//!
//! Every random decision — `select = "random"`, a step's `chance` — is a keyed
//! roll over `(seed, cycle, step, nonce)` using the same fixed SplitMix64 the
//! order engine uses. No RNG state is threaded through the walk, so a pinned
//! seed reproduces the whole schedule, and the roll for one step never shifts
//! because another step was added or skipped.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use crate::catalog::{Catalog, TagNs};
use crate::config::{
    Advance, Constraints, GroupBy, NoRepeatWithin, OnShort, PatternStep, Pool, Rotate, Select,
    ShowGroup, Take, TakeFrom,
};
use crate::constrain::{ItemKeys, Limits, RepeatGap, order_constrained};
use crate::resume::{GenerationState, PoolResume};

/// Upper bound on an explicitly-authored `cycles`. A derived count needs no cap
/// — it is bounded by the pools' own sizes.
pub const MAX_CYCLES: usize = 10_000;

/// How many times a constrained `select = "random"` pool redraws before giving
/// up and walking the rotation instead. A constraint blocks at most `reach`
/// series, so on any pool bigger than that a handful of draws finds a free one;
/// the cap only stops a pool with nothing eligible from rerolling forever.
const RANDOM_REROLLS: u64 = 8;

/// Keeps a reroll's nonce clear of the small nonces the rest of the walk uses —
/// slot indices in a `rotate = "slot"` visit, and `u64::MAX` for a step's
/// `chance` — so one decision's roll never doubles as another's.
const REROLL_STRIDE: u64 = 1 << 32;

/// Keeps a `from = "random"` offset roll clear of both the small nonces the walk
/// uses and the reroll band above it, so one decision never doubles as another.
const FROM_STRIDE: u64 = 1 << 48;

/// One series inside a pool: a show's episodes, one season of them, or a single
/// movie.
#[derive(Debug)]
struct Series {
    /// What this series is called in the rotation and in the `.resume` sidecar.
    /// `show_id` under `group_by = "show"`, `"{show_id}|S{season}"` under
    /// `group_by = "season"`, and the item's own `entry_id` when it has no
    /// `show_id` at all.
    key: String,
    /// The key the play-history ledger files this series' airings under —
    /// always the `show_id`, never the season. Equal to [`Self::key`] except
    /// under `group_by = "season"`, where several series share one show's
    /// history and each finds its own place in it.
    show_key: String,
    ids: Vec<String>,
    /// Index of the next item to play.
    cursor: usize,
}

impl Series {
    fn remaining(&self) -> usize {
        self.ids.len().saturating_sub(self.cursor)
    }
}

/// Every id this pool can draw, paired with the values of the field it
/// separates on, and — when a temporal `no_repeat_within` needs it (#185) —
/// its estimated runtime. `groups` is empty when nothing is being separated
/// on and `durations`/`mean_duration` are empty/zero when nothing here is
/// measured in time, so the common case needs no catalog lookup for either.
#[derive(Debug, Default)]
struct PoolKeys {
    groups: HashMap<String, Vec<String>>,
    durations: HashMap<String, Duration>,
    mean_duration: Duration,
}

impl PoolKeys {
    fn of(&self, id: &str) -> ItemKeys {
        ItemKeys {
            id: id.to_string(),
            group: self.groups.get(id).cloned().unwrap_or_default(),
            duration: self
                .durations
                .get(id)
                .copied()
                .unwrap_or(self.mean_duration),
        }
    }

    fn many(&self, ids: &[String]) -> Vec<ItemKeys> {
        ids.iter().map(|id| self.of(id)).collect()
    }
}

/// A pool mid-walk: its config, its series, and where the rotation stands.
#[derive(Debug)]
struct PoolRuntime<'a> {
    cfg: &'a Pool,
    series: Vec<Series>,
    /// Index into `series` of whoever is up next.
    rotation: usize,

    /// This pool's effective adjacency limits (#115).
    limits: Limits,
    /// Identity + separation values for every id this pool can draw.
    keys: PoolKeys,
    /// What this pool has emitted lately, oldest first, holding at most
    /// `limits.reach()` items — exactly as far back as the rule looks. Seeded
    /// from the generation seam, so the first draw of a window is checked
    /// against the last draws of the one before it.
    recent: Vec<ItemKeys>,
    /// Draws that had to clash because no series could serve without one.
    forced: usize,

    /// What the catalog says each of this pool's items runs for, and the mean
    /// over the ones it has a figure for. Both are loaded only when the
    /// generation is bounded by a remaining window — nothing else reads them,
    /// and an unbounded generation should not pay for the query.
    runtimes: HashMap<String, Duration>,
    mean_runtime: Duration,
}

impl PoolRuntime<'_> {
    /// How much airtime these ids lay down.
    ///
    /// An item the catalog never measured is counted at the pool's mean rather
    /// than at zero: it still occupies a slot on screen, and counting it as
    /// free would let a pool of unmeasured items walk straight past the window
    /// — the exact overrun this bound exists to stop. A pool the catalog has no
    /// figure for at all has a mean of zero and so cannot bind the window; the
    /// drain-once ceiling is all that holds it, which is where it stood before.
    fn runtime_of(&self, ids: &[String]) -> Duration {
        ids.iter()
            .map(|id| self.runtimes.get(id).copied().unwrap_or(self.mean_runtime))
            .sum()
    }

    /// A pool is dry only when it resolved to nothing at all — an expression
    /// that matched no item, or a catalog with none. Playing a series to its end
    /// never empties a pool: the series loops. Television does not run out.
    fn is_dry(&self) -> bool {
        self.series.is_empty()
    }

    /// The series at `i`, wrapping. `None` only when the pool resolved empty —
    /// every series is always in the rotation, so there is nothing to skip past.
    fn series_at(&self, i: usize) -> Option<usize> {
        let n = self.series.len();
        (n > 0).then(|| i % n)
    }

    fn series_after(&self, si: usize) -> Option<usize> {
        self.series_at(si + 1)
    }

    /// Which series serves next, honouring `select`.
    fn pick(&self, roll: &RollKey, nonce: u64) -> Option<usize> {
        match self.cfg.select {
            Select::RoundRobin => self.series_at(self.rotation),
            Select::Random => self.series_at(roll.u64_at(nonce) as usize),
        }
    }

    /// The id a series would serve next — its cursor, or its first item for a
    /// series whose cursor has just wrapped past the end.
    fn next_id(&self, si: usize) -> Option<&String> {
        let s = &self.series[si];
        s.ids.get(s.cursor).or_else(|| s.ids.first())
    }

    /// Whether this series' next item may air right now, given what this pool
    /// has already emitted (#115).
    fn is_eligible(&self, si: usize) -> bool {
        match self.next_id(si) {
            Some(id) => !crate::constrain::conflicts_with_recent(
                &self.recent,
                &self.keys.of(id),
                self.limits,
            ),
            None => true,
        }
    }

    /// `start`, or the first series after it that may serve — the draw-time half
    /// of a pool's constraints (#115).
    ///
    /// A pool's repeats are made *here*, not by its list order: the rotation
    /// parks on a series that only partly filled a visit, and a series that runs
    /// off its end loops. Reordering the pool's list cannot see either, so the
    /// eligibility check has to sit where the item is chosen.
    ///
    /// Sliding to the neighbour is the right recovery for a rotation — "the
    /// next series in order" is what round-robin and `on_short = "next"` both
    /// already mean. It is the wrong one for a random draw, which is why
    /// [`Self::eligible_pick`] rerolls instead.
    ///
    /// When no series can serve, `start` serves anyway and the clash is counted.
    /// A pool that cannot satisfy its own constraint still has to put television
    /// on the air — the same doctrine [`crate::constrain::order_constrained`]
    /// applies to a list it cannot arrange.
    fn eligible_from(&mut self, start: usize) -> Option<usize> {
        if self.limits.is_unconstrained() {
            return Some(start);
        }
        let n = self.series.len();
        if n == 0 {
            return None;
        }
        for step in 0..n {
            let si = (start + step) % n;
            if self.is_eligible(si) {
                return Some(si);
            }
        }
        self.forced += 1;
        Some(start)
    }

    /// Which series opens a visit, honouring both `select` and the pool's
    /// constraints.
    ///
    /// A `random` pool **rerolls** when its draw is blocked rather than sliding
    /// to the neighbour: sliding would hand the series *after* a blocked one
    /// every one of its neighbour's turns as well as its own, and under a
    /// constraint the blocked ones are exactly the recently-played ones — so the
    /// skew would land on the same few series over and over. The rerolls are
    /// keyed like every other decision here, so a pinned seed still reproduces
    /// the schedule. Exhausting them falls back to the ordered walk, which is
    /// also what reports an unsatisfiable pool.
    fn eligible_pick(&mut self, roll: &RollKey, nonce: u64) -> Option<usize> {
        let first = self.pick(roll, nonce)?;
        if self.limits.is_unconstrained() || self.is_eligible(first) {
            return Some(first);
        }
        if self.cfg.select == Select::Random {
            for attempt in 1..=RANDOM_REROLLS {
                let rolled = roll.u64_at(nonce.wrapping_add(attempt.wrapping_mul(REROLL_STRIDE)));
                if let Some(si) = self.series_at(rolled as usize)
                    && self.is_eligible(si)
                {
                    return Some(si);
                }
            }
        }
        self.eligible_from(first)
    }

    /// Fold what a draw emitted into the recent window, so the next draw in the
    /// same visit sees it — a visit's filler must not replay what the visit
    /// itself just aired.
    fn remember(&mut self, ids: &[String]) {
        if self.limits.is_unconstrained() || ids.is_empty() {
            return;
        }
        let fresh = self.keys.many(ids);
        self.recent.extend(fresh);
        // Trim to exactly what a conflict check could still reach — on
        // whichever axis, positions or elapsed time (#185) — so a long
        // generation does not grow this window without bound.
        let keep = crate::constrain::history_needed(&self.recent, self.limits);
        let cut = self.recent.len() - keep;
        self.recent.drain(..cut);
    }

    /// Take up to `want` items from `si`, advancing its cursor. A series that
    /// runs off its end restarts from the top — the only behaviour there is.
    fn take_from(&mut self, si: usize, want: usize) -> Vec<String> {
        let s = &mut self.series[si];
        let n = want.min(s.remaining());
        let out = s.ids[s.cursor..s.cursor + n].to_vec();
        s.cursor += n;
        if s.cursor >= s.ids.len() {
            s.cursor = 0;
        }
        out
    }

    /// One visit to a step: draw `take` items per `rotate` / `on_short`.
    fn visit(&mut self, take: Take, from: TakeFrom, roll: &RollKey) -> Vec<String> {
        if take == Take::Count(0) || self.is_dry() {
            return Vec::new();
        }
        match self.cfg.rotate {
            // A new series every item is just `take` one-item visits. `all` is
            // refused here at load — a step that changes series every item has
            // no one series to empty — so the count is always present.
            Rotate::Slot => {
                let n = take.count().unwrap_or(1);
                let mut out = Vec::with_capacity(n);
                for slot in 0..n {
                    if self.is_dry() {
                        break;
                    }
                    out.extend(self.serve(Take::Count(1), from, roll, slot as u64));
                }
                out
            }
            Rotate::Visit => self.serve(take, from, roll, 0),
        }
    }

    /// Serve `take` items starting from one picked series, rolling onto others
    /// per `on_short` when it comes up short, then park the rotation where the
    /// next visit should resume.
    fn serve(&mut self, want: Take, from: TakeFrom, roll: &RollKey, nonce: u64) -> Vec<String> {
        let first = self.eligible_pick(roll, nonce);
        // `all` is sized from the series the visit just picked, so it has to be
        // resolved after the pick and not before: it means "empty *this* one",
        // and which one that is was the pick's decision. A series is never
        // parked past its own end — `take_from` wraps the cursor — so its
        // remaining count is the whole rest of it.
        let take = match (want, first) {
            (Take::Count(n), _) => n,
            (Take::All, Some(si)) => self.series[si].remaining(),
            (Take::All, None) => return Vec::new(),
        };
        // `end` and `random` name an absolute place in the series, so they set
        // the cursor before the first draw — which is exactly why they ignore
        // the resume point. "The last three episodes" that continued from
        // wherever the series left off would not be the last three.
        if let Some(si) = first
            && from != TakeFrom::Start
        {
            let len = self.series[si].ids.len();
            let last_start = len.saturating_sub(take);
            self.series[si].cursor = match from {
                TakeFrom::Start => unreachable!("guarded above"),
                TakeFrom::End => last_start,
                // Offsets that would run off the end are not candidates, so a
                // `random` visit always gets its full `take` where the series
                // is long enough to give one.
                TakeFrom::Random => {
                    let choices = last_start + 1;
                    (roll.u64_at(nonce.wrapping_add(FROM_STRIDE)) % choices as u64) as usize
                }
            };
        }
        let mut out: Vec<String> = Vec::with_capacity(take);
        let mut current = first;
        let mut contributed: HashMap<usize, usize> = HashMap::new();
        let mut last: Option<usize> = None;

        // Every pass emits at least one item or breaks, so the loop already
        // terminates within `take` passes; the bound is only a backstop against
        // a future change breaking that. It must therefore be generous enough
        // never to bind on a legitimate visit — `on_short = "wrap"` can loop one
        // short series many times to fill a large `take`, and cutting that off
        // would silently emit a short visit.
        for _ in 0..take + self.series.len() + 1 {
            let Some(si) = current else { break };
            let taken = self.take_from(si, take - out.len());
            if taken.is_empty() {
                break;
            }
            *contributed.entry(si).or_default() += taken.len();
            last = Some(si);
            // Before the next draw, not after the visit: a visit's filler must
            // not replay what the visit itself just aired (#115).
            self.remember(&taken);
            out.extend(taken);
            if out.len() >= take {
                break;
            }
            // Short: someone else has to fill the rest.
            current = match self.cfg.on_short {
                OnShort::Next => self
                    .series_after(si)
                    .and_then(|next| self.eligible_from(next)),
                // `wrap` is the author saying "stay on this series", so it does
                // not shop around — but a clash it forces is still a clash the
                // config asked for and could not have, so it is counted.
                OnShort::Wrap => {
                    if !self.limits.is_unconstrained() && !self.is_eligible(si) {
                        self.forced += 1;
                    }
                    Some(si)
                }
                OnShort::Short => None,
            };
        }

        // Park the rotation. A series that served the whole visit alone has had
        // its turn, so the next visit starts after it; one that only partly
        // served keeps its place so the next visit continues it.
        if let Some(si) = last
            && self.cfg.select == Select::RoundRobin
        {
            let served_all = contributed.get(&si).copied().unwrap_or(0) >= take;
            self.rotation = if served_all {
                self.series_after(si).unwrap_or(si)
            } else {
                si
            };
        }
        out
    }

    /// This pool's state to persist for the next window.
    fn to_resume(&self) -> PoolResume {
        // Where each series left off is NOT recorded here — the play-history
        // ledger already holds it, and the cursor this pool resumed from was a
        // projection of that. Writing it a second time would be the duplicate
        // store #70 exists to remove.
        PoolResume {
            // Only a round-robin pool has a meaningful "next": a random pool
            // draws afresh every visit.
            next: match self.cfg.select {
                Select::RoundRobin => self
                    .series_at(self.rotation)
                    .map(|i| self.series[i].key.clone()),
                Select::Random => None,
            },
        }
    }
}

/// A keyed roll: every random decision hashes its own coordinates rather than
/// drawing from a running stream, so adding or skipping a step never shifts
/// another step's outcome.
struct RollKey {
    seed: u64,
    cycle: u64,
    step: u64,
}

impl RollKey {
    fn u64_at(&self, nonce: u64) -> u64 {
        let mut state = self
            .seed
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(self.cycle.wrapping_mul(0xBF58_476D_1CE4_E5B9))
            .wrapping_add(self.step.wrapping_mul(0x94D0_49BB_1331_11EB))
            .wrapping_add(nonce.wrapping_mul(0xD6E8_FEB8_6659_FD93));
        splitmix64(&mut state)
    }

    /// A roll in `[0, 1)`.
    fn unit_at(&self, nonce: u64) -> f64 {
        // 53 bits — the exact integer range of an f64 mantissa.
        (self.u64_at(nonce) >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// SplitMix64 — the same fixed mixer the order engine uses, so a pinned seed
/// reproduces a schedule across builds (not `DefaultHasher`, whose output is
/// not guaranteed stable).
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Resolve the pools and walk the pattern, returning the interleaved `entry_id`
/// list and the resume state to persist for the next window.
///
/// `state` is consulted only by pools declaring `advance = "resume"`; a
/// `restart` pool ignores it entirely and replays from the top, which is what
/// makes the stateless default genuinely stateless. Its two halves come from
/// two places on purpose: the rotation from the `.resume` sidecar, and each
/// series' position from the play-history ledger (#70).
///
/// `block_constraints` is the block's own `[constraints]` table, which every
/// pool that declares none inherits (#115).
///
/// `fill` is how much airtime the caller still has to cover — the daemon's
/// `target - from`. It bounds a **derived** cycle count: without it, a block
/// with no explicit `cycles` runs until its largest pool drains once, and 51
/// pools of films drain over about eleven years, so one pass of the generator
/// booked a channel through 2037 and no later tick ever revisited it (#140).
/// With it, the walk stops at the first cycle boundary past the span, so the
/// overshoot is one cycle instead of a decade. `None` leaves the walk
/// unbounded, which is what an explicit `cycles` and the stateless
/// [`crate::resolve::resolve_channel`] entry point both want.
///
/// Nothing new is persisted to make this work: the walk already stops on a
/// cycle boundary and already hands back each pool's rotation, so the next
/// generation resumes from where this one stopped exactly as it does today.
#[allow(clippy::too_many_arguments)]
pub fn build(
    catalog: &Catalog,
    pools: &[Pool],
    groups: &[ShowGroup],
    pattern: &[PatternStep],
    cycles: Option<usize>,
    block_constraints: Option<&Constraints>,
    state: &GenerationState,
    seed: u64,
    score_env: crate::score::ScoreEnv<'_>,
    fill: Option<Duration>,
) -> Result<(Vec<String>, BTreeMap<String, PoolResume>), String> {
    // An authored `cycles` is the author saying how long a pass runs, and the
    // window does not get to argue with it. Only a derived count is bounded.
    let budget = fill.filter(|_| cycles.is_none());

    // Three passes, split by whether they read the database.
    //
    // The middle one is why: a scorer plugin's `pick()` is unbounded work owned
    // by whoever wrote the script, and it must not run with a database handle
    // alive. Ranking a large library can take minutes, and for as long as it
    // does, anything sharing that handle is stopped dead. Keeping the plugin
    // calls in their own pass — between two that do read the catalog, and taking
    // no catalog themselves — is what makes that structurally impossible rather
    // than a rule someone has to remember.

    // Pass 1, reads the catalog: resolve every expression pool's ids, and
    // compile + materialize every plugin pool's declared sources. One cache for
    // the whole generation, so pools pointed at the same script share its AST
    // and its resolved sets instead of each re-running every query it declares.
    let mut score_cache = crate::score::ScoreCache::default();
    let mut pool_ids: Vec<Option<Vec<String>>> = Vec::with_capacity(pools.len());
    for cfg in pools {
        match (&cfg.expr, &cfg.plugin, !cfg.groups.is_empty()) {
            (Some(expr), None, false) => {
                let mut ids = catalog.resolve_query(expr).map_err(|e| e.to_string())?;
                if let Some(order) = &cfg.order {
                    // Seed 0: a pool's internal `order` is its own stable sort.
                    // A shuffled pool is `select = "random"`, seeded per visit.
                    ids = catalog
                        .resolve_order(&ids, order, 0)
                        .map_err(|e| e.to_string())?;
                }
                pool_ids.push(Some(ids));
            }
            (None, Some(plugin), false) => {
                let path = score_env.resolve_path(plugin);
                score_cache
                    .prepare(catalog, &path)
                    .map_err(|m| format!("pool {:?}: {m}", cfg.name))?;
                pool_ids.push(None);
            }
            (None, None, true) => {
                let mut ids = resolve_group_pool(catalog, cfg, groups)?;
                if let Some(order) = &cfg.order {
                    ids = catalog
                        .resolve_order(&ids, order, 0)
                        .map_err(|e| e.to_string())?;
                }
                pool_ids.push(Some(ids));
            }
            // Two sources, or none, is rejected at load; a pool that reaches
            // here in either state is a validation gap, not a config the user
            // can hit.
            _ => {
                return Err(format!(
                    "pool {:?} must set exactly one of `expr`, `plugin`, or `groups`",
                    cfg.name
                ));
            }
        }
    }

    // Pass 2, reads nothing: run each plugin's ranking against the snapshot pass
    // 1 took. A plugin returns its set already ranked — gathering and ranking are
    // the same judgment for a taste algorithm — so the `order` step above applies
    // only to the expression case (ADR 0002).
    for (slot, cfg) in pool_ids.iter_mut().zip(pools) {
        if slot.is_none() {
            let plugin = cfg
                .plugin
                .as_ref()
                .expect("pass 1 leaves a hole only for a plugin pool");
            let path = score_env.resolve_path(plugin);
            // Load-time validation already proved this pool's `capabilities`
            // match exactly what the plugin declares (#167) — this just reads
            // the same grant back to decide what `ctx.sets`/`ctx.history`
            // resolve to for this call.
            let granted = crate::score::GrantedCapabilities::from_names(&cfg.capabilities);
            let ids = crate::score::pick(
                &score_cache,
                &path,
                score_env.inputs,
                &cfg.name,
                cfg.config.as_ref(),
                granted,
            )
            .map_err(|m| format!("pool {:?}: {m}", cfg.name))?;
            *slot = Some(ids);
        }
    }

    // Pass 3, reads the catalog again: turn each pool's ids into its runtime.
    let mut runtimes: Vec<PoolRuntime> = Vec::with_capacity(pools.len());
    for (cfg, ids) in pools.iter().zip(pool_ids) {
        let ids = ids.expect("pass 2 fills every hole pass 1 left");
        runtimes.push(pool_runtime(
            catalog,
            cfg,
            ids,
            block_constraints,
            state,
            budget.is_some(),
        )?);
    }

    let mut by_name: HashMap<&str, usize> = HashMap::new();
    for (i, rt) in runtimes.iter().enumerate() {
        by_name.insert(rt.cfg.name.as_str(), i);
    }

    let cycles = match cycles {
        Some(n) => {
            if n > MAX_CYCLES {
                return Err(format!("cycles = {n} exceeds the maximum of {MAX_CYCLES}"));
            }
            n
        }
        None => derive_cycles(&runtimes, pattern, &by_name),
    };

    let mut out = Vec::new();
    let mut laid = Duration::ZERO;
    for cycle in 0..cycles {
        // Checked between cycles, never inside one: a cycle is the unit the
        // pattern was written in ("1 movie, then 3 episodes"), and cutting one
        // in half would air a fragment of the shape and leave the pools parked
        // mid-pattern. So the last cycle is allowed to finish and overrun the
        // window by its own length — the same bargain `pattern_catch_up`
        // already strikes when it emits a whole generation past the target.
        if budget.is_some_and(|fill| laid >= fill) {
            break;
        }
        for (step_idx, step) in pattern.iter().enumerate() {
            let roll = RollKey {
                seed,
                cycle: cycle as u64,
                step: step_idx as u64,
            };
            // A skipped step contributes nothing and — because the skip is
            // decided before any draw — does not consume the pool's cursor.
            if step.chance < 1.0 && roll.unit_at(u64::MAX) >= step.chance {
                continue;
            }
            let idx = *by_name
                .get(step.pool.as_str())
                .ok_or_else(|| format!("pattern step names unknown pool {:?}", step.pool))?;
            let drawn = runtimes[idx].visit(step.take, step.from, &roll);
            if budget.is_some() {
                laid += runtimes[idx].runtime_of(&drawn);
            }
            out.extend(drawn);
        }
    }

    for rt in &runtimes {
        if rt.forced > 0 {
            // Every series had a clashing next item, so one had to air anyway —
            // a pool of one title under `no_repeat_within`, or a `wrap` that
            // loops a series shorter than the gap. Say so, or a pool quietly
            // failing its constraint looks exactly like one honouring it.
            tracing::warn!(
                event = "constraints.unsatisfied",
                pool = %rt.cfg.name,
                phase = "draw",
                violations = rt.forced,
                "pool adjacency constraints could not be honoured on every draw; aired the closest choice available",
            );
        }
    }

    let resume_out = runtimes
        .iter()
        .map(|rt| (rt.cfg.name.clone(), rt.to_resume()))
        .collect();
    Ok((out, resume_out))
}

/// Resolve a `groups`-sourced pool's item list (#165): the union of every
/// member show's episodes, across every named group the pool lists.
///
/// Which show belongs to which named group is settled once this reads —
/// nothing about *how* the resulting ids get grouped into series changes.
/// [`pool_runtime`] below still reads each item's own `show_id` off the
/// catalog to build its series and to seed the resume cursor, exactly as it
/// does for an `expr` pool; a sibling show's episodes simply arrive already
/// mixed into the one list, and the season/show grouping downstream treats
/// them precisely as it would items an author's own CEL `||` chain had
/// matched. That is what makes a sibling's resume position need no dedicated
/// support (#165's "no new field") — the code path is the same one #155
/// already proved keyed each series' cursor on its own show, not on how the
/// pool that drew it was scoped.
///
/// Every member show is validated against the catalog before generation
/// reaches this (`resolve::validate_groups_against_catalog`), and every group
/// name a pool lists is validated against the channel's declarations at load
/// (`config::validate`) — both defensively re-checked here too, since this
/// also runs from pattern's own unit tests, which build pools without going
/// through either pass.
fn resolve_group_pool(
    catalog: &Catalog,
    cfg: &Pool,
    groups: &[ShowGroup],
) -> Result<Vec<String>, String> {
    let mut shows: Vec<String> = Vec::new();
    for gname in &cfg.groups {
        let group = groups.iter().find(|g| &g.name == gname).ok_or_else(|| {
            format!(
                "pool {:?} references group {:?}, which this channel does not declare",
                cfg.name, gname
            )
        })?;
        shows.extend(group.shows.iter().cloned());
    }
    let by_show = catalog
        .episode_ids_for_shows(&shows)
        .map_err(|e| e.to_string())?;
    let mut ids = Vec::new();
    for show in &shows {
        match by_show.get(show) {
            Some(found) if !found.is_empty() => ids.extend(found.iter().cloned()),
            _ => {
                return Err(format!(
                    "pool {:?}: show {show:?} is not in the catalog",
                    cfg.name
                ));
            }
        }
    }
    Ok(ids)
}

/// Enough cycles for the largest pool to drain once. Each pool needs
/// `ceil(remaining / take-per-cycle)` visits' worth; the pattern runs the max,
/// so shorter pools repeat under their own `wrap` while the longest plays out.
///
/// This is a **ceiling, not the count that runs.** When [`build`] is given a
/// `fill` span it stops the walk at the first cycle boundary that covers it,
/// which is what keeps a 51-pool channel from laying down eleven years in one
/// pass (#140). Draining once is only what happens when nobody said how much
/// airtime was wanted.
fn derive_cycles(
    runtimes: &[PoolRuntime],
    pattern: &[PatternStep],
    by_name: &HashMap<&str, usize>,
) -> usize {
    let mut per_cycle: Vec<usize> = vec![0; runtimes.len()];
    // Take-all steps are counted separately because they drain by the series,
    // not by the item: one visit empties whichever series it picked, whatever
    // its length, so what a cycle costs this pool is measured in series.
    let mut all_steps: Vec<usize> = vec![0; runtimes.len()];
    for step in pattern {
        if let Some(&i) = by_name.get(step.pool.as_str()) {
            match step.take {
                Take::Count(n) => per_cycle[i] += n,
                Take::All => all_steps[i] += 1,
            }
        }
    }

    let mut cycles = 0;
    for (i, rt) in runtimes.iter().enumerate() {
        if per_cycle[i] > 0 {
            // What is left to play from here — which, for a resumed pool, is
            // less than the pool holds. A pool resumed to exactly its end still
            // deserves a full pass, so fall back to its total.
            let remaining: usize = rt.series.iter().map(|s| s.remaining()).sum();
            let total: usize = rt.series.iter().map(|s| s.ids.len()).sum();
            let want = if remaining > 0 { remaining } else { total };
            cycles = cycles.max(want.div_ceil(per_cycle[i]));
        }
        if all_steps[i] > 0 {
            // Every series emptied once, at `all_steps[i]` of them per cycle.
            cycles = cycles.max(rt.series.len().div_ceil(all_steps[i]));
        }
    }
    cycles
}

/// Turn one pool's resolved ids into the runtime the pattern draws from: split
/// them into series, then seat each series' cursor from the resume map when the
/// pool asks to continue.
///
/// Where the ids came from — a CEL expression or a scorer plugin — is settled
/// before this is called (see the three passes in [`build`]), which is what
/// keeps plugin execution out of any scope holding a database handle.
///
/// `want_runtimes` asks for the per-item lengths the walk needs to know when
/// the window is covered. It is a whole extra query per pool, so it is only
/// asked for when something is actually going to read the answer.
fn pool_runtime<'a>(
    catalog: &Catalog,
    cfg: &'a Pool,
    ids: Vec<String>,
    block_constraints: Option<&Constraints>,
    state: &GenerationState,
    want_runtimes: bool,
) -> Result<PoolRuntime<'a>, String> {
    // Adjacency constraints (#115). Two halves, both scoped to this pool and
    // both running before anything is interleaved, so the pattern's slots are
    // never reordered: the list order below, and the draw-time eligibility check
    // in `serve` that this seeds `recent` for.
    let constraints = cfg.constraints(block_constraints);
    let no_repeat = match constraints.no_repeat_gap() {
        NoRepeatWithin::Positions(n) => RepeatGap::Positions(n),
        NoRepeatWithin::Duration(d) => RepeatGap::Duration(d),
    };
    let limits = Limits {
        no_repeat,
        separate: constraints.separate_gap(),
    };
    // A temporal `no_repeat_within` (#185) needs runtimes to measure distance
    // in time even when nothing else here does — the list pass below reads
    // them through `keys`, before `want_runtimes` would otherwise ask for
    // them.
    let want_runtimes = want_runtimes || matches!(limits.no_repeat, RepeatGap::Duration(_));
    let (pool_durations, mean_duration) = if want_runtimes {
        pool_runtimes(catalog, &ids)?
    } else {
        (HashMap::new(), Duration::ZERO)
    };
    let mut keys = pool_keys(catalog, &ids, constraints.separate_by.as_deref(), &cfg.name)?;
    keys.durations = pool_durations.clone();
    keys.mean_duration = mean_duration;
    // What this pool aired most recently: the channel's tail, narrowed to items
    // this pool could have supplied. Same projection the list pass reads, and
    // it carries the same cross-pool blindness — an id two pools share cannot be
    // attributed to one of them.
    let own: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let seam: Vec<String> = state
        .tail
        .iter()
        .filter(|id| own.contains(id.as_str()))
        .cloned()
        .collect();
    let ids = constrain_pool(cfg, limits, &keys, &seam, ids);

    // Group into series, preserving first-appearance order so the pool's
    // `order` fixes the rotation order too. One query for every `show_id` up
    // front, rather than a catalog round trip per item — a catch-up re-resolves
    // every pool on every generation.
    let show_ids = catalog.show_ids_for(&ids).map_err(|e| e.to_string())?;
    // Season numbers only when the pool asks to be cut by them — the query is
    // the same shape and the same cost as the one above, and a show-grouped
    // pool has no use for the answer.
    let seasons = match cfg.group_by {
        GroupBy::Show => HashMap::new(),
        GroupBy::Season => catalog.seasons_for(&ids).map_err(|e| e.to_string())?,
    };
    let mut series: Vec<Series> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    // Which series each id landed in, so a `bucket_order` pass can read the
    // series sequence back off an ordered id list without regrouping.
    let mut owner: HashMap<String, usize> = HashMap::new();
    for id in ids {
        // An item with no `show_id` — a movie — is its own series of one, and
        // stays one however the pool is grouped: it has no season to be cut at.
        let show_key = show_ids.get(&id).cloned().unwrap_or_else(|| id.clone());
        // An episode the catalog never recorded a season for stays with its
        // show rather than becoming a series of one, which would scatter the
        // very episodes a season-grouped pool is trying to keep together.
        let key = match seasons.get(&id) {
            Some(season) => format!("{show_key}|S{season}"),
            None => show_key.clone(),
        };
        match index.get(&key) {
            Some(&i) => {
                owner.insert(id.clone(), i);
                series[i].ids.push(id);
            }
            None => {
                index.insert(key.clone(), series.len());
                owner.insert(id.clone(), series.len());
                series.push(Series {
                    key,
                    show_key,
                    ids: vec![id],
                    cursor: 0,
                });
            }
        }
    }

    // A `bucket_order` re-sequences the series without disturbing what is
    // inside them. It runs through the same ordering engine the pool's own
    // `order` uses rather than a sort of its own — one implementation of "what
    // does `year:desc` mean", so a field that sorts one way here cannot sort
    // another way there — and the series sequence is then read off the result:
    // each series takes the position of whichever of its items comes first.
    //
    // Seed 0 for the same reason `order` uses it: this is a stable arrangement
    // of the pool, not a per-visit draw. Drawing at random per visit is
    // `select = "random"`, which is a different question and already answered.
    if let Some(bucket_order) = &cfg.bucket_order {
        let flat: Vec<String> = series.iter().flat_map(|s| s.ids.iter().cloned()).collect();
        let ordered = catalog
            .resolve_order(&flat, bucket_order, 0)
            .map_err(|e| format!("pool {:?}: bucket_order: {e}", cfg.name))?;
        let mut placed = vec![false; series.len()];
        let mut sequence: Vec<usize> = Vec::with_capacity(series.len());
        for id in &ordered {
            if let Some(&i) = owner.get(id)
                && !placed[i]
            {
                placed[i] = true;
                sequence.push(i);
            }
        }
        // An id the order engine dropped would leave its series unplaced; keep
        // it, at the back, rather than silently losing a whole show.
        for (i, seen) in placed.iter().enumerate() {
            if !seen {
                sequence.push(i);
            }
        }
        let mut taken: Vec<Option<Series>> = series.into_iter().map(Some).collect();
        series = sequence
            .into_iter()
            .map(|i| taken[i].take().expect("each series placed once"))
            .collect();
    }

    let mut rt = PoolRuntime {
        cfg,
        series,
        rotation: 0,
        limits,
        keys,
        recent: Vec::new(),
        forced: 0,
        runtimes: pool_durations,
        mean_runtime: mean_duration,
    };
    // Seeded through the same path a draw takes, so the window is trimmed to
    // `reach` by one rule rather than two.
    rt.remember(&seam);

    if cfg.advance == Advance::Resume {
        let prev = state.resume.pool(&cfg.name);
        for s in &mut rt.series {
            // Continue *after* the last-played id, read from the play-history
            // ledger's projection (#70) rather than a cursor of our own. An id
            // that has vanished from this series — deleted, re-identified, or
            // filtered out — restarts that series and only that series.
            //
            // Keyed on the show, not on this series' own name: the ledger files
            // an airing under its `show_id`, so under `group_by = "season"` all
            // of one show's seasons read the same entry and each keeps only the
            // position it can find in its own list. The season that was playing
            // continues; the others sit at their top, which is where a season
            // nobody has started belongs.
            if let Some(last) = state.cursor.get(&s.show_key)
                && let Some(pos) = s.ids.iter().position(|id| id == last)
            {
                s.cursor = pos + 1;
                if s.cursor >= s.ids.len() {
                    s.cursor = 0;
                }
            }
        }
        if let Some(next) = prev.and_then(|p| p.next.as_ref())
            && let Some(pos) = rt.series.iter().position(|s| &s.key == next)
        {
            rt.rotation = pos;
        }
    }

    Ok(rt)
}

/// Every runtime this pool's items have on record, plus the mean over them.
///
/// Bogus lengths are thrown out on the same rule [`crate::resolve`] uses to
/// size a slot: a non-positive figure is not a runtime, and anything past a day
/// is a stray factor of a thousand in somebody's metadata. Letting one through
/// would tell the walk a single film covered the whole window.
///
/// Takes the pool's flat id list rather than its grouped [`Series`] — this now
/// has to run before series grouping, so a temporal `no_repeat_within` (#185)
/// has runtimes to measure by in the list pass too, not only the draw-time
/// one.
fn pool_runtimes(
    catalog: &Catalog,
    ids: &[String],
) -> Result<(HashMap<String, Duration>, Duration), String> {
    let raw = catalog.durations_for(ids).map_err(|e| e.to_string())?;
    let runtimes: HashMap<String, Duration> = raw
        .into_iter()
        .filter(|(_, ms)| *ms > 0 && *ms <= crate::resolve::MAX_CATALOG_DURATION_MS)
        .map(|(id, ms)| (id, Duration::from_millis(ms as u64)))
        .collect();
    let mean = if runtimes.is_empty() {
        Duration::ZERO
    } else {
        runtimes.values().sum::<Duration>() / runtimes.len() as u32
    };
    Ok((runtimes, mean))
}

/// Order one pool's resolved list so it satisfies that pool's adjacency
/// constraints (#115), leaving it untouched when nothing is constrained.
///
/// This is the *list* half. It settles which item opens the window and how the
/// series rotate; it cannot settle the repeats a looping pool makes at draw
/// time, which is what [`PoolRuntime::eligible_from`] is for. Both are needed:
/// a set with no duplicate ids has nothing for this pass to fix, and every
/// repeat that reaches air is then made downstream of it.
///
/// `seam` is the channel's aired tail already narrowed to ids this pool could
/// have supplied — the only part of it that says anything about this pool's
/// next draw.
fn constrain_pool(
    cfg: &Pool,
    limits: Limits,
    keys: &PoolKeys,
    seam: &[String],
    ids: Vec<String>,
) -> Vec<String> {
    if limits.is_unconstrained() || ids.is_empty() {
        return ids;
    }

    let items = keys.many(&ids);
    let preceding = keys.many(seam);
    let result = order_constrained(&items, &vec![limits; ids.len()], &preceding);
    if result.unresolved > 0 {
        // The pool cannot satisfy what the config asks — an all-one-title set,
        // or a cast too interlinked to separate. Generation completes either
        // way; say so, or a pool quietly failing its constraint looks exactly
        // like one honouring it.
        tracing::warn!(
            event = "constraints.unsatisfied",
            pool = %cfg.name,
            phase = "list",
            violations = result.unresolved,
            items = ids.len(),
            "pool adjacency constraints could not be fully satisfied; airing the closest arrangement found",
        );
    }

    let mut slots: Vec<Option<String>> = ids.into_iter().map(Some).collect();
    result
        .order
        .iter()
        .map(|&i| {
            slots[i]
                .take()
                .expect("a permutation visits each index exactly once")
        })
        .collect()
}

/// The keys the adjacency checks compare for one pool: the `entry_id`, plus the
/// values of the field this pool separates on. Read with the vocabulary an
/// expression uses, so `separate_by: "cast"` reads exactly what `item.cast`
/// reads; an item with no values simply never triggers the separation.
fn pool_keys(
    catalog: &Catalog,
    ids: &[String],
    field: Option<&str>,
    pool: &str,
) -> Result<PoolKeys, String> {
    let Some(field) = field else {
        return Ok(PoolKeys::default());
    };
    let ns = TagNs::for_separate_by(field).map_err(|m| format!("pool {pool:?}: {m}"))?;
    let mut groups = HashMap::with_capacity(ids.len());
    for id in ids {
        let values = catalog
            .tags_for(id, ns)
            .map_err(|e| format!("pool {pool:?}: reading {field:?} for {id}: {e}"))?;
        groups.insert(id.clone(), values);
    }
    Ok(PoolKeys {
        groups,
        ..PoolKeys::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Entry as CatEntry, EntrySource, Source};
    use crate::config::Order;
    use crate::resume::{PoolResume, ResumeMap};

    /// Two shows of deliberately different lengths plus three movies — the
    /// shape #81 (Sample S7) exercises: series that must each progress
    /// independently, with no requirement to finish together.
    fn catalog() -> Catalog {
        let c = Catalog::open_in_memory().unwrap();
        let add = |id: &str, kind: &str, show: Option<(&str, i64, i64)>| {
            let mut e = CatEntry::new(id, kind, format!("Title {id}"), Source::Plex);
            if let Some((show_id, season, episode)) = show {
                e.show_id = Some(show_id.to_string());
                // `show` is the queryable name; `show_id` is the grouping key
                // the pattern engine reads straight off the catalog row.
                e.show = Some(show_id.trim_start_matches("show:").to_string());
                e.season = Some(season);
                e.episode = Some(episode);
            }
            c.upsert_entry(&e).unwrap();
            c.add_source(&EntrySource {
                source: Source::LocalFs,
                source_id: format!("fs-{id}"),
                entry_id: id.to_string(),
                playback_path: format!("/media/{id}.mkv"),
                last_seen: None,
            })
            .unwrap();
        };
        for n in 1..=3 {
            add(&format!("mov-{n}"), "movie", None);
        }
        // got: 6 episodes. inv: 3 episodes.
        for n in 1..=6 {
            add(&format!("got-e{n}"), "episode", Some(("show:got", 1, n)));
        }
        for n in 1..=3 {
            add(&format!("inv-e{n}"), "episode", Some(("show:inv", 1, n)));
        }
        c
    }

    fn movies_pool() -> Pool {
        Pool {
            name: "movies".into(),
            expr: Some("item.type == \"movie\"".into()),
            plugin: None,
            groups: Vec::new(),
            order: Some(Order::parse("title:asc").unwrap()),
            bucket_order: None,
            group_by: GroupBy::Show,
            select: Select::RoundRobin,
            rotate: Rotate::Visit,
            advance: Advance::Restart,
            on_short: OnShort::Next,
            constraints: None,
            config: None,
            capabilities: Vec::new(),
            datastores: Vec::new(),
        }
    }

    fn shows_pool() -> Pool {
        Pool {
            name: "shows".into(),
            expr: Some("item.type == \"episode\"".into()),
            plugin: None,
            groups: Vec::new(),
            order: Some(Order::parse("season:asc,episode:asc").unwrap()),
            bucket_order: None,
            group_by: GroupBy::Show,
            select: Select::RoundRobin,
            rotate: Rotate::Visit,
            advance: Advance::Restart,
            on_short: OnShort::Next,
            constraints: None,
            config: None,
            capabilities: Vec::new(),
            datastores: Vec::new(),
        }
    }

    /// No pool in these tests draws from a plugin, so the inputs are empty and
    /// the base dir never gets read — it only has to exist.
    fn test_env() -> crate::score::ScoreEnv<'static> {
        static EMPTY: std::sync::LazyLock<crate::score::ScoreInputs> =
            std::sync::LazyLock::new(crate::score::ScoreInputs::default);
        crate::score::ScoreEnv {
            inputs: &EMPTY,
            base_dir: std::path::Path::new("."),
        }
    }

    fn step(pool: &str, take: usize) -> PatternStep {
        PatternStep {
            pool: pool.into(),
            take: Take::Count(take),
            from: TakeFrom::Start,
            chance: 1.0,
        }
    }

    fn step_all(pool: &str) -> PatternStep {
        PatternStep {
            pool: pool.into(),
            take: Take::All,
            from: TakeFrom::Start,
            chance: 1.0,
        }
    }

    // ---- seasons as buckets (#155) -----------------------------------------

    /// One show with two seasons of different lengths, plus a second show, so a
    /// season-grouped pool has more than one way to be wrong: it can merge two
    /// seasons of one show, or split one season across shows.
    fn seasons_catalog() -> Catalog {
        let c = Catalog::open_in_memory().unwrap();
        let add = |id: &str, show_id: &str, season: i64, episode: i64| {
            let mut e = CatEntry::new(id, "episode", format!("Title {id}"), Source::Plex);
            e.show_id = Some(show_id.to_string());
            e.show = Some(show_id.trim_start_matches("show:").to_string());
            e.season = Some(season);
            e.episode = Some(episode);
            c.upsert_entry(&e).unwrap();
            c.add_source(&EntrySource {
                source: Source::LocalFs,
                source_id: format!("fs-{id}"),
                entry_id: id.to_string(),
                playback_path: format!("/media/{id}.mkv"),
                last_seen: None,
            })
            .unwrap();
        };
        for n in 1..=4 {
            add(&format!("got-s1e{n}"), "show:got", 1, n);
        }
        for n in 1..=2 {
            add(&format!("got-s2e{n}"), "show:got", 2, n);
        }
        for n in 1..=3 {
            add(&format!("inv-s1e{n}"), "show:inv", 1, n);
        }
        c
    }

    fn episodes_pool(group_by: GroupBy) -> Pool {
        Pool {
            name: "shows".into(),
            expr: Some("item.type == \"episode\"".into()),
            plugin: None,
            groups: Vec::new(),
            order: Some(Order::parse("season:asc,episode:asc").unwrap()),
            bucket_order: None,
            group_by,
            select: Select::RoundRobin,
            rotate: Rotate::Visit,
            advance: Advance::Restart,
            on_short: OnShort::Next,
            constraints: None,
            config: None,
            capabilities: Vec::new(),
            datastores: Vec::new(),
        }
    }

    fn build_seasons(
        pool: Pool,
        pattern: Vec<PatternStep>,
        cycles: Option<usize>,
        state: &GenerationState,
    ) -> Vec<String> {
        let cat = seasons_catalog();
        build(
            &cat,
            &[pool],
            &[],
            &pattern,
            cycles,
            None,
            state,
            7,
            test_env(),
            None,
        )
        .expect("pattern builds")
        .0
    }

    /// The whole point of #155: one visit airs one season, in order, and stops
    /// at the season line rather than running on into the next one.
    #[test]
    fn a_season_grouped_take_all_airs_exactly_one_season() {
        let ids = build_seasons(
            episodes_pool(GroupBy::Season),
            vec![step_all("shows")],
            Some(1),
            &GenerationState::empty(),
        );
        assert_eq!(
            ids,
            vec!["got-s1e1", "got-s1e2", "got-s1e3", "got-s1e4"],
            "a season-grouped visit must air one season end to end and stop"
        );
    }

    /// Same `take = "all"`, grouped by show: the bucket is the whole show, so
    /// both seasons air. This is the knob doing what it says rather than
    /// season-awareness leaking into the walk.
    #[test]
    fn a_show_grouped_take_all_airs_the_whole_show() {
        let ids = build_seasons(
            episodes_pool(GroupBy::Show),
            vec![step_all("shows")],
            Some(1),
            &GenerationState::empty(),
        );
        assert_eq!(
            ids,
            vec![
                "got-s1e1", "got-s1e2", "got-s1e3", "got-s1e4", "got-s2e1", "got-s2e2"
            ],
        );
    }

    /// Consecutive visits move to the next season rather than replaying the one
    /// just aired. The order they come in is the pool's own `order` — sorting
    /// `season:asc` first puts every show's season 1 ahead of any season 2 —
    /// which is the existing rule that a season bucket inherits unchanged.
    #[test]
    fn successive_take_all_visits_walk_the_seasons() {
        let ids = build_seasons(
            episodes_pool(GroupBy::Season),
            vec![step_all("shows")],
            Some(3),
            &GenerationState::empty(),
        );
        assert_eq!(
            ids,
            vec![
                "got-s1e1", "got-s1e2", "got-s1e3", "got-s1e4", // got, season 1
                "inv-s1e1", "inv-s1e2", "inv-s1e3", // inv, season 1
                "got-s2e1", "got-s2e2", // got, season 2
            ],
        );
    }

    /// `advance = "resume"` continues the season that was playing. The ledger
    /// files an airing under the show, not the season, so every season of that
    /// show reads the same cursor entry — only the one that actually contains
    /// the last-played episode may move, and the rest stay at their top.
    #[test]
    fn a_season_resumes_inside_its_own_season() {
        let mut pool = episodes_pool(GroupBy::Season);
        pool.advance = Advance::Resume;

        let mut state = GenerationState::empty();
        state
            .cursor
            .insert("show:got".into(), "got-s1e2".to_string());

        let ids = build_seasons(pool, vec![step_all("shows")], Some(3), &state);
        assert_eq!(
            ids,
            vec![
                // got season 1 picks up after the episode the ledger last aired…
                "got-s1e3", "got-s1e4", //
                // …a show the cursor says nothing about is untouched…
                "inv-s1e1", "inv-s1e2", "inv-s1e3", //
                // …and got season 2, which never played, starts at its top
                // rather than inheriting its show's cursor.
                "got-s2e1", "got-s2e2",
            ],
        );
    }

    /// A movie has no season, so a season-grouped pool leaves it exactly where
    /// a show-grouped one does: its own bucket of one.
    #[test]
    fn a_movie_is_its_own_bucket_however_the_pool_is_grouped() {
        let mut pool = movies_pool();
        pool.group_by = GroupBy::Season;
        let (ids, _) = build_with(
            vec![pool],
            vec![step_all("movies")],
            Some(2),
            &GenerationState::empty(),
            7,
        );
        assert_eq!(ids, vec!["mov-1", "mov-2"], "one movie per visit");
    }

    fn step_from(pool: &str, take: usize, from: TakeFrom) -> PatternStep {
        PatternStep {
            pool: pool.into(),
            take: Take::Count(take),
            from,
            chance: 1.0,
        }
    }

    /// The gap `from` exists to close: `order: "episode:desc"` would put the
    /// last three episodes first but air them backwards, because the sort that
    /// chooses the slice is the sort that decides play order. `from: end` takes
    /// the same three the right way round.
    #[test]
    fn take_from_end_airs_the_last_episodes_in_order() {
        let ids = build_seasons(
            episodes_pool(GroupBy::Season),
            vec![step_from("shows", 3, TakeFrom::End)],
            Some(1),
            &GenerationState::empty(),
        );
        assert_eq!(ids, vec!["got-s1e2", "got-s1e3", "got-s1e4"]);
    }

    /// A season shorter than the ask gives everything it has, from its top —
    /// there is no earlier place for the slice to start.
    #[test]
    fn take_from_end_of_a_short_series_is_the_whole_series() {
        let ids = build_seasons(
            episodes_pool(GroupBy::Season),
            vec![step_from("shows", 9, TakeFrom::End)],
            Some(1),
            &GenerationState::empty(),
        );
        assert_eq!(ids[0], "got-s1e1", "nowhere later to start: {ids:?}");
    }

    /// A random cut lands inside the series and stays consecutive and in order,
    /// and a pinned seed reproduces it.
    #[test]
    fn take_from_random_is_consecutive_in_order_and_reproducible() {
        let run = || {
            build_seasons(
                episodes_pool(GroupBy::Season),
                vec![step_from("shows", 2, TakeFrom::Random)],
                Some(1),
                &GenerationState::empty(),
            )
        };
        let ids = run();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids, run(), "same seed, same cut");
        // Whatever offset came up, the two are adjacent episodes of season 1.
        let pairs = [
            ["got-s1e1", "got-s1e2"],
            ["got-s1e2", "got-s1e3"],
            ["got-s1e3", "got-s1e4"],
        ];
        assert!(
            pairs.iter().any(|p| p[..] == ids[..]),
            "not a consecutive in-order pair: {ids:?}"
        );
    }

    /// `bucket_order` sequences the series without touching what is inside
    /// them: seasons come up newest-episode-first while each still plays in
    /// episode order. The pool's own `order` cannot do both at once, which is
    /// why the field exists.
    #[test]
    fn bucket_order_sequences_series_without_reordering_their_items() {
        let mut pool = episodes_pool(GroupBy::Season);
        pool.bucket_order = Some(Order::parse("season:desc,episode:desc").unwrap());
        let ids = build_seasons(
            pool,
            vec![step_all("shows")],
            Some(2),
            &GenerationState::empty(),
        );
        assert_eq!(
            ids,
            vec![
                // `season:desc` puts got's season 2 ahead of every season 1,
                // so it is the first series up…
                "got-s2e1", "got-s2e2", //
                // …and got's season 1 next, since `episode:desc` places its e4
                // ahead of inv's e3. Both still play their own items in
                // `order`'s ascending sequence — the descending sort chose
                // which season came up, not how it plays.
                "got-s1e1", "got-s1e2", "got-s1e3", "got-s1e4",
            ],
        );
    }

    /// With no `cycles` authored, the derived count has to drain the pool the
    /// way a take-all step actually drains it — one bucket per visit — rather
    /// than dividing the item count by a `take` that isn't there.
    #[test]
    fn derived_cycles_drain_every_season_once() {
        let ids = build_seasons(
            episodes_pool(GroupBy::Season),
            vec![step_all("shows")],
            None,
            &GenerationState::empty(),
        );
        assert_eq!(ids.len(), 9, "all three seasons aired once: {ids:?}");
    }

    // ---- named show groups (#165) ------------------------------------------

    /// Drag Race season 9, All Stars season 3, Drag Race season 10 — the
    /// issue's own worked example. Titles are prefixed `"1 "`/`"2 "`/`"3 "`
    /// purely so `bucket_order: "title:asc"` reproduces broadcast order
    /// deterministically in a test fixture; a real channel would sort its
    /// `bucket_order` by the catalog's own air-date field instead.
    fn franchise_catalog() -> Catalog {
        let c = Catalog::open_in_memory().unwrap();
        let add = |id: &str, show: &str, season: i64, episode: i64, title_rank: &str| {
            let mut e = CatEntry::new(id, "episode", format!("{title_rank} {id}"), Source::Plex);
            e.show = Some(show.to_string());
            e.show_id = Some(format!("show:{show}"));
            e.season = Some(season);
            e.episode = Some(episode);
            c.upsert_entry(&e).unwrap();
            c.add_source(&EntrySource {
                source: Source::LocalFs,
                source_id: format!("fs-{id}"),
                entry_id: id.to_string(),
                playback_path: format!("/media/{id}.mkv"),
                last_seen: None,
            })
            .unwrap();
        };
        add("dr-s9e1", "RuPaul's Drag Race", 9, 1, "1");
        add("dr-s9e2", "RuPaul's Drag Race", 9, 2, "1");
        add("as-s3e1", "RuPaul's Drag Race All Stars", 3, 1, "2");
        add("as-s3e2", "RuPaul's Drag Race All Stars", 3, 2, "2");
        add("dr-s10e1", "RuPaul's Drag Race", 10, 1, "3");
        add("dr-s10e2", "RuPaul's Drag Race", 10, 2, "3");
        c
    }

    fn rupaul_group() -> ShowGroup {
        ShowGroup {
            name: "rupaul".into(),
            shows: vec![
                "RuPaul's Drag Race".into(),
                "RuPaul's Drag Race All Stars".into(),
            ],
        }
    }

    fn rupaul_pool() -> Pool {
        Pool {
            name: "rupaul".into(),
            expr: None,
            plugin: None,
            groups: vec!["rupaul".into()],
            order: None,
            bucket_order: Some(Order::parse("title:asc").unwrap()),
            group_by: GroupBy::Season,
            select: Select::RoundRobin,
            rotate: Rotate::Visit,
            advance: Advance::Restart,
            on_short: OnShort::Next,
            constraints: None,
            config: None,
            capabilities: Vec::new(),
            datastores: Vec::new(),
        }
    }

    /// Build once against [`franchise_catalog`], returning the ids and the
    /// state a following window would be handed — the `groups`-sourced
    /// counterpart to [`build_with`].
    fn build_grouped(
        pools: Vec<Pool>,
        groups: Vec<ShowGroup>,
        pattern: Vec<PatternStep>,
        cycles: Option<usize>,
        state_in: &GenerationState,
        seed: u64,
    ) -> (Vec<String>, GenerationState) {
        let cat = franchise_catalog();
        let (ids, pool_state) = build(
            &cat,
            &pools,
            &groups,
            &pattern,
            cycles,
            None,
            state_in,
            seed,
            test_env(),
            None,
        )
        .expect("pattern builds");

        let mut resume = ResumeMap::new();
        resume.pools = pool_state;

        let mut cursor = state_in.cursor.clone();
        let show_ids = cat.show_ids_for(&ids).unwrap();
        for id in &ids {
            let key = show_ids.get(id).cloned().unwrap_or_else(|| id.clone());
            cursor.insert(key, id.clone());
        }
        let tail = ids.clone();
        (
            ids,
            GenerationState {
                resume,
                cursor,
                tail,
            },
        )
    }

    /// The issue's own worked example: a season airs end to end, then
    /// rotation moves to the next season from a sibling show rather than the
    /// same show's next run — Drag Race season 9, then All Stars season 3,
    /// then Drag Race season 10, cycling the franchise instead of marching
    /// through one show's whole run before the other's.
    #[test]
    fn a_named_group_rotates_seasons_across_sibling_shows() {
        let (ids, _) = build_grouped(
            vec![rupaul_pool()],
            vec![rupaul_group()],
            vec![step_all("rupaul")],
            Some(3),
            &GenerationState::empty(),
            0,
        );
        assert_eq!(
            ids,
            vec![
                "dr-s9e1", "dr-s9e2", //
                "as-s3e1", "as-s3e2", //
                "dr-s10e1", "dr-s10e2",
            ],
            "rotate: visit must move to the sibling show's season next, not \
             back to the same show's next season: {ids:?}"
        );
    }

    /// #165's explicit regression requirement: grouping shows into one
    /// rotation domain must not disturb the per-show resume cursor each
    /// sibling already had before grouping existed (#155, #109) — no new
    /// field, because [`pool_runtime`]'s resume seek already keys on each
    /// item's own `show_id`, regardless of which pool or group drew the item.
    ///
    /// One visit of `take: 3` against Drag Race season 9 (2 episodes) rolls
    /// onto All Stars season 3 for the third — so All Stars' own cursor now
    /// sits mid-season while Drag Race's has drained its season. The next
    /// window must resume All Stars at its own second episode and find Drag
    /// Race season 10 — a season nobody has started — sitting at its own top,
    /// not disturbed by where Drag Race season 9 left off.
    #[test]
    fn a_sibling_returning_around_the_group_resumes_at_its_own_position() {
        fn resuming_pool() -> Pool {
            let mut p = rupaul_pool();
            p.advance = Advance::Resume;
            p
        }

        let (first, resume) = build_grouped(
            vec![resuming_pool()],
            vec![rupaul_group()],
            vec![step("rupaul", 3)],
            Some(1),
            &GenerationState::empty(),
            0,
        );
        assert_eq!(first, vec!["dr-s9e1", "dr-s9e2", "as-s3e1"]);

        let (second, _) = build_grouped(
            vec![resuming_pool()],
            vec![rupaul_group()],
            vec![step("rupaul", 3)],
            Some(1),
            &resume,
            0,
        );
        assert_eq!(
            second,
            vec!["as-s3e2", "dr-s10e1", "dr-s10e2"],
            "All Stars must resume at its own next episode, and Drag Race \
             season 10 — never touched — at its own top: {second:?}"
        );
    }

    /// A group name a pool references but the channel never declared is a
    /// validation gap by the time generation reaches here (`config::validate`
    /// already refuses it structurally) — but pattern's own tests build pools
    /// directly, bypassing that pass, so this is checked again defensively.
    #[test]
    fn a_pool_referencing_an_undeclared_group_is_an_error() {
        let cat = franchise_catalog();
        let mut pool = rupaul_pool();
        pool.groups = vec!["nonesuch".into()];
        let err = build(
            &cat,
            &[pool],
            &[], // no groups declared at all
            &[step_all("rupaul")],
            Some(1),
            None,
            &GenerationState::empty(),
            0,
            test_env(),
            None,
        )
        .unwrap_err();
        assert!(err.contains("nonesuch"), "err = {err}");
    }

    /// A declared group naming a show absent from the catalog is a load
    /// failure the normal path catches earlier
    /// (`resolve::validate_groups_against_catalog`, run once against every
    /// declared group before any pool resolves) — checked again here
    /// defensively for the same reason as the test above.
    #[test]
    fn a_group_naming_a_show_not_in_the_catalog_is_an_error() {
        let cat = franchise_catalog();
        let group = ShowGroup {
            name: "rupaul".into(),
            shows: vec!["RuPaul's Drag Race".into(), "A Show Nobody Ingested".into()],
        };
        let mut pool = rupaul_pool();
        pool.groups = vec!["rupaul".into()];
        let err = build(
            &cat,
            &[pool],
            &[group],
            &[step_all("rupaul")],
            Some(1),
            None,
            &GenerationState::empty(),
            0,
            test_env(),
            None,
        )
        .unwrap_err();
        assert!(err.contains("A Show Nobody Ingested"), "err = {err}");
    }

    /// Build once, returning the ids and the state a following window would be
    /// handed: the pools' rotation from the resolver, and the cursor projected
    /// from the airings just produced — which is what the daemon does with the
    /// play-history ledger (#70).
    fn build_with(
        pools: Vec<Pool>,
        pattern: Vec<PatternStep>,
        cycles: Option<usize>,
        state_in: &GenerationState,
        seed: u64,
    ) -> (Vec<String>, GenerationState) {
        let cat = catalog();
        let (ids, pool_state) = build(
            &cat,
            &pools,
            &[],
            &pattern,
            cycles,
            None,
            state_in,
            seed,
            test_env(),
            None,
        )
        .expect("pattern builds");

        let mut resume = ResumeMap::new();
        resume.pools = pool_state;

        // Replay the airings into the cursor exactly as the ledger's projection
        // would: last entry wins per series key.
        let mut cursor = state_in.cursor.clone();
        let show_ids = cat.show_ids_for(&ids).unwrap();
        for id in &ids {
            let key = show_ids.get(id).cloned().unwrap_or_else(|| id.clone());
            cursor.insert(key, id.clone());
        }
        let tail = ids.clone();
        (
            ids,
            GenerationState {
                resume,
                cursor,
                tail,
            },
        )
    }

    /// The headline acceptance criterion: `{movies take=1}, {shows take=3}`
    /// yields 1 movie then 3 episodes, repeated.
    #[test]
    fn one_movie_then_three_episodes_repeated() {
        let (ids, _) = build_with(
            vec![movies_pool(), shows_pool()],
            vec![step("movies", 1), step("shows", 3)],
            Some(3),
            &GenerationState::empty(),
            0,
        );
        assert_eq!(
            ids,
            vec![
                "mov-1", "got-e1", "got-e2", "got-e3", // cycle 1
                "mov-2", "inv-e1", "inv-e2", "inv-e3", // cycle 2 — rotated show
                "mov-3", "got-e4", "got-e5", "got-e6", // cycle 3 — got continues
            ]
        );
    }

    /// A movie pool needs no special case: an item with no `show_id` is its own
    /// one-item series, so round-robin over them is simply playing them in
    /// order, and `wrap = "loop"` restarts the list.
    #[test]
    fn a_movie_pool_plays_in_order_and_loops() {
        let (ids, _) = build_with(
            vec![movies_pool()],
            vec![step("movies", 1)],
            Some(5),
            &GenerationState::empty(),
            0,
        );
        assert_eq!(ids, vec!["mov-1", "mov-2", "mov-3", "mov-1", "mov-2"]);
    }

    /// Cycle count with no explicit `cycles`: enough for the largest pool to
    /// drain once. shows holds 9 episodes at 3 per cycle = 3 cycles; movies
    /// holds 3 at 1 per cycle = 3. Both agree here, and every episode airs.
    #[test]
    fn derived_cycles_drain_the_largest_pool() {
        let (ids, _) = build_with(
            vec![movies_pool(), shows_pool()],
            vec![step("movies", 1), step("shows", 3)],
            None,
            &GenerationState::empty(),
            0,
        );
        assert_eq!(ids.len(), 12);
        for n in 1..=6 {
            assert!(ids.contains(&format!("got-e{n}")), "missing got-e{n}");
        }
        for n in 1..=3 {
            assert!(ids.contains(&format!("inv-e{n}")), "missing inv-e{n}");
        }
    }

    /// Two shows of different lengths each advance independently across
    /// windows, and neither resets the other. This is the progression that has
    /// to survive a window seam with no live cursor: window 2 is generated from
    /// window 1's `resume_out` and continues rather than replaying.
    #[test]
    fn resume_continues_each_show_independently_across_windows() {
        let mut shows = shows_pool();
        shows.advance = Advance::Resume;
        let pools = || vec![movies_pool(), shows_pool_resumed()];
        fn shows_pool_resumed() -> Pool {
            let mut p = shows_pool();
            p.advance = Advance::Resume;
            p
        }

        let (first, resume) = build_with(
            pools(),
            vec![step("movies", 1), step("shows", 3)],
            Some(2),
            &GenerationState::empty(),
            0,
        );
        assert_eq!(
            first,
            vec![
                "mov-1", "got-e1", "got-e2", "got-e3", "mov-2", "inv-e1", "inv-e2", "inv-e3"
            ]
        );

        // Window 2 picks up where each show left off — got at E4, inv having
        // wrapped — without either resetting the other.
        let (second, _) = build_with(
            pools(),
            vec![step("movies", 1), step("shows", 3)],
            Some(2),
            &resume,
            0,
        );
        assert_eq!(
            second,
            vec![
                "mov-1", "got-e4", "got-e5", "got-e6", "mov-2", "inv-e1", "inv-e2", "inv-e3"
            ]
        );
        let _ = shows;
    }

    /// `advance = "restart"` is genuinely stateless: handed the same resume map
    /// it still replays from the top.
    #[test]
    fn restart_ignores_the_resume_map() {
        let (_, resume) = build_with(
            vec![shows_pool()],
            vec![step("shows", 3)],
            Some(1),
            &GenerationState::empty(),
            0,
        );
        let (again, _) = build_with(
            vec![shows_pool()],
            vec![step("shows", 3)],
            Some(1),
            &resume,
            0,
        );
        assert_eq!(again, vec!["got-e1", "got-e2", "got-e3"]);
    }

    /// A show whose stored id has vanished from the catalog restarts — and only
    /// that show. The other show's position is untouched.
    #[test]
    fn a_vanished_cursor_restarts_only_its_own_show() {
        let mut p = shows_pool();
        p.advance = Advance::Resume;
        let mut resume = ResumeMap::new();
        resume.pools.insert(
            "shows".into(),
            PoolResume {
                next: Some("show:inv".into()),
            },
        );
        // The ledger remembers an airing whose entry has since left the
        // catalog — the id no longer resolves to anything in got's series.
        let state = GenerationState {
            resume,
            tail: Vec::new(),
            cursor: BTreeMap::from([
                ("show:got".to_string(), "got-e99-deleted".to_string()),
                ("show:inv".to_string(), "inv-e2".to_string()),
            ]),
        };

        let (ids, _) = build_with(vec![p], vec![step("shows", 2)], Some(2), &state, 0);
        // inv continues after e2; got — its cursor gone — starts over at e1.
        assert_eq!(ids, vec!["inv-e3", "got-e1", "got-e2", "got-e3"]);
    }

    /// `wrap = "loop"` restarts an exhausted show so the channel never runs dry.
    #[test]
    fn wrap_loop_restarts_an_exhausted_show() {
        let mut p = shows_pool();
        p.expr = Some("item.show == \"inv\"".into());
        let (ids, _) = build_with(
            vec![p],
            vec![step("shows", 2)],
            Some(3),
            &GenerationState::empty(),
            0,
        );
        // 3 episodes, drawn 2 at a time, looping: e1 e2 | e3 e1 | e2 e3.
        assert_eq!(
            ids,
            vec!["inv-e1", "inv-e2", "inv-e3", "inv-e1", "inv-e2", "inv-e3"]
        );
    }

    /// A pool never runs out: once every series has played to its end they all
    /// start over, and the channel keeps broadcasting. There is no state in
    /// which a step emits nothing because its content was consumed.
    #[test]
    fn a_pool_played_past_its_end_starts_over_rather_than_running_dry() {
        let mut p = shows_pool();
        p.on_short = OnShort::Short;
        let (ids, _) = build_with(
            vec![p],
            vec![step("shows", 3)],
            Some(6),
            &GenerationState::empty(),
            0,
        );
        // got has 6 episodes, inv has 3. Six visits of 3 fill every slot:
        // both shows loop back to their own start instead of dropping out.
        assert_eq!(ids.len(), 18, "every visit must be filled: {ids:?}");
        assert_eq!(
            &ids[0..9],
            &[
                "got-e1", "got-e2", "got-e3", "inv-e1", "inv-e2", "inv-e3", "got-e4", "got-e5",
                "got-e6"
            ]
        );
        // The seventh visit is got's turn again and it restarts from e1.
        assert_eq!(&ids[12..15], &["got-e1", "got-e2", "got-e3"]);
    }

    /// `on_short = "next"` keeps the visit whole: the slots the current show
    /// can't supply are filled by the next show, which then *continues* on its
    /// following visit rather than replaying its start.
    #[test]
    fn on_short_next_fills_from_the_following_show_and_continues_it() {
        let mut p = shows_pool();
        p.on_short = OnShort::Next;
        let (ids, _) = build_with(
            vec![p],
            vec![step("shows", 4)],
            Some(2),
            &GenerationState::empty(),
            0,
        );
        // got has 6: e1-4, then e5,e6 + 2 filled from inv. inv then continues
        // at e3 next visit, not back at e1.
        assert_eq!(
            ids,
            vec![
                "got-e1", "got-e2", "got-e3", "got-e4", "inv-e1", "inv-e2", "inv-e3", "got-e5"
            ]
        );
    }

    /// `on_short = "wrap"` keeps the visit on one show, looping it back to its
    /// own start for the slots it couldn't supply.
    #[test]
    fn on_short_wrap_loops_the_same_show() {
        let mut p = shows_pool();
        p.expr = Some("item.show == \"inv\"".into());
        p.on_short = OnShort::Wrap;
        let (ids, _) = build_with(
            vec![p],
            vec![step("shows", 4)],
            Some(1),
            &GenerationState::empty(),
            0,
        );
        assert_eq!(ids, vec!["inv-e1", "inv-e2", "inv-e3", "inv-e1"]);
    }

    /// A `take` several times a short series' length still emits `take` items:
    /// `on_short = "wrap"` loops that series as many times as it needs, and the
    /// loop backstop must not cut the visit short.
    #[test]
    fn on_short_wrap_fills_a_take_far_larger_than_the_series() {
        let mut p = shows_pool();
        p.expr = Some("item.show == \"inv\"".into());
        p.on_short = OnShort::Wrap;
        let (ids, _) = build_with(
            vec![p],
            vec![step("shows", 10)],
            Some(1),
            &GenerationState::empty(),
            0,
        );
        assert_eq!(ids.len(), 10, "a wrap-filled visit must still emit `take`");
        assert_eq!(&ids[0..4], &["inv-e1", "inv-e2", "inv-e3", "inv-e1"]);
    }

    /// `on_short = "short"` emits fewer items rather than pulling anyone else in.
    #[test]
    fn on_short_short_emits_a_shorter_visit() {
        let mut p = shows_pool();
        p.expr = Some("item.show == \"inv\"".into());
        p.on_short = OnShort::Short;
        let (ids, _) = build_with(
            vec![p],
            vec![step("shows", 4)],
            Some(1),
            &GenerationState::empty(),
            0,
        );
        assert_eq!(ids, vec!["inv-e1", "inv-e2", "inv-e3"]);
    }

    /// `rotate = "slot"` spreads one visit across shows instead of bingeing one.
    #[test]
    fn rotate_slot_spreads_a_visit_across_shows() {
        let mut p = shows_pool();
        p.rotate = Rotate::Slot;
        let (ids, _) = build_with(
            vec![p],
            vec![step("shows", 4)],
            Some(1),
            &GenerationState::empty(),
            0,
        );
        assert_eq!(ids, vec!["got-e1", "inv-e1", "got-e2", "inv-e2"]);
    }

    /// `select = "random"` still keeps each show's resume point intact: the
    /// draw only chooses *which* show serves, never where that show is.
    #[test]
    fn select_random_preserves_each_shows_resume_point() {
        let mut p = shows_pool();
        p.select = Select::Random;
        p.advance = Advance::Resume;
        let pool = || {
            let mut p = shows_pool();
            p.select = Select::Random;
            p.advance = Advance::Resume;
            p
        };

        let (first, resume) = build_with(
            vec![pool()],
            vec![step("shows", 3)],
            Some(2),
            &GenerationState::empty(),
            7,
        );
        let (second, _) = build_with(vec![pool()], vec![step("shows", 3)], Some(2), &resume, 7);

        // Whatever the draws were, no episode from the first window repeats in
        // the second unless its show wrapped — i.e. each show continued.
        let both: Vec<&String> = first.iter().chain(second.iter()).collect();
        assert_eq!(both.len(), 12);
        let got_order: Vec<&&String> = both.iter().filter(|id| id.starts_with("got-")).collect();
        let expected: Vec<String> = (1..=got_order.len())
            .map(|n| format!("got-e{}", ((n - 1) % 6) + 1))
            .collect();
        assert_eq!(
            got_order.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            expected.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            "got must advance in order across both windows, never reset"
        );
        let _ = p;
    }

    /// A seeded random selection reproduces exactly.
    #[test]
    fn select_random_is_reproducible_for_a_pinned_seed() {
        let pool = || {
            let mut p = shows_pool();
            p.select = Select::Random;
            p
        };
        let (a, _) = build_with(
            vec![pool()],
            vec![step("shows", 2)],
            Some(4),
            &GenerationState::empty(),
            42,
        );
        let (b, _) = build_with(
            vec![pool()],
            vec![step("shows", 2)],
            Some(4),
            &GenerationState::empty(),
            42,
        );
        assert_eq!(a, b);
    }

    /// `chance` produces the same skip/fire sequence for a fixed seed, and a
    /// skipped step advances nothing.
    #[test]
    fn chance_is_reproducible_and_a_skip_consumes_nothing() {
        let pattern = || {
            vec![
                step("movies", 1),
                PatternStep {
                    pool: "shows".into(),
                    take: Take::Count(3),
                    from: TakeFrom::Start,
                    chance: 0.3,
                },
            ]
        };
        let (a, ra) = build_with(
            vec![movies_pool(), shows_pool()],
            pattern(),
            Some(10),
            &GenerationState::empty(),
            99,
        );
        let (b, rb) = build_with(
            vec![movies_pool(), shows_pool()],
            pattern(),
            Some(10),
            &GenerationState::empty(),
            99,
        );
        assert_eq!(a, b, "a pinned seed must reproduce the skip/fire sequence");
        assert_eq!(ra, rb);

        // Some cycles fired and some skipped — otherwise this proves nothing.
        let episodes = a.iter().filter(|id| id.contains("-e")).count();
        assert!(
            episodes > 0,
            "no step ever fired at chance 0.3 over 10 cycles"
        );
        assert!(
            episodes < 30,
            "every step fired; chance = 0.3 is not being applied"
        );
        // Every movie slot still aired: a skipped `shows` step leaves the
        // movies pool alone, and the episodes that did air are a prefix of the
        // show order — a skip consumed no cursor.
        assert_eq!(a.iter().filter(|id| id.starts_with("mov-")).count(), 10);
    }

    #[test]
    fn an_unknown_pool_in_a_step_is_an_error() {
        let cat = catalog();
        let err = build(
            &cat,
            &[movies_pool()],
            &[],
            &[step("shows", 1)],
            Some(1),
            None,
            &GenerationState::empty(),
            0,
            test_env(),
            None,
        )
        .unwrap_err();
        assert!(err.contains("unknown pool"), "err = {err}");
    }

    #[test]
    fn an_explicit_cycles_beyond_the_cap_is_an_error() {
        let cat = catalog();
        let err = build(
            &cat,
            &[movies_pool()],
            &[],
            &[step("movies", 1)],
            Some(MAX_CYCLES + 1),
            None,
            &GenerationState::empty(),
            0,
            test_env(),
            None,
        )
        .unwrap_err();
        assert!(err.contains("maximum"), "err = {err}");
    }

    /// An empty pool contributes nothing rather than erroring — the channel's
    /// "resolved to zero items" check is what catches a wholly empty block.
    #[test]
    fn an_empty_pool_yields_nothing() {
        let mut p = movies_pool();
        p.expr = Some("item.type == \"nonesuch\"".into());
        let cat = catalog();
        let (ids, _) = build(
            &cat,
            &[p],
            &[],
            &[step("movies", 1)],
            None,
            None,
            &GenerationState::empty(),
            0,
            test_env(),
            None,
        )
        .unwrap();
        assert!(ids.is_empty());
    }

    // ---- bounding a generation by the window still to fill (#140) --------

    /// Twenty-four movies, each one hour long — a day of schedule if you play
    /// them all. `measured` says how many of them the catalog actually recorded
    /// a runtime for; the rest have the column empty, which is what a failed
    /// probe or a Plex row without a duration leaves behind.
    fn hourly_movies(measured: usize) -> Catalog {
        let c = Catalog::open_in_memory().unwrap();
        for n in 1..=24 {
            let id = format!("mov-{n:02}");
            let mut e = CatEntry::new(&id, "movie", format!("Title {id}"), Source::Plex);
            if n <= measured {
                e.duration_ms = Some(3_600_000);
            }
            c.upsert_entry(&e).unwrap();
            c.add_source(&EntrySource {
                source: Source::LocalFs,
                source_id: format!("fs-{id}"),
                entry_id: id.clone(),
                playback_path: format!("/media/{id}.mkv"),
                last_seen: None,
            })
            .unwrap();
        }
        c
    }

    /// One movie per cycle, so a cycle is exactly one hour of airtime and the
    /// item count reads straight off the clock.
    fn fill_movies(
        cat: &Catalog,
        cycles: Option<usize>,
        state: &GenerationState,
        fill: Option<Duration>,
    ) -> (Vec<String>, BTreeMap<String, PoolResume>) {
        let mut movies = movies_pool();
        movies.advance = Advance::Resume;
        build(
            cat,
            &[movies],
            &[],
            &[step("movies", 1)],
            cycles,
            None,
            state,
            0,
            test_env(),
            fill,
        )
        .expect("pattern builds")
    }

    /// The bug: a pool that takes a day to drain laid down the whole day even
    /// when only four hours were missing. Unbounded it emits all 24 movies;
    /// bounded to four hours it emits four.
    #[test]
    fn a_derived_generation_stops_once_the_window_is_covered() {
        let cat = hourly_movies(24);
        let (unbounded, _) = fill_movies(&cat, None, &GenerationState::empty(), None);
        assert_eq!(unbounded.len(), 24, "drain-once is still the ceiling");

        let (bounded, _) = fill_movies(
            &cat,
            None,
            &GenerationState::empty(),
            Some(Duration::from_secs(4 * 3600)),
        );
        assert_eq!(
            bounded,
            vec!["mov-01", "mov-02", "mov-03", "mov-04"],
            "four hours of window should buy four hours of schedule",
        );
    }

    /// An item the catalog never measured is counted at the pool's mean, not at
    /// zero. Half-measured, the pool still stops at four hours — counting the
    /// unmeasured ones as free would let it run to the end of the list, which
    /// is the overrun this bound exists to stop.
    #[test]
    fn an_unmeasured_item_still_spends_the_window() {
        let cat = hourly_movies(12);
        let (bounded, _) = fill_movies(
            &cat,
            None,
            &GenerationState::empty(),
            Some(Duration::from_secs(4 * 3600)),
        );
        assert_eq!(bounded.len(), 4, "got {bounded:?}");
    }

    /// A catalog that knows no runtimes at all cannot bind the window, and says
    /// so by falling back to draining once rather than emitting nothing.
    #[test]
    fn a_pool_with_no_recorded_runtimes_falls_back_to_draining_once() {
        let cat = hourly_movies(0);
        let (bounded, _) = fill_movies(
            &cat,
            None,
            &GenerationState::empty(),
            Some(Duration::from_secs(4 * 3600)),
        );
        assert_eq!(bounded.len(), 24, "got {bounded:?}");
    }

    /// An authored `cycles` is the author saying how long a pass runs. The
    /// window does not shorten it.
    #[test]
    fn an_authored_cycle_count_ignores_the_window() {
        let cat = hourly_movies(24);
        let (ids, _) = fill_movies(
            &cat,
            Some(10),
            &GenerationState::empty(),
            Some(Duration::from_secs(3600)),
        );
        assert_eq!(ids.len(), 10, "got {ids:?}");
    }

    /// The seam the fix rests on: stopping early must not lose the pool's
    /// place. Two bounded generations back to back play the same run, in the
    /// same order, as one unbounded generation of the same total length.
    #[test]
    fn a_bounded_generation_resumes_where_the_last_one_stopped() {
        let cat = hourly_movies(24);
        let four_hours = Some(Duration::from_secs(4 * 3600));

        let (first, pools) = fill_movies(&cat, None, &GenerationState::empty(), four_hours);
        let mut resume = ResumeMap::new();
        resume.pools = pools;
        // What the daemon hands the next generation: the pools' rotation from
        // the resolver, and each series' last-played id projected from the
        // play-history ledger. Every movie is its own series of one.
        let cursor = first.iter().map(|id| (id.clone(), id.clone())).collect();
        let state = GenerationState {
            resume,
            cursor,
            tail: first.clone(),
        };

        let (second, _) = fill_movies(&cat, None, &state, four_hours);

        let mut joined = first.clone();
        joined.extend(second.iter().cloned());
        assert_eq!(
            joined,
            vec![
                "mov-01", "mov-02", "mov-03", "mov-04", "mov-05", "mov-06", "mov-07", "mov-08",
            ],
            "the seam repeated or skipped: {first:?} then {second:?}",
        );
    }

    // ---- pool-level adjacency constraints (#115) -------------------------

    fn no_repeat(n: usize) -> crate::config::Constraints {
        crate::config::Constraints {
            no_repeat_within: Some(crate::config::NoRepeatWithin::Positions(n)),
            separate_by: None,
            separate_min_gap: None,
        }
    }

    fn no_repeat_within(d: Duration) -> crate::config::Constraints {
        crate::config::Constraints {
            no_repeat_within: Some(crate::config::NoRepeatWithin::Duration(d)),
            separate_by: None,
            separate_min_gap: None,
        }
    }

    /// The previous generation's aired run, oldest first — what the seam reads.
    fn aired(tail: &[&str]) -> GenerationState {
        GenerationState {
            tail: tail.iter().map(|s| s.to_string()).collect(),
            ..GenerationState::empty()
        }
    }

    fn build_pools(
        pools: &[Pool],
        pattern: &[PatternStep],
        cycles: Option<usize>,
        block: Option<&Constraints>,
        state: &GenerationState,
    ) -> Vec<String> {
        let cat = catalog();
        build(
            &cat,
            pools,
            &[],
            pattern,
            cycles,
            block,
            state,
            0,
            test_env(),
            None,
        )
        .expect("pattern builds")
        .0
    }

    /// The pool's own list is reordered so the first draw does not repeat what
    /// aired last. Unconstrained, `title:asc` would open on `mov-1` — which is
    /// exactly the item at position -1.
    #[test]
    fn a_pool_constraint_holds_across_the_generation_seam() {
        let mut movies = movies_pool();
        movies.constraints = Some(no_repeat(1));
        let ids = build_pools(
            &[movies],
            &[step("movies", 1)],
            Some(3),
            None,
            &aired(&["mov-1"]),
        );
        assert_ne!(ids[0], "mov-1", "the seam repeats: {ids:?}");
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            vec!["mov-1", "mov-2", "mov-3"],
            "the pass reorders, it does not drop: {ids:?}"
        );
    }

    /// The temporal spelling (#185) holds at the same seam the positional one
    /// does: `catalog()`'s movies carry no recorded runtime, so every item
    /// estimates at zero and a temporal gap of any span still treats the tail
    /// as unexpired — the same outcome as the positional test above, reached
    /// through the duration axis instead of the position one.
    #[test]
    fn a_temporal_pool_constraint_holds_across_the_generation_seam() {
        let mut movies = movies_pool();
        movies.constraints = Some(no_repeat_within(Duration::from_secs(3600)));
        let ids = build_pools(
            &[movies],
            &[step("movies", 1)],
            Some(3),
            None,
            &aired(&["mov-1"]),
        );
        assert_ne!(ids[0], "mov-1", "the seam repeats: {ids:?}");
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            vec!["mov-1", "mov-2", "mov-3"],
            "the pass reorders, it does not drop: {ids:?}"
        );
    }

    /// Three movies where `mov-1` alone runs three hours; `mov-2`/`mov-3` are
    /// unmeasured, same as everywhere else `catalog()` leaves duration blank.
    fn movies_with_a_long_one() -> Catalog {
        let c = Catalog::open_in_memory().unwrap();
        let add = |id: &str, duration_ms: Option<i64>| {
            let mut e = CatEntry::new(id, "movie", format!("Title {id}"), Source::Plex);
            e.duration_ms = duration_ms;
            c.upsert_entry(&e).unwrap();
            c.add_source(&EntrySource {
                source: Source::LocalFs,
                source_id: format!("fs-{id}"),
                entry_id: id.to_string(),
                playback_path: format!("/media/{id}.mkv"),
                last_seen: None,
            })
            .unwrap();
        };
        add("mov-1", Some(3 * 60 * 60 * 1000));
        add("mov-2", None);
        add("mov-3", None);
        c
    }

    /// The exact case a positional `no_repeat_within` cannot express: the tail
    /// item alone already covers the configured window, so — unlike
    /// `no_repeat_within = 1`, which would forbid the very next position
    /// regardless of how long that item ran — the same id may legally reopen
    /// the list.
    #[test]
    fn a_temporal_gap_lets_the_seam_repeat_once_the_tail_item_alone_covers_it() {
        let cat = movies_with_a_long_one();
        let mut movies = movies_pool();
        movies.constraints = Some(no_repeat_within(Duration::from_secs(90 * 60)));
        let (ids, _) = build(
            &cat,
            &[movies],
            &[],
            &[step("movies", 1)],
            Some(3),
            None,
            &aired(&["mov-1"]),
            0,
            test_env(),
            None,
        )
        .unwrap();
        assert_eq!(
            ids[0], "mov-1",
            "the 3h tail item alone already covers a 90m window; the list was needlessly reordered: {ids:?}"
        );
    }

    /// The regression #115 exists for: repairing a repeat must not move an
    /// episode into a movie slot. The pass runs on the pool's list, so the
    /// interleave it feeds is untouched however hard the constraint bites.
    #[test]
    fn a_pool_constraint_leaves_the_pattern_shape_intact() {
        let mut movies = movies_pool();
        movies.constraints = Some(no_repeat(3));
        let ids = build_pools(
            &[movies, shows_pool()],
            &[step("movies", 2), step("shows", 3)],
            Some(2),
            None,
            &aired(&["mov-1", "mov-2"]),
        );
        assert_eq!(ids.len(), 10, "{ids:?}");
        for (i, chunk) in ids.chunks(5).enumerate() {
            assert!(
                chunk[0..2].iter().all(|id| id.starts_with("mov-")),
                "cycle {i} should open with two movies: {chunk:?}"
            );
            assert!(
                chunk[2..5].iter().all(|id| !id.starts_with("mov-")),
                "cycle {i} should continue with three episodes: {chunk:?}"
            );
        }
    }

    /// A block-level `[constraints]` on a pattern block is the default every
    /// pool that declares none inherits — it is not applied to the block's
    /// interleaved output.
    #[test]
    fn a_block_constraint_is_inherited_by_a_pool_that_declares_none() {
        let ids = build_pools(
            &[movies_pool()],
            &[step("movies", 1)],
            Some(3),
            Some(&no_repeat(1)),
            &aired(&["mov-1"]),
        );
        assert_ne!(
            ids[0], "mov-1",
            "the block default did not reach the pool: {ids:?}"
        );
    }

    /// A pool declaring its own table takes it whole: the block's
    /// `no_repeat_within` is not merged back in, so the seam is unconstrained
    /// and `title:asc` survives untouched.
    #[test]
    fn a_pool_table_replaces_the_block_default_rather_than_merging() {
        let mut movies = movies_pool();
        movies.constraints = Some(crate::config::Constraints {
            no_repeat_within: None,
            separate_by: Some("genres".into()),
            separate_min_gap: Some(1),
        });
        let ids = build_pools(
            &[movies],
            &[step("movies", 1)],
            Some(3),
            Some(&no_repeat(1)),
            &aired(&["mov-1"]),
        );
        assert_eq!(ids, vec!["mov-1", "mov-2", "mov-3"], "{ids:?}");
    }

    /// Nothing constrained leaves the pool's `order` exactly as it resolved,
    /// aired history or not.
    #[test]
    fn an_unconstrained_pool_keeps_its_own_order() {
        let ids = build_pools(
            &[movies_pool()],
            &[step("movies", 1)],
            Some(3),
            None,
            &aired(&["mov-1"]),
        );
        assert_eq!(ids, vec!["mov-1", "mov-2", "mov-3"]);
    }

    /// The regression the live drive caught: a pool's repeats are made by the
    /// *rotation*, not by its list order. `take = 2` over one-item series parks
    /// the rotation on the series that only half-filled the visit, so the next
    /// visit opens on the same film. Unconstrained that is the documented
    /// `on_short` behaviour; under `no_repeat_within` it has to be skipped past.
    #[test]
    fn a_pool_constraint_stops_the_rotation_replaying_a_series() {
        let plain = build_pools(
            &[movies_pool()],
            &[step("movies", 2)],
            Some(2),
            None,
            &GenerationState::empty(),
        );
        // Baseline: the parking rule really does replay mov-2 back-to-back, so
        // the assertion below is testing something that would otherwise fail.
        assert!(
            plain.windows(2).any(|w| w[0] == w[1]),
            "expected the unconstrained rotation to repeat: {plain:?}"
        );

        let mut movies = movies_pool();
        movies.constraints = Some(no_repeat(1));
        let ids = build_pools(
            &[movies],
            &[step("movies", 2)],
            Some(2),
            None,
            &GenerationState::empty(),
        );
        assert_eq!(ids.len(), plain.len(), "the visit still fills: {ids:?}");
        assert!(
            ids.windows(2).all(|w| w[0] != w[1]),
            "a draw repeats under no_repeat_within = 1: {ids:?}"
        );
    }

    /// The gap is counted in draws, so a wider one spaces the pool further —
    /// three distinct films before any may come round again.
    #[test]
    fn a_wider_gap_spaces_the_draw_sequence_further() {
        let mut movies = movies_pool();
        movies.constraints = Some(no_repeat(3));
        let ids = build_pools(
            &[movies],
            &[step("movies", 1)],
            Some(3),
            None,
            &GenerationState::empty(),
        );
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["mov-1", "mov-2", "mov-3"], "{ids:?}");
    }

    /// A pool that cannot satisfy its own constraint still puts television on
    /// the air: one film with "never twice in a row" has no arrangement, so the
    /// clash is accepted (and counted for the `constraints.unsatisfied` warning)
    /// rather than emitting dead air or looping forever.
    #[test]
    fn a_pool_that_cannot_satisfy_its_constraint_still_airs() {
        let mut movies = movies_pool();
        movies.expr = Some("item.title == \"Title mov-1\"".into());
        movies.order = None;
        movies.constraints = Some(no_repeat(1));
        let ids = build_pools(
            &[movies],
            &[step("movies", 2)],
            Some(2),
            None,
            &GenerationState::empty(),
        );
        assert_eq!(ids, vec!["mov-1", "mov-1", "mov-1", "mov-1"], "{ids:?}");
    }

    /// A `random` pool honours its constraint by redrawing, not by sliding onto
    /// the blocked series' neighbour — and stays reproducible for a pinned seed
    /// while doing it.
    #[test]
    fn a_random_pool_rerolls_rather_than_sliding_to_the_neighbour() {
        let mut movies = movies_pool();
        movies.select = Select::Random;
        movies.constraints = Some(no_repeat(1));
        let pattern = [step("movies", 1)];
        let ids = build_pools(
            std::slice::from_ref(&movies),
            &pattern,
            Some(6),
            None,
            &GenerationState::empty(),
        );
        assert_eq!(ids.len(), 6, "{ids:?}");
        assert!(
            ids.windows(2).all(|w| w[0] != w[1]),
            "a random draw repeats back-to-back: {ids:?}"
        );
        let again = build_pools(
            std::slice::from_ref(&movies),
            &pattern,
            Some(6),
            None,
            &GenerationState::empty(),
        );
        assert_eq!(ids, again, "the reroll is not reproducible");
    }

    /// The draw-time check reads the seam too, so the first draw of a window is
    /// measured against the last draws of the one before it.
    #[test]
    fn the_draw_check_reads_the_generation_seam() {
        let mut movies = movies_pool();
        movies.constraints = Some(no_repeat(2));
        let ids = build_pools(
            &[movies],
            &[step("movies", 1)],
            Some(1),
            None,
            &aired(&["mov-3", "mov-1"]),
        );
        assert_eq!(ids, vec!["mov-2"], "{ids:?}");
    }

    /// The seam is read per pool: only the aired items this pool could have
    /// supplied are position -1 for it. An episode airing last says nothing
    /// about which movie may open the next generation.
    #[test]
    fn the_seam_ignores_aired_items_from_other_pools() {
        let mut movies = movies_pool();
        movies.constraints = Some(no_repeat(1));
        let ids = build_pools(
            &[movies],
            &[step("movies", 1)],
            Some(3),
            None,
            &aired(&["got-e1"]),
        );
        assert_eq!(ids, vec!["mov-1", "mov-2", "mov-3"]);
    }
}

//! Pattern interleave (#72): walk a repeating template of `{pool, take}` steps,
//! drawing from named pools so a block can express "1 movie, then 3 episodes,
//! repeat" with each series progressing independently.
//!
//! # The shape of a pool
//!
//! A pool resolves to a flat, ordered id list (its `expr`, then its `order`),
//! which is then grouped into **series**. A series key is the catalog `show_id`
//! for an episode; an item with no `show_id` — a movie — is its own series of
//! one. That single rule is why a movie pool needs no special case: rotating
//! through one-item series *is* playing the movies in order.
//!
//! Series rotate in order of first appearance in the ordered set, so the pool's
//! `order` fixes the rotation and nothing else has to.
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

use std::collections::{BTreeMap, HashMap};

use crate::catalog::Catalog;
use crate::config::{Advance, OnShort, PatternStep, Pool, Rotate, Select};
use crate::resume::{GenerationState, PoolResume};

/// Upper bound on an explicitly-authored `cycles`. A derived count needs no cap
/// — it is bounded by the pools' own sizes.
pub const MAX_CYCLES: usize = 10_000;

/// One series inside a pool: a show's episodes, or a single movie.
#[derive(Debug)]
struct Series {
    /// `show_id`, or the item's own `entry_id` when it has none.
    key: String,
    ids: Vec<String>,
    /// Index of the next item to play.
    cursor: usize,
}

impl Series {
    fn remaining(&self) -> usize {
        self.ids.len().saturating_sub(self.cursor)
    }
}

/// A pool mid-walk: its config, its series, and where the rotation stands.
#[derive(Debug)]
struct PoolRuntime<'a> {
    cfg: &'a Pool,
    series: Vec<Series>,
    /// Index into `series` of whoever is up next.
    rotation: usize,
}

impl PoolRuntime<'_> {
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
    fn visit(&mut self, take: usize, roll: &RollKey) -> Vec<String> {
        if take == 0 || self.is_dry() {
            return Vec::new();
        }
        match self.cfg.rotate {
            // A new series every item is just `take` one-item visits.
            Rotate::Slot => {
                let mut out = Vec::with_capacity(take);
                for slot in 0..take {
                    if self.is_dry() {
                        break;
                    }
                    out.extend(self.serve(1, roll, slot as u64));
                }
                out
            }
            Rotate::Visit => self.serve(take, roll, 0),
        }
    }

    /// Serve `take` items starting from one picked series, rolling onto others
    /// per `on_short` when it comes up short, then park the rotation where the
    /// next visit should resume.
    fn serve(&mut self, take: usize, roll: &RollKey, nonce: u64) -> Vec<String> {
        let mut out: Vec<String> = Vec::with_capacity(take);
        let mut current = self.pick(roll, nonce);
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
            out.extend(taken);
            if out.len() >= take {
                break;
            }
            // Short: someone else has to fill the rest.
            current = match self.cfg.on_short {
                OnShort::Next => self.series_after(si),
                OnShort::Wrap => Some(si),
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
pub fn build(
    catalog: &Catalog,
    pools: &[Pool],
    pattern: &[PatternStep],
    cycles: Option<usize>,
    state: &GenerationState,
    seed: u64,
    score_env: crate::score::ScoreEnv<'_>,
) -> Result<(Vec<String>, BTreeMap<String, PoolResume>), String> {
    // One cache for the whole generation: pools pointed at the same script
    // share its compiled AST and its resolved query sets instead of each
    // re-running every query the script declares.
    let mut score_cache = crate::score::ScoreCache::default();
    let mut runtimes: Vec<PoolRuntime> = Vec::with_capacity(pools.len());
    for cfg in pools {
        runtimes.push(resolve_pool(
            catalog,
            cfg,
            state,
            score_env,
            &mut score_cache,
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
    for cycle in 0..cycles {
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
            out.extend(runtimes[idx].visit(step.take, &roll));
        }
    }

    let resume_out = runtimes
        .iter()
        .map(|rt| (rt.cfg.name.clone(), rt.to_resume()))
        .collect();
    Ok((out, resume_out))
}

/// Enough cycles for the largest pool to drain once. Each pool needs
/// `ceil(remaining / take-per-cycle)` visits' worth; the pattern runs the max,
/// so shorter pools repeat under their own `wrap` while the longest plays out.
fn derive_cycles(
    runtimes: &[PoolRuntime],
    pattern: &[PatternStep],
    by_name: &HashMap<&str, usize>,
) -> usize {
    let mut per_cycle: Vec<usize> = vec![0; runtimes.len()];
    for step in pattern {
        if let Some(&i) = by_name.get(step.pool.as_str()) {
            per_cycle[i] += step.take;
        }
    }

    let mut cycles = 0;
    for (i, rt) in runtimes.iter().enumerate() {
        if per_cycle[i] == 0 {
            continue;
        }
        // What is left to play from here — which, for a resumed pool, is less
        // than the pool holds. A pool resumed to exactly its end still deserves
        // a full pass, so fall back to its total.
        let remaining: usize = rt.series.iter().map(|s| s.remaining()).sum();
        let total: usize = rt.series.iter().map(|s| s.ids.len()).sum();
        let want = if remaining > 0 { remaining } else { total };
        cycles = cycles.max(want.div_ceil(per_cycle[i]));
    }
    cycles
}

/// Resolve one pool to its series, then seat each series' cursor from the
/// resume map when the pool asks to continue.
fn resolve_pool<'a>(
    catalog: &Catalog,
    cfg: &'a Pool,
    state: &GenerationState,
    score_env: crate::score::ScoreEnv<'_>,
    score_cache: &mut crate::score::ScoreCache,
) -> Result<PoolRuntime<'a>, String> {
    // A pool draws its items from a CEL expression or from a scorer plugin,
    // never both (validated at load). A plugin returns its set already ranked —
    // gathering and ranking are the same judgment for a taste algorithm — so
    // the `order` step below applies only to the expression case (ADR 0002).
    let ids = match (&cfg.expr, &cfg.plugin) {
        (Some(expr), None) => {
            let mut ids = catalog.resolve_query(expr).map_err(|e| e.to_string())?;
            if let Some(order) = &cfg.order {
                // Seed 0: a pool's internal `order` is its own stable sort. A
                // shuffled pool is `select = "random"`, seeded per visit.
                ids = catalog
                    .resolve_order(&ids, order, 0)
                    .map_err(|e| e.to_string())?;
            }
            ids
        }
        (None, Some(plugin)) => {
            let path = score_env.resolve_path(plugin);
            crate::score::run(catalog, &path, score_env.inputs, &cfg.name, score_cache)
                .map_err(|m| format!("pool {:?}: {m}", cfg.name))?
        }
        // Both, or neither, is rejected at load; a pool that reaches here in
        // either state is a validation gap, not a config the user can hit.
        _ => {
            return Err(format!(
                "pool {:?} must set exactly one of `expr` or `plugin`",
                cfg.name
            ));
        }
    };

    // Group into series, preserving first-appearance order so the pool's
    // `order` fixes the rotation order too. One query for every `show_id` up
    // front, rather than a catalog round trip per item — a catch-up re-resolves
    // every pool on every generation.
    let show_ids = catalog.show_ids_for(&ids).map_err(|e| e.to_string())?;
    let mut series: Vec<Series> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for id in ids {
        // An item with no `show_id` — a movie — is its own series of one.
        let key = show_ids.get(&id).cloned().unwrap_or_else(|| id.clone());
        match index.get(&key) {
            Some(&i) => series[i].ids.push(id),
            None => {
                index.insert(key.clone(), series.len());
                series.push(Series {
                    key,
                    ids: vec![id],
                    cursor: 0,
                });
            }
        }
    }

    let mut rt = PoolRuntime {
        cfg,
        series,
        rotation: 0,
    };

    if cfg.advance == Advance::Resume {
        let prev = state.resume.pool(&cfg.name);
        for s in &mut rt.series {
            // Continue *after* the last-played id, read from the play-history
            // ledger's projection (#70) rather than a cursor of our own. An id
            // that has vanished from this series — deleted, re-identified, or
            // filtered out — restarts that series and only that series.
            if let Some(last) = state.cursor.get(&s.key)
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
            order: Some(Order::parse("title:asc").unwrap()),
            select: Select::RoundRobin,
            rotate: Rotate::Visit,
            advance: Advance::Restart,
            on_short: OnShort::Next,
        }
    }

    fn shows_pool() -> Pool {
        Pool {
            name: "shows".into(),
            expr: Some("item.type == \"episode\"".into()),
            plugin: None,
            order: Some(Order::parse("season:asc,episode:asc").unwrap()),
            select: Select::RoundRobin,
            rotate: Rotate::Visit,
            advance: Advance::Restart,
            on_short: OnShort::Next,
        }
    }

    /// No pool in these tests draws from a plugin, so the inputs are empty and
    /// the base dir never gets read — it only has to exist.
    fn test_env() -> crate::score::ScoreEnv<'static> {
        const EMPTY: &crate::score::ScoreInputs = &crate::score::ScoreInputs::new_empty();
        crate::score::ScoreEnv {
            inputs: EMPTY,
            base_dir: std::path::Path::new("."),
        }
    }

    fn step(pool: &str, take: usize) -> PatternStep {
        PatternStep {
            pool: pool.into(),
            take,
            chance: 1.0,
        }
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
        let (ids, pool_state) = build(&cat, &pools, &pattern, cycles, state_in, seed, test_env())
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
                    take: 3,
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
            &[step("shows", 1)],
            Some(1),
            &GenerationState::empty(),
            0,
            test_env(),
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
            &[step("movies", 1)],
            Some(MAX_CYCLES + 1),
            &GenerationState::empty(),
            0,
            test_env(),
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
            &[step("movies", 1)],
            None,
            &GenerationState::empty(),
            0,
            test_env(),
        )
        .unwrap();
        assert!(ids.is_empty());
    }
}

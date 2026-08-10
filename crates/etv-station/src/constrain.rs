//! Post-order adjacency constraint pass (#73).
//!
//! The pipeline is `resolve → duplicates → order → constraint pass → emit`.
//! `duplicates` is identity over the whole list and play-history is identity
//! over time; this pass is the third axis — identity over *adjacent positions*.
//!
//! Two constraints, both spacing rules, differing only in what counts as a
//! clash:
//!
//! - **`no_repeat_within = N`** — identity. The same `entry_id` may not recur
//!   within N positions (`N = 1` = no back-to-back), **or** within a span of
//!   wall-clock time when `N` is spelled as a duration (`"24h"`, #185) —
//!   measured against the emitted schedule rather than list position.
//! - **`separate_by = "<field>"` + `separate_min_gap = N`** — property. Two
//!   items sharing **any** value of a multi-valued field may not sit within N
//!   positions, so `separate_by: "cast"` spreads out films sharing a performer.
//!   Always positional; #185 only gave `no_repeat_within` a temporal spelling.
//!
//! # Two axes, one distance
//!
//! Every conflict check answers one question — "how far apart are these two?"
//! — on two axes at once: [`Distance::positions`] (list positions, what the
//! pass always measured) and [`Distance::elapsed`] (wall-clock time, #185).
//! `elapsed` is estimated, not observed: an item's own [`ItemKeys::duration`]
//! is filled in by the caller from the catalog before this module ever sees
//! it, the same estimate [`crate::resolve::estimated_runtimes`] already uses
//! to size a generation — so a temporal gap is honoured to the same precision
//! the rest of the station already schedules by, not to the second. A purely
//! positional pass never reads `duration` at all, and every existing config
//! — which only ever wrote positions — produces the identical schedule it did
//! before #185, byte for byte: `Distance::elapsed` never becomes the deciding
//! factor unless something asked for it in wall-clock terms.
//!
//! # Resolution
//!
//! A deterministic greedy: walk the ordered list, and whenever the next item
//! would violate a constraint, defer it and take the first item behind it that
//! would not. A swap-repair follows, in case the greedy consumed every
//! alternative and reached a position holding only clashing items. When nothing
//! improves — an all-one-title pool with `no_repeat_within = 1`, or a cast so
//! interlinked no arrangement separates it — the remaining violations are
//! accepted rather than looped on forever, and logged so a channel that is
//! quietly failing its constraint is distinguishable from one that is not.
//!
//! # The seam
//!
//! [`crate::rule::Sequential`] plays a list once and the next generation lays a
//! fresh list after it, so the constraints reach *backwards across that
//! boundary*: the first item of this list airs immediately after the last item
//! of the previous one. `preceding` carries that tail — the most recently aired
//! items, oldest first — projected from the play-history ledger
//! ([`crate::history::Ledger::tail`]).
//!
//! The list is emphatically **not** circular. Position `n-1` and position `0` of
//! one generation never air next to each other; `n-1` is followed by whatever
//! the *next* generation resolves first, which this pass will constrain when it
//! runs for that generation with this list's tail as its `preceding`.

use std::collections::VecDeque;
use std::time::Duration;

/// How much aired history to carry when the caller has no channel config to
/// size it from — the stateless [`crate::resolve::resolve_channel`] path and
/// tests. The daemon instead asks for exactly what the config reaches back
/// (`ChannelConfig::adjacency_reach`), so a wide `separate_min_gap` is enforced
/// at the seam rather than silently truncated.
pub const DEFAULT_SEAM_TAIL: usize = 64;

/// Give up after this many improving swaps. Each one strictly lowers the
/// violation count, so this can only bind on a pathological list; it exists so
/// a future change to the objective cannot turn the repair into a hang.
const MAX_REPAIR_ROUNDS: usize = 10_000;

/// What the pass needs to know about one item: its identity, the values of the
/// field being separated on (empty when nothing is), and how long it runs.
///
/// `duration` is read only by a temporal `no_repeat_within` (#185) — it is how
/// [`Distance::elapsed`] is built while walking the list backward. A purely
/// positional pass never looks at it, so it defaults to zero everywhere a
/// caller has no duration to offer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemKeys {
    pub id: String,
    pub group: Vec<String>,
    pub duration: Duration,
}

impl ItemKeys {
    /// An item with identity only — nothing to separate on, and no known
    /// duration (fine unless something here is measured in time).
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            group: Vec::new(),
            duration: Duration::ZERO,
        }
    }

    fn shares_group_with(&self, other: &Self) -> bool {
        !self.group.is_empty()
            && !other.group.is_empty()
            && self.group.iter().any(|v| other.group.contains(v))
    }
}

/// How far apart repeats of the same `entry_id` must be kept — the algorithm's
/// own copy of the two spellings [`crate::config::NoRepeatWithin`] accepts.
/// `Positions(0)` is the unconstrained sentinel, matching `separate`'s `0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatGap {
    Positions(usize),
    Duration(Duration),
}

impl Default for RepeatGap {
    fn default() -> Self {
        RepeatGap::Positions(0)
    }
}

/// How far back one constraint reaches, on both axes at once. `positions`
/// folds together a positional `no_repeat` and `separate` — both are
/// position-only. `duration` is `no_repeat`'s span when it is temporal,
/// `Duration::ZERO` otherwise.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reach {
    pub positions: usize,
    pub duration: Duration,
}

impl Reach {
    fn is_unconstrained(self) -> bool {
        self.positions == 0 && self.duration.is_zero()
    }

    fn widen(self, other: Self) -> Self {
        Reach {
            positions: self.positions.max(other.positions),
            duration: self.duration.max(other.duration),
        }
    }
}

/// How far one item's two constraints reach. The unconstrained value when
/// neither is set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limits {
    pub no_repeat: RepeatGap,
    pub separate: usize,
}

impl Limits {
    /// This item's reach on both axes at once.
    pub fn reach(&self) -> Reach {
        let (no_repeat_positions, no_repeat_duration) = match self.no_repeat {
            RepeatGap::Positions(n) => (n, Duration::ZERO),
            RepeatGap::Duration(d) => (0, d),
        };
        Reach {
            positions: no_repeat_positions.max(self.separate),
            duration: no_repeat_duration,
        }
    }

    pub fn is_unconstrained(&self) -> bool {
        self.reach().is_unconstrained()
    }
}

/// The result of one pass: the ordering, and how many constraint violations
/// could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ordering {
    pub order: Vec<usize>,
    /// Violations left after the repair gave up. `0` on a satisfied list.
    pub unresolved: usize,
}

/// Order `items` so that no two conflict within their limits. `limits[i]` is
/// item `i`'s own settings, so blocks configured differently can be
/// concatenated and constrained in one pass. `preceding` is the tail of what
/// already aired, oldest first — the item at its end is position `-1` relative
/// to this list.
///
/// Deterministic: the same inputs always yield the same ordering.
pub fn order_constrained(
    items: &[ItemKeys],
    limits: &[Limits],
    preceding: &[ItemKeys],
) -> Ordering {
    debug_assert_eq!(items.len(), limits.len(), "items/limits length mismatch");

    let total = items.len();
    let bound = limits
        .iter()
        .map(Limits::reach)
        .fold(Reach::default(), Reach::widen);
    if bound.is_unconstrained() || total == 0 {
        return Ordering {
            order: (0..total).collect(),
            unresolved: 0,
        };
    }

    let mut pending: VecDeque<usize> = (0..total).collect();
    let mut out: Vec<usize> = Vec::with_capacity(total);

    while !pending.is_empty() {
        let pick = pending
            .iter()
            .position(|&cand| !violates(&out, cand, items, limits, bound, preceding))
            // Nothing left is eligible, so every remaining choice violates.
            // Take the head — accepting the violation keeps generation
            // deterministic and finite instead of hanging.
            .unwrap_or(0);
        let cand = pending
            .remove(pick)
            .expect("pick index came from the queue itself");
        out.push(cand);
    }

    let unresolved = repair(&mut out, items, limits, bound, preceding);
    Ordering {
        order: out,
        unresolved,
    }
}

/// How far a history item sits from whatever plays next, on both axes at
/// once: `positions` is the position count the pass has always measured;
/// `elapsed` is that same distance in wall-clock time — the item's own
/// duration, plus everything that airs after it, up to (not including)
/// whatever it is being measured against.
#[derive(Debug, Clone, Copy, Default)]
struct Distance {
    positions: usize,
    elapsed: Duration,
}

/// Whether both axes of `bound` have been left behind at `(positions,
/// elapsed)` — the walk may stop, because nothing farther back can be any
/// closer on either axis (a duration is never negative). An axis `bound`
/// leaves at zero is not "reachable at distance zero", it is *off* — reported
/// as exhausted immediately rather than never, or a zero-length `bound` would
/// hold the walk open forever instead of ending it at once.
fn axes_exhausted(positions: usize, elapsed: Duration, bound: Reach) -> bool {
    (positions > bound.positions) && (bound.duration.is_zero() || elapsed > bound.duration)
}

/// Whether `gap` alone puts `d` inside its reach.
fn gap_conflict(gap: RepeatGap, d: Distance) -> bool {
    match gap {
        RepeatGap::Positions(n) => n > 0 && d.positions <= n,
        RepeatGap::Duration(within) => !within.is_zero() && d.elapsed <= within,
    }
}

/// Whether two items conflict at `d` apart, under the stricter of their two
/// limits per axis — either side's own setting is enough to call it a
/// conflict, since a repeat that only violates one side's rule is still a
/// repeat that rule was written to stop.
fn conflict(a: &ItemKeys, b: &ItemKeys, la: Limits, lb: Limits, d: Distance) -> bool {
    (a.id == b.id && (gap_conflict(la.no_repeat, d) || gap_conflict(lb.no_repeat, d)))
        || (d.positions <= la.separate.max(lb.separate) && a.shares_group_with(b))
}

/// Walk `history` backward from its end (most-recent-first), pairing each
/// item with its [`Distance`] from whatever plays next, and stopping the
/// moment [`axes_exhausted`] says neither axis of `bound` could still be
/// reached.
fn walk_back(history: &[ItemKeys], bound: Reach) -> impl Iterator<Item = (&ItemKeys, Distance)> {
    let mut elapsed = Duration::ZERO;
    history
        .iter()
        .rev()
        .enumerate()
        .map_while(move |(back, item)| {
            elapsed += item.duration;
            let positions = back + 1;
            if axes_exhausted(positions, elapsed, bound) {
                None
            } else {
                Some((item, Distance { positions, elapsed }))
            }
        })
}

/// How many of `history`'s trailing items (oldest-first) a conflict check
/// against `limits` could still reach — the same walk [`conflicts_with_recent`]
/// performs, counted rather than checked. Exposed so a caller holding its own
/// recency window (#115's per-pool `recent`) can trim to exactly this and no
/// more, on both axes at once.
pub fn history_needed(history: &[ItemKeys], limits: Limits) -> usize {
    if limits.is_unconstrained() {
        return 0;
    }
    walk_back(history, limits.reach()).count()
}

/// Whether placing `cand` right after `history` would conflict with something
/// in it — the emitted-sequence check ([`crate::pattern`]'s draw loop, #115),
/// where a pool's repeats are made by the rotation rather than by its list
/// order. `history` is oldest-first, so its last element is position `-1`
/// relative to `cand`.
///
/// Only `cand`'s own limits apply: what is emitted is emitted, and the
/// settings that produced it are not ours to revisit — the same rule the seam
/// half of [`violates`] follows.
pub fn conflicts_with_recent(recent: &[ItemKeys], cand: &ItemKeys, limits: Limits) -> bool {
    if limits.is_unconstrained() {
        return false;
    }
    walk_back(recent, limits.reach())
        .any(|(prev, d)| conflict(cand, prev, limits, Limits::default(), d))
}

/// Would placing `cand` at position `out.len()` conflict with something? Looks
/// back through what this pass has already placed, then on into `preceding`
/// once this list runs out.
fn violates(
    out: &[usize],
    cand: usize,
    items: &[ItemKeys],
    limits: &[Limits],
    bound: Reach,
    preceding: &[ItemKeys],
) -> bool {
    let me = &items[cand];
    let mine = limits[cand];
    let mut positions = 0usize;
    let mut elapsed = Duration::ZERO;

    for &placed in out.iter().rev() {
        positions += 1;
        elapsed += items[placed].duration;
        if axes_exhausted(positions, elapsed, bound) {
            // Nothing farther back — in `out` or across the seam — can be any
            // closer than this on either axis, and `bound` is the widest any
            // item here reaches.
            return false;
        }
        let d = Distance { positions, elapsed };
        if conflict(me, &items[placed], mine, limits[placed], d) {
            return true;
        }
    }

    // Across the seam. Only the candidate's own limits apply from here — the
    // previous generation is already emitted and its settings are not ours to
    // revisit. `positions`/`elapsed` carry on from wherever the walk through
    // `out` left off, so a short new list still reaches back at the right
    // offset.
    let mine_reach = mine.reach();
    for prev in preceding.iter().rev() {
        positions += 1;
        elapsed += prev.duration;
        if axes_exhausted(positions, elapsed, mine_reach) {
            break;
        }
        let d = Distance { positions, elapsed };
        if conflict(me, prev, mine, Limits::default(), d) {
            return true;
        }
    }

    false
}

/// Swap-repair whatever the forward greedy could not place — a position it
/// reached holding only conflicting items. Returns the violations still
/// standing when no swap improves matters.
fn repair(
    order: &mut [usize],
    items: &[ItemKeys],
    limits: &[Limits],
    bound: Reach,
    preceding: &[ItemKeys],
) -> usize {
    let n = order.len();
    let mut best = violation_count(order, items, limits, bound, preceding);

    for _ in 0..MAX_REPAIR_ROUNDS {
        if best == 0 {
            return 0;
        }
        let mut improved = false;
        // Only positions that actually clash are worth moving, and there are
        // usually a handful at most — the greedy has done the bulk already.
        'search: for i in violating_positions(order, items, limits, bound, preceding) {
            for j in 0..n {
                if i == j {
                    continue;
                }
                order.swap(i, j);
                let count = violation_count(order, items, limits, bound, preceding);
                if count < best {
                    best = count;
                    improved = true;
                    break 'search;
                }
                order.swap(i, j);
            }
        }
        // No swap helps: this list cannot do better, so accept what is left.
        if !improved {
            break;
        }
    }
    best
}

/// Cumulative duration of `order[0..i]`, one entry per prefix — `prefix[0] ==
/// Duration::ZERO`, `prefix[order.len()]` is the whole list's estimated span.
/// Shared by [`violation_count`] and [`violating_positions`] so a forward
/// distance from any position is one subtraction rather than a re-walk.
fn duration_prefix(order: &[usize], items: &[ItemKeys]) -> Vec<Duration> {
    let mut prefix = Vec::with_capacity(order.len() + 1);
    prefix.push(Duration::ZERO);
    let mut total = Duration::ZERO;
    for &idx in order {
        total += items[idx].duration;
        prefix.push(total);
    }
    prefix
}

/// How many ordered pairs conflict, counting the seam against `preceding`. Used
/// as a monotone objective for [`repair`], and reported so the caller can say
/// that a channel is airing a schedule its constraints do not fully hold on.
fn violation_count(
    order: &[usize],
    items: &[ItemKeys],
    limits: &[Limits],
    bound: Reach,
    preceding: &[ItemKeys],
) -> usize {
    let n = order.len();
    let prefix = duration_prefix(order, items);
    let mut count = 0;
    for i in 0..n {
        let a = order[i];
        for d in 1.. {
            if i + d >= n {
                break;
            }
            let elapsed = prefix[i + d] - prefix[i];
            if axes_exhausted(d, elapsed, bound) {
                break;
            }
            let b = order[i + d];
            let dist = Distance {
                positions: d,
                elapsed,
            };
            if conflict(&items[a], &items[b], limits[a], limits[b], dist) {
                count += 1;
            }
        }

        let mine_reach = limits[a].reach();
        let mut positions = i;
        let mut elapsed = prefix[i];
        for prev in preceding.iter().rev() {
            positions += 1;
            elapsed += prev.duration;
            if axes_exhausted(positions, elapsed, mine_reach) {
                break;
            }
            let dist = Distance { positions, elapsed };
            if conflict(&items[a], prev, limits[a], Limits::default(), dist) {
                count += 1;
            }
        }
    }
    count
}

/// Positions holding an item that conflicts with a neighbour, ascending. Only
/// looks *forward* from each position — a conflicting pair is reported once,
/// at the earlier of the two, which is all [`repair`] needs to find and move
/// it.
fn violating_positions(
    order: &[usize],
    items: &[ItemKeys],
    limits: &[Limits],
    bound: Reach,
    preceding: &[ItemKeys],
) -> Vec<usize> {
    let n = order.len();
    let prefix = duration_prefix(order, items);
    (0..n)
        .filter(|&i| {
            let a = order[i];
            let within = (1..).take_while(|&d| i + d < n).any(|d| {
                let elapsed = prefix[i + d] - prefix[i];
                if axes_exhausted(d, elapsed, bound) {
                    return false;
                }
                let b = order[i + d];
                let dist = Distance {
                    positions: d,
                    elapsed,
                };
                conflict(&items[a], &items[b], limits[a], limits[b], dist)
            });
            let mine_reach = limits[a].reach();
            let mut positions = i;
            let mut elapsed = prefix[i];
            let seam = preceding.iter().rev().any(|prev| {
                positions += 1;
                elapsed += prev.duration;
                if axes_exhausted(positions, elapsed, mine_reach) {
                    return false;
                }
                let dist = Distance { positions, elapsed };
                conflict(&items[a], prev, limits[a], Limits::default(), dist)
            });
            within || seam
        })
        .collect()
}

/// Whether any item in `limits` is constrained at all — lets the caller skip
/// building keys when nothing would use them.
pub fn any_constrained(limits: &[Limits]) -> bool {
    limits.iter().any(|l| !l.is_unconstrained())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(list: &[&str]) -> Vec<ItemKeys> {
        list.iter().map(|s| ItemKeys::new(*s)).collect()
    }

    /// Items carrying group values: `("id", &["cast-a", "cast-b"])`.
    fn grouped(list: &[(&str, &[&str])]) -> Vec<ItemKeys> {
        list.iter()
            .map(|(id, g)| ItemKeys {
                id: (*id).to_string(),
                group: g.iter().map(|s| (*s).to_string()).collect(),
                duration: Duration::ZERO,
            })
            .collect()
    }

    /// Items carrying an estimated duration: `("id", secs)`.
    fn timed(list: &[(&str, u64)]) -> Vec<ItemKeys> {
        list.iter()
            .map(|(id, secs)| ItemKeys {
                id: (*id).to_string(),
                group: Vec::new(),
                duration: Duration::from_secs(*secs),
            })
            .collect()
    }

    fn repeat_only(n: usize, gap: usize) -> Vec<Limits> {
        vec![
            Limits {
                no_repeat: RepeatGap::Positions(gap),
                separate: 0
            };
            n
        ]
    }

    fn repeat_within(n: usize, secs: u64) -> Vec<Limits> {
        vec![
            Limits {
                no_repeat: RepeatGap::Duration(Duration::from_secs(secs)),
                separate: 0
            };
            n
        ]
    }

    fn separate_only(n: usize, gap: usize) -> Vec<Limits> {
        vec![
            Limits {
                no_repeat: RepeatGap::Positions(0),
                separate: gap
            };
            n
        ]
    }

    fn apply(items: &[ItemKeys], order: &[usize]) -> Vec<String> {
        order.iter().map(|&i| items[i].id.clone()).collect()
    }

    fn assert_no_adjacent_repeat(ordered: &[String]) {
        for i in 1..ordered.len() {
            assert_ne!(
                ordered[i - 1],
                ordered[i],
                "positions {} and {i} repeat in {ordered:?}",
                i - 1
            );
        }
    }

    // ---- no_repeat_within (positional) --------------------------------------

    #[test]
    fn unconstrained_is_identity() {
        let items = keys(&["a", "a", "b"]);
        let got = order_constrained(&items, &repeat_only(3, 0), &[]);
        assert_eq!(got.order, vec![0, 1, 2]);
        assert_eq!(got.unresolved, 0);
    }

    #[test]
    fn already_satisfying_list_is_untouched() {
        let items = keys(&["a", "b", "c", "d"]);
        assert_eq!(
            order_constrained(&items, &repeat_only(4, 1), &[]).order,
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn separates_back_to_back_repeat() {
        let items = keys(&["a", "a", "b", "c"]);
        let got = order_constrained(&items, &repeat_only(4, 1), &[]);
        let out = apply(&items, &got.order);
        assert_no_adjacent_repeat(&out);
        assert_eq!(got.unresolved, 0);
        let (mut a, mut b) = (out.clone(), vec!["a", "a", "b", "c"]);
        a.sort();
        b.sort();
        assert_eq!(a, b, "the pass changed the item multiset");
    }

    #[test]
    fn honours_a_gap_wider_than_one() {
        let items = keys(&["a", "a", "b", "c", "d", "e"]);
        let out = apply(
            &items,
            &order_constrained(&items, &repeat_only(6, 2), &[]).order,
        );
        for i in 0..out.len() {
            for d in 1..=2 {
                if i + d < out.len() {
                    assert_ne!(out[i], out[i + d], "gap {d} violated at {i}: {out:?}");
                }
            }
        }
    }

    /// `Sequential` plays the list once, so a repeat between position 0 and
    /// position n-1 is not a violation and must not be "fixed".
    #[test]
    fn the_lists_own_head_and_tail_are_not_adjacent() {
        let items = keys(&["a", "b", "c", "a"]);
        assert_eq!(
            order_constrained(&items, &repeat_only(4, 1), &[]).order,
            vec![0, 1, 2, 3],
            "an already-legal list was reordered"
        );
    }

    #[test]
    fn does_not_repeat_across_the_generation_seam() {
        let items = keys(&["a", "b", "c"]);
        let got = order_constrained(&items, &repeat_only(3, 1), &keys(&["x", "a"]));
        let out = apply(&items, &got.order);
        assert_ne!(out[0], "a", "repeated the previously-aired item: {out:?}");
        assert_eq!(got.unresolved, 0);
    }

    #[test]
    fn a_wide_gap_reaches_further_across_the_seam() {
        let items = keys(&["a", "b", "c", "d"]);
        let out = apply(
            &items,
            &order_constrained(&items, &repeat_only(4, 3), &keys(&["c", "b", "a"])).order,
        );
        // An id that aired `k` positions ago may next sit at index `i` only
        // where `i + k > 3`. "a" aired 1 back, "b" 2 back, "c" 3 back.
        assert!(out[0..3].iter().all(|s| s != "a"), "{out:?}");
        assert!(out[0..2].iter().all(|s| s != "b"), "{out:?}");
        assert!(out[0..1].iter().all(|s| s != "c"), "{out:?}");
    }

    #[test]
    fn accepts_the_violation_when_nothing_is_eligible() {
        let items = keys(&["a", "a", "a"]);
        let got = order_constrained(&items, &repeat_only(3, 1), &[]);
        assert_eq!(got.order, vec![0, 1, 2]);
        assert!(
            got.unresolved > 0,
            "an impossible list reported no violations"
        );
    }

    #[test]
    fn degrades_partially_when_one_title_dominates() {
        let items = keys(&["a", "a", "a", "b"]);
        let mut seen = order_constrained(&items, &repeat_only(4, 1), &[]).order;
        seen.sort_unstable();
        assert_eq!(
            seen,
            vec![0, 1, 2, 3],
            "the pass dropped or duplicated items"
        );
    }

    #[test]
    fn is_deterministic_for_a_fixed_input() {
        let items = keys(&["a", "b", "a", "b", "c", "a"]);
        let first = order_constrained(&items, &repeat_only(6, 1), &keys(&["a"]));
        for _ in 0..5 {
            assert_eq!(
                order_constrained(&items, &repeat_only(6, 1), &keys(&["a"])),
                first
            );
        }
    }

    #[test]
    fn mixed_gaps_use_the_stricter_of_the_pair() {
        let items = keys(&["a", "b", "c", "a", "d", "e", "f", "g"]);
        let mut limits = repeat_only(8, 0);
        limits[0].no_repeat = RepeatGap::Positions(3);
        let out = apply(&items, &order_constrained(&items, &limits, &[]).order);
        let first = out.iter().position(|s| s == "a").unwrap();
        let last = out.iter().rposition(|s| s == "a").unwrap();
        assert!(last - first > 3, "the two `a`s are within 3 in {out:?}");
    }

    // ---- no_repeat_within (temporal, #185) -----------------------------------

    #[test]
    fn a_temporal_gap_separates_by_elapsed_time_not_position() {
        // Two 10-minute episodes either side of one 3-hour film: 20 minutes
        // apart in position-1 terms, but the film alone already covers the
        // 15-minute gap, so the two "a"s must not land adjacent to it either.
        let items = timed(&[("a", 600), ("film", 10_800), ("a", 600)]);
        let got = order_constrained(&items, &repeat_within(3, 900), &[]);
        let out = apply(&items, &got.order);
        assert_no_adjacent_repeat(&out);
        assert_eq!(got.unresolved, 0);
    }

    #[test]
    fn a_temporal_gap_lets_a_repeat_through_once_enough_time_has_passed() {
        // A film alone covers the 15-minute gap, so two "a"s either side of
        // it are already far enough apart in time and the list is untouched.
        let items = timed(&[("a", 600), ("film", 10_800), ("a", 600)]);
        let got = order_constrained(&items, &repeat_within(3, 3600), &[]);
        assert_eq!(
            got.order,
            vec![0, 1, 2],
            "already satisfies the temporal gap; must not be reordered"
        );
    }

    #[test]
    fn a_temporal_gap_holds_across_the_generation_seam() {
        // The tail's last item ran only 5 minutes, well inside a 1-hour gap,
        // so the id it carries must not open the new list either.
        let items = timed(&[("a", 600), ("b", 600)]);
        let preceding = timed(&[("x", 300), ("a", 300)]);
        let got = order_constrained(&items, &repeat_within(2, 3600), &preceding);
        let out = apply(&items, &got.order);
        assert_eq!(
            out[0], "b",
            "repeated across the seam within the hour: {out:?}"
        );
    }

    #[test]
    fn a_temporal_gap_expires_once_the_tail_item_alone_covers_it() {
        // The opposite case: the tail's last item alone (a 90-minute film)
        // already covers a 1-hour gap, so the id it carries is free to open
        // the new list — the exact case a positional gap cannot express,
        // since it would forbid the very next position regardless of runtime.
        let items = timed(&[("a", 600), ("b", 600)]);
        let preceding = timed(&[("x", 300), ("a", 5_400)]);
        let got = order_constrained(&items, &repeat_within(2, 3600), &preceding);
        assert_eq!(
            got.order,
            vec![0, 1],
            "a legal list was needlessly reordered: {:?}",
            apply(&items, &got.order)
        );
    }

    #[test]
    fn a_temporal_gap_is_measured_against_duration_not_item_count() {
        // Five 2-minute shorts easily fit inside "positions" reach of a small
        // number, but a 20-minute no-repeat window outlasts all five put
        // together, so the id at the front must not repeat until position 5.
        let items = timed(&[
            ("a", 120),
            ("b", 120),
            ("c", 120),
            ("d", 120),
            ("e", 120),
            ("a", 120),
        ]);
        let got = order_constrained(&items, &repeat_within(6, 1200), &[]);
        let out = apply(&items, &got.order);
        let first = out.iter().position(|s| s == "a").unwrap();
        let last = out.iter().rposition(|s| s == "a").unwrap();
        assert!(
            last - first >= 5,
            "the two `a`s sit within a 20-minute window in {out:?}"
        );
    }

    #[test]
    fn history_needed_stops_once_the_temporal_gap_is_covered() {
        let history = timed(&[("a", 600), ("b", 600), ("c", 600)]);
        let limits = Limits {
            no_repeat: RepeatGap::Duration(Duration::from_secs(900)),
            separate: 0,
        };
        // "c" alone (600s) is still inside the 900s gap; "c" + "b" (1200s) is
        // not, so only the trailing item is still reachable.
        assert_eq!(history_needed(&history, limits), 1);
    }

    #[test]
    fn history_needed_is_zero_when_unconstrained() {
        let history = timed(&[("a", 600)]);
        assert_eq!(history_needed(&history, Limits::default()), 0);
    }

    #[test]
    fn history_needed_covers_positional_limits_the_same_as_before() {
        let history = keys(&["a", "b", "c", "d"]);
        let limits = Limits {
            no_repeat: RepeatGap::Positions(2),
            separate: 0,
        };
        assert_eq!(history_needed(&history, limits), 2);
    }

    // ---- separate_by --------------------------------------------------------

    /// Two films sharing a performer must not sit adjacent, even though their
    /// ids differ — that is the whole difference from `no_repeat_within`.
    #[test]
    fn separates_items_sharing_a_group_value() {
        let items = grouped(&[
            ("f1", &["Bruce Lee"]),
            ("f2", &["Bruce Lee"]),
            ("f3", &["Jackie Chan"]),
            ("f4", &["Gordon Liu"]),
        ]);
        let got = order_constrained(&items, &separate_only(4, 1), &[]);
        let order = &got.order;
        let pos1 = order.iter().position(|&i| i == 0).unwrap();
        let pos2 = order.iter().position(|&i| i == 1).unwrap();
        assert!(
            pos1.abs_diff(pos2) > 1,
            "two Bruce Lee films are adjacent: {:?}",
            apply(&items, order)
        );
        assert_eq!(got.unresolved, 0);
    }

    /// Sharing ANY value conflicts — casts do not have to match outright.
    #[test]
    fn one_shared_value_is_enough_to_conflict() {
        let items = grouped(&[
            ("f1", &["Bruce Lee", "Jackie Chan"]),
            ("f2", &["Jackie Chan", "Sammo Hung"]),
            ("f3", &["Gordon Liu"]),
        ]);
        let order = order_constrained(&items, &separate_only(3, 1), &[]).order;
        let pos1 = order.iter().position(|&i| i == 0).unwrap();
        let pos2 = order.iter().position(|&i| i == 1).unwrap();
        assert!(pos1.abs_diff(pos2) > 1, "{order:?}");
    }

    #[test]
    fn items_with_no_group_values_never_conflict() {
        // An item the field is empty for (no cast recorded) is not "sharing
        // nothing with everyone" — it simply never triggers the constraint.
        let items = grouped(&[("f1", &[]), ("f2", &[]), ("f3", &[])]);
        assert_eq!(
            order_constrained(&items, &separate_only(3, 1), &[]).order,
            vec![0, 1, 2]
        );
    }

    #[test]
    fn separation_holds_across_the_seam() {
        let items = grouped(&[("f1", &["Bruce Lee"]), ("f2", &["Gordon Liu"])]);
        let preceding = grouped(&[("f9", &["Bruce Lee"])]);
        let out = apply(
            &items,
            &order_constrained(&items, &separate_only(2, 1), &preceding).order,
        );
        assert_eq!(
            out[0], "f2",
            "a shared performer aired back-to-back: {out:?}"
        );
    }

    #[test]
    fn separation_degrades_when_everyone_shares_a_value() {
        let items = grouped(&[
            ("f1", &["Jackie Chan"]),
            ("f2", &["Jackie Chan"]),
            ("f3", &["Jackie Chan"]),
        ]);
        let got = order_constrained(&items, &separate_only(3, 1), &[]);
        assert_eq!(got.order.len(), 3, "the pass dropped items");
        assert!(
            got.unresolved > 0,
            "an impossible separation reported clean"
        );
    }

    /// Both constraints at once: distinct films, but two share a performer.
    #[test]
    fn identity_and_property_constraints_compose() {
        let items = grouped(&[
            ("f1", &["Bruce Lee"]),
            ("f1", &["Bruce Lee"]),
            ("f2", &["Bruce Lee"]),
            ("f3", &["Gordon Liu"]),
            ("f4", &["Ti Lung"]),
        ]);
        let limits = vec![
            Limits {
                no_repeat: RepeatGap::Positions(1),
                separate: 1
            };
            5
        ];
        let got = order_constrained(&items, &limits, &[]);
        let out = apply(&items, &got.order);
        assert_no_adjacent_repeat(&out);
        // Three Bruce Lee films and two others cannot all be separated, so some
        // violation is expected — but the pass must still return every item.
        assert_eq!(got.order.len(), 5);
    }

    #[test]
    fn any_constrained_reports_whether_the_pass_would_do_anything() {
        assert!(!any_constrained(&repeat_only(3, 0)));
        assert!(any_constrained(&repeat_only(3, 1)));
        assert!(any_constrained(&separate_only(3, 2)));
        assert!(any_constrained(&repeat_within(3, 60)));
    }

    // ---- conflicts_with_recent: the emitted-sequence check (#115) ----------

    fn keyed(id: &str, group: &[&str]) -> ItemKeys {
        ItemKeys {
            id: id.into(),
            group: group.iter().map(|s| s.to_string()).collect(),
            duration: Duration::ZERO,
        }
    }

    #[test]
    fn recent_check_is_off_when_nothing_is_constrained() {
        let recent = vec![ItemKeys::new("a")];
        assert!(!conflicts_with_recent(
            &recent,
            &ItemKeys::new("a"),
            Limits::default()
        ));
    }

    #[test]
    fn recent_check_catches_a_repeat_inside_the_gap() {
        let recent = vec![ItemKeys::new("a"), ItemKeys::new("b")];
        let limits = Limits {
            no_repeat: RepeatGap::Positions(2),
            separate: 0,
        };
        // `a` is two back, `b` one back — both inside a gap of 2.
        assert!(conflicts_with_recent(&recent, &ItemKeys::new("a"), limits));
        assert!(conflicts_with_recent(&recent, &ItemKeys::new("b"), limits));
        assert!(!conflicts_with_recent(&recent, &ItemKeys::new("c"), limits));
    }

    #[test]
    fn recent_check_lets_a_repeat_through_once_it_is_far_enough_back() {
        let recent = vec![ItemKeys::new("a"), ItemKeys::new("b"), ItemKeys::new("c")];
        let limits = Limits {
            no_repeat: RepeatGap::Positions(2),
            separate: 0,
        };
        // `a` is three back, outside a gap of 2.
        assert!(!conflicts_with_recent(&recent, &ItemKeys::new("a"), limits));
    }

    #[test]
    fn recent_check_covers_the_separation_axis_too() {
        let recent = vec![keyed("a", &["Bruce Lee"])];
        let limits = Limits {
            no_repeat: RepeatGap::Positions(0),
            separate: 1,
        };
        assert!(conflicts_with_recent(
            &recent,
            &keyed("b", &["Bruce Lee", "Nora Miao"]),
            limits
        ));
        assert!(!conflicts_with_recent(
            &recent,
            &keyed("c", &["Jackie Chan"]),
            limits
        ));
    }

    /// The two axes are measured independently: a wide `separate` must not drag
    /// a narrow `no_repeat` along with it.
    #[test]
    fn recent_check_measures_each_axis_at_its_own_distance() {
        let recent = vec![keyed("a", &["x"]), keyed("b", &["y"])];
        let limits = Limits {
            no_repeat: RepeatGap::Positions(1),
            separate: 2,
        };
        // `a` is two back: outside no_repeat = 1, inside separate = 2.
        assert!(!conflicts_with_recent(&recent, &ItemKeys::new("a"), limits));
        assert!(conflicts_with_recent(&recent, &keyed("z", &["x"]), limits));
    }

    #[test]
    fn recent_check_honours_a_temporal_gap() {
        let recent = timed(&[("a", 600), ("film", 5_400)]);
        let limits = Limits {
            no_repeat: RepeatGap::Duration(Duration::from_secs(3600)),
            separate: 0,
        };
        // The film alone (5400s) already exceeds the hour, so "a" — two back —
        // is out of reach even though it would be in reach at position 2.
        assert!(!conflicts_with_recent(
            &recent,
            &ItemKeys {
                id: "a".into(),
                group: Vec::new(),
                duration: Duration::ZERO,
            },
            limits
        ));
    }
}

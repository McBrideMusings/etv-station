//! Post-order adjacency constraint pass (#73).
//!
//! The pipeline is `resolve → duplicates → order → constraint pass → emit`.
//! `duplicates` is identity over the whole list and play-history is identity
//! over time; this pass is the third axis — identity over *adjacent positions*.
//!
//! v1 implements `no_repeat_within = N`: the same `entry_id` may not recur
//! within N positions (`N = 1` = no back-to-back). Resolution is a deterministic
//! greedy: walk the ordered list, and whenever the next item would violate the
//! constraint, defer it and take the first item behind it that would not. A
//! swap-repair follows, in case the greedy consumed every alternative and
//! reached a position holding only a clashing item. When nothing improves —
//! an all-one-title pool with `no_repeat_within = 1` — the remaining violations
//! are accepted rather than looped on forever.
//!
//! # The seam
//!
//! [`crate::rule::Sequential`] plays a list once and the next generation lays a
//! fresh list after it, so the constraint has to reach *backwards across that
//! boundary*: the first item of this list airs immediately after the last item
//! of the previous one. `preceding` carries that tail — the most recently aired
//! entry ids, oldest first — read out of the play-history ledger
//! ([`crate::history::Ledger::tail`]).
//!
//! The list is emphatically **not** circular. Position `n-1` and position `0` of
//! one generation never air next to each other; `n-1` is followed by whatever
//! the *next* generation resolves first, which this pass will constrain when it
//! runs for that generation with this list's tail as its `preceding`.

use std::collections::VecDeque;

/// How many recently-aired ids to carry across a generation seam. Far above any
/// sane `no_repeat_within`, and the cost is one short `Vec` per generation.
pub const SEAM_TAIL: usize = 64;

/// Order `ids` so that no two entries sharing an id sit within their gap of each
/// other. `gaps[i]` is item `i`'s own `no_repeat_within` (`0` = unconstrained),
/// so blocks with different settings can be concatenated and constrained in one
/// pass. `preceding` is the tail of what already aired, oldest first — the item
/// at its end is position `-1` relative to this list.
///
/// Returns a permutation of `0..ids.len()`. Deterministic: the same inputs
/// always yield the same permutation.
pub fn order_no_repeat(ids: &[String], gaps: &[usize], preceding: &[String]) -> Vec<usize> {
    debug_assert_eq!(ids.len(), gaps.len(), "ids/gaps length mismatch");

    let total = ids.len();
    let max_gap = gaps.iter().copied().max().unwrap_or(0);
    if max_gap == 0 || total == 0 {
        return (0..total).collect();
    }

    let mut pending: VecDeque<usize> = (0..total).collect();
    let mut out: Vec<usize> = Vec::with_capacity(total);

    while !pending.is_empty() {
        let pick = pending
            .iter()
            .position(|&cand| !violates(&out, cand, ids, gaps, max_gap, preceding))
            // Nothing left is eligible, so every remaining choice violates.
            // Take the head — accepting the violation keeps generation
            // deterministic and finite instead of hanging.
            .unwrap_or(0);
        let cand = pending
            .remove(pick)
            .expect("pick index came from the queue itself");
        out.push(cand);
    }

    repair(&mut out, ids, gaps, max_gap, preceding);
    out
}

/// Would placing `cand` at position `out.len()` put it within its gap of another
/// occurrence of the same id? Looks back through what this pass has already
/// placed, then on into `preceding` once this list runs out.
fn violates(
    out: &[usize],
    cand: usize,
    ids: &[String],
    gaps: &[usize],
    max_gap: usize,
    preceding: &[String],
) -> bool {
    // Within this list: `back + 1` is the distance to an already-placed item.
    // Two items with different gaps use the larger, so a strict block's
    // constraint is not weakened by a lax neighbour.
    for (back, &placed) in out.iter().rev().enumerate().take(max_gap) {
        if ids[placed] == ids[cand] && back < gaps[cand].max(gaps[placed]) {
            return true;
        }
    }

    // Across the seam: the previously-aired tail sits immediately before
    // position 0, so its last entry is `out.len() + 1` positions back. Only the
    // candidate's own gap applies — the previous generation is already emitted
    // and its settings are not ours to reconsider.
    let already = out.len();
    if already < gaps[cand] {
        for (back, id) in preceding.iter().rev().enumerate() {
            let distance = already + back + 1;
            if distance > gaps[cand] {
                break;
            }
            if id == &ids[cand] {
                return true;
            }
        }
    }

    false
}

/// Swap-repair whatever the forward greedy could not place — a position it
/// reached holding only items that clash.
fn repair(
    order: &mut [usize],
    ids: &[String],
    gaps: &[usize],
    max_gap: usize,
    preceding: &[String],
) {
    let n = order.len();
    let mut best = violation_count(order, ids, gaps, max_gap, preceding);

    while best > 0 {
        let mut improved = false;
        // Only positions that actually clash are worth moving, and there are
        // usually a handful at most — the greedy has done the bulk already.
        'search: for i in violating_positions(order, ids, gaps, max_gap, preceding) {
            for j in 0..n {
                if i == j {
                    continue;
                }
                order.swap(i, j);
                let count = violation_count(order, ids, gaps, max_gap, preceding);
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
}

/// How many ordered pairs within `max_gap` of each other share an id closer than
/// their gap allows, counting the seam against `preceding`. Used only as a
/// monotone objective for [`repair`], so the exact scale does not matter — only
/// that it falls when the arrangement improves.
fn violation_count(
    order: &[usize],
    ids: &[String],
    gaps: &[usize],
    max_gap: usize,
    preceding: &[String],
) -> usize {
    let n = order.len();
    let mut count = 0;
    for i in 0..n {
        for d in 1..=max_gap {
            if i + d >= n {
                break;
            }
            let a = order[i];
            let b = order[i + d];
            if ids[a] == ids[b] && d <= gaps[a].max(gaps[b]) {
                count += 1;
            }
        }
        // The seam: how far position `i` sits from the end of `preceding`.
        let cand = order[i];
        for (back, id) in preceding.iter().rev().enumerate() {
            let distance = i + back + 1;
            if distance > gaps[cand] {
                break;
            }
            if id == &ids[cand] {
                count += 1;
            }
        }
    }
    count
}

/// Positions holding an item that clashes with a neighbour, in ascending order.
fn violating_positions(
    order: &[usize],
    ids: &[String],
    gaps: &[usize],
    max_gap: usize,
    preceding: &[String],
) -> Vec<usize> {
    let n = order.len();
    (0..n)
        .filter(|&i| {
            let cand = order[i];
            let within = (1..=max_gap).any(|d| {
                if i + d >= n {
                    return false;
                }
                let b = order[i + d];
                ids[cand] == ids[b] && d <= gaps[cand].max(gaps[b])
            });
            let seam = preceding.iter().rev().enumerate().any(|(back, id)| {
                let distance = i + back + 1;
                distance <= gaps[cand] && id == &ids[cand]
            });
            within || seam
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    fn apply(ids: &[String], perm: &[usize]) -> Vec<String> {
        perm.iter().map(|&i| ids[i].clone()).collect()
    }

    /// Every adjacent pair differs, for `no_repeat_within = 1`. Linear — the
    /// list is played once, so its last item is not next to its first.
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

    #[test]
    fn unconstrained_is_identity() {
        let ids = v(&["a", "a", "b"]);
        assert_eq!(order_no_repeat(&ids, &[0, 0, 0], &[]), vec![0, 1, 2]);
    }

    #[test]
    fn already_satisfying_list_is_untouched() {
        let ids = v(&["a", "b", "c", "d"]);
        assert_eq!(order_no_repeat(&ids, &[1; 4], &[]), vec![0, 1, 2, 3]);
    }

    #[test]
    fn separates_back_to_back_repeat() {
        let ids = v(&["a", "a", "b", "c"]);
        let out = apply(&ids, &order_no_repeat(&ids, &[1; 4], &[]));
        assert_no_adjacent_repeat(&out);
        let (mut got, mut want) = (out.clone(), ids.clone());
        got.sort();
        want.sort();
        assert_eq!(got, want, "the pass changed the item multiset");
    }

    #[test]
    fn honours_a_gap_wider_than_one() {
        // With N = 2 an id may not recur within two positions.
        let ids = v(&["a", "a", "b", "c", "d", "e"]);
        let out = apply(&ids, &order_no_repeat(&ids, &[2; 6], &[]));
        for i in 0..out.len() {
            for d in 1..=2 {
                if i + d < out.len() {
                    assert_ne!(out[i], out[i + d], "gap {d} violated at {i}: {out:?}");
                }
            }
        }
    }

    /// The list's own ends are NOT adjacent: `Sequential` plays it once, so a
    /// repeat between position 0 and position n-1 is not a violation and must
    /// not be "fixed".
    #[test]
    fn the_lists_own_head_and_tail_are_not_adjacent() {
        let ids = v(&["a", "b", "c", "a"]);
        let perm = order_no_repeat(&ids, &[1; 4], &[]);
        assert_eq!(
            perm,
            vec![0, 1, 2, 3],
            "an already-legal list was reordered"
        );
    }

    /// The real seam: the previous generation's last item is position −1.
    #[test]
    fn does_not_repeat_across_the_generation_seam() {
        let ids = v(&["a", "b", "c"]);
        let out = apply(&ids, &order_no_repeat(&ids, &[1; 3], &v(&["x", "a"])));
        assert_ne!(out[0], "a", "repeated the previously-aired item: {out:?}");
        assert_no_adjacent_repeat(&out);
    }

    /// A wider gap reaches further back into what already aired.
    #[test]
    fn a_wide_gap_reaches_further_across_the_seam() {
        let ids = v(&["a", "b", "c", "d"]);
        let preceding = v(&["c", "b", "a"]);
        let out = apply(&ids, &order_no_repeat(&ids, &[3; 4], &preceding));
        // An id that aired `k` positions ago may next sit at index `i` only
        // where `i + k > 3`. "a" aired 1 back, "b" 2 back, "c" 3 back.
        assert!(out[0..3].iter().all(|s| s != "a"), "{out:?}");
        assert!(out[0..2].iter().all(|s| s != "b"), "{out:?}");
        assert!(out[0..1].iter().all(|s| s != "c"), "{out:?}");
    }

    #[test]
    fn an_empty_preceding_tail_constrains_nothing() {
        let ids = v(&["a", "b"]);
        assert_eq!(order_no_repeat(&ids, &[1; 2], &[]), vec![0, 1]);
    }

    #[test]
    fn accepts_the_violation_when_nothing_is_eligible() {
        // A pool of one title with "no two in a row" cannot be satisfied.
        let ids = v(&["a", "a", "a"]);
        assert_eq!(order_no_repeat(&ids, &[1; 3], &[]), vec![0, 1, 2]);
    }

    #[test]
    fn degrades_partially_when_one_title_dominates() {
        let ids = v(&["a", "a", "a", "b"]);
        let mut seen = order_no_repeat(&ids, &[1; 4], &[]);
        seen.sort_unstable();
        assert_eq!(
            seen,
            vec![0, 1, 2, 3],
            "the pass dropped or duplicated items"
        );
    }

    #[test]
    fn is_deterministic_for_a_fixed_input() {
        let ids = v(&["a", "b", "a", "b", "c", "a"]);
        let first = order_no_repeat(&ids, &[1; 6], &v(&["a"]));
        for _ in 0..5 {
            assert_eq!(order_no_repeat(&ids, &[1; 6], &v(&["a"])), first);
        }
    }

    #[test]
    fn mixed_gaps_use_the_stricter_of_the_pair() {
        // Item 0 wants a gap of 3; item 3 shares its id but is unconstrained.
        let ids = v(&["a", "b", "c", "a", "d", "e", "f", "g"]);
        let gaps = vec![3, 0, 0, 0, 0, 0, 0, 0];
        let out = apply(&ids, &order_no_repeat(&ids, &gaps, &[]));
        let first = out.iter().position(|s| s == "a").unwrap();
        let last = out.iter().rposition(|s| s == "a").unwrap();
        assert!(last - first > 3, "the two `a`s are within 3 in {out:?}");
    }
}

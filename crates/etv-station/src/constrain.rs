//! Post-order adjacency constraint pass (#73).
//!
//! The Phase C pipeline is `resolve → duplicates → order → constraint pass →
//! loop`. `duplicates` is identity over the whole list and play-history is
//! identity over time; this pass is the third axis — identity over *adjacent
//! positions*.
//!
//! v1 implements `no_repeat_within = N`: the same `entry_id` may not recur
//! within N positions (`N = 1` = no back-to-back). Resolution is a deterministic
//! greedy: walk the ordered list, and whenever the next item would violate the
//! constraint, defer it and take the first item behind it that would not. When
//! *nothing* left is eligible — an all-one-title pool with `no_repeat_within =
//! 1` — the violation is accepted rather than looped on forever.
//!
//! The greedy alone cannot satisfy a *circular* constraint: it can consume every
//! alternative and arrive at the last slot holding only an item that clashes
//! with the head (`a b c a` → the trailing `a` has nowhere else to go). A repair
//! pass follows it: while any violation remains, take the first violating
//! position and swap it with the first partner that strictly lowers the total
//! violation count. The count decreases on every accepted swap and never rises,
//! so the pass terminates; when no swap improves matters the remaining
//! violations are accepted.
//!
//! **The seam.** [`crate::rule::LoopForever`] replays one resolved list end to
//! end from the anchor, so the list's tail is genuinely adjacent to its head:
//! the constraint is enforced *circularly*, not just linearly. The cross-*window*
//! seam described in #73 — reading the previous window's last aired item out of
//! a play-history ledger — needs that ledger, which is #70 and does not exist
//! yet; under the current model every window replays the same list, so the loop
//! wrap is the only seam that exists.

use std::collections::VecDeque;

/// Order `ids` so that no two entries sharing an id sit within their gap of each
/// other, treating the list as circular. `gaps[i]` is item `i`'s own
/// `no_repeat_within` (`0` = unconstrained), so blocks with different settings
/// can be concatenated and constrained in one pass.
///
/// Returns a permutation of `0..ids.len()`. Deterministic: the same input order
/// always yields the same permutation.
pub fn order_no_repeat(ids: &[String], gaps: &[usize]) -> Vec<usize> {
    debug_assert_eq!(ids.len(), gaps.len(), "ids/gaps length mismatch");

    let total = ids.len();
    let max_gap = gaps.iter().copied().max().unwrap_or(0);
    if max_gap == 0 || total < 2 {
        return (0..total).collect();
    }

    let mut pending: VecDeque<usize> = (0..total).collect();
    let mut out: Vec<usize> = Vec::with_capacity(total);

    while !pending.is_empty() {
        let pick = pending
            .iter()
            .position(|&cand| !violates(&out, cand, total, ids, gaps, max_gap))
            // Nothing left is eligible, so every remaining choice violates.
            // Take the head — accepting the violation keeps generation
            // deterministic and finite instead of hanging.
            .unwrap_or(0);
        let cand = pending
            .remove(pick)
            .expect("pick index came from the queue itself");
        out.push(cand);
    }

    repair(&mut out, ids, gaps, max_gap);
    out
}

/// Swap-repair whatever the forward greedy could not place — chiefly clashes
/// against the loop wrap, which are only discoverable once the head is fixed.
fn repair(order: &mut [usize], ids: &[String], gaps: &[usize], max_gap: usize) {
    let n = order.len();
    let mut best = violation_count(order, ids, gaps, max_gap);

    while best > 0 {
        let mut improved = false;
        // Only positions that actually clash are worth moving, and there are
        // usually a handful at most — the greedy has done the bulk already.
        'search: for i in violating_positions(order, ids, gaps, max_gap) {
            for j in 0..n {
                if i == j {
                    continue;
                }
                order.swap(i, j);
                let count = violation_count(order, ids, gaps, max_gap);
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

/// How many ordered pairs within `max_gap` of each other (circularly) share an
/// id closer than their gap allows. Used only as a monotone objective for
/// [`repair`], so the exact scale does not matter — only that it falls when the
/// arrangement improves.
fn violation_count(order: &[usize], ids: &[String], gaps: &[usize], max_gap: usize) -> usize {
    let n = order.len();
    let mut count = 0;
    for i in 0..n {
        for d in 1..=max_gap {
            // `d == n` would compare a position with itself round the loop.
            if d >= n {
                break;
            }
            let a = order[i];
            let b = order[(i + d) % n];
            if ids[a] == ids[b] && d <= gaps[a].max(gaps[b]) {
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
) -> Vec<usize> {
    let n = order.len();
    (0..n)
        .filter(|&i| {
            (1..=max_gap).any(|d| {
                if d >= n {
                    return false;
                }
                let a = order[i];
                let b = order[(i + d) % n];
                ids[a] == ids[b] && d <= gaps[a].max(gaps[b])
            })
        })
        .collect()
}

/// Would placing `cand` at position `out.len()` put it within its gap of another
/// occurrence of the same id — looking backwards at what is already placed, and
/// forwards across the loop wrap at the already-placed head?
fn violates(
    out: &[usize],
    cand: usize,
    total: usize,
    ids: &[String],
    gaps: &[usize],
    max_gap: usize,
) -> bool {
    // Backwards: `back + 1` is the distance from the candidate's position to an
    // already-placed item. Two items with different gaps use the larger, so a
    // strict block's constraint is not weakened by a lax neighbour.
    for (back, &placed) in out.iter().rev().enumerate().take(max_gap) {
        if ids[placed] == ids[cand] && back < gaps[cand].max(gaps[placed]) {
            return true;
        }
    }

    // Forwards across the wrap: distance from this position round the end of
    // the list to head position `front`. Only bites for the last `max_gap`
    // positions, where the head is already placed and therefore knowable.
    let tail = total - out.len();
    if tail <= max_gap {
        for (front, &placed) in out.iter().enumerate().take(max_gap) {
            let distance = tail + front;
            if distance > max_gap {
                break;
            }
            if ids[placed] == ids[cand] && distance <= gaps[cand].max(gaps[placed]) {
                return true;
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    /// Every adjacent pair (circularly) differs, for `no_repeat_within = 1`.
    fn assert_no_adjacent_repeat(ordered: &[String]) {
        let n = ordered.len();
        for i in 0..n {
            assert_ne!(
                ordered[i],
                ordered[(i + 1) % n],
                "positions {i} and {} repeat in {ordered:?}",
                (i + 1) % n
            );
        }
    }

    fn apply(ids: &[String], perm: &[usize]) -> Vec<String> {
        perm.iter().map(|&i| ids[i].clone()).collect()
    }

    #[test]
    fn unconstrained_is_identity() {
        let ids = ids(&["a", "a", "b"]);
        assert_eq!(order_no_repeat(&ids, &[0, 0, 0]), vec![0, 1, 2]);
    }

    #[test]
    fn already_satisfying_list_is_untouched() {
        let ids = ids(&["a", "b", "c", "d"]);
        assert_eq!(order_no_repeat(&ids, &[1; 4]), vec![0, 1, 2, 3]);
    }

    #[test]
    fn separates_back_to_back_repeat() {
        // a a b c → the second `a` is deferred behind `b`.
        let ids = ids(&["a", "a", "b", "c"]);
        let perm = order_no_repeat(&ids, &[1; 4]);
        let out = apply(&ids, &perm);
        assert_no_adjacent_repeat(&out);
        assert_eq!(out, ids_sorted_multiset(&out, &ids));
    }

    #[test]
    fn honours_a_gap_wider_than_one() {
        // With N = 2 an id may not recur within two positions: a _ _ a is the
        // tightest legal spacing.
        let ids = ids(&["a", "a", "b", "c", "d", "e"]);
        let perm = order_no_repeat(&ids, &[2; 6]);
        let out = apply(&ids, &perm);
        let n = out.len();
        for i in 0..n {
            for d in 1..=2 {
                assert_ne!(out[i], out[(i + d) % n], "gap {d} violated at {i}: {out:?}");
            }
        }
    }

    #[test]
    fn enforces_the_loop_wrap_seam() {
        // Linearly `a b a` is fine, but LoopForever replays it end to end, so
        // the trailing `a` lands next to the leading `a`. The pass must break
        // that pair too.
        let ids = ids(&["a", "b", "a", "c"]);
        let perm = order_no_repeat(&ids, &[1; 4]);
        assert_no_adjacent_repeat(&apply(&ids, &perm));
    }

    #[test]
    fn accepts_the_violation_when_nothing_is_eligible() {
        // A pool of one title with "no two in a row" cannot be satisfied.
        // It must terminate, keep every item, and stay deterministic.
        let ids = ids(&["a", "a", "a"]);
        let perm = order_no_repeat(&ids, &[1; 3]);
        assert_eq!(perm, vec![0, 1, 2]);
    }

    #[test]
    fn degrades_partially_when_one_title_dominates() {
        // Three `a` and one `b` cannot avoid every repeat, but must still
        // complete with all four items present.
        let ids = ids(&["a", "a", "a", "b"]);
        let perm = order_no_repeat(&ids, &[1; 4]);
        let mut seen = perm.clone();
        seen.sort_unstable();
        assert_eq!(seen, vec![0, 1, 2, 3]);
    }

    #[test]
    fn is_deterministic_for_a_fixed_input() {
        let ids = ids(&["a", "b", "a", "b", "c", "a"]);
        let first = order_no_repeat(&ids, &[1; 6]);
        for _ in 0..5 {
            assert_eq!(order_no_repeat(&ids, &[1; 6]), first);
        }
    }

    #[test]
    fn mixed_gaps_use_the_stricter_of_the_pair() {
        // Item 0 wants a gap of 3; item 3 shares its id but is unconstrained.
        // The stricter setting still separates them.
        let ids = ids(&["a", "b", "c", "a", "d", "e", "f", "g"]);
        let gaps = vec![3, 0, 0, 0, 0, 0, 0, 0];
        let perm = order_no_repeat(&ids, &gaps);
        let out = apply(&ids, &perm);
        let n = out.len();
        let first = out.iter().position(|s| s == "a").unwrap();
        let last = out.iter().rposition(|s| s == "a").unwrap();
        let forward = last - first;
        assert!(
            forward > 3 && (n - forward) > 3,
            "the two `a`s are within 3 either way in {out:?}"
        );
    }

    /// The pass reorders, never adds or drops: same multiset in, same out.
    fn ids_sorted_multiset(out: &[String], input: &[String]) -> Vec<String> {
        let mut a: Vec<String> = out.to_vec();
        let mut b: Vec<String> = input.to_vec();
        a.sort();
        b.sort();
        assert_eq!(a, b, "the pass changed the item multiset");
        out.to_vec()
    }
}

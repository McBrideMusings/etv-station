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
//!   within N positions (`N = 1` = no back-to-back).
//! - **`separate_by = "<field>"` + `separate_min_gap = N`** — property. Two
//!   items sharing **any** value of a multi-valued field may not sit within N
//!   positions, so `separate_by: "cast"` spreads out films sharing a performer.
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

/// What the pass needs to know about one item: its identity, and the values of
/// the field being separated on (empty when nothing is).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemKeys {
    pub id: String,
    pub group: Vec<String>,
}

impl ItemKeys {
    /// An item with identity only — nothing to separate on.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            group: Vec::new(),
        }
    }

    fn shares_group_with(&self, other: &Self) -> bool {
        !self.group.is_empty()
            && !other.group.is_empty()
            && self.group.iter().any(|v| other.group.contains(v))
    }
}

/// How far one item's two constraints reach. `0` means unconstrained.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limits {
    pub no_repeat: usize,
    pub separate: usize,
}

impl Limits {
    fn reach(&self) -> usize {
        self.no_repeat.max(self.separate)
    }

    fn is_unconstrained(&self) -> bool {
        self.reach() == 0
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
    let max_reach = limits.iter().map(Limits::reach).max().unwrap_or(0);
    if max_reach == 0 || total == 0 {
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
            .position(|&cand| !violates(&out, cand, items, limits, max_reach, preceding))
            // Nothing left is eligible, so every remaining choice violates.
            // Take the head — accepting the violation keeps generation
            // deterministic and finite instead of hanging.
            .unwrap_or(0);
        let cand = pending
            .remove(pick)
            .expect("pick index came from the queue itself");
        out.push(cand);
    }

    let unresolved = repair(&mut out, items, limits, max_reach, preceding);
    Ordering {
        order: out,
        unresolved,
    }
}

/// Whether two items conflict at `distance` positions apart, under the stricter
/// of their two limits per axis.
fn conflict(a: &ItemKeys, b: &ItemKeys, la: Limits, lb: Limits, distance: usize) -> bool {
    (a.id == b.id && distance <= la.no_repeat.max(lb.no_repeat))
        || (distance <= la.separate.max(lb.separate) && a.shares_group_with(b))
}

/// Would placing `cand` at position `out.len()` conflict with something? Looks
/// back through what this pass has already placed, then on into `preceding`
/// once this list runs out.
fn violates(
    out: &[usize],
    cand: usize,
    items: &[ItemKeys],
    limits: &[Limits],
    max_reach: usize,
    preceding: &[ItemKeys],
) -> bool {
    let me = &items[cand];
    let mine = limits[cand];

    for (back, &placed) in out.iter().rev().enumerate().take(max_reach) {
        if conflict(me, &items[placed], mine, limits[placed], back + 1) {
            return true;
        }
    }

    // Across the seam. Only the candidate's own limits apply — the previous
    // generation is already emitted and its settings are not ours to revisit.
    let already = out.len();
    if already < mine.reach() {
        for (back, prev) in preceding.iter().rev().enumerate() {
            let distance = already + back + 1;
            if distance > mine.reach() {
                break;
            }
            if conflict(me, prev, mine, Limits::default(), distance) {
                return true;
            }
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
    max_reach: usize,
    preceding: &[ItemKeys],
) -> usize {
    let n = order.len();
    let mut best = violation_count(order, items, limits, max_reach, preceding);

    for _ in 0..MAX_REPAIR_ROUNDS {
        if best == 0 {
            return 0;
        }
        let mut improved = false;
        // Only positions that actually clash are worth moving, and there are
        // usually a handful at most — the greedy has done the bulk already.
        'search: for i in violating_positions(order, items, limits, max_reach, preceding) {
            for j in 0..n {
                if i == j {
                    continue;
                }
                order.swap(i, j);
                let count = violation_count(order, items, limits, max_reach, preceding);
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

/// How many ordered pairs conflict, counting the seam against `preceding`. Used
/// as a monotone objective for [`repair`], and reported so the caller can say
/// that a channel is airing a schedule its constraints do not fully hold on.
fn violation_count(
    order: &[usize],
    items: &[ItemKeys],
    limits: &[Limits],
    max_reach: usize,
    preceding: &[ItemKeys],
) -> usize {
    let n = order.len();
    let mut count = 0;
    for i in 0..n {
        let a = order[i];
        for d in 1..=max_reach {
            if i + d >= n {
                break;
            }
            let b = order[i + d];
            if conflict(&items[a], &items[b], limits[a], limits[b], d) {
                count += 1;
            }
        }
        for (back, prev) in preceding.iter().rev().enumerate() {
            let distance = i + back + 1;
            if distance > limits[a].reach() {
                break;
            }
            if conflict(&items[a], prev, limits[a], Limits::default(), distance) {
                count += 1;
            }
        }
    }
    count
}

/// Positions holding an item that conflicts with a neighbour, ascending.
fn violating_positions(
    order: &[usize],
    items: &[ItemKeys],
    limits: &[Limits],
    max_reach: usize,
    preceding: &[ItemKeys],
) -> Vec<usize> {
    let n = order.len();
    (0..n)
        .filter(|&i| {
            let a = order[i];
            let within = (1..=max_reach).any(|d| {
                i + d < n && {
                    let b = order[i + d];
                    conflict(&items[a], &items[b], limits[a], limits[b], d)
                }
            });
            let seam = preceding.iter().rev().enumerate().any(|(back, prev)| {
                let distance = i + back + 1;
                distance <= limits[a].reach()
                    && conflict(&items[a], prev, limits[a], Limits::default(), distance)
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
            })
            .collect()
    }

    fn repeat_only(n: usize, gap: usize) -> Vec<Limits> {
        vec![
            Limits {
                no_repeat: gap,
                separate: 0
            };
            n
        ]
    }

    fn separate_only(n: usize, gap: usize) -> Vec<Limits> {
        vec![
            Limits {
                no_repeat: 0,
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

    // ---- no_repeat_within ---------------------------------------------------

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
        limits[0].no_repeat = 3;
        let out = apply(&items, &order_constrained(&items, &limits, &[]).order);
        let first = out.iter().position(|s| s == "a").unwrap();
        let last = out.iter().rposition(|s| s == "a").unwrap();
        assert!(last - first > 3, "the two `a`s are within 3 in {out:?}");
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
                no_repeat: 1,
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
    }
}

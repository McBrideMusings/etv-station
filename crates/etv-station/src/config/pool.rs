//! Pools + pattern interleave (#72) — the on-disk config shape.
//!
//! A *pattern block* is the alternative to an `[[entries]]` block: instead of a
//! flat authored list it declares named [`Pool`]s (each its own resolved set)
//! and a repeating [`PatternStep`] template. The generator walks the pattern,
//! drawing `take` items from the named pool per step and looping the pattern to
//! fill the window — "1 movie, then 3 episodes, repeat".
//!
//! Every knob defaults to the stateless, least-surprising behavior, so a pool
//! that names only `expr` behaves like today's `query` entry.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::constraints::Constraints;
use super::order::Order;

/// Which series the next draw comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Select {
    /// Cycle series in order — the most broadcast-like, and the default.
    #[default]
    RoundRobin,
    /// Pick a series at random (seeded, so a pinned `seed` reproduces it).
    Random,
}

/// *When* the series changes — orthogonal to [`Select`], which says *which*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Rotate {
    /// One series per visit to the step: `take = 3` is three consecutive
    /// episodes of the same show (a mini-binge), then rotate on the next visit.
    #[default]
    Visit,
    /// A new series every item: `take = 3` spreads across three series.
    Slot,
}

/// Where a pool picks up when the generator runs again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Advance {
    /// Stateless: replay the same first N every generation.
    #[default]
    Restart,
    /// Continue from this pool's stored resume point (the `.resume` sidecar —
    /// see [`crate::resume`]). Combined with `take = N` this is "the next N
    /// episodes each time".
    Resume,
}

/// How a visit fills slots the current series can't supply. Only meaningful
/// with `rotate = "visit"`, where one visit draws `take` items from one series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OnShort {
    /// Rotate to the next series and fill the remaining slots from it, so a
    /// visit always emits `take` items unless the whole pool is dry.
    #[default]
    Next,
    /// Loop the same series back to its own start for the remaining slots.
    Wrap,
    /// Emit fewer items this visit and move on.
    Short,
}

/// One named resolved set inside a pattern block.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Pool {
    /// Pool name, referenced by [`PatternStep::pool`]. Unique within a channel
    /// (validated), which is what lets the `.resume` sidecar key on the name
    /// alone and survive block reordering.
    pub name: String,

    /// CEL expression resolved against the catalog, exactly like a `query`
    /// entry. Mutually exclusive with [`Pool::plugin`]: a pool names one source
    /// of items or the other, and validation rejects both or neither.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expr: Option<String>,

    /// A scorer plugin script that supplies this pool's items instead of a CEL
    /// expression — it runs its own queries, ranks what it finds, and returns
    /// the ordered set. Path is relative to the channel config's directory.
    ///
    /// It replaces `expr` rather than `order` because picking the candidates
    /// and ranking them are the same judgment: a "For You" pool cannot be
    /// written as a hand-authored expression plus a sort, since the expression
    /// is the half the config author least knows how to write. See ADR 0002.
    ///
    /// Everything downstream — `select`, `rotate`, `advance`, `on_short`, and
    /// the pattern's `take` — treats the returned list exactly like a
    /// CEL-resolved one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<PathBuf>,

    /// Internal order of the pool's resolved set. Unset keeps the query's own
    /// order. This also fixes the series rotation order: series rotate in
    /// order of first appearance in the ordered set.
    ///
    /// Meaningless on a `plugin` pool — the plugin returns its set already
    /// ranked, and re-sorting it would discard exactly the judgment the plugin
    /// exists to make — so validation rejects the pair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<Order>,

    #[serde(default)]
    pub select: Select,

    #[serde(default)]
    pub rotate: Rotate,

    #[serde(default)]
    pub advance: Advance,

    #[serde(default)]
    pub on_short: OnShort,

    /// Adjacency constraints applied to *this pool's* ordered list, before the
    /// pattern interleaves it (#115). Unset inherits the block's `constraints`;
    /// set, it **replaces** them wholesale rather than merging field by field,
    /// so a pool's table reads as the complete rule for that pool.
    ///
    /// The gap counts **this pool's own draws**, not aired channel positions:
    /// `no_repeat_within: 10` on a pool the pattern visits once per cycle spaces
    /// its repeats ten *draws* apart, which is ten cycles of aired schedule.
    /// That is the honest reading of a number authored on a pool, and it is what
    /// keeps the pattern's shape intact — the rule is enforced entirely inside
    /// the pool, so the interleave is never reordered and "2 movies then 3
    /// episodes" cannot be repaired into something else.
    ///
    /// Enforced in two places, because a pool makes repeats in two ways. Its
    /// resolved list is ordered under the rule before the pattern runs; and each
    /// draw is checked against what the pool recently emitted, which is where
    /// the rotation's own repeats come from — a series that only half-filled a
    /// visit keeps its turn, and a series played to its end loops. The list
    /// order alone cannot see either.
    ///
    /// **Sizing.** `cycles` is derived to drain the largest pool once, so a
    /// no-repeat rule on a pool the pattern draws heavily from will march
    /// through that pool's whole set in one window. That is the rule working;
    /// but on a channel whose replay policy lives elsewhere — a scorer plugin
    /// suppressing what recently aired — it leaves that policy nothing to hold
    /// back. See `examples/samples/foryou.yaml`, which declines the field for
    /// exactly this reason.
    ///
    /// **Blind across pools.** Each pool is constrained against its own list
    /// alone, so if two pools in one block resolve the same `entry_id`, neither
    /// pool's constraint can see the collision. Pools that must not collide have
    /// to be disjoint by construction — by their own expressions, or by a plugin
    /// that partitions what it returns — because the pool contract does not
    /// guarantee it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraints: Option<Constraints>,
}

impl Pool {
    /// This pool's effective constraints: its own if it declares any, otherwise
    /// the block's, otherwise unconstrained.
    pub fn constraints(&self, block: Option<&Constraints>) -> Constraints {
        self.constraints
            .clone()
            .or_else(|| block.cloned())
            .unwrap_or_default()
    }
}

/// One step of the repeating pattern template.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PatternStep {
    /// Name of the [`Pool`] this step draws from.
    pub pool: String,

    /// How many items to draw per visit.
    pub take: usize,

    /// Probability this step fires on a given pass through the pattern —
    /// "occasionally binge". `1.0` (the default) always fires. The roll is
    /// seeded from the channel `seed` plus the step's position, so a pinned
    /// seed reproduces the whole skip/fire sequence. A skipped step contributes
    /// nothing and does **not** consume the pool's resume point.
    #[serde(default = "default_chance")]
    pub chance: f64,
}

fn default_chance() -> f64 {
    1.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Dir;

    #[test]
    fn parses_a_pool_with_defaults_from_yaml() {
        let yaml = r#"
name: shows
expr: 'item.type == "episode"'
"#;
        let pool: Pool = serde_norway::from_str(yaml).unwrap();
        assert_eq!(pool.name, "shows");
        assert!(pool.order.is_none());
        assert!(pool.plugin.is_none());
        // Every knob defaults to the stateless, least-surprising behavior.
        assert_eq!(pool.select, Select::RoundRobin);
        assert_eq!(pool.rotate, Rotate::Visit);
        assert_eq!(pool.advance, Advance::Restart);
        assert_eq!(pool.on_short, OnShort::Next);
    }

    #[test]
    fn parses_a_fully_specified_pool() {
        let yaml = r#"
name: shows
expr: 'item.type == "episode"'
order: "season:asc,episode:asc"
select: random
rotate: slot
advance: resume
on_short: short
"#;
        let pool: Pool = serde_norway::from_str(yaml).unwrap();
        assert_eq!(pool.select, Select::Random);
        assert_eq!(pool.rotate, Rotate::Slot);
        assert_eq!(pool.advance, Advance::Resume);
        assert_eq!(pool.on_short, OnShort::Short);
        match pool.order.as_ref().unwrap() {
            Order::Fields(terms) => {
                assert_eq!(terms.len(), 2);
                assert_eq!(terms[0].field, "season");
                assert_eq!(terms[0].dir, Dir::Asc);
            }
            other => panic!("expected field order, got {other:?}"),
        }
    }

    #[test]
    fn pattern_step_chance_defaults_to_always_fire() {
        let step: PatternStep = serde_norway::from_str("pool: shows\ntake: 3\n").unwrap();
        assert_eq!(step.take, 3);
        assert_eq!(step.chance, 1.0);
    }

    #[test]
    fn parses_pattern_step_from_toml_inline_table() {
        let step: PatternStep =
            toml::from_str("pool = \"shows\"\ntake = 3\nchance = 0.3\n").unwrap();
        assert_eq!(step.chance, 0.3);
    }

    #[test]
    fn parses_pool_constraints() {
        let yaml = "name: movies\nexpr: 'x'\nconstraints:\n  no_repeat_within: 5\n";
        let pool: Pool = serde_norway::from_str(yaml).unwrap();
        assert_eq!(pool.constraints.as_ref().unwrap().no_repeat_within, Some(5));
    }

    fn bare(name: &str) -> Pool {
        serde_norway::from_str(&format!("name: {name}\nexpr: 'x'\n")).unwrap()
    }

    #[test]
    fn a_pool_declaring_nothing_inherits_the_block() {
        let block = Constraints {
            no_repeat_within: Some(3),
            separate_by: None,
            separate_min_gap: None,
        };
        assert_eq!(bare("movies").constraints(Some(&block)), block);
    }

    #[test]
    fn a_pool_with_no_block_default_is_unconstrained() {
        assert_eq!(bare("movies").constraints(None), Constraints::default());
    }

    /// A pool's own table *replaces* the block's rather than merging field by
    /// field: the pool separates on cast and does not inherit the block's
    /// `no_repeat_within`, so its table reads as the whole rule for that pool.
    #[test]
    fn a_pool_declaring_its_own_replaces_the_block_wholesale() {
        let block = Constraints {
            no_repeat_within: Some(3),
            separate_by: None,
            separate_min_gap: None,
        };
        let mut pool = bare("movies");
        pool.constraints = Some(Constraints {
            no_repeat_within: None,
            separate_by: Some("cast".into()),
            separate_min_gap: Some(2),
        });
        let effective = pool.constraints(Some(&block));
        assert_eq!(effective.no_repeat_gap(), 0);
        assert_eq!(effective.separate_gap(), 2);
    }

    #[test]
    fn rejects_an_unknown_pool_field() {
        let yaml = "name: shows\nexpr: 'x'\nselekt: random\n";
        assert!(serde_norway::from_str::<Pool>(yaml).is_err());
    }
}

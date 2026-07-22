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
    fn rejects_an_unknown_pool_field() {
        let yaml = "name: shows\nexpr: 'x'\nselekt: random\n";
        assert!(serde_norway::from_str::<Pool>(yaml).is_err());
    }
}

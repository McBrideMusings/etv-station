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

use std::fmt;
use std::path::PathBuf;

use serde::de::{self, Deserializer, Visitor};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

use super::constraints::Constraints;
use super::order::Order;

/// A named set of sibling shows — a franchise — declared once on a channel
/// and referenced by name from any pool's [`Pool::groups`] (#165).
///
/// Plex stores a franchise's spin-offs as unrelated shows (*RuPaul's Drag
/// Race* and *RuPaul's Drag Race All Stars* share no `show_id`), so a pool
/// grouped or rotated by show treats them as unrelated series. A group makes
/// their union one rotation domain: `group_by = "season"` still cuts at each
/// member's own season boundaries, but the series it produces come from every
/// member show, so `rotate = "visit"` cycles the franchise instead of parking
/// on one show's runs before moving to the next.
///
/// A member is named by its Plex show title, matched exactly against the
/// catalog's `show` column — the same string a `query` entry's `item.show`
/// would compare against. A title with no episodes on record fails the
/// channel's generation, naming both the show and the group (validated
/// against the catalog, since the structural load pass has no catalog to
/// check against). A show may belong to more than one group; a pool whose own
/// [`Pool::groups`] would combine two that share a member is rejected at
/// load, naming the show — folding it into two rotation domains at once has
/// no single answer.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShowGroup {
    /// Group name, referenced by [`Pool::groups`]. Unique within a channel
    /// (validated).
    pub name: String,

    /// Member show titles, matched exactly against the catalog's `show`
    /// column.
    pub shows: Vec<String>,
}

/// Where a pool's items are cut into series — the buckets a draw picks between.
///
/// The cut is the whole of the "play a season start to finish" feature (#155):
/// with a season-sized bucket, `select = "random"` plus `take = "all"` *is*
/// "pick a random season and air it end to end", and nothing in the walk needs
/// to know what a season is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    /// One bucket per show — every episode of a show in one pile, seasons
    /// running together. The broadcast-like default.
    #[default]
    Show,
    /// One bucket per season of a show. An episode the catalog has no season
    /// number for stays with its show rather than becoming a bucket of one.
    Season,
}

/// How many items one visit to a pattern step draws.
///
/// Written in the channel file as either a number (`take: 3`) or the word
/// `all` (`take: "all"`). `All` empties the bucket the visit picked, which is
/// how a whole season — or a whole show, under `group_by = "show"` — airs
/// without the author having to know how many episodes are in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    Count(usize),
    All,
}

impl Take {
    /// The authored number, or `None` for `all`. Callers that must size a step
    /// before anything has touched the catalog use this and say what they do
    /// with the `None` — the count `all` stands for is not knowable until a
    /// bucket has been picked.
    pub fn count(self) -> Option<usize> {
        match self {
            Take::Count(n) => Some(n),
            Take::All => None,
        }
    }
}

impl Serialize for Take {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Take::Count(n) => serializer.serialize_u64(*n as u64),
            Take::All => serializer.serialize_str("all"),
        }
    }
}

impl<'de> Deserialize<'de> for Take {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // A hand-written visitor rather than an untagged enum: untagged buffers
        // the value through a self-describing intermediate, which the TOML and
        // YAML front ends do not agree on for a bare scalar.
        struct TakeVisitor;

        impl Visitor<'_> for TakeVisitor {
            type Value = Take;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a positive integer or the string \"all\"")
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Take, E> {
                Ok(Take::Count(v as usize))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Take, E> {
                if v < 0 {
                    return Err(E::custom(format!(
                        "take = {v} is negative (want a positive integer or \"all\")"
                    )));
                }
                Ok(Take::Count(v as usize))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Take, E> {
                match v {
                    "all" => Ok(Take::All),
                    other => Err(E::custom(format!(
                        "invalid take {other:?} (want a positive integer or \"all\")"
                    ))),
                }
            }
        }

        deserializer.deserialize_any(TakeVisitor)
    }
}

/// Where in a series a visit cuts its slice.
///
/// A sort cannot express this, which is why it is its own word: `order =
/// "episode:desc"` does put the last three episodes first, but it also plays
/// them backwards, because the sort that chooses the slice is the same sort
/// that decides play order. `from` moves the cut without touching either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TakeFrom {
    /// Where the series left off — its resume cursor, or its top. The default,
    /// and the only one that continues a series across visits.
    #[default]
    Start,
    /// The last `take` items of the series, in list order. An absolute
    /// position, so it ignores the resume cursor: "the last three episodes"
    /// means the same thing every visit, which is the point of asking for it.
    End,
    /// A seeded offset, then `take` consecutive items in list order. Also
    /// absolute, and re-rolled per visit; a pinned `seed` reproduces it.
    Random,
}

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
    /// entry. Mutually exclusive with [`Pool::plugin`] and [`Pool::groups`]: a
    /// pool names exactly one source of items, and validation rejects two or
    /// none.
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
    ///
    /// **Replay is the plugin's, unless this pool claims it.** ETV computes no
    /// replay policy for a plugin pool: it hands the script `ctx.recent` and
    /// takes back whatever order comes out. A scorer that suppresses what it
    /// recently returned holds a title back for as long as its own policy says;
    /// one written without suppression hands back the same top-ranked item
    /// every generation, and with
    /// `advance: restart` nothing else in the config stops it — a valid
    /// schedule that plays one film forever. Swapping the script swaps that
    /// behavior, and neither the pool nor the pattern can tell which kind it
    /// got. [`Pool::constraints`] with `no_repeat_within` is the channel
    /// author's own floor, enforced inside pool resolution over the list the
    /// plugin returned, independent of what the script does.
    ///
    /// It is deliberately opt-in and it is **not** a free addition on top of a
    /// scorer that already suppresses: the two do not layer. `cycles` is
    /// derived to drain the largest pool once, so a no-repeat rule marches the
    /// pool through its whole returned set inside one window and leaves the
    /// script's own `ctx.recent` suppression nothing left to hold back. Set it
    /// where the config is the only thing guarding replay; leave it unset where
    /// the script is. See the sizing note on [`Pool::constraints`], ADR 0002,
    /// and `examples/samples/foryou.yaml`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<PathBuf>,

    /// Named show groups (#165) this pool draws its items from — every member
    /// show's episodes, across every group listed, unioned. Mutually
    /// exclusive with [`Pool::expr`] and [`Pool::plugin`]: a pool names
    /// exactly one source of items.
    ///
    /// Each name is looked up in the channel's own `groups:` declarations
    /// (unknown name: rejected at load, naming the pool and the group).
    /// Everything downstream — `group_by`, `select`, `rotate`, `advance`,
    /// `on_short`, `order`, and the pattern's `take` — treats the unioned set
    /// exactly like a CEL-resolved one; a group only changes *which* items a
    /// pool draws, never how the draw itself works.
    ///
    /// Naming more than one group is for combining separate franchises in one
    /// pool (a general "Bravo" pool where RuPaul and Below Deck each stay
    /// their own rotation domain rather than merging into one). The groups
    /// named here must be disjoint: two that share a member show are rejected
    /// at load, naming the show — its episodes would otherwise belong to two
    /// rotation domains at once, and there is no rule for picking one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,

    /// Internal order of the pool's resolved set. Unset keeps the query's own
    /// order. This also fixes the series rotation order: series rotate in
    /// order of first appearance in the ordered set.
    ///
    /// Meaningless on a `plugin` pool — the plugin returns its set already
    /// ranked, and re-sorting it would discard exactly the judgment the plugin
    /// exists to make — so validation rejects the pair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<Order>,

    /// The order the *series* come up in, when it should differ from the order
    /// their items play in.
    ///
    /// [`Pool::order`] does both jobs by default — it sorts the flat list, and
    /// the series then rotate in order of first appearance in it. That is one
    /// answer to two questions, and it only holds while both want the same
    /// sort. "Newest show first, but its episodes in episode order" wants
    /// `year:desc` between series and `season:asc,episode:asc` inside one, and
    /// no single sort is both.
    ///
    /// Set here, the series sequence is read from this sort instead and `order`
    /// keeps its other job unchanged. A series is placed by whichever of its
    /// items this sort puts first, so "the season with the newest episode"
    /// rather than an average of anything — a rule that can be checked against
    /// the catalog by eye.
    ///
    /// Meaningless without series to sequence and meaningless on a plugin pool,
    /// on the same grounds as `order`: validation rejects the pair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket_order: Option<Order>,

    /// Where this pool's items are cut into series. Unset groups by show, which
    /// is what every channel written before #155 means.
    #[serde(default)]
    pub group_by: GroupBy,

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

    /// Arbitrary configuration handed to this pool's [`Pool::plugin`] verbatim,
    /// reaching the script as `ctx.config`. Any YAML shape is accepted — maps,
    /// arrays, strings, numbers, booleans, nested to any depth — and no key is
    /// reserved.
    ///
    /// **The station never reads it.** It is converted and passed through, and
    /// that is the whole of the contract: nothing here is validated, no key is
    /// known, no default is injected, and an unrecognised key is not an error.
    /// This is what keeps a scorer swappable — a script's tunables are its own
    /// vocabulary, and ETV holding opinions about them would make the station a
    /// party to the taste it exists not to have. See ADR 0002.
    ///
    /// Unset yields an empty map rather than a missing key, so a script may read
    /// `ctx.config.anything` unconditionally and get unit back.
    ///
    /// **A typo is silent, by construction.** `afinity_window_days` reads as
    /// unset and the script falls back to its own default, exactly as if the key
    /// had been omitted. That is the price of the station not interpreting the
    /// bag, and it is not worth buying off with station-side validation: any
    /// list of known keys here would have to be updated for every script anyone
    /// writes, which is the coupling this field exists to avoid. A script that
    /// wants strictness can declare its own expected keys and check them.
    ///
    /// **A non-finite float is refused at load, naming the key.** `weight: .inf`
    /// (or `-.inf`, or `.nan`) has no carrier representation, so rather than let
    /// it reach the script as unit the channel fails to load and says which key
    /// is at fault. An author wanting "never decays" writes a large finite
    /// number. This is the one thing inside the bag the station does judge, and
    /// it is not a judgement about meaning: the value cannot be carried at all.
    /// The overlay side refuses the same value in the same words (#130).
    ///
    /// Meaningless on an `expr` pool — a CEL expression has no script to read
    /// it — where it is ignored rather than rejected, on the same terms as an
    /// unrecognised key inside it. The non-finite refusal above is the one thing
    /// that still bites there, because it happens while the file is being
    /// parsed, before anything knows whether this pool has a script at all.
    ///
    /// Carried as a `serde_json::Value` rather than the channel format's own
    /// value type: one opaque-config carrier for the whole project, so a third
    /// scripting surface inherits the decision instead of picking a type by
    /// looking at which file format it happens to parse. The same type carries
    /// `etv_overlay::OverlaySpec::config`. It is a carrier, not a format — a
    /// pool's `config` is authored in the channel YAML like everything else.
    #[serde(
        default,
        deserialize_with = "etv_overlay::config_carrier::deserialize_config",
        skip_serializing_if = "Option::is_none"
    )]
    pub config: Option<serde_json::Value>,
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

    /// How many items to draw per visit — a number, or `"all"` to empty the
    /// bucket the visit picked.
    pub take: Take,

    /// Where in the series the visit cuts those items from. Unset continues
    /// from where the series left off, which is what every channel written
    /// before #155 means.
    #[serde(default)]
    pub from: TakeFrom,

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
    use crate::config::NoRepeatWithin;

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
        assert!(pool.groups.is_empty());
        // Every knob defaults to the stateless, least-surprising behavior.
        assert_eq!(pool.select, Select::RoundRobin);
        assert_eq!(pool.rotate, Rotate::Visit);
        assert_eq!(pool.advance, Advance::Restart);
        assert_eq!(pool.on_short, OnShort::Next);
    }

    #[test]
    fn parses_a_pool_sourced_from_named_groups() {
        let yaml = r#"
name: rupaul
groups: ["rupaul", "below_deck"]
group_by: season
"#;
        let pool: Pool = serde_norway::from_str(yaml).unwrap();
        assert_eq!(pool.groups, vec!["rupaul", "below_deck"]);
        assert!(pool.expr.is_none());
        assert!(pool.plugin.is_none());
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
        assert_eq!(step.take, Take::Count(3));
        assert_eq!(step.chance, 1.0);
    }

    #[test]
    fn parses_pattern_step_from_toml_inline_table() {
        let step: PatternStep =
            toml::from_str("pool = \"shows\"\ntake = 3\nchance = 0.3\n").unwrap();
        assert_eq!(step.chance, 0.3);
    }

    #[test]
    fn group_by_defaults_to_show_and_parses_season() {
        let bare: Pool = serde_norway::from_str("name: shows\nexpr: 'x'\n").unwrap();
        assert_eq!(bare.group_by, GroupBy::Show);
        let seasoned: Pool =
            serde_norway::from_str("name: shows\nexpr: 'x'\ngroup_by: season\n").unwrap();
        assert_eq!(seasoned.group_by, GroupBy::Season);
    }

    /// `take` accepts a number or the word `all`, in both channel formats — the
    /// integer-or-string shape is the one thing a hand-written visitor buys, so
    /// it is worth proving on each front end rather than one.
    #[test]
    fn take_parses_a_count_or_all_from_either_format() {
        let yaml: PatternStep = serde_norway::from_str("pool: shows\ntake: all\n").unwrap();
        assert_eq!(yaml.take, Take::All);
        let yaml_quoted: PatternStep =
            serde_norway::from_str("pool: shows\ntake: \"all\"\n").unwrap();
        assert_eq!(yaml_quoted.take, Take::All);
        let toml_all: PatternStep = toml::from_str("pool = \"shows\"\ntake = \"all\"\n").unwrap();
        assert_eq!(toml_all.take, Take::All);
        let toml_count: PatternStep = toml::from_str("pool = \"shows\"\ntake = 5\n").unwrap();
        assert_eq!(toml_count.take, Take::Count(5));
    }

    #[test]
    fn from_defaults_to_start_and_parses_its_other_two() {
        let bare: PatternStep = serde_norway::from_str("pool: shows\ntake: 3\n").unwrap();
        assert_eq!(bare.from, TakeFrom::Start);
        let end: PatternStep = serde_norway::from_str("pool: shows\ntake: 3\nfrom: end\n").unwrap();
        assert_eq!(end.from, TakeFrom::End);
        let random: PatternStep =
            toml::from_str("pool = \"shows\"\ntake = 3\nfrom = \"random\"\n").unwrap();
        assert_eq!(random.from, TakeFrom::Random);
    }

    #[test]
    fn bucket_order_is_its_own_optional_sort() {
        let bare: Pool = serde_norway::from_str("name: shows\nexpr: 'x'\n").unwrap();
        assert!(bare.bucket_order.is_none());
        let both: Pool = serde_norway::from_str(
            "name: shows\nexpr: 'x'\norder: \"episode:asc\"\nbucket_order: \"year:desc\"\n",
        )
        .unwrap();
        assert_eq!(
            both.bucket_order,
            Some(Order::parse("year:desc").unwrap()),
            "the two sorts are independent"
        );
        assert_eq!(both.order, Some(Order::parse("episode:asc").unwrap()));
    }

    /// A misspelling has to fail at load naming what was wanted, not read as
    /// some default and quietly air the wrong amount.
    #[test]
    fn an_unknown_take_word_is_refused_by_name() {
        let err = serde_norway::from_str::<PatternStep>("pool: shows\ntake: everything\n")
            .expect_err("only \"all\" is a word take accepts")
            .to_string();
        assert!(err.contains("everything"), "err = {err}");
        assert!(err.contains("\"all\""), "err = {err}");
    }

    #[test]
    fn parses_pool_constraints() {
        let yaml = "name: movies\nexpr: 'x'\nconstraints:\n  no_repeat_within: 5\n";
        let pool: Pool = serde_norway::from_str(yaml).unwrap();
        assert_eq!(
            pool.constraints.as_ref().unwrap().no_repeat_within,
            Some(NoRepeatWithin::Positions(5))
        );
    }

    #[test]
    fn parses_a_show_group() {
        let yaml = "name: rupaul\nshows:\n  - \"RuPaul's Drag Race\"\n  - \"RuPaul's Drag Race All Stars\"\n";
        let group: ShowGroup = serde_norway::from_str(yaml).unwrap();
        assert_eq!(group.name, "rupaul");
        assert_eq!(
            group.shows,
            vec!["RuPaul's Drag Race", "RuPaul's Drag Race All Stars"]
        );
    }

    fn bare(name: &str) -> Pool {
        serde_norway::from_str(&format!("name: {name}\nexpr: 'x'\n")).unwrap()
    }

    #[test]
    fn a_pool_declaring_nothing_inherits_the_block() {
        let block = Constraints {
            no_repeat_within: Some(NoRepeatWithin::Positions(3)),
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
            no_repeat_within: Some(NoRepeatWithin::Positions(3)),
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
        assert_eq!(effective.no_repeat_gap(), NoRepeatWithin::Positions(0));
        assert_eq!(effective.separate_gap(), 2);
    }

    #[test]
    fn rejects_an_unknown_pool_field() {
        let yaml = "name: shows\nexpr: 'x'\nselekt: random\n";
        assert!(serde_norway::from_str::<Pool>(yaml).is_err());
    }

    /// The channel YAML parses straight into the carrier type with no
    /// intermediate value — nesting and scalar types intact, and a YAML
    /// timestamp landing as the text the author wrote, which is the same thing
    /// an overlay's TOML datetime becomes on the other surface (#129).
    #[test]
    fn config_parses_from_yaml_into_the_shared_carrier_type() {
        let yaml = r#"
name: movies
expr: 'x'
config:
  name: taste
  released: 2026-07-28
  weights:
    affinity: 3.0
    steps: 3
    enabled: true
    labels: ["a", "b"]
"#;
        let pool: Pool = serde_norway::from_str(yaml).unwrap();
        let config = pool.config.as_ref().unwrap();
        assert_eq!(config["name"], serde_json::json!("taste"));
        assert_eq!(config["released"], serde_json::json!("2026-07-28"));
        assert_eq!(config["weights"]["affinity"], serde_json::json!(3.0));
        assert_eq!(config["weights"]["steps"], serde_json::json!(3));
        assert_eq!(config["weights"]["enabled"], serde_json::json!(true));
        assert_eq!(config["weights"]["labels"][1], serde_json::json!("b"));
    }

    /// A float the carrier cannot hold fails the pool's load and names the key
    /// that holds it, wherever in the bag it sits — rather than reaching the
    /// scorer as unit with nothing said (#130).
    #[test]
    fn a_non_finite_float_in_config_fails_the_load_and_names_the_key() {
        for (bag, key) in [
            ("  weight: .inf\n", "config.weight"),
            ("  weight: .nan\n", "config.weight"),
            ("  weight: -.inf\n", "config.weight"),
            ("  steps: [1.0, .inf]\n", "config.steps[1]"),
            ("  fade:\n    weight: .nan\n", "config.fade.weight"),
        ] {
            let yaml = format!("name: movies\nexpr: 'x'\nconfig:\n{bag}");
            let err = serde_norway::from_str::<Pool>(&yaml)
                .expect_err("a non-finite float must fail the load")
                .to_string();
            assert!(err.contains(key), "error did not name `{key}`: {err}");
            assert!(
                err.contains("large finite number"),
                "error did not tell the author what to write instead: {err}"
            );
        }
    }
}

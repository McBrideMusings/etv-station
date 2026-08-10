use std::fmt;
use std::time::Duration;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Assumed floor for how short an item can run, used only to size how much
/// aired history a temporal `no_repeat_within` needs at the generation seam
/// (see [`Constraints::reach`]). Nothing about the constraint itself is this
/// coarse — once the history is in hand it is measured against real
/// (estimated) item durations, in [`crate::constrain`] — this only bounds how
/// far back to look for it, generously enough that a real channel's items
/// never run shorter.
const MIN_ASSUMED_ITEM_SECS: u64 = 60;

/// The two spellings `no_repeat_within` accepts (#185).
///
/// **Positional** (`no_repeat_within = 10`) counts list positions: the same
/// `entry_id` may not recur within N items, full stop. Right for a pool whose
/// items run a uniform length, where N positions is also a fixed span of
/// time — and the original meaning, which every config written before #185
/// is authored against.
///
/// **Temporal** (`no_repeat_within = "24h"`) counts wall-clock time instead,
/// measured against the emitted schedule rather than list position — a
/// duration string in the shape `humantime` parses (`"24h"`, `"90m"`,
/// `"1h30m"`). Right for a pool that mixes durations: ten *positions* in a
/// pool mixing 22-minute episodes with 3-hour films spans anywhere from 3.5
/// to 30 hours depending on what the pattern happens to draw, which is not
/// the guarantee an author writing "not twice in a day" is asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoRepeatWithin {
    /// `entry_id` may not recur within this many list positions.
    Positions(usize),
    /// `entry_id` may not recur within this much time, measured from the
    /// start of one airing to the start of the next.
    Duration(Duration),
}

impl Default for NoRepeatWithin {
    /// `Positions(0)` — the unconstrained sentinel `Constraints::no_repeat_gap`
    /// resolves an absent value to, matching `separate`'s own `0`.
    fn default() -> Self {
        NoRepeatWithin::Positions(0)
    }
}

impl<'de> Deserialize<'de> for NoRepeatWithin {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct NoRepeatWithinVisitor;

        impl Visitor<'_> for NoRepeatWithinVisitor {
            type Value = NoRepeatWithin;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("no_repeat_within: a position count, or a duration string like \"24h\"")
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(NoRepeatWithin::Positions(v as usize))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                usize::try_from(v)
                    .map(NoRepeatWithin::Positions)
                    .map_err(|_| E::custom(format!("no_repeat_within must be positive, got {v}")))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                humantime::parse_duration(v)
                    .map(NoRepeatWithin::Duration)
                    .map_err(|e| {
                        E::custom(format!("no_repeat_within: invalid duration {v:?}: {e}"))
                    })
            }
        }

        deserializer.deserialize_any(NoRepeatWithinVisitor)
    }
}

impl Serialize for NoRepeatWithin {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            NoRepeatWithin::Positions(n) => serializer.serialize_u64(*n as u64),
            NoRepeatWithin::Duration(d) => {
                serializer.serialize_str(&humantime::format_duration(*d).to_string())
            }
        }
    }
}

/// Adjacency constraints applied to a block's list *after* ordering (#73).
///
/// Distinct from [`super::block::Duplicates`], which is identity over the whole
/// block list: these govern what may sit *next to* what.
///
/// ```toml
/// [constraints]
/// no_repeat_within = 1        # positions
/// # no_repeat_within = "24h"  # or wall-clock time (#185)
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Constraints {
    /// The same `entry_id` may not recur within N positions, or within a span
    /// of wall-clock time — see [`NoRepeatWithin`]. Absent leaves the block
    /// unconstrained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_repeat_within: Option<NoRepeatWithin>,

    /// A multi-valued catalog field whose values must be spread apart — the
    /// property-level constraint, as opposed to `no_repeat_within`'s identity
    /// one. Named with the same vocabulary an expression uses, so
    /// `separate_by: "cast"` separates on the same values `item.cast` reads.
    ///
    /// Two items are in conflict when they share **any** value of the field, so
    /// `separate_by: "cast"` means no two films close together share a
    /// performer — not that they have identical casts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub separate_by: Option<String>,

    /// How many positions apart [`Self::separate_by`] values must sit. `1` means
    /// never adjacent. Defaults to `1` when `separate_by` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub separate_min_gap: Option<usize>,
}

impl Constraints {
    /// The effective repeat gap: the unconstrained sentinel `Positions(0)` when
    /// unset.
    pub fn no_repeat_gap(&self) -> NoRepeatWithin {
        self.no_repeat_within.unwrap_or_default()
    }

    /// The effective separation gap: `0` when no field is being separated on.
    /// A field with no explicit gap separates adjacent items only.
    pub fn separate_gap(&self) -> usize {
        match self.separate_by {
            Some(_) => self.separate_min_gap.unwrap_or(1),
            None => 0,
        }
    }

    /// The widest distance this block reaches back, in **positions** — how
    /// much recently-aired history the adjacency pass needs to size at a
    /// generation seam (#73).
    ///
    /// A temporal `no_repeat_within` has no position count of its own, so this
    /// converts it to a conservative one: see [`MIN_ASSUMED_ITEM_SECS`]. The
    /// constraint itself is not this coarse — [`crate::constrain`] measures it
    /// against real (estimated) item durations once the history this sizes is
    /// in hand.
    pub fn reach(&self) -> usize {
        let no_repeat_positions = match self.no_repeat_gap() {
            NoRepeatWithin::Positions(n) => n,
            NoRepeatWithin::Duration(d) => {
                usize::try_from(d.as_secs().div_ceil(MIN_ASSUMED_ITEM_SECS)).unwrap_or(usize::MAX)
            }
        };
        no_repeat_positions.max(self.separate_gap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_no_repeat_within() {
        let c: Constraints = toml::from_str("no_repeat_within = 2").unwrap();
        assert_eq!(c.no_repeat_within, Some(NoRepeatWithin::Positions(2)));
        assert_eq!(c.no_repeat_gap(), NoRepeatWithin::Positions(2));
    }

    #[test]
    fn parses_a_temporal_no_repeat_within() {
        let c: Constraints = toml::from_str("no_repeat_within = \"24h\"").unwrap();
        assert_eq!(
            c.no_repeat_within,
            Some(NoRepeatWithin::Duration(Duration::from_secs(24 * 60 * 60)))
        );
    }

    #[test]
    fn rejects_an_unparseable_duration_naming_the_key() {
        let err = toml::from_str::<Constraints>("no_repeat_within = \"not-a-duration\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no_repeat_within"), "{err}");
    }

    #[test]
    fn defaults_to_unconstrained() {
        let c = Constraints::default();
        assert_eq!(c.no_repeat_within, None);
        assert_eq!(c.no_repeat_gap(), NoRepeatWithin::Positions(0));
        assert_eq!(c.reach(), 0);
    }

    #[test]
    fn parses_from_yaml() {
        let c: Constraints = serde_norway::from_str("no_repeat_within: 1").unwrap();
        assert_eq!(c.no_repeat_within, Some(NoRepeatWithin::Positions(1)));

        let c: Constraints = serde_norway::from_str("no_repeat_within: 24h").unwrap();
        assert_eq!(
            c.no_repeat_within,
            Some(NoRepeatWithin::Duration(Duration::from_secs(24 * 60 * 60)))
        );
    }

    #[test]
    fn a_temporal_gap_reaches_a_conservative_position_estimate() {
        let c: Constraints = toml::from_str("no_repeat_within = \"2m\"").unwrap();
        // 120s / 60s-per-assumed-item = 2 positions, never fewer than what a
        // real channel — where nothing runs that short — would need.
        assert_eq!(c.reach(), 2);
    }

    #[test]
    fn a_positional_gap_still_reaches_exactly_itself() {
        let c: Constraints =
            toml::from_str("no_repeat_within = 5\nseparate_by = \"cast\"\nseparate_min_gap = 2")
                .unwrap();
        assert_eq!(c.reach(), 5);
    }

    #[test]
    fn round_trips_a_temporal_value_through_toml() {
        let c = Constraints {
            no_repeat_within: Some(NoRepeatWithin::Duration(Duration::from_secs(3600))),
            ..Default::default()
        };
        let text = toml::to_string(&c).unwrap();
        assert!(text.contains("1h"), "{text}");
        let back: Constraints = toml::from_str(&text).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn separate_by_defaults_to_adjacent_only() {
        let c: Constraints = toml::from_str("separate_by = \"cast\"").unwrap();
        assert_eq!(c.separate_gap(), 1);
        assert_eq!(c.reach(), 1);
    }

    #[test]
    fn separate_gap_is_zero_without_a_field() {
        // A gap with nothing to separate on constrains nothing; validation
        // rejects that pairing, but the accessor must not claim otherwise.
        let c = Constraints {
            no_repeat_within: None,
            separate_by: None,
            separate_min_gap: Some(3),
        };
        assert_eq!(c.separate_gap(), 0);
    }

    #[test]
    fn reach_is_the_wider_of_the_two() {
        let c: Constraints =
            toml::from_str("no_repeat_within = 1\nseparate_by = \"cast\"\nseparate_min_gap = 4")
                .unwrap();
        assert_eq!(c.reach(), 4);
    }

    #[test]
    fn rejects_unknown_field() {
        assert!(toml::from_str::<Constraints>("separate_bye = \"cast\"").is_err());
    }
}

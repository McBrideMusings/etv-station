use serde::{Deserialize, Serialize};

/// Adjacency constraints applied to a block's list *after* ordering (#73).
///
/// Distinct from [`super::block::Duplicates`], which is identity over the whole
/// block list: these govern what may sit *next to* what.
///
/// ```toml
/// [constraints]
/// no_repeat_within = 1
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Constraints {
    /// The same `entry_id` may not recur within N positions. `1` means no
    /// back-to-back repeat. Absent leaves the block unconstrained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_repeat_within: Option<usize>,

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
    /// The effective repeat gap: `0` means unconstrained.
    pub fn no_repeat_gap(&self) -> usize {
        self.no_repeat_within.unwrap_or(0)
    }

    /// The effective separation gap: `0` when no field is being separated on.
    /// A field with no explicit gap separates adjacent items only.
    pub fn separate_gap(&self) -> usize {
        match self.separate_by {
            Some(_) => self.separate_min_gap.unwrap_or(1),
            None => 0,
        }
    }

    /// The widest distance this block reaches back — how much recently-aired
    /// history the adjacency pass needs to enforce it across a generation seam.
    pub fn reach(&self) -> usize {
        self.no_repeat_gap().max(self.separate_gap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_no_repeat_within() {
        let c: Constraints = toml::from_str("no_repeat_within = 2").unwrap();
        assert_eq!(c.no_repeat_within, Some(2));
        assert_eq!(c.no_repeat_gap(), 2);
    }

    #[test]
    fn defaults_to_unconstrained() {
        let c = Constraints::default();
        assert_eq!(c.no_repeat_within, None);
        assert_eq!(c.no_repeat_gap(), 0);
    }

    #[test]
    fn parses_from_yaml() {
        let c: Constraints = serde_norway::from_str("no_repeat_within: 1").unwrap();
        assert_eq!(c.no_repeat_within, Some(1));
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

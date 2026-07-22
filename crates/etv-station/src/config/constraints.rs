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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Constraints {
    /// The same `entry_id` may not recur within N positions. `1` means no
    /// back-to-back repeat. Absent leaves the block unconstrained.
    ///
    /// The proposed property-level form (`separate_by = "<field>"` +
    /// `separate_min_gap`, e.g. no two adjacent films sharing any `cast`) is
    /// deliberately not part of v1; it would land beside this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_repeat_within: Option<usize>,
}

impl Constraints {
    /// The effective repeat gap: `0` means unconstrained.
    pub fn no_repeat_gap(&self) -> usize {
        self.no_repeat_within.unwrap_or(0)
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
    fn rejects_unknown_field() {
        // `separate_by` is proposed, not v1 — it must fail loudly rather than
        // be silently ignored by a config that expects it to work.
        assert!(toml::from_str::<Constraints>("separate_by = \"cast\"").is_err());
    }
}

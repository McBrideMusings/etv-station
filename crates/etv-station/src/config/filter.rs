use serde::{Deserialize, Serialize};

/// Structured filter applied to a block's resolved items before mode/order.
///
/// The fields are intentionally narrow for now — the broader field set and the
/// actual filtering pass live with the resolution engine (#69) and query field
/// set (#68). This issue (#46) only fixes the on-disk shape so dependent issues
/// parse against a settled type. The resolver rejects a present filter as
/// not-yet-implemented.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    /// Restrict to these season numbers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seasons: Option<Vec<u32>>,

    /// Restrict to these entry/episode ids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode_ids: Option<Vec<String>>,
}

impl Filter {
    /// True when no filter field is set — a `[filter]` table with no keys is
    /// treated as absent by the resolver.
    pub fn is_empty(&self) -> bool {
        self.seasons.is_none() && self.episode_ids.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Holder {
        filter: Filter,
    }

    #[test]
    fn parses_fields() {
        let h: Holder =
            toml::from_str("[filter]\nseasons = [1, 2]\nepisode_ids = [\"a\", \"b\"]").unwrap();
        assert_eq!(h.filter.seasons, Some(vec![1, 2]));
        assert_eq!(
            h.filter.episode_ids,
            Some(vec!["a".to_string(), "b".to_string()])
        );
        assert!(!h.filter.is_empty());
    }

    #[test]
    fn empty_table_is_empty() {
        let h: Holder = toml::from_str("[filter]\n").unwrap();
        assert!(h.filter.is_empty());
    }

    #[test]
    fn rejects_unknown_field() {
        assert!(toml::from_str::<Holder>("[filter]\nbogus = 1").is_err());
    }
}

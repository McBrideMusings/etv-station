use serde::{Deserialize, Serialize};

/// Structured filter applied to a block's resolved items before mode/order.
///
/// The fields are intentionally narrow for now — a broader field set lives
/// with the query field set (#68), if one is ever needed here too. The
/// resolver applies both fields as a narrowing set: an item survives only
/// when it satisfies every field that is set (see `resolve::apply_filter`,
/// #197).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
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

    /// An unknown key parses rather than failing the file, and is
    /// reported by name so a misspelling is still visible. The reporting is
    /// `config::load`'s job — this proves the type no longer refuses.
    #[test]
    fn an_unknown_field_is_ignored_and_named() {
        let holder: Holder =
            toml::from_str("[filter]\nbogus = 1").expect("an unknown key must not fail the file");
        assert!(holder.filter.is_empty(), "the unknown key set nothing");

        let mut ignored = Vec::new();
        let de = toml::Deserializer::new("[filter]\nbogus = 1");
        let _: Holder =
            serde_ignored::deserialize(de, |p| ignored.push(p.to_string())).expect("parses");
        assert_eq!(ignored, vec!["filter.bogus".to_string()]);
    }
}

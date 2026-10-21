use std::sync::Arc;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};

use super::arcs::Arcs;

/// One entry of `filter.seasons`: a catalog season number, or (#163) the name
/// of a show arc, which counts as a pseudo-season of the show that declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeasonSel {
    Number(u32),
    Arc(String),
}

impl From<u32> for SeasonSel {
    fn from(n: u32) -> Self {
        SeasonSel::Number(n)
    }
}

impl From<&str> for SeasonSel {
    fn from(name: &str) -> Self {
        SeasonSel::Arc(name.to_string())
    }
}

impl Serialize for SeasonSel {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            SeasonSel::Number(n) => serializer.serialize_u32(*n),
            SeasonSel::Arc(name) => serializer.serialize_str(name),
        }
    }
}

impl<'de> Deserialize<'de> for SeasonSel {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // A hand-written visitor rather than an untagged enum, for the reason
        // `Take` gives: the TOML and YAML front ends do not agree on how a bare
        // scalar buffers through an untagged enum.
        struct SelVisitor;
        impl Visitor<'_> for SelVisitor {
            type Value = SeasonSel;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a season number or an arc name")
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<SeasonSel, E> {
                u32::try_from(v)
                    .map(SeasonSel::Number)
                    .map_err(|_| E::custom(format!("season {v} is out of range")))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<SeasonSel, E> {
                u32::try_from(v)
                    .map(SeasonSel::Number)
                    .map_err(|_| E::custom(format!("season {v} is out of range")))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<SeasonSel, E> {
                Ok(SeasonSel::Arc(v.to_string()))
            }
        }
        deserializer.deserialize_any(SelVisitor)
    }
}

/// Structured filter applied to a block's resolved items before mode/order.
///
/// The fields are intentionally narrow for now — a broader field set lives
/// with the query field set (#68), if one is ever needed here too. The
/// resolver applies both fields as a narrowing set: an item survives only
/// when it satisfies every field that is set (see `resolve::apply_filter`,
/// #197).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Filter {
    /// Restrict to these seasons. A number matches the catalog's season; a
    /// string names a show arc (#163) and matches the episodes in that arc's
    /// range of the show that declares it. Entries combine as a union.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seasons: Option<Vec<SeasonSel>>,

    /// Restrict to these entry/episode ids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode_ids: Option<Vec<String>>,

    /// The station's arcs, attached by the load when `seasons` names one.
    /// `None` on a channel read outside a station, which the resolver rejects
    /// for an arc name. Serialized so a fingerprint of the loaded config
    /// changes when an arcs file does.
    #[serde(
        default,
        skip_deserializing,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_arcs"
    )]
    pub arcs: Option<Arc<Arcs>>,
}

pub(super) fn serialize_arcs<S: Serializer>(
    arcs: &Option<Arc<Arcs>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    arcs.as_deref().serialize(serializer)
}

impl Filter {
    /// True when no filter field is set — a `[filter]` table with no keys is
    /// treated as absent by the resolver.
    pub fn is_empty(&self) -> bool {
        self.seasons.is_none() && self.episode_ids.is_none()
    }

    /// The arc names `seasons` asks for.
    pub fn arc_names(&self) -> impl Iterator<Item = &str> {
        self.seasons.iter().flatten().filter_map(|s| match s {
            SeasonSel::Arc(name) => Some(name.as_str()),
            SeasonSel::Number(_) => None,
        })
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
        assert_eq!(h.filter.seasons, Some(vec![1.into(), 2.into()]));
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

    /// An arc's name sits beside season numbers in one list (#163), in both
    /// config formats.
    #[test]
    fn seasons_accept_arc_names_beside_numbers() {
        let t: Holder = toml::from_str("[filter]\nseasons = [1, \"Dungeon\"]").unwrap();
        assert_eq!(t.filter.seasons, Some(vec![1.into(), "Dungeon".into()]));
        let y: Holder = serde_norway::from_str("filter:\n  seasons: [1, \"Dungeon\"]").unwrap();
        assert_eq!(y.filter.seasons, Some(vec![1.into(), "Dungeon".into()]));
        assert_eq!(y.filter.arc_names().collect::<Vec<_>>(), ["Dungeon"]);
    }
}

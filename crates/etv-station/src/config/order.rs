use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

/// Sort direction for a field-based order term.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Asc,
    Desc,
}

impl Dir {
    fn as_str(self) -> &'static str {
        match self {
            Dir::Asc => "asc",
            Dir::Desc => "desc",
        }
    }
}

/// A single `field:dir` sort term. A bare field (`release_date`) defaults to
/// ascending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSort {
    pub field: String,
    pub dir: Dir,
}

/// Block ordering, parsed from a string per the Phase C locked decisions (#46).
///
/// - `field:dir` (e.g. `release_date:desc`), compound comma-separated
///   (`season:asc,episode:asc`); a bare field defaults to `:asc`.
/// - bare keywords: `manual` (authored file order), `random`, `score`
///   (plugin-ranked).
///
/// Every variant here is computable from the items themselves — their columns,
/// the authored list, or the set plus a seed. A collection's authored sequence
/// is deliberately *not* one of them: `collection_items.position` belongs to the
/// (collection, item) pair, not to the item, so a flat set handed to a sort no
/// longer knows which collection's positions to read. That order rides on the
/// entry that names the collection instead — see
/// [`CollectionEntry`](super::entry::CollectionEntry) (#107).
///
/// An implicit `entry_id` tiebreaker and null-handling are the resolution
/// engine's concern (#69); this type only captures the parsed shape.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Order {
    #[default]
    Manual,
    Random,
    Score,
    Fields(Vec<FieldSort>),
}

impl Order {
    /// Parse the `order = "..."` string form.
    pub fn parse(s: &str) -> Result<Order, String> {
        match s {
            "manual" => return Ok(Order::Manual),
            "random" => return Ok(Order::Random),
            "score" => return Ok(Order::Score),
            // Named explicitly so the removed keyword fails loudly here rather
            // than falling through to the field-sort branch and surfacing much
            // later as "cannot order by item.collection".
            "collection" => {
                return Err(
                    "order = \"collection\" was removed (#107): a collection's authored \
                     order belongs to the collection, not to the items, so it rides on a \
                     kind = \"collection\" entry instead of on a block's order"
                        .to_string(),
                );
            }
            _ => {}
        }

        let mut terms = Vec::new();
        for raw in s.split(',') {
            let term = raw.trim();
            if term.is_empty() {
                return Err(format!("empty order term in {s:?}"));
            }
            let (field, dir) = match term.split_once(':') {
                Some((field, dir)) => {
                    let dir = match dir {
                        "asc" => Dir::Asc,
                        "desc" => Dir::Desc,
                        other => {
                            return Err(format!(
                                "invalid sort direction {other:?} in {term:?} (want asc or desc)"
                            ));
                        }
                    };
                    (field.trim(), dir)
                }
                None => (term, Dir::Asc),
            };
            if field.is_empty() {
                return Err(format!("empty field name in order term {term:?}"));
            }
            terms.push(FieldSort {
                field: field.to_string(),
                dir,
            });
        }
        Ok(Order::Fields(terms))
    }

    fn to_order_string(&self) -> String {
        match self {
            Order::Manual => "manual".to_string(),
            Order::Random => "random".to_string(),
            Order::Score => "score".to_string(),
            Order::Fields(terms) => terms
                .iter()
                .map(|t| format!("{}:{}", t.field, t.dir.as_str()))
                .collect::<Vec<_>>()
                .join(","),
        }
    }
}

impl Serialize for Order {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_order_string())
    }
}

impl<'de> Deserialize<'de> for Order {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Order::parse(&s).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Holder {
        order: Order,
    }

    fn parse(s: &str) -> Order {
        let toml = format!("order = \"{s}\"");
        toml::from_str::<Holder>(&toml).unwrap().order
    }

    #[test]
    fn bare_keywords() {
        assert_eq!(parse("manual"), Order::Manual);
        assert_eq!(parse("random"), Order::Random);
        assert_eq!(parse("score"), Order::Score);
    }

    #[test]
    fn collection_keyword_is_rejected_with_a_pointer_to_the_entry_kind() {
        let err = Order::parse("collection").unwrap_err();
        assert!(err.contains("kind = \"collection\" entry"), "err = {err}");
    }

    #[test]
    fn bare_field_defaults_asc() {
        assert_eq!(
            parse("release_date"),
            Order::Fields(vec![FieldSort {
                field: "release_date".into(),
                dir: Dir::Asc,
            }])
        );
    }

    #[test]
    fn field_with_dir() {
        assert_eq!(
            parse("release_date:desc"),
            Order::Fields(vec![FieldSort {
                field: "release_date".into(),
                dir: Dir::Desc,
            }])
        );
    }

    #[test]
    fn compound() {
        assert_eq!(
            parse("season:asc,episode:asc"),
            Order::Fields(vec![
                FieldSort {
                    field: "season".into(),
                    dir: Dir::Asc,
                },
                FieldSort {
                    field: "episode".into(),
                    dir: Dir::Asc,
                },
            ])
        );
    }

    #[test]
    fn rejects_bad_direction() {
        assert!(Order::parse("title:sideways").is_err());
    }

    #[test]
    fn rejects_empty_term() {
        assert!(Order::parse("season:asc,").is_err());
    }

    #[test]
    fn round_trips_via_string() {
        for s in [
            "manual",
            "random",
            "release_date:desc",
            "season:asc,episode:asc",
        ] {
            let o = Order::parse(s).unwrap();
            assert_eq!(Order::parse(&o.to_order_string()).unwrap(), o);
        }
    }
}

use serde::de::{self, Deserializer, Visitor};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use std::fmt;

/// How many of a block's resolved items a channel pulls.
///
/// TOML authoring:
/// ```toml
/// mode = "all"          # every resolved item
/// mode = { count = 5 }  # the first N after ordering
/// ```
///
/// The legacy `flood` and `duration` modes are intentionally dropped (see #46).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    All,
    Count(usize),
}

impl Serialize for Mode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Mode::All => serializer.serialize_str("all"),
            Mode::Count(n) => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("count", n)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Mode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ModeVisitor;

        impl<'de> Visitor<'de> for ModeVisitor {
            type Value = Mode;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str(r#"the string "all" or a table { count = N }"#)
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Mode, E> {
                match value {
                    "all" => Ok(Mode::All),
                    other => Err(de::Error::unknown_variant(other, &["all"])),
                }
            }

            fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Mode, A::Error> {
                let mut count: Option<usize> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "count" => {
                            if count.is_some() {
                                return Err(de::Error::duplicate_field("count"));
                            }
                            count = Some(map.next_value()?);
                        }
                        other => {
                            return Err(de::Error::unknown_field(other, &["count"]));
                        }
                    }
                }
                let count = count.ok_or_else(|| de::Error::missing_field("count"))?;
                Ok(Mode::Count(count))
            }
        }

        deserializer.deserialize_any(ModeVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Holder {
        mode: Mode,
    }

    #[test]
    fn parses_all() {
        let h: Holder = toml::from_str(r#"mode = "all""#).unwrap();
        assert_eq!(h.mode, Mode::All);
    }

    #[test]
    fn parses_count_table() {
        let h: Holder = toml::from_str("mode = { count = 5 }").unwrap();
        assert_eq!(h.mode, Mode::Count(5));
    }

    #[test]
    fn rejects_unknown_string() {
        assert!(toml::from_str::<Holder>(r#"mode = "flood""#).is_err());
    }

    #[test]
    fn round_trips_count() {
        let m = Mode::Count(3);
        let s = toml::to_string(&Holder2 { mode: m }).unwrap();
        let back: Holder = toml::from_str(&s).unwrap();
        assert_eq!(back.mode, Mode::Count(3));
    }

    #[derive(Serialize)]
    struct Holder2 {
        mode: Mode,
    }
}

//! The opaque script-config carrier, and the one rule both scripting surfaces
//! enforce on it.
//!
//! A pool's `config:` in a channel config and an overlay spec's `[config]`
//! table are the same thing wearing two file formats: an arbitrary bag of
//! values the station never reads, handed to a script verbatim. Both are
//! carried as a [`serde_json::Value`] (#129), which can hold every shape either
//! format expresses with exactly one exception — a non-finite float. `inf`,
//! `-inf` and `nan` have no representation in it, and left alone they reach the
//! script as unit, quietly changing what the script does.
//!
//! So both surfaces refuse them at load, naming the key (#130). The wording
//! lives here, once, because the two surfaces reach it by different routes —
//! the channel side deserializes straight into the carrier, the overlay side
//! detours through `toml::Value` so a TOML datetime can become its text — and
//! two spellings of one rule is how these surfaces drifted apart last time.

use std::fmt;

use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};

/// The load error for a float the carrier cannot hold, naming the key that
/// holds it.
///
/// `path` is the dotted route from the config bag's root, so an author reading
/// the error knows which line to edit: `config.fade.weight`, `config.steps[2]`.
pub fn non_finite_message(path: &str, value: f64) -> String {
    format!(
        "`{path}` is `{value}`, but a script config can only carry finite \
         numbers. Write a large finite number instead — `inf` and `nan` have no \
         meaning here, and a script would receive nothing at all."
    )
}

/// Deserialize a script config bag into the shared carrier, refusing a
/// non-finite float by key path.
///
/// Format-agnostic on purpose: it drives the deserializer through
/// `deserialize_any`, exactly as [`serde_json::Value`]'s own `Deserialize`
/// impl does, so a channel config authored in YAML and one authored in TOML
/// read identically. It differs from the stock impl in one place — a
/// non-finite float is an error here rather than a silent `null`.
///
/// The overlay surface does not use this, because it has to walk `toml::Value`
/// first to turn a TOML datetime into its text; it reaches the same refusal
/// through [`non_finite_message`].
pub fn deserialize_config<'de, D>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_option(OptionalBag {
        path: "config".to_string(),
    })
}

/// The `config:` key itself: absent or null yields no bag at all, anything else
/// is walked by [`Bag`].
struct OptionalBag {
    path: String,
}

impl<'de> Visitor<'de> for OptionalBag {
    type Value = Option<serde_json::Value>;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a script config bag, or nothing")
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        DeserializeSeed::deserialize(Bag { path: self.path }, deserializer).map(Some)
    }
}

/// One value inside the bag, carrying the dotted path that reached it so an
/// error can name the key rather than just the file.
struct Bag {
    path: String,
}

impl<'de> DeserializeSeed<'de> for Bag {
    type Value = serde_json::Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Bag {
    type Value = serde_json::Value;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any config value")
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
        Ok(v.into())
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
        Ok(v.into())
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
        Ok(v.into())
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
        serde_json::Number::from_f64(v)
            .map(serde_json::Value::Number)
            .ok_or_else(|| E::custom(non_finite_message(&self.path, v)))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(v.into())
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
        Ok(v.into())
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        DeserializeSeed::deserialize(self, deserializer)
    }

    fn visit_newtype_struct<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        DeserializeSeed::deserialize(self, deserializer)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut out = Vec::new();
        while let Some(value) = seq.next_element_seed(Bag {
            path: format!("{}[{}]", self.path, out.len()),
        })? {
            out.push(value);
        }
        Ok(serde_json::Value::Array(out))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut out = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let value = map.next_value_seed(Bag {
                path: format!("{}.{}", self.path, key),
            })?;
            out.insert(key, value);
        }
        Ok(serde_json::Value::Object(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Holder {
        #[serde(default, deserialize_with = "deserialize_config")]
        config: Option<serde_json::Value>,
    }

    fn from_toml(src: &str) -> Result<Option<serde_json::Value>, String> {
        toml::from_str::<Holder>(src)
            .map(|h| h.config)
            .map_err(|e| e.to_string())
    }

    #[test]
    fn a_finite_float_survives() {
        let config = from_toml("[config]\nweight = 0.5\n").unwrap().unwrap();
        assert_eq!(config["weight"], serde_json::json!(0.5));
    }

    #[test]
    fn an_absent_bag_stays_absent() {
        assert!(from_toml("").unwrap().is_none());
    }

    #[test]
    fn an_infinite_float_is_refused_and_names_the_key() {
        let err = from_toml("[config]\nweight = inf\n").unwrap_err();
        assert!(err.contains("config.weight"), "{err}");
        assert!(err.contains("inf"), "{err}");
    }

    #[test]
    fn a_nan_float_is_refused_and_names_the_key() {
        let err = from_toml("[config]\nweight = nan\n").unwrap_err();
        assert!(err.contains("config.weight"), "{err}");
    }

    #[test]
    fn a_non_finite_float_inside_an_array_names_its_index() {
        let err = from_toml("[config]\nsteps = [1.0, -inf]\n").unwrap_err();
        assert!(err.contains("config.steps[1]"), "{err}");
    }

    #[test]
    fn a_non_finite_float_inside_a_sub_table_names_the_full_path() {
        let err = from_toml("[config.fade]\nweight = inf\n").unwrap_err();
        assert!(err.contains("config.fade.weight"), "{err}");
    }
}

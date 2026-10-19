//! A block's align/fill settings: the clock grid its programmes end on, and
//! the bumper scripts that fill the gap between a programme's end and the next
//! grid line ([`crate::bumper::fill`]).

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// How long a gap may be and still be black with the channel logo rather than
/// a bumper, when a block does not say.
pub const DEFAULT_MIN_FILL: Duration = Duration::from_secs(2);

/// A block's `align:` — the local-clock grid its programmes end on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    /// `none`: no gap is filled; programmes follow each other directly.
    #[default]
    Off,
    /// Grid lines every this-long from local midnight (`30m`, `1h`).
    Every(Duration),
}

impl Align {
    /// The grid spacing, or `None` for `align: none`.
    pub fn every(self) -> Option<Duration> {
        match self {
            Align::Off => None,
            Align::Every(d) => Some(d),
        }
    }
}

impl<'de> Deserialize<'de> for Align {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        if text.trim().eq_ignore_ascii_case("none") {
            return Ok(Align::Off);
        }
        humantime::parse_duration(text.trim())
            .map(Align::Every)
            .map_err(|e| {
                serde::de::Error::custom(format!(
                    "align {text:?} is neither `none` nor a duration like `30m`: {e}"
                ))
            })
    }
}

impl Serialize for Align {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Align::Off => s.serialize_str("none"),
            Align::Every(d) => s.serialize_str(&humantime::format_duration(*d).to_string()),
        }
    }
}

/// One entry of a block's `fill:` list: a bumper script and when it may be
/// drawn.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FillEntry {
    /// The bumper script, relative to the channel config's directory.
    pub script: PathBuf,

    /// Share of draws, relative to the other candidates. Default 1.
    #[serde(default = "one")]
    pub weight: u32,

    /// CEL over `next`, `prev`, `gap_ms` and `channel`; the script is a
    /// candidate only when it is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,

    /// The script is a candidate only while the remaining gap is at least this.
    #[serde(
        default,
        with = "humantime_serde",
        skip_serializing_if = "Option::is_none"
    )]
    pub min_gap: Option<Duration>,

    /// The script is a candidate only while the remaining gap is at most this.
    #[serde(
        default,
        with = "humantime_serde",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_gap: Option<Duration>,

    /// Capabilities the block grants the script; must equal what its
    /// `capabilities()` declares.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,

    /// Handed to the script verbatim as `ctx.config`; the station reads
    /// nothing out of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

fn one() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_parses_none_and_durations() {
        let none: Align = serde_norway::from_str("none").unwrap();
        assert_eq!(none, Align::Off);
        let half: Align = serde_norway::from_str("30m").unwrap();
        assert_eq!(half, Align::Every(Duration::from_secs(1800)));
        assert!(serde_norway::from_str::<Align>("soon").is_err());
    }

    #[test]
    fn a_fill_entry_defaults_to_weight_one_and_no_limits() {
        let e: FillEntry = serde_norway::from_str("script: bumpers/test_card.rhai").unwrap();
        assert_eq!(e.weight, 1);
        assert_eq!((e.when, e.min_gap, e.max_gap), (None, None, None));
    }

    #[test]
    fn a_fill_entry_rejects_an_unknown_key() {
        assert!(serde_norway::from_str::<FillEntry>("script: a.rhai\nwieght: 2").is_err());
    }
}

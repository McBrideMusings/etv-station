//! `seed:` and `temperature:` — the two values every random choice on a
//! channel is computed from, and the schedule each one changes on.
//!
//! Both fields take the same three shapes, at the station, the channel or a
//! pool (the lowest level that sets one wins, wholesale):
//!
//! ```yaml
//! seed: 34                                   # fixed
//! seed: { rotate: 2d }                       # a new seed every 2 days
//! seed: { rotate: 3d, cycle: [34, 99, 7] }   # walk this list, 3 days each
//!
//! temperature: 0.5                                     # fixed
//! temperature: { rotate: 2d, cycle: [0.7, 0.7, 0.25] } # walk this list
//! temperature: { rotate: 2d, between: [0.25, 0.7] }    # a seeded draw per period
//! ```
//!
//! What a value *means* at a given instant — which period it is, which seed
//! that period gets — is [`crate::variety`]'s job. This module only parses and
//! validates the authored shape.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The shortest `rotate:` accepted. A rollover re-ranks every plugin pool it
/// reaches, and a cadence shorter than the items it schedules would re-rank on
/// every draw while changing nothing a viewer could see.
pub const MIN_ROTATE: Duration = Duration::from_secs(3600);

/// An authored `seed:`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(
    untagged,
    expecting = "seed: a number, or a map like { rotate: 2d } or { rotate: 3d, cycle: [34, 99] }"
)]
pub enum SeedSpec {
    /// One seed for ever — the same schedule on every run.
    Fixed(u64),
    /// A new seed every `rotate`.
    Rotate(SeedRotate),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SeedRotate {
    /// How long each period lasts. Periods count from 1970-01-01 00:00 in the
    /// station's local time, so a whole-day cadence rolls over at local
    /// midnight.
    #[serde(with = "humantime_serde")]
    pub rotate: Duration,
    /// The seeds to walk, one per period, wrapping at the end. Unset draws a
    /// fresh seed per period from a fixed built-in base instead — still
    /// reproducible, since the period is all it depends on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycle: Option<Vec<u64>>,
}

/// An authored `temperature:`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(
    untagged,
    expecting = "temperature: a number, or a map like { rotate: 2d, cycle: [0.7, 0.25] } \
                 or { rotate: 2d, between: [0.25, 0.7] }"
)]
pub enum TemperatureSpec {
    Fixed(f64),
    Rotate(TemperatureRotate),
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TemperatureRotate {
    #[serde(with = "humantime_serde")]
    pub rotate: Duration,
    /// The temperatures to walk, one per period, wrapping at the end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycle: Option<Vec<f64>>,
    /// `[low, high]`: each period draws its temperature uniformly from this
    /// range, seeded by the period so a rerun draws the same value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub between: Option<[f64; 2]>,
}

impl SeedSpec {
    /// Why this value cannot be used, or `None` when it can.
    pub fn problem(&self) -> Option<String> {
        match self {
            SeedSpec::Fixed(_) => None,
            SeedSpec::Rotate(r) => rotate_problem("seed", r.rotate).or_else(|| {
                r.cycle
                    .as_ref()
                    .filter(|c| c.is_empty())
                    .map(|_| "seed.cycle must list at least one seed".to_string())
            }),
        }
    }
}

impl TemperatureSpec {
    /// Why this value cannot be used, or `None` when it can.
    pub fn problem(&self) -> Option<String> {
        match self {
            TemperatureSpec::Fixed(t) => temperature_problem("temperature", *t),
            TemperatureSpec::Rotate(r) => {
                if let Some(p) = rotate_problem("temperature", r.rotate) {
                    return Some(p);
                }
                match (&r.cycle, &r.between) {
                    (Some(_), Some(_)) => {
                        Some("temperature: set `cycle` or `between`, not both".to_string())
                    }
                    (None, None) => Some(
                        "temperature.rotate needs `cycle: [..]` or `between: [low, high]` to say \
                         which temperature each period gets"
                            .to_string(),
                    ),
                    (Some(c), None) if c.is_empty() => {
                        Some("temperature.cycle must list at least one value".to_string())
                    }
                    (Some(c), None) => c
                        .iter()
                        .find_map(|t| temperature_problem("temperature.cycle", *t)),
                    (None, Some([lo, hi])) => temperature_problem("temperature.between", *lo)
                        .or_else(|| temperature_problem("temperature.between", *hi))
                        .or_else(|| {
                            (lo > hi).then(|| {
                                format!("temperature.between: low {lo} is above high {hi}")
                            })
                        }),
                }
            }
        }
    }
}

fn rotate_problem(field: &str, rotate: Duration) -> Option<String> {
    (rotate < MIN_ROTATE).then(|| {
        format!(
            "{field}.rotate must be at least {} (got {})",
            humantime::format_duration(MIN_ROTATE),
            humantime::format_duration(rotate)
        )
    })
}

fn temperature_problem(field: &str, t: f64) -> Option<String> {
    (!t.is_finite() || t < 0.0)
        .then(|| format!("{field}: {t} is not a temperature (want a finite number >= 0)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(yaml: &str) -> SeedSpec {
        serde_norway::from_str(yaml).unwrap()
    }

    fn temp(yaml: &str) -> TemperatureSpec {
        serde_norway::from_str(yaml).unwrap()
    }

    /// All three authored shapes of each field parse to what they say.
    #[test]
    fn every_authored_shape_parses() {
        assert_eq!(seed("34"), SeedSpec::Fixed(34));
        assert_eq!(
            seed("{ rotate: 2d }"),
            SeedSpec::Rotate(SeedRotate {
                rotate: Duration::from_secs(2 * 86_400),
                cycle: None
            })
        );
        assert_eq!(
            seed("{ rotate: 3d, cycle: [34, 99, 7] }"),
            SeedSpec::Rotate(SeedRotate {
                rotate: Duration::from_secs(3 * 86_400),
                cycle: Some(vec![34, 99, 7])
            })
        );
        assert_eq!(temp("0.5"), TemperatureSpec::Fixed(0.5));
        assert_eq!(temp("0"), TemperatureSpec::Fixed(0.0));
        let TemperatureSpec::Rotate(r) = temp("{ rotate: 4h, between: [0.25, 0.7] }") else {
            panic!("a map is a rotation");
        };
        assert_eq!(r.rotate, Duration::from_secs(4 * 3600));
        assert_eq!(r.between, Some([0.25, 0.7]));
    }

    /// Each unusable value is refused with the field it is about, so a load
    /// failure names what to fix.
    #[test]
    fn unusable_values_name_the_field() {
        let cases = [
            (temp("{ rotate: 2d }").problem(), "needs `cycle"),
            (
                temp("{ rotate: 2d, cycle: [1], between: [0, 1] }").problem(),
                "not both",
            ),
            (temp("-0.5").problem(), "not a temperature"),
            (
                temp("{ rotate: 2d, between: [0.7, 0.2] }").problem(),
                "above high",
            ),
            (seed("{ rotate: 30m }").problem(), "at least 1h"),
            (
                seed("{ rotate: 2d, cycle: [] }").problem(),
                "at least one seed",
            ),
        ];
        for (problem, want) in cases {
            let problem = problem.expect("refused");
            assert!(problem.contains(want), "{problem:?} should say {want:?}");
        }
        assert_eq!(seed("{ rotate: 1h }").problem(), None);
    }
}

//! The render plan: the audio layers and picture cuts a plugin has decided on.
//!
//! A plan names its sources, lists every audio layer (which source or stem,
//! the range read, where it lands, its tempo, pitch, level, fades and filters)
//! and every picture cut (which source, the range read, where it lands, its
//! speed), and says how long the result is. The renderer does exactly that and
//! decides nothing: which sources, which stems, how two of them join and what is on
//! screen are the plugin's choices, already resolved into layers and cuts
//! (ADR 0023). `notes` is the plugin's own record of why; the renderer keeps it
//! and never reads it. The format is documented in `docs/schema.md` § Render plan.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// How far a cut may start from where the previous one ended, in seconds.
const CUT_TOLERANCE_S: f64 = 0.05;
/// How far past the plan's end a layer may run before it is an error.
const LAYER_TOLERANCE_S: f64 = 0.05;
/// The most steps a tempo ride may take.
const MAX_RIDE_STEPS: usize = 64;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// Every media file the layers and cuts read, by id.
    pub sources: Vec<Source>,
    /// The audio, mixed: every layer is summed at its place on the timeline.
    pub layers: Vec<Layer>,
    /// The picture, in air order, with no gap and no overlap.
    pub cuts: Vec<Cut>,
    /// Seconds the result lasts.
    pub duration_s: f64,
    /// Who is on screen when, for the overlay. The renderer does not draw them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cards: Vec<Card>,
    /// Where the rendered file lands.
    pub output: PathBuf,
    /// The plugin's own record of why the plan looks the way it does. Kept with
    /// the plan for audits; the renderer never reads it.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub notes: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub id: String,
    pub path: PathBuf,
    /// The stretch of the source split into stems, when its layers read
    /// `vocals` or `instrumental`. Unset separates from the earliest such read
    /// to the latest, widened by a margin either side. Naming it lets plans that
    /// read different parts of one source share one separation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub separate: Option<Span>,
}

/// A stretch of a source, in its own seconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Span {
    pub start_s: f64,
    pub len_s: f64,
}

/// Which part of a source's audio a layer reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stem {
    /// The file's own audio, unseparated.
    #[default]
    Full,
    /// The voice, separated by Demucs.
    Vocals,
    /// Everything but the voice, separated by Demucs.
    Instrumental,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layer {
    /// What an error calls it.
    pub name: String,
    /// A [`Source::id`].
    pub source: String,
    #[serde(default, skip_serializing_if = "is_full")]
    pub stem: Stem,
    /// Where the excerpt starts in the source, in its own seconds.
    pub src_start_s: f64,
    /// Where it starts in the result.
    pub out_start_s: f64,
    /// Seconds it lasts in the result, after stretching.
    pub out_len_s: f64,
    /// Source seconds played per result second; 1.0 is the source's own speed.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub ratio: f64,
    /// A tempo that changes once per `step_src_s` of the source, opening the
    /// layer; once it ends the layer keeps its last ratio. Replaces `ratio`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ride: Option<Ride>,
    /// Frequency ratio of the pitch shift; 1.0 leaves it alone.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub pitch: f64,
    /// Bring the excerpt's mean level to this many dB before `gain_db`. Unset
    /// leaves its level as the source has it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level_db: Option<f64>,
    /// dB added after levelling.
    #[serde(default, skip_serializing_if = "is_zero_f")]
    pub gain_db: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fades: Vec<Fade>,
    /// Applied after the stretch and before the fades.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<Filter>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ride {
    /// Source seconds each step lasts.
    pub step_src_s: f64,
    /// The ratio of each step in turn.
    pub ratios: Vec<f64>,
}

impl Ride {
    /// Result seconds the steps take.
    pub fn out_s(&self) -> f64 {
        self.ratios.iter().map(|r| self.step_src_s / r).sum()
    }

    /// Source seconds the steps cover.
    pub fn src_s(&self) -> f64 {
        self.step_src_s * self.ratios.len() as f64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FadeKind {
    In,
    Out,
}

/// A fade's shape: `linear`, or `equal_power`, which keeps two crossfading
/// layers at a steady combined loudness.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Curve {
    #[default]
    Linear,
    EqualPower,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fade {
    pub kind: FadeKind,
    /// Seconds from the layer's start in the result.
    pub start_s: f64,
    pub len_s: f64,
    #[serde(default)]
    pub curve: Curve,
}

/// An audio filter, applied over the whole layer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Filter {
    /// Cut everything below `hz`.
    Highpass { hz: f64 },
    /// Cut everything above `hz`.
    Lowpass { hz: f64 },
    /// Repeats `delay_ms` apart, each `decay` times the last, mixed at
    /// `in_gain` and `out_gain`.
    Echo {
        in_gain: f64,
        out_gain: f64,
        delay_ms: f64,
        decay: f64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cut {
    /// A [`Source::id`].
    pub source: String,
    /// Where the picture starts in the source, in its own seconds.
    pub src_start_s: f64,
    pub out_start_s: f64,
    pub out_len_s: f64,
    /// Source seconds shown per result second.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub ratio: f64,
}

/// The copy for one title card, drawn by the channel's overlay at `at_s`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Card {
    pub at_s: f64,
    pub artist: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<u32>,
}

fn one() -> f64 {
    1.0
}

fn is_one(x: &f64) -> bool {
    (*x - 1.0).abs() < 1e-12
}

fn is_zero_f(x: &f64) -> bool {
    *x == 0.0
}

fn is_full(s: &Stem) -> bool {
    *s == Stem::Full
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("plan is not valid JSON for the render plan format: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("plan is invalid: {0}")]
    Invalid(String),
}

impl Layer {
    /// Source seconds the excerpt reads.
    pub fn src_len_s(&self) -> f64 {
        match &self.ride {
            Some(ride) => ride.src_s() + (self.out_len_s - ride.out_s()) * self.exit_ratio(),
            None => self.out_len_s * self.ratio,
        }
    }

    /// The ratio the layer keeps once any ride is over.
    pub fn exit_ratio(&self) -> f64 {
        self.ride
            .as_ref()
            .and_then(|r| r.ratios.last().copied())
            .unwrap_or(self.ratio)
    }
}

fn positive(x: f64) -> bool {
    x.is_finite() && x > 0.0
}

fn not_negative(x: f64) -> bool {
    x.is_finite() && x >= 0.0
}

impl Plan {
    pub fn parse(json: &str) -> Result<Self, PlanError> {
        let plan: Plan = serde_json::from_str(json)?;
        plan.validate()?;
        Ok(plan)
    }

    /// The media file of the source `id`.
    pub fn source_path(&self, id: &str) -> Option<&PathBuf> {
        self.sources.iter().find(|s| s.id == id).map(|s| &s.path)
    }

    /// Structure and arithmetic only: every rule here holds without touching a
    /// disk, so `enqueue` can refuse a malformed plan at once. A named file that
    /// does not exist is the renderer's finding, recorded against the job.
    pub fn validate(&self) -> Result<(), PlanError> {
        let bad = |msg: String| Err(PlanError::Invalid(msg));
        if !positive(self.duration_s) {
            return bad(format!(
                "duration_s must be positive, got {}",
                self.duration_s
            ));
        }
        if self.output.as_os_str().is_empty() {
            return bad("output is empty".into());
        }
        if self.sources.is_empty() {
            return bad("sources is empty".into());
        }
        let mut ids = BTreeSet::new();
        for (i, s) in self.sources.iter().enumerate() {
            if s.id.is_empty() {
                return bad(format!("sources[{i}]: id is empty"));
            }
            if !ids.insert(s.id.as_str()) {
                return bad(format!("sources[{i}] ({}): id appears twice", s.id));
            }
            if s.path.as_os_str().is_empty() {
                return bad(format!("sources[{i}] ({}): path is empty", s.id));
            }
            if let Some(sep) = s.separate
                && (!not_negative(sep.start_s) || !positive(sep.len_s))
            {
                return bad(format!(
                    "sources[{i}] ({}): separate needs a start of 0 or more and a positive length",
                    s.id
                ));
            }
        }
        if self.layers.is_empty() {
            return bad("layers is empty".into());
        }
        for (i, l) in self.layers.iter().enumerate() {
            let at = format!("layers[{i}] ({})", l.name);
            if !ids.contains(l.source.as_str()) {
                return bad(format!("{at}: source {} is not in sources", l.source));
            }
            if !not_negative(l.src_start_s) {
                return bad(format!("{at}: src_start_s must not be negative"));
            }
            if !not_negative(l.out_start_s) {
                return bad(format!("{at}: out_start_s must not be negative"));
            }
            if !positive(l.out_len_s) {
                return bad(format!("{at}: out_len_s must be positive"));
            }
            if !positive(l.ratio) {
                return bad(format!("{at}: ratio must be positive"));
            }
            if !positive(l.pitch) {
                return bad(format!("{at}: pitch must be positive"));
            }
            if !l.gain_db.is_finite() || l.level_db.is_some_and(|d| !d.is_finite()) {
                return bad(format!("{at}: gain_db and level_db must be finite"));
            }
            if let Some(ride) = &l.ride {
                if !positive(ride.step_src_s) {
                    return bad(format!("{at}: ride.step_src_s must be positive"));
                }
                if ride.ratios.is_empty() || ride.ratios.len() > MAX_RIDE_STEPS {
                    return bad(format!(
                        "{at}: a ride has 1 to {MAX_RIDE_STEPS} steps, got {}",
                        ride.ratios.len()
                    ));
                }
                if ride.ratios.iter().any(|r| !positive(*r)) {
                    return bad(format!("{at}: every ride ratio must be positive"));
                }
                if ride.out_s() > l.out_len_s + 1e-6 {
                    return bad(format!(
                        "{at}: the ride lasts {:.3}s, longer than the layer's {:.3}s",
                        ride.out_s(),
                        l.out_len_s
                    ));
                }
            }
            if let Some(sep) = self
                .sources
                .iter()
                .find(|s| s.id == l.source)
                .and_then(|s| s.separate)
                && l.stem != Stem::Full
                && (l.src_start_s < sep.start_s - 1e-6
                    || l.src_start_s + l.src_len_s() > sep.start_s + sep.len_s + 1e-6)
            {
                return bad(format!(
                    "{at}: reads {:.3}s to {:.3}s of {}, outside its separate span {:.3}s to {:.3}s",
                    l.src_start_s,
                    l.src_start_s + l.src_len_s(),
                    l.source,
                    sep.start_s,
                    sep.start_s + sep.len_s
                ));
            }
            if l.out_start_s + l.out_len_s > self.duration_s + LAYER_TOLERANCE_S {
                return bad(format!(
                    "{at}: ends at {:.3}s, past the plan's {:.3}s",
                    l.out_start_s + l.out_len_s,
                    self.duration_s
                ));
            }
            for (k, f) in l.fades.iter().enumerate() {
                if !not_negative(f.start_s) || !positive(f.len_s) {
                    return bad(format!(
                        "{at}: fades[{k}] needs a start of 0 or more and a positive length"
                    ));
                }
                if f.start_s + f.len_s > l.out_len_s + 1e-6 {
                    return bad(format!(
                        "{at}: fades[{k}] ends at {:.3}s, past the layer's {:.3}s",
                        f.start_s + f.len_s,
                        l.out_len_s
                    ));
                }
            }
            for (k, f) in l.filters.iter().enumerate() {
                let ok = match f {
                    Filter::Highpass { hz } | Filter::Lowpass { hz } => positive(*hz),
                    Filter::Echo {
                        in_gain,
                        out_gain,
                        delay_ms,
                        decay,
                    } => {
                        positive(*in_gain)
                            && positive(*out_gain)
                            && positive(*delay_ms)
                            && positive(*decay)
                    }
                };
                if !ok {
                    return bad(format!(
                        "{at}: filters[{k}] has a value that is not positive"
                    ));
                }
            }
        }
        if self.cuts.is_empty() {
            return bad("cuts is empty".into());
        }
        let mut end = 0.0;
        for (i, c) in self.cuts.iter().enumerate() {
            let at = format!("cuts[{i}] ({})", c.source);
            if !ids.contains(c.source.as_str()) {
                return bad(format!("{at}: source {} is not in sources", c.source));
            }
            if !positive(c.out_len_s) {
                return bad(format!("{at}: out_len_s must be positive"));
            }
            if !positive(c.ratio) {
                return bad(format!("{at}: ratio must be positive"));
            }
            if !not_negative(c.src_start_s) {
                return bad(format!("{at}: src_start_s must not be negative"));
            }
            if (c.out_start_s - end).abs() > CUT_TOLERANCE_S {
                return bad(format!(
                    "{at}: starts at {:.3}s but the previous cut ended at {end:.3}s",
                    c.out_start_s
                ));
            }
            end = c.out_start_s + c.out_len_s;
        }
        if (end - self.duration_s).abs() > CUT_TOLERANCE_S {
            return bad(format!(
                "cuts end at {end:.3}s but the plan lasts {:.3}s",
                self.duration_s
            ));
        }
        let mut prev = 0.0;
        for (i, card) in self.cards.iter().enumerate() {
            if !(card.at_s.is_finite() && card.at_s >= 0.0 && card.at_s <= self.duration_s) {
                return bad(format!(
                    "cards[{i}]: at_s {} is outside the plan (0 to {:.3}s)",
                    card.at_s, self.duration_s
                ));
            }
            if card.at_s < prev {
                return bad(format!(
                    "cards[{i}]: at_s {} comes before cards[{}] at {prev}s; cards are in air order",
                    card.at_s,
                    i - 1
                ));
            }
            prev = card.at_s;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sample() -> Plan {
        Plan {
            sources: vec![
                Source {
                    id: "a".into(),
                    path: "/m/a.mp4".into(),
                    separate: None,
                },
                Source {
                    id: "b".into(),
                    path: "/m/b.mp4".into(),
                    separate: None,
                },
            ],
            layers: vec![
                Layer {
                    name: "a vocals".into(),
                    source: "a".into(),
                    stem: Stem::Vocals,
                    src_start_s: 30.0,
                    out_start_s: 0.0,
                    out_len_s: 8.0,
                    ratio: 1.2,
                    ride: None,
                    pitch: 1.0,
                    level_db: Some(-20.0),
                    gain_db: 0.0,
                    fades: vec![],
                    filters: vec![],
                },
                Layer {
                    name: "b whole".into(),
                    source: "b".into(),
                    stem: Stem::Full,
                    src_start_s: 10.0,
                    out_start_s: 6.0,
                    out_len_s: 10.0,
                    ratio: 1.0,
                    ride: None,
                    pitch: 1.0,
                    level_db: None,
                    gain_db: -3.0,
                    fades: vec![Fade {
                        kind: FadeKind::In,
                        start_s: 0.0,
                        len_s: 2.0,
                        curve: Curve::EqualPower,
                    }],
                    filters: vec![Filter::Highpass { hz: 250.0 }],
                },
            ],
            cuts: vec![
                Cut {
                    source: "a".into(),
                    src_start_s: 30.0,
                    out_start_s: 0.0,
                    out_len_s: 6.0,
                    ratio: 1.2,
                },
                Cut {
                    source: "b".into(),
                    src_start_s: 10.0,
                    out_start_s: 6.0,
                    out_len_s: 10.0,
                    ratio: 1.0,
                },
            ],
            duration_s: 16.0,
            cards: vec![],
            output: "/out/x.mp4".into(),
            notes: serde_json::Value::Null,
        }
    }

    fn invalid(p: &Plan) -> String {
        match p.validate() {
            Err(PlanError::Invalid(m)) => m,
            other => panic!("expected invalid, got {other:?}"),
        }
    }

    #[test]
    fn the_sample_is_valid_and_round_trips_through_json() {
        let p = sample();
        p.validate().unwrap();
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(Plan::parse(&json).unwrap(), p);
    }

    #[test]
    fn defaults_fill_ratio_pitch_stem_and_curve() {
        let json = r#"{"sources":[{"id":"a","path":"/a.mp4"}],
            "layers":[{"name":"x","source":"a","src_start_s":0,"out_start_s":0,"out_len_s":4,
                       "fades":[{"kind":"out","start_s":3,"len_s":1}]}],
            "cuts":[{"source":"a","src_start_s":0,"out_start_s":0,"out_len_s":4}],
            "duration_s":4,"output":"/o.mp4"}"#;
        let p = Plan::parse(json).unwrap();
        let l = &p.layers[0];
        assert_eq!((l.ratio, l.pitch, l.stem), (1.0, 1.0, Stem::Full));
        assert_eq!(l.fades[0].curve, Curve::Linear);
    }

    #[test]
    fn an_unknown_field_is_refused_so_an_old_plan_never_renders_half_understood() {
        let json = r#"{"tempo":120,"cuts":[],"output":"/o.mp4"}"#;
        assert!(matches!(Plan::parse(json), Err(PlanError::Parse(_))));
    }

    #[test]
    fn a_layer_naming_an_unknown_source_is_refused() {
        let mut p = sample();
        p.layers[0].source = "zz".into();
        assert!(invalid(&p).contains("source zz is not in sources"));
    }

    #[test]
    fn a_gap_between_cuts_is_refused() {
        let mut p = sample();
        p.cuts[1].out_start_s = 7.0;
        assert!(invalid(&p).contains("previous cut ended at 6.000s"));
    }

    #[test]
    fn cuts_must_reach_the_end() {
        let mut p = sample();
        p.duration_s = 20.0;
        assert!(invalid(&p).contains("cuts end at 16.000s"));
    }

    #[test]
    fn a_layer_past_the_end_is_refused() {
        let mut p = sample();
        p.layers[1].out_len_s = 12.0;
        assert!(invalid(&p).contains("past the plan's 16.000s"));
    }

    #[test]
    fn a_fade_past_its_layer_is_refused() {
        let mut p = sample();
        p.layers[1].fades[0].start_s = 9.0;
        assert!(invalid(&p).contains("fades[0] ends at 11.000s"));
    }

    #[test]
    fn a_stem_read_outside_its_sources_separate_span_is_refused() {
        let mut p = sample();
        p.sources[0].separate = Some(Span {
            start_s: 32.0,
            len_s: 20.0,
        });
        // `a vocals` reads 30 s to 39.6 s; the span starts at 32 s.
        assert!(invalid(&p).contains("outside its separate span"));
        p.sources[0].separate = Some(Span {
            start_s: 20.0,
            len_s: 20.0,
        });
        p.validate().unwrap();
    }

    #[test]
    fn a_ride_reads_its_steps_then_the_last_ratio() {
        let mut p = sample();
        p.layers[1].ride = Some(Ride {
            step_src_s: 2.0,
            ratios: vec![0.5, 1.0],
        });
        p.validate().unwrap();
        let l = &p.layers[1];
        // Steps: 2 s of source at 0.5 = 4 s out, then 2 s at 1.0 = 2 s out.
        assert!((l.ride.as_ref().unwrap().out_s() - 6.0).abs() < 1e-9);
        // The other 4 s of the layer run at 1.0: 4 + 4 = 8 s of source.
        assert!((l.src_len_s() - 8.0).abs() < 1e-9);
    }

    #[test]
    fn cards_out_of_air_order_are_refused() {
        let mut p = sample();
        let card = |at_s: f64| Card {
            at_s,
            artist: String::new(),
            title: "t".into(),
            year: None,
        };
        p.cards = vec![card(5.0), card(2.0)];
        assert!(invalid(&p).contains("cards are in air order"));
    }
}

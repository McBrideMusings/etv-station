//! The chapter plan: the JSON document a plugin writes and the renderer reads.
//!
//! The plan is also the audit record for a chapter, so it names everything the
//! renderer needs and nothing about how a plugin chose it. The format is
//! documented in `docs/schema.md` § Chapter plan.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Bars of a hook's source the renderer separates before the landing bar.
pub const REGION_BARS_BEFORE: f64 = 4.0;
/// Bars of a hook's source the renderer separates from the landing bar on.
pub const REGION_BARS_AFTER: u32 = 18;
/// How far `bar_seconds` may sit from `240 / bpm` (four beats).
const BAR_SECONDS_TOLERANCE: f64 = 0.001;
/// How far a cut may sit from its neighbour, and the last from the chapter's end.
const CUT_TOLERANCE_S: f64 = 0.05;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// Chapter tempo in beats per minute; every layer is stretched to it.
    pub bpm: f64,
    /// Chapter key, for the audit record (`A minor`).
    pub key: String,
    /// The key's Camelot code (`8A`), for the audit record.
    pub camelot: String,
    /// Seconds in one bar at `bpm`: `240 / bpm`.
    pub bar_seconds: f64,
    /// The songs of the chapter in air order.
    pub hooks: Vec<Hook>,
    /// Video-only sources the cuts may draw from besides the hooks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub broll: Vec<Source>,
    /// The video, in air order, covering the chapter without a gap.
    pub cuts: Vec<Cut>,
    /// Who is singing what, for the overlay. The renderer does not draw them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cards: Vec<Card>,
    /// Where the rendered chapter lands.
    pub output: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    pub entry_id: String,
    /// The song's media file: its stems are cut from it and its picture can be cut to.
    pub path: PathBuf,
    /// Where the hook's landing bar starts in the file, in seconds.
    pub landing_s: f64,
    /// The song's own tempo.
    pub native_bpm: f64,
    /// Semitones the song is moved to reach the chapter key.
    pub semitones: i32,
    /// Bars in which this song's vocal sings over the previous song's
    /// instrumental. Zero for the first hook.
    pub vocal_over_bars: u32,
    /// Bars of this song's vocal and instrumental together, after the vocal-over.
    pub full_bars: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub entry_id: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cut {
    /// A hook's or a B-roll source's `entry_id`.
    pub entry_id: String,
    /// Where the cut starts in the source picture, in the source's own seconds.
    pub native_start_s: f64,
    /// Where the cut starts in the chapter.
    pub out_start_s: f64,
    pub length_s: f64,
    /// Source seconds played per chapter second; 1.2 plays the picture 20% faster.
    pub ratio: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Card {
    pub at_s: f64,
    pub artist: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<u32>,
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("plan is not valid JSON for the chapter plan format: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("plan is invalid: {0}")]
    Invalid(String),
}

impl Plan {
    pub fn parse(json: &str) -> Result<Self, PlanError> {
        let plan: Plan = serde_json::from_str(json)?;
        plan.validate()?;
        Ok(plan)
    }

    /// Bars the chapter lasts: every hook's vocal-over and full bars.
    pub fn total_bars(&self) -> u32 {
        self.hooks
            .iter()
            .map(|h| h.vocal_over_bars + h.full_bars)
            .sum()
    }

    pub fn duration_s(&self) -> f64 {
        f64::from(self.total_bars()) * self.bar_seconds
    }

    /// The media file a cut or a stem for `entry_id` is read from.
    pub fn source_path(&self, entry_id: &str) -> Option<&PathBuf> {
        self.hooks
            .iter()
            .map(|h| (&h.entry_id, &h.path))
            .chain(self.broll.iter().map(|s| (&s.entry_id, &s.path)))
            .find(|(id, _)| id.as_str() == entry_id)
            .map(|(_, path)| path)
    }

    /// Structure only: every rule here holds without touching a disk, so
    /// `enqueue` can refuse a malformed plan at once. A named file that does not
    /// exist is the renderer's finding, recorded against the job.
    pub fn validate(&self) -> Result<(), PlanError> {
        let bad = |msg: String| Err(PlanError::Invalid(msg));
        if !(self.bpm.is_finite() && self.bpm > 0.0) {
            return bad(format!("bpm must be positive, got {}", self.bpm));
        }
        if (self.bar_seconds - 240.0 / self.bpm).abs() > BAR_SECONDS_TOLERANCE {
            return bad(format!(
                "bar_seconds {} is not 240 / bpm ({}) = {:.4}",
                self.bar_seconds,
                self.bpm,
                240.0 / self.bpm
            ));
        }
        if self.hooks.is_empty() {
            return bad("hooks is empty".into());
        }
        let mut ids = BTreeSet::new();
        for (i, hook) in self.hooks.iter().enumerate() {
            let at = format!("hooks[{i}] ({})", hook.entry_id);
            if !ids.insert(hook.entry_id.as_str()) {
                return bad(format!("{at}: entry_id appears twice in hooks"));
            }
            if !(hook.native_bpm.is_finite() && hook.native_bpm > 0.0) {
                return bad(format!("{at}: native_bpm must be positive"));
            }
            if hook.full_bars == 0 {
                return bad(format!("{at}: full_bars must be at least 1"));
            }
            if i == 0 && hook.vocal_over_bars != 0 {
                return bad(format!(
                    "{at}: the first hook has no previous instrumental to sing over, so vocal_over_bars must be 0"
                ));
            }
            let native_bar = 240.0 / hook.native_bpm;
            if !(hook.landing_s.is_finite() && hook.landing_s >= REGION_BARS_BEFORE * native_bar) {
                return bad(format!(
                    "{at}: landing_s {} leaves less than {REGION_BARS_BEFORE} bars ({:.2}s) of song before it",
                    hook.landing_s,
                    REGION_BARS_BEFORE * native_bar
                ));
            }
            let next_over = self.hooks.get(i + 1).map_or(0, |n| n.vocal_over_bars);
            let used = hook.vocal_over_bars + hook.full_bars + next_over;
            if used > REGION_BARS_AFTER {
                return bad(format!(
                    "{at}: vocal_over_bars + full_bars + the next hook's vocal_over_bars is {used}, past the {REGION_BARS_AFTER} bars after the landing that are separated"
                ));
            }
        }
        for source in &self.broll {
            if !ids.insert(source.entry_id.as_str()) {
                return bad(format!(
                    "broll {}: entry_id is already a hook or listed twice",
                    source.entry_id
                ));
            }
        }
        if self.cuts.is_empty() {
            return bad("cuts is empty".into());
        }
        let mut end = 0.0;
        for (i, cut) in self.cuts.iter().enumerate() {
            let at = format!("cuts[{i}] ({})", cut.entry_id);
            if self.source_path(&cut.entry_id).is_none() {
                return bad(format!("{at}: no hook or broll has that entry_id"));
            }
            if !(cut.length_s.is_finite() && cut.length_s > 0.0) {
                return bad(format!("{at}: length_s must be positive"));
            }
            if !(cut.ratio.is_finite() && cut.ratio > 0.0) {
                return bad(format!("{at}: ratio must be positive"));
            }
            if !(cut.native_start_s.is_finite() && cut.native_start_s >= 0.0) {
                return bad(format!("{at}: native_start_s must not be negative"));
            }
            if (cut.out_start_s - end).abs() > CUT_TOLERANCE_S {
                return bad(format!(
                    "{at}: starts at {:.3}s but the previous cut ended at {end:.3}s",
                    cut.out_start_s
                ));
            }
            end = cut.out_start_s + cut.length_s;
        }
        let total = self.duration_s();
        if (end - total).abs() > CUT_TOLERANCE_S {
            return bad(format!(
                "cuts end at {end:.3}s but the chapter lasts {total:.3}s ({} bars x {}s)",
                self.total_bars(),
                self.bar_seconds
            ));
        }
        for (i, card) in self.cards.iter().enumerate() {
            if !(card.at_s.is_finite() && card.at_s >= 0.0 && card.at_s <= total) {
                return bad(format!(
                    "cards[{i}]: at_s {} is outside the chapter (0 to {total:.3}s)",
                    card.at_s
                ));
            }
        }
        if self.output.as_os_str().is_empty() {
            return bad("output is empty".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample() -> Plan {
        Plan {
            bpm: 120.0,
            key: "A minor".into(),
            camelot: "8A".into(),
            bar_seconds: 2.0,
            hooks: vec![
                Hook {
                    entry_id: "a".into(),
                    path: "/m/a.mp4".into(),
                    landing_s: 20.0,
                    native_bpm: 100.0,
                    semitones: 0,
                    vocal_over_bars: 0,
                    full_bars: 4,
                },
                Hook {
                    entry_id: "b".into(),
                    path: "/m/b.mp4".into(),
                    landing_s: 20.0,
                    native_bpm: 130.0,
                    semitones: -2,
                    vocal_over_bars: 2,
                    full_bars: 4,
                },
            ],
            broll: vec![],
            cuts: vec![
                Cut {
                    entry_id: "a".into(),
                    native_start_s: 20.0,
                    out_start_s: 0.0,
                    length_s: 8.0,
                    ratio: 0.833,
                },
                Cut {
                    entry_id: "b".into(),
                    native_start_s: 20.0,
                    out_start_s: 8.0,
                    length_s: 12.0,
                    ratio: 1.083,
                },
            ],
            cards: vec![],
            output: "/out/chapter.mp4".into(),
        }
    }

    #[test]
    fn total_bars_is_every_hooks_vocal_over_plus_full_bars() {
        let plan = sample();
        assert_eq!(plan.total_bars(), 10);
        assert_eq!(plan.duration_s(), 20.0);
    }

    #[test]
    fn a_valid_plan_round_trips_through_json() {
        let plan = sample();
        let json = serde_json::to_string(&plan).unwrap();
        assert_eq!(Plan::parse(&json).unwrap(), plan);
    }

    fn rejects(edit: impl FnOnce(&mut Plan), needle: &str) {
        let mut plan = sample();
        edit(&mut plan);
        let err = plan.validate().unwrap_err().to_string();
        assert!(err.contains(needle), "{err:?} lacks {needle:?}");
    }

    #[test]
    fn bar_seconds_must_match_the_tempo() {
        rejects(|p| p.bar_seconds = 1.5, "240 / bpm");
    }

    #[test]
    fn the_first_hook_cannot_sing_over_nothing() {
        rejects(|p| p.hooks[0].vocal_over_bars = 1, "must be 0");
    }

    #[test]
    fn a_hook_cannot_use_more_than_the_separated_region() {
        rejects(
            |p| {
                p.hooks[0].full_bars = 17;
                p.hooks[1].vocal_over_bars = 2;
            },
            "past the 18 bars",
        );
    }

    #[test]
    fn a_landing_needs_four_bars_of_song_before_it() {
        rejects(|p| p.hooks[1].landing_s = 5.0, "less than 4 bars");
    }

    #[test]
    fn a_cut_from_an_unknown_source_is_refused() {
        rejects(|p| p.cuts[0].entry_id = "nobody".into(), "no hook or broll");
    }

    #[test]
    fn cuts_must_cover_the_chapter_without_a_gap() {
        rejects(|p| p.cuts[1].out_start_s = 9.0, "previous cut ended");
        rejects(|p| p.cuts[1].length_s = 10.0, "cuts end at");
    }

    #[test]
    fn an_unknown_field_is_refused() {
        let mut v = serde_json::to_value(sample()).unwrap();
        v["tempo"] = 1.into();
        assert!(Plan::parse(&v.to_string()).is_err());
    }

    #[test]
    fn a_card_outside_the_chapter_is_refused() {
        rejects(
            |p| {
                p.cards.push(Card {
                    at_s: 99.0,
                    artist: "x".into(),
                    title: "y".into(),
                    year: None,
                })
            },
            "outside the chapter",
        );
    }
}

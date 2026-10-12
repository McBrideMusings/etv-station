//! Track-selection rules (#261): which audio stream and which subtitle stream
//! of a file an item plays.
//!
//! The rules are YAML, never code. A level of the config says what it prefers
//! and what it refuses; the station resolves them against the streams it
//! probed and writes the winners into the playout JSON as
//! `tracks.audio.stream_index` and `tracks.subtitle.stream_index`, so
//! ETV-next receives integers and holds no policy of its own.
//!
//! ```yaml
//! tracks:
//!   audio:
//!     exclude:
//!       - { title: "commentary|director|isolated score" }
//!     prefer:
//!       - { lang: "^en", channels: 2 }
//!       - { lang: "^en" }
//!   subtitle:
//!     prefer:
//!       - { image: false, lang: "^en" }              # text: the viewer can toggle it
//!       - { image: true, lang: "^en",                # picture: burned in, cannot be toggled
//!           when: { audio_lang_not: "^en" } }        # ...but only for a foreign-language film
//! ```
//!
//! The levels are station → channel → block → pool → item. [`TrackRules::cascade`]
//! combines them: `exclude` entries add up, `prefer` entries from a more
//! specific level go first, and `inherit: false` on a list discards everything
//! above it.

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::tracks::StreamInfo;

/// The rule lists for one level of the config, one per kind of stream.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct TrackRules {
    #[serde(default, skip_serializing_if = "RuleList::is_default")]
    pub audio: RuleList,
    #[serde(default, skip_serializing_if = "RuleList::is_default")]
    pub subtitle: RuleList,
}

/// `exclude` runs first and removes any matching stream outright. `prefer` is
/// ordered: the first entry that matches a surviving stream wins, and the
/// fields inside one entry are ANDed.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct RuleList {
    /// `false` discards every rule from the levels above this one.
    #[serde(default = "yes")]
    pub inherit: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<StreamMatch>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prefer: Vec<StreamMatch>,
}

fn yes() -> bool {
    true
}

impl Default for RuleList {
    fn default() -> Self {
        RuleList {
            inherit: true,
            exclude: Vec::new(),
            prefer: Vec::new(),
        }
    }
}

impl RuleList {
    fn is_default(&self) -> bool {
        *self == RuleList::default()
    }

    /// `over` laid on top of `base`.
    fn layered(base: &RuleList, over: &RuleList) -> RuleList {
        if !over.inherit {
            return RuleList {
                inherit: true,
                exclude: over.exclude.clone(),
                prefer: over.prefer.clone(),
            };
        }
        RuleList {
            inherit: true,
            exclude: base.exclude.iter().chain(&over.exclude).cloned().collect(),
            prefer: over.prefer.iter().chain(&base.prefer).cloned().collect(),
        }
    }
}

/// One stream matcher. Every field that is present must hold; a field left out
/// matches anything.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct StreamMatch {
    /// The stream's index in the file: `2`, or a comparison such as `">=3"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Number>,
    /// Regex against the stream's title tag. Case-insensitive, unanchored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Regex against the stream's language tag as the file stores it
    /// (`eng`, `jpn`). A stream with no tag, or `und`, has no language and
    /// never matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// Regex against the codec name (`ac3`, `subrip`, `hdmv_pgs_subtitle`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// Audio channel count: `2`, or a comparison such as `">=6"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forced: Option<bool>,
    /// Subtitle only. `true` matches picture subtitles (PGS, VobSub), which
    /// cannot become WebVTT and are burned into the video when selected;
    /// `false` matches text subtitles, which the viewer can switch off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<bool>,
    /// A condition on the file as a whole, checked after the stream fields.
    /// Subtitle lists only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<When>,
}

/// Conditions that read the audio stream the station selected, which is how a
/// subtitle rule says "only for a film in a foreign language".
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct When {
    /// Regex that the selected audio stream's language must match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_lang: Option<String>,
    /// Regex that the selected audio stream's language must not match. The
    /// stream still has to carry a language: unknown is not foreign.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_lang_not: Option<String>,
}

/// An integer matcher: an exact value, or a comparison written as a string.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Number {
    Exact(i64),
    Compare(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, Copy)]
pub struct Comparison {
    op: Op,
    value: i64,
}

impl Comparison {
    fn parse(n: &Number) -> Result<Comparison, String> {
        let text = match n {
            Number::Exact(v) => {
                return Ok(Comparison {
                    op: Op::Eq,
                    value: *v,
                });
            }
            Number::Compare(s) => s.trim(),
        };
        // Longest operators first so ">=" is not read as ">" followed by "=3".
        let (op, rest) = [
            (">=", Op::Ge),
            ("<=", Op::Le),
            ("!=", Op::Ne),
            ("==", Op::Eq),
            (">", Op::Gt),
            ("<", Op::Lt),
            ("=", Op::Eq),
        ]
        .iter()
        .find_map(|(sym, op)| text.strip_prefix(sym).map(|rest| (*op, rest)))
        .unwrap_or((Op::Eq, text));
        let value = rest.trim().parse::<i64>().map_err(|_| {
            format!("{text:?} is not a number or a comparison like \">=6\" or \"<8\"")
        })?;
        Ok(Comparison { op, value })
    }

    fn holds(&self, actual: i64) -> bool {
        match self.op {
            Op::Eq => actual == self.value,
            Op::Ne => actual != self.value,
            Op::Lt => actual < self.value,
            Op::Le => actual <= self.value,
            Op::Gt => actual > self.value,
            Op::Ge => actual >= self.value,
        }
    }
}

fn regex(pattern: &str) -> Result<Regex, String> {
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .map_err(|e| format!("invalid regex {pattern:?}: {e}"))
}

/// A [`StreamMatch`] with its regexes built, so a bad pattern is found when the
/// config loads and matching a stream cannot fail.
#[derive(Debug, Clone)]
pub struct CompiledMatch {
    id: Option<Comparison>,
    title: Option<Regex>,
    lang: Option<Regex>,
    codec: Option<Regex>,
    channels: Option<Comparison>,
    default: Option<bool>,
    forced: Option<bool>,
    image: Option<bool>,
    audio_lang: Option<Regex>,
    audio_lang_not: Option<Regex>,
}

impl StreamMatch {
    /// Build the matcher, naming the field that is wrong.
    pub fn compile(&self) -> Result<CompiledMatch, String> {
        fn field<T>(name: &str, r: Result<T, String>) -> Result<T, String> {
            r.map_err(|m| format!("{name}: {m}"))
        }
        let re = |name: &str, p: &Option<String>| -> Result<Option<Regex>, String> {
            p.as_deref().map(|p| field(name, regex(p))).transpose()
        };
        let num = |name: &str, n: &Option<Number>| -> Result<Option<Comparison>, String> {
            n.as_ref()
                .map(|n| field(name, Comparison::parse(n)))
                .transpose()
        };
        let when = self.when.clone().unwrap_or_default();
        Ok(CompiledMatch {
            id: num("id", &self.id)?,
            title: re("title", &self.title)?,
            lang: re("lang", &self.lang)?,
            codec: re("codec", &self.codec)?,
            channels: num("channels", &self.channels)?,
            default: self.default,
            forced: self.forced,
            image: self.image,
            audio_lang: re("when.audio_lang", &when.audio_lang)?,
            audio_lang_not: re("when.audio_lang_not", &when.audio_lang_not)?,
        })
    }
}

impl CompiledMatch {
    /// Whether `stream` satisfies every field this matcher names.
    /// `audio_lang` is the language of the audio stream the station selected,
    /// for the `when` conditions.
    pub fn matches(&self, stream: &StreamInfo, audio_lang: Option<&str>) -> bool {
        if let Some(c) = &self.id
            && !c.holds(i64::from(stream.index))
        {
            return false;
        }
        if let Some(c) = &self.channels
            && !stream.channels.is_some_and(|n| c.holds(i64::from(n)))
        {
            return false;
        }
        if let Some(re) = &self.title
            && !stream.title.as_deref().is_some_and(|t| re.is_match(t))
        {
            return false;
        }
        if let Some(re) = &self.lang
            && !stream.lang.as_deref().is_some_and(|l| re.is_match(l))
        {
            return false;
        }
        if let Some(re) = &self.codec
            && !re.is_match(&stream.codec)
        {
            return false;
        }
        if self.default.is_some_and(|d| d != stream.default) {
            return false;
        }
        if self.forced.is_some_and(|f| f != stream.forced) {
            return false;
        }
        if self.image.is_some_and(|i| i != stream.is_image()) {
            return false;
        }
        if let Some(re) = &self.audio_lang
            && !audio_lang.is_some_and(|l| re.is_match(l))
        {
            return false;
        }
        if let Some(re) = &self.audio_lang_not
            && !audio_lang.is_some_and(|l| !re.is_match(l))
        {
            return false;
        }
        true
    }
}

/// A [`RuleList`] ready to match against streams.
#[derive(Debug, Clone, Default)]
pub struct CompiledList {
    pub exclude: Vec<CompiledMatch>,
    pub prefer: Vec<CompiledMatch>,
}

/// Both lists of a [`TrackRules`], compiled.
#[derive(Debug, Clone, Default)]
pub struct CompiledRules {
    pub audio: CompiledList,
    pub subtitle: CompiledList,
}

impl RuleList {
    fn compile(&self, kind: &str) -> Result<CompiledList, String> {
        let build = |list: &str, entries: &[StreamMatch]| -> Result<Vec<CompiledMatch>, String> {
            entries
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    m.compile()
                        .map_err(|e| format!("tracks.{kind}.{list}[{i}].{e}"))
                })
                .collect()
        };
        Ok(CompiledList {
            exclude: build("exclude", &self.exclude)?,
            prefer: build("prefer", &self.prefer)?,
        })
    }

    /// Audio rules have no "selected audio" to condition on, and `image` has
    /// no meaning for an audio stream; accepting either would be a rule that
    /// silently never does what it says.
    fn check_for_kind(&self, kind: &str, is_audio: bool) -> Result<(), String> {
        for (list, entries) in [("exclude", &self.exclude), ("prefer", &self.prefer)] {
            for (i, m) in entries.iter().enumerate() {
                if is_audio && m.when.is_some() {
                    return Err(format!(
                        "tracks.{kind}.{list}[{i}].when: only subtitle rules can use `when`"
                    ));
                }
                if is_audio && m.image.is_some() {
                    return Err(format!(
                        "tracks.{kind}.{list}[{i}].image: only subtitle rules can use `image`"
                    ));
                }
                if !is_audio && m.channels.is_some() {
                    return Err(format!(
                        "tracks.{kind}.{list}[{i}].channels: only audio rules can use `channels`"
                    ));
                }
            }
        }
        Ok(())
    }
}

impl TrackRules {
    /// The rules every cascade starts from: a subtitle track is any text
    /// stream. Written as a rule so that a level can drop it with
    /// `inherit: false` like any other.
    pub fn floor() -> TrackRules {
        TrackRules {
            audio: RuleList::default(),
            subtitle: RuleList {
                inherit: true,
                exclude: Vec::new(),
                prefer: vec![StreamMatch {
                    image: Some(false),
                    ..StreamMatch::default()
                }],
            },
        }
    }

    /// Combine levels given most specific first. Absent levels are skipped.
    pub fn cascade(levels: &[Option<&TrackRules>]) -> TrackRules {
        levels
            .iter()
            .rev()
            .flatten()
            .fold(TrackRules::default(), |acc, over| TrackRules {
                audio: RuleList::layered(&acc.audio, &over.audio),
                subtitle: RuleList::layered(&acc.subtitle, &over.subtitle),
            })
    }

    /// Reject a rule that cannot work. `label` names the config level for the
    /// error, e.g. `channel` or `block #2`.
    pub fn validate(&self, label: &str) -> Result<(), String> {
        self.audio
            .check_for_kind("audio", true)
            .and_then(|()| self.subtitle.check_for_kind("subtitle", false))
            .and_then(|()| self.compile().map(|_| ()))
            .map_err(|m| format!("{label}: {m}"))
    }

    pub fn compile(&self) -> Result<CompiledRules, String> {
        Ok(CompiledRules {
            audio: self.audio.compile("audio")?,
            subtitle: self.subtitle.compile("subtitle")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(yaml: &str) -> TrackRules {
        serde_norway::from_str(yaml).expect("valid rules")
    }

    #[test]
    fn exclude_entries_add_up_across_levels() {
        let station = rules("audio: { exclude: [ { title: commentary } ] }");
        let channel = rules("audio: { exclude: [ { title: isolated } ] }");
        let merged = TrackRules::cascade(&[Some(&channel), Some(&station)]);
        assert_eq!(merged.audio.exclude.len(), 2);
    }

    #[test]
    fn a_more_specific_prefer_entry_goes_first() {
        let station = rules("audio: { prefer: [ { lang: '^en' } ] }");
        let block = rules("audio: { prefer: [ { lang: '^ja' } ] }");
        let merged = TrackRules::cascade(&[Some(&block), Some(&station)]);
        let langs: Vec<_> = merged
            .audio
            .prefer
            .iter()
            .map(|m| m.lang.clone().unwrap())
            .collect();
        assert_eq!(langs, ["^ja", "^en"]);
    }

    #[test]
    fn inherit_false_discards_every_level_above() {
        let station =
            rules("audio: { exclude: [ { title: commentary } ], prefer: [ { lang: '^en' } ] }");
        let block = rules("audio: { inherit: false, prefer: [ { lang: '^ja' } ] }");
        let merged = TrackRules::cascade(&[Some(&block), Some(&station)]);
        assert!(
            merged.audio.exclude.is_empty(),
            "the station exclude is gone"
        );
        assert_eq!(merged.audio.prefer.len(), 1);
    }

    #[test]
    fn inherit_false_on_one_kind_leaves_the_other_alone() {
        let station = rules(
            "subtitle: { prefer: [ { lang: '^en' } ] }\naudio: { prefer: [ { lang: '^en' } ] }",
        );
        let block = rules("subtitle: { inherit: false }");
        let merged = TrackRules::cascade(&[Some(&block), Some(&station)]);
        assert!(merged.subtitle.prefer.is_empty());
        assert_eq!(merged.audio.prefer.len(), 1);
    }

    #[test]
    fn the_floor_is_any_text_subtitle_and_a_level_can_drop_it() {
        let floor = TrackRules::floor();
        assert_eq!(floor.subtitle.prefer.len(), 1);
        assert_eq!(floor.subtitle.prefer[0].image, Some(false));
        let block = rules("subtitle: { inherit: false }");
        let merged = TrackRules::cascade(&[Some(&block), Some(&floor)]);
        assert!(merged.subtitle.prefer.is_empty());
    }

    #[test]
    fn comparisons_parse_in_both_forms() {
        let c = |n: Number| Comparison::parse(&n).unwrap();
        assert!(c(Number::Exact(2)).holds(2));
        assert!(!c(Number::Exact(2)).holds(6));
        assert!(c(Number::Compare(">=6".into())).holds(6));
        assert!(!c(Number::Compare(">=6".into())).holds(2));
        assert!(c(Number::Compare("<8".into())).holds(6));
        assert!(Comparison::parse(&Number::Compare("lots".into())).is_err());
    }

    #[test]
    fn an_invalid_regex_names_the_pattern_and_where_it_is() {
        let bad = rules("subtitle: { prefer: [ { lang: ok }, { title: '(' } ] }");
        let err = bad.validate("channel").unwrap_err();
        assert!(err.contains("channel"), "{err}");
        assert!(err.contains("tracks.subtitle.prefer[1].title"), "{err}");
        assert!(err.contains("\"(\""), "{err}");
    }

    #[test]
    fn a_comparison_that_is_not_a_number_is_rejected() {
        let bad = rules("audio: { prefer: [ { channels: 'many' } ] }");
        assert!(bad.validate("station").unwrap_err().contains("channels"));
    }

    #[test]
    fn rules_that_cannot_apply_to_their_kind_are_rejected() {
        let when_on_audio = rules("audio: { prefer: [ { when: { audio_lang: en } } ] }");
        assert!(when_on_audio.validate("x").unwrap_err().contains("when"));
        let channels_on_subtitle = rules("subtitle: { prefer: [ { channels: 2 } ] }");
        assert!(
            channels_on_subtitle
                .validate("x")
                .unwrap_err()
                .contains("channels")
        );
    }
}

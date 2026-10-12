//! Choosing an item's audio and subtitle streams from its probed file (#261).
//!
//! [`crate::config::tracks`] holds the rules as YAML; this module applies them
//! to the streams `ffprobe` listed and returns stream indexes, which
//! [`crate::rule`] writes into the playout JSON. The only built-in behaviour is
//! the fallback when no audio rule decides: the file's default-flagged audio
//! stream, else its first. Which language, which channel count, and whether to
//! skip a commentary track are all rules the config states.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::tracks::{CompiledList, CompiledRules};
use crate::config::{SourceConfig, TrackRules};
use crate::duration::DurationCache;
use crate::resolve::{ResolvedItem, SelectedTracks};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamKind {
    Audio,
    Video,
    Subtitle,
    Other,
}

/// One stream of a probed file, with the fields the rules can match.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamInfo {
    /// The stream's index in the file, which is what ETV-next's
    /// `stream_index` names.
    pub index: u32,
    pub kind: StreamKind,
    pub codec: String,
    /// The `language` tag as stored (`eng`). `None` when the file has no tag
    /// or says `und`, so "unknown" is one state, not two.
    pub lang: Option<String>,
    pub title: Option<String>,
    pub channels: Option<u32>,
    pub default: bool,
    pub forced: bool,
}

impl StreamInfo {
    /// A picture subtitle (PGS, VobSub): it cannot become WebVTT, so ETV-next
    /// burns it into the video.
    pub fn is_image(&self) -> bool {
        self.kind == StreamKind::Subtitle && ffpipeline::probe::is_image_subtitle_codec(&self.codec)
    }
}

/// Why a stream won, for the log line that records the choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// The `prefer` entry at this index matched.
    Prefer(usize),
    /// No `prefer` entry decided, and this stream carries the default flag.
    DefaultFlag,
    /// No `prefer` entry decided and none is flagged: the first stream.
    First,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pick {
    pub index: u32,
    pub why: Why,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    pub audio: Option<Pick>,
    pub subtitle: Option<Pick>,
    /// The audio rules include `prefer` entries and none matched, so the
    /// fallback chose. A config that was meant to decide and did not.
    pub audio_unmatched: bool,
    /// Every audio stream was excluded; the choice was made among all of them
    /// rather than leave the item silent.
    pub audio_all_excluded: bool,
}

/// Whether this selection picked a picture subtitle, which ETV-next will burn
/// into the video where the viewer cannot switch it off.
pub fn burned(selection: &Selection, streams: &[StreamInfo]) -> bool {
    selection
        .subtitle
        .and_then(|p| streams.iter().find(|s| s.index == p.index))
        .is_some_and(StreamInfo::is_image)
}

pub fn select(streams: &[StreamInfo], rules: &CompiledRules) -> Selection {
    let audio: Vec<&StreamInfo> = streams
        .iter()
        .filter(|s| s.kind == StreamKind::Audio)
        .collect();
    let mut kept = survivors(&audio, &rules.audio, None);
    let audio_all_excluded = kept.is_empty() && !audio.is_empty();
    if audio_all_excluded {
        kept = audio.clone();
    }

    let mut audio_unmatched = false;
    let audio_pick = match prefer(&kept, &rules.audio, None) {
        Some(pick) => Some(pick),
        None => {
            audio_unmatched = !rules.audio.prefer.is_empty() && !kept.is_empty();
            kept.iter()
                .find(|s| s.default)
                .map(|s| Pick {
                    index: s.index,
                    why: Why::DefaultFlag,
                })
                .or_else(|| {
                    kept.first().map(|s| Pick {
                        index: s.index,
                        why: Why::First,
                    })
                })
        }
    };

    let audio_lang = audio_pick
        .and_then(|p| streams.iter().find(|s| s.index == p.index))
        .and_then(|s| s.lang.as_deref());

    let subtitles: Vec<&StreamInfo> = streams
        .iter()
        .filter(|s| s.kind == StreamKind::Subtitle)
        .collect();
    let subtitle_survivors = survivors(&subtitles, &rules.subtitle, audio_lang);
    // No fallback for subtitles: an unconfigured picture subtitle must never be
    // burned into a viewer's picture because nothing else matched.
    let subtitle_pick = prefer(&subtitle_survivors, &rules.subtitle, audio_lang);

    Selection {
        audio: audio_pick,
        subtitle: subtitle_pick,
        audio_unmatched,
        audio_all_excluded,
    }
}

fn survivors<'a>(
    candidates: &[&'a StreamInfo],
    list: &CompiledList,
    audio_lang: Option<&str>,
) -> Vec<&'a StreamInfo> {
    candidates
        .iter()
        .copied()
        .filter(|s| !list.exclude.iter().any(|m| m.matches(s, audio_lang)))
        .collect()
}

/// The first `prefer` entry that matches any survivor decides; ties inside it
/// go to the fewest channels, then the lowest index.
fn prefer(
    survivors: &[&StreamInfo],
    list: &CompiledList,
    audio_lang: Option<&str>,
) -> Option<Pick> {
    list.prefer.iter().enumerate().find_map(|(i, entry)| {
        survivors
            .iter()
            .filter(|s| entry.matches(s, audio_lang))
            .min_by_key(|s| (s.channels.unwrap_or(u32::MAX), s.index))
            .map(|s| Pick {
                index: s.index,
                why: Why::Prefer(i),
            })
    })
}

/// The levels of the track-selection cascade that are the same for every item
/// of a channel, most general first. An item's own pool or item level sits on
/// top of them.
pub struct Levels<'a> {
    pub station: Option<&'a TrackRules>,
    pub channel: Option<&'a TrackRules>,
    /// One entry per `rule.blocks` index.
    pub blocks: Vec<Option<&'a TrackRules>>,
}

/// What a pass over a generation's items did, for the one log line that
/// summarises it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Tally {
    pub items: usize,
    pub with_subtitle: usize,
    pub burned: usize,
    pub audio_unmatched: usize,
}

/// Choose each probed local item's audio and subtitle streams and record them
/// on the item, for [`crate::rule`] to write into the playout JSON.
///
/// Items with nothing to choose from are left alone: a lavfi or HTTP source
/// has no probe, and an error card is not a file.
pub fn assign(
    items: &mut [ResolvedItem],
    cache: &DurationCache,
    levels: &Levels<'_>,
    channel: &str,
) -> Tally {
    let floor = TrackRules::floor();
    let mut tally = Tally::default();
    let mut by_block: HashMap<usize, Option<CompiledRules>> = HashMap::new();
    let mut by_rules: HashMap<String, Option<CompiledRules>> = HashMap::new();

    for item in items.iter_mut() {
        let SourceConfig::Local { path } = &item.source else {
            continue;
        };
        if item.error_card {
            continue;
        }
        let Some(streams) = cache.streams_for(Path::new(path)) else {
            continue;
        };

        let block_level = levels.blocks.get(item.block).copied().flatten();
        let merged = |own: Option<&TrackRules>| {
            TrackRules::cascade(&[
                own,
                block_level,
                levels.channel,
                levels.station,
                Some(&floor),
            ])
        };
        let compiled = match &item.tracks {
            None => by_block
                .entry(item.block)
                .or_insert_with(|| compile(&merged(None), channel, &item.id))
                .as_ref(),
            Some(own) => {
                let key = serde_json::to_string(own).unwrap_or_default();
                by_rules
                    .entry(format!("{}:{key}", item.block))
                    .or_insert_with(|| compile(&merged(Some(own)), channel, &item.id))
                    .as_ref()
            }
        };
        let Some(compiled) = compiled else {
            continue;
        };

        let selection = select(streams, compiled);
        let is_burned = burned(&selection, streams);
        tracing::debug!(
            event = "tracks.selected",
            channel = %channel,
            item = %item.id,
            path = %path,
            audio = ?selection.audio,
            subtitle = ?selection.subtitle,
            burned = is_burned,
            streams = streams.len(),
            "chose this item's audio and subtitle streams",
        );
        if selection.audio_all_excluded {
            tracing::warn!(
                event = "tracks.audio_all_excluded",
                channel = %channel,
                item = %item.id,
                path = %path,
                "every audio stream matches an exclude rule; chose among all of them",
            );
        }
        if selection.audio_unmatched {
            tally.audio_unmatched += 1;
            tracing::warn!(
                event = "tracks.audio_unmatched",
                channel = %channel,
                item = %item.id,
                path = %path,
                chosen = ?selection.audio,
                "no audio `prefer` rule matched; fell back to the default-flagged stream",
            );
        }
        if is_burned {
            tally.burned += 1;
            tracing::info!(
                event = "tracks.burned",
                channel = %channel,
                item = %item.id,
                path = %path,
                subtitle = ?selection.subtitle,
                "a picture subtitle was selected and will be burned into the video",
            );
        }
        tally.items += 1;
        tally.with_subtitle += usize::from(selection.subtitle.is_some());
        item.selected = Some(SelectedTracks {
            audio: selection.audio.map(|p| p.index),
            subtitle: selection.subtitle.map(|p| p.index),
        });
    }
    tally
}

fn compile(rules: &TrackRules, channel: &str, item: &str) -> Option<CompiledRules> {
    match rules.compile() {
        Ok(compiled) => Some(compiled),
        Err(error) => {
            // Load-time validation rejects every pattern that could land here.
            tracing::error!(
                event = "tracks.rules_invalid",
                channel = %channel,
                item = %item,
                error = %error,
                "track rules did not compile; the item plays without a track selection",
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tracks::TrackRules;

    fn audio(index: u32, lang: &str, title: &str, channels: u32, default: bool) -> StreamInfo {
        StreamInfo {
            index,
            kind: StreamKind::Audio,
            codec: "ac3".into(),
            lang: Some(lang.into()),
            title: Some(title.into()),
            channels: Some(channels),
            default,
            forced: false,
        }
    }

    fn sub(index: u32, lang: &str, codec: &str) -> StreamInfo {
        StreamInfo {
            index,
            kind: StreamKind::Subtitle,
            codec: codec.into(),
            lang: Some(lang.into()),
            title: None,
            channels: None,
            default: false,
            forced: false,
        }
    }

    fn run(yaml: &str, streams: &[StreamInfo]) -> Selection {
        let rules: TrackRules = serde_norway::from_str(yaml).unwrap();
        let rules = TrackRules::cascade(&[Some(&rules), Some(&TrackRules::floor())]);
        select(streams, &rules.compile().unwrap())
    }

    #[test]
    fn with_no_rules_the_default_flagged_audio_plays() {
        let streams = [
            audio(1, "eng", "5.1", 6, false),
            audio(2, "eng", "Stereo", 2, true),
        ];
        let sel = run("{}", &streams);
        assert_eq!(
            sel.audio,
            Some(Pick {
                index: 2,
                why: Why::DefaultFlag
            })
        );
        assert!(!sel.audio_unmatched, "no prefer entries, nothing unmatched");
    }

    #[test]
    fn with_no_rules_and_no_flag_the_first_audio_plays() {
        let streams = [audio(3, "eng", "", 6, false), audio(4, "eng", "", 2, false)];
        assert_eq!(run("{}", &streams).audio.unwrap().index, 3);
    }

    #[test]
    fn stereo_beats_surround_when_a_rule_asks_for_stereo() {
        let streams = [audio(1, "eng", "", 6, true), audio(2, "eng", "", 2, false)];
        let sel = run(
            "audio: { prefer: [ { lang: '^en', channels: 2 } ] }",
            &streams,
        );
        assert_eq!(sel.audio.unwrap().index, 2);
    }

    #[test]
    fn ties_in_a_winning_entry_go_to_fewest_channels_then_lowest_index() {
        let streams = [
            audio(1, "eng", "", 6, false),
            audio(2, "eng", "", 2, false),
            audio(3, "eng", "", 2, false),
        ];
        let sel = run("audio: { prefer: [ { lang: '^en' } ] }", &streams);
        assert_eq!(sel.audio.unwrap().index, 2);
    }

    // The Hard Boiled file: both tracks are Cantonese audio titled as English
    // commentary. A commentary exclude removes both, so the choice falls back
    // to the default flag among all of them, and the config's mistake is
    // reported rather than silent.
    #[test]
    fn excluding_every_audio_stream_falls_back_and_says_so() {
        let streams = [
            audio(1, "eng", "English Commentary", 6, false),
            audio(2, "eng", "English Commentary", 2, true),
        ];
        let sel = run("audio: { exclude: [ { title: commentary } ] }", &streams);
        assert!(sel.audio_all_excluded);
        assert_eq!(sel.audio.unwrap().index, 2);
    }

    #[test]
    fn a_commentary_exclude_skips_the_commentary_but_keeps_the_film() {
        let streams = [
            audio(1, "eng", "Director Commentary", 2, true),
            audio(2, "jpn", "Main", 6, false),
        ];
        let sel = run("audio: { exclude: [ { title: commentary } ] }", &streams);
        assert_eq!(sel.audio.unwrap().index, 2);
        assert!(!sel.audio_all_excluded);
    }

    #[test]
    fn prefer_entries_that_match_nothing_are_reported() {
        let streams = [audio(1, "jpn", "", 2, true)];
        let sel = run("audio: { prefer: [ { lang: '^en' } ] }", &streams);
        assert!(sel.audio_unmatched);
        assert_eq!(sel.audio.unwrap().index, 1);
    }

    #[test]
    fn with_no_rules_a_text_subtitle_is_selected_and_a_picture_one_is_not() {
        let text = [audio(1, "eng", "", 2, true), sub(2, "eng", "subrip")];
        assert_eq!(run("{}", &text).subtitle.unwrap().index, 2);
        let picture = [
            audio(1, "eng", "", 2, true),
            sub(2, "eng", "hdmv_pgs_subtitle"),
        ];
        assert_eq!(run("{}", &picture).subtitle, None);
    }

    #[test]
    fn a_text_subtitle_wins_over_a_picture_one_in_the_same_file() {
        let streams = [
            audio(1, "jpn", "", 2, true),
            sub(2, "eng", "hdmv_pgs_subtitle"),
            sub(3, "eng", "ass"),
        ];
        let sel = run(
            "subtitle: { prefer: [ { image: false, lang: '^en' }, { image: true, lang: '^en' } ] }",
            &streams,
        );
        assert_eq!(sel.subtitle.unwrap().index, 3);
        assert!(!burned(&sel, &streams));
    }

    const ANIME: &str = "subtitle:
  prefer:
    - { image: false, lang: '^en' }
    - { image: true, lang: '^en', when: { audio_lang_not: '^en' } }";

    #[test]
    fn a_picture_subtitle_is_burned_for_a_foreign_language_film() {
        let streams = [
            audio(1, "jpn", "", 2, true),
            sub(2, "eng", "hdmv_pgs_subtitle"),
        ];
        let sel = run(ANIME, &streams);
        assert_eq!(sel.subtitle.unwrap().index, 2);
        assert!(burned(&sel, &streams));
    }

    #[test]
    fn a_picture_subtitle_is_not_selected_for_an_english_film() {
        let streams = [
            audio(1, "eng", "", 2, true),
            sub(2, "eng", "hdmv_pgs_subtitle"),
        ];
        assert_eq!(run(ANIME, &streams).subtitle, None);
    }

    #[test]
    fn unknown_audio_language_is_not_foreign() {
        let mut a = audio(1, "x", "", 2, true);
        a.lang = None;
        let streams = [a, sub(2, "eng", "hdmv_pgs_subtitle")];
        assert_eq!(run(ANIME, &streams).subtitle, None);
    }

    #[test]
    fn when_reads_the_audio_that_was_selected_not_the_first_audio() {
        let streams = [
            audio(1, "eng", "Commentary", 2, true),
            audio(2, "jpn", "Main", 2, false),
            sub(3, "eng", "hdmv_pgs_subtitle"),
        ];
        let yaml = format!("audio: {{ exclude: [ {{ title: commentary }} ] }}\n{ANIME}");
        let sel = run(&yaml, &streams);
        assert_eq!(sel.audio.unwrap().index, 2);
        assert_eq!(sel.subtitle.unwrap().index, 3);
    }

    #[test]
    fn a_file_with_no_subtitle_streams_selects_none() {
        let streams = [audio(1, "eng", "", 2, true)];
        assert_eq!(run("{}", &streams).subtitle, None);
    }

    fn local_item(block: usize, tracks: Option<TrackRules>) -> ResolvedItem {
        ResolvedItem {
            block,
            id: "film".into(),
            source: SourceConfig::Local {
                path: "/m/film.mkv".into(),
            },
            in_point: None,
            out_point: None,
            program: None,
            catalog_duration: None,
            error_card: false,
            metadata: None,
            guide: None,
            guide_fields: crate::guide::GuideFields::default(),
            tracks,
            selected: None,
        }
    }

    fn picked(items: &[ResolvedItem]) -> (Option<u32>, Option<u32>) {
        let s = items[0].selected.expect("selected");
        (s.audio, s.subtitle)
    }

    fn film() -> Vec<StreamInfo> {
        vec![
            audio(1, "jpn", "Main", 2, true),
            audio(2, "eng", "Commentary", 2, false),
            sub(3, "eng", "hdmv_pgs_subtitle"),
            sub(4, "eng", "subrip"),
        ]
    }

    fn rules_of(yaml: &str) -> TrackRules {
        serde_norway::from_str(yaml).unwrap()
    }

    // The anime case from the design: the station default never burns a picture,
    // and one block opts in. Same file, two blocks, two outcomes.
    #[test]
    fn a_block_can_opt_into_burning_pictures_while_the_rest_of_the_channel_does_not() {
        let cache = DurationCache::with_streams(
            "/m/film.mkv",
            vec![
                audio(1, "jpn", "", 2, true),
                sub(2, "eng", "hdmv_pgs_subtitle"),
            ],
        );
        let anime = rules_of(
            "subtitle: { prefer: [ { image: true, lang: '^en', when: { audio_lang_not: '^en' } } ] }",
        );
        let levels = Levels {
            station: None,
            channel: None,
            blocks: vec![None, Some(&anime)],
        };
        let mut items = vec![local_item(0, None), local_item(1, None)];
        let tally = assign(&mut items, &cache, &levels, "ch");

        assert_eq!(items[0].selected.unwrap().subtitle, None);
        assert_eq!(items[1].selected.unwrap().subtitle, Some(2));
        assert_eq!((tally.items, tally.burned), (2, 1));
    }

    #[test]
    fn the_pool_level_beats_the_block_level() {
        let cache = DurationCache::with_streams("/m/film.mkv", film());
        let block = rules_of("subtitle: { prefer: [ { image: true } ] }");
        let pool = rules_of("subtitle: { prefer: [ { image: false } ] }");
        let levels = Levels {
            station: None,
            channel: None,
            blocks: vec![Some(&block)],
        };
        let mut items = vec![local_item(0, Some(pool))];
        assign(&mut items, &cache, &levels, "ch");
        assert_eq!(
            picked(&items).1,
            Some(4),
            "the pool's text preference went first"
        );
    }

    #[test]
    fn excludes_from_every_level_apply() {
        let cache = DurationCache::with_streams("/m/film.mkv", film());
        let station = rules_of("audio: { exclude: [ { title: commentary } ] }");
        let channel = rules_of("audio: { exclude: [ { lang: jpn } ] }");
        let levels = Levels {
            station: Some(&station),
            channel: Some(&channel),
            blocks: vec![None],
        };
        let mut items = vec![local_item(0, None)];
        assign(&mut items, &cache, &levels, "ch");
        // Both audio streams are excluded (one by each level), so it falls
        // back to the default-flagged one among all of them.
        assert_eq!(picked(&items).0, Some(1));
    }

    #[test]
    fn items_without_a_probe_are_left_unselected() {
        let cache = DurationCache::default();
        let levels = Levels {
            station: None,
            channel: None,
            blocks: vec![None],
        };
        let mut lavfi = local_item(0, None);
        lavfi.source = SourceConfig::Lavfi {
            params: "testsrc".into(),
        };
        let mut items = vec![lavfi, local_item(0, None)];
        let tally = assign(&mut items, &cache, &levels, "ch");
        assert!(items.iter().all(|i| i.selected.is_none()));
        assert_eq!(tally.items, 0);
    }

    #[test]
    fn a_stream_with_an_unknown_channel_count_loses_the_tie_to_a_known_one() {
        let mut unknown = audio(1, "eng", "", 2, false);
        unknown.channels = None;
        let streams = [unknown, audio(2, "eng", "", 6, false)];
        let sel = run("audio: { prefer: [ { lang: '^en' } ] }", &streams);
        assert_eq!(sel.audio.unwrap().index, 2);
    }

    #[test]
    fn dvb_and_xsub_pictures_are_not_text_subtitles() {
        let streams = [audio(1, "eng", "", 2, true), sub(2, "eng", "dvb_subtitle")];
        assert_eq!(run("{}", &streams).subtitle, None);
        assert!(sub(3, "eng", "xsub").is_image());
    }

    #[test]
    fn a_file_with_no_audio_selects_none() {
        let streams = [sub(1, "eng", "subrip")];
        assert_eq!(run("{}", &streams).audio, None);
    }
}

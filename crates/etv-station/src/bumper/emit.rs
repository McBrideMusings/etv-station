//! Turn a [`ResolvedTimeline`] into playout items.
//!
//! Each resolved segment becomes one [`PlayoutItem`], or several when a
//! looping clip is shorter than its fitted length. Nothing is added to the
//! ETV-next schema: a bumper is told apart from a programme by its item id
//! ([`BUMPER_ID_PREFIX`]), and the overlay process reads the per-item
//! `overlay` and `metadata` fields.
//!
//! An item's length is its `start`/`finish` span. A lavfi source has no
//! in/out point of its own, so both its video and its audio are open-ended
//! and that span is what ends them; no `-t` goes in the params.
//!
//! Loudness gain and fades on a clip's own audio are not applied here: the
//! station turns on ETV-next's `normalize_loudness` for every item
//! (etv-station-7aa7.9), so `SegmentAudio::Own` omits `tracks.audio`. A bed or
//! mute segment sets its level on purpose, so its items carry
//! `normalize_loudness: false` and the channel's loudnorm leaves them alone.
//! Ducking a bed under clip audio is etv-station-7aa7.27, and swapping a
//! pending stand-in for its real file is etv-station-7aa7.35.

use std::path::Path;

use ersatztv_playout::playout::{
    BUMPER_ID_PREFIX, ItemOverlay, OverlayNone, PlayoutItem, PlayoutItemSource, PlayoutItemTracks,
    TrackSelection,
};
use serde_json::json;
use time::{Duration, OffsetDateTime};

use super::timeline::{
    AudioSource, OverlayVars, ResolvedSegment, ResolvedTimeline, SegmentAudio, SegmentSource,
};

/// What [`emit_timeline`] needs to know about the channel it emits for.
pub struct EmitContext<'a> {
    pub channel: &'a str,
    /// The channel framerate; a Still is resampled to it.
    pub fps: f64,
    /// Directory a relative overlay script path is resolved against, so the
    /// path written into the playout is absolute. etv-overlay resolves a
    /// relative path against its own working directory, not the playout
    /// folder.
    pub overlay_root: &'a Path,
    /// A media file's length in ms, or `None` when it cannot be read. A
    /// non-looping Clip or Render shorter than its window is cut at the file's
    /// end and the rest is black, so no out-point lands past the file.
    pub media_len_ms: &'a dyn Fn(&Path) -> Option<u64>,
}

/// Push one Local item playing `path` from `in_ms` for `duration_ms`. When the
/// file ends sooner, the item is cut at the file's end and a black item
/// (`#pad`) covers the rest.
fn push_local_padded(
    items: &mut Vec<PlayoutItem>,
    ctx: &EmitContext<'_>,
    id: &str,
    path: &Path,
    in_ms: u64,
    begin: OffsetDateTime,
    duration_ms: u64,
) {
    let playable = (ctx.media_len_ms)(path).map(|len| len.saturating_sub(in_ms));
    if playable.is_none() {
        tracing::warn!(
            event = "bumper.media_len_unknown",
            channel = ctx.channel,
            path = %path.display(),
            window_ms = duration_ms,
            "a bumper file's length could not be read; its window is emitted unpadded"
        );
    }
    let file_ms = playable.map_or(duration_ms, |p| p.min(duration_ms));
    let mut at = begin;
    if file_ms > 0 {
        let finish = at + Duration::milliseconds(file_ms as i64);
        let source = PlayoutItemSource::local(
            path.to_string_lossy().into_owned(),
            Some(in_ms),
            Some(in_ms + file_ms),
        );
        items.push(PlayoutItem::scheduled(id.to_string(), at, finish, source));
        at = finish;
    }
    if file_ms < duration_ms {
        tracing::warn!(
            event = "bumper.media_short",
            channel = ctx.channel,
            path = %path.display(),
            in_ms,
            file_ms,
            window_ms = duration_ms,
            "a bumper file ends before its window; the rest is black"
        );
        let finish = begin + Duration::milliseconds(duration_ms as i64);
        let id = if file_ms == 0 {
            id.to_string()
        } else {
            format!("{id}#pad")
        };
        let source = PlayoutItemSource::lavfi("color=c=black".to_string());
        items.push(PlayoutItem::scheduled(id, at, finish, source));
    }
}

/// Emit `resolved` as consecutive playout items beginning at `start`.
///
/// Item boundaries come from each segment's `start_ms` and `duration_ms`, so
/// they are consecutive and their total is exactly [`ResolvedTimeline::total_ms`].
pub fn emit_timeline(
    resolved: &ResolvedTimeline,
    ctx: &EmitContext<'_>,
    start: OffsetDateTime,
) -> Vec<PlayoutItem> {
    let start_unix_ms = start.unix_timestamp() * 1000 + i64::from(start.millisecond());
    let mut items = Vec::new();
    for (index, seg) in resolved.segments.iter().enumerate() {
        let id_base = format!("{BUMPER_ID_PREFIX}{}:{start_unix_ms}:{index}", ctx.channel);
        let begin = start + Duration::milliseconds(seg.start_ms as i64);
        let end = begin + Duration::milliseconds(seg.duration_ms as i64);
        let first = items.len();
        match &seg.segment.source {
            SegmentSource::Clip {
                path,
                in_ms,
                out_ms,
            } => {
                let window = out_ms.saturating_sub(*in_ms);
                if !(seg.segment.loop_ && window > 0 && seg.duration_ms > window) {
                    push_local_padded(
                        &mut items,
                        ctx,
                        &id_base,
                        path,
                        *in_ms,
                        begin,
                        seg.duration_ms,
                    );
                } else {
                    let mut parts = Vec::new();
                    for _ in 0..seg.duration_ms / window {
                        parts.push(window);
                    }
                    let tail = seg.duration_ms % window;
                    if tail > 0 {
                        parts.push(tail);
                    }
                    let mut at = begin;
                    for (n, len) in parts.into_iter().enumerate() {
                        let finish = at + Duration::milliseconds(len as i64);
                        let id = format!("{id_base}#{n}");
                        let source = PlayoutItemSource::local(
                            path.to_string_lossy().into_owned(),
                            Some(*in_ms),
                            Some(in_ms + len),
                        );
                        items.push(PlayoutItem::scheduled(id, at, finish, source));
                        at = finish;
                    }
                }
            }
            SegmentSource::Render { path, .. } => {
                push_local_padded(&mut items, ctx, &id_base, path, 0, begin, seg.duration_ms);
            }
            SegmentSource::Http { url, in_ms, .. } => {
                tracing::info!(
                    event = "bumper.http_source",
                    channel = ctx.channel,
                    segment = %seg.segment.name,
                    host = %crate::plugin_http::display_host(url),
                    in_ms = *in_ms,
                    length_ms = seg.duration_ms,
                    "a bumper segment plays a remote file"
                );
                // A remote file cannot be repeated, so the item plays from the
                // in point for the fitted length; a script keeps that within
                // the file with a flex range that ends at its length.
                let source = PlayoutItemSource::http(
                    url.clone(),
                    Some(*in_ms),
                    Some(in_ms + seg.duration_ms),
                    None,
                    None,
                );
                items.push(PlayoutItem::scheduled(id_base, begin, end, source));
            }
            SegmentSource::Lavfi { params } => {
                let source = PlayoutItemSource::lavfi(params.clone());
                items.push(PlayoutItem::scheduled(id_base, begin, end, source));
            }
            SegmentSource::Still { image_path } => {
                let graph = still_graph(image_path, ctx.fps, seg, resolved.bed.as_ref());
                let source = PlayoutItemSource::lavfi(graph);
                items.push(PlayoutItem::scheduled(id_base, begin, end, source));
            }
        }

        // A Still carries its audio inside its own graph; every other source
        // gets a `tracks.audio` override when the segment asks for a bed or
        // silence.
        let own_graph = matches!(seg.segment.source, SegmentSource::Still { .. });
        let parts = items.len() - first;
        for (n, item) in items[first..].iter_mut().enumerate() {
            let len_ms = (item.finish - item.start).whole_milliseconds() as u64;
            let edges = (n == 0, n + 1 == parts);
            if !own_graph
                && let Some(audio) = audio_override(seg, resolved.bed.as_ref(), len_ms, edges)
            {
                item.tracks = Some(PlayoutItemTracks {
                    audio: Some(TrackSelection {
                        source: Some(audio),
                        stream_index: None,
                    }),
                    video: None,
                    subtitle: None,
                });
            }
            if seg.segment.audio != SegmentAudio::Own {
                item.normalize_loudness = Some(false);
            }
            item.overlay = Some(overlay_for(seg.segment.overlay.as_ref(), ctx.overlay_root));
            item.metadata = Some(json!({
                "bumper": true,
                "segment": seg.segment.name,
                "vars": seg.segment.overlay.as_ref().map(|o| &o.vars),
            }));
        }
    }
    items
}

/// The resolved timeline as an OpenTimelineIO JSON document: one video track,
/// one audio track, a Clip per segment with its `media_reference.target_url`
/// and `source_range`, and the overlay vars under `metadata.etv`. For
/// debugging and NLE handoff only; nothing reads it back.
pub fn otio_json(resolved: &ResolvedTimeline, fps: f64) -> serde_json::Value {
    let range = |start_ms: u64, duration_ms: u64| {
        json!({
            "OTIO_SCHEMA": "TimeRange.1",
            "start_time": rational(start_ms, fps),
            "duration": rational(duration_ms, fps),
        })
    };
    let clips = |audio: bool| -> Vec<serde_json::Value> {
        resolved
            .segments
            .iter()
            .map(|seg| {
                let (target, in_ms) = match &seg.segment.source {
                    SegmentSource::Clip { path, in_ms, .. } => {
                        (path.to_string_lossy().into_owned(), *in_ms)
                    }
                    SegmentSource::Render { path, .. } => (path.to_string_lossy().into_owned(), 0),
                    SegmentSource::Http { url, in_ms, .. } => (url.clone(), *in_ms),
                    SegmentSource::Still { image_path } => {
                        (image_path.to_string_lossy().into_owned(), 0)
                    }
                    SegmentSource::Lavfi { params } => (format!("lavfi:{params}"), 0),
                };
                let target = if audio && seg.segment.audio == SegmentAudio::Bed {
                    resolved
                        .bed
                        .as_ref()
                        .map(|b| b.path.to_string_lossy().into_owned())
                        .unwrap_or(target)
                } else {
                    target
                };
                json!({
                    "OTIO_SCHEMA": "Clip.2",
                    "name": seg.segment.name,
                    "source_range": range(in_ms, seg.duration_ms),
                    "media_reference": {
                        "OTIO_SCHEMA": "ExternalReference.1",
                        "target_url": target,
                    },
                    "metadata": { "etv": {
                        "overlay": seg.segment.overlay,
                        "audio": seg.segment.audio,
                    } },
                })
            })
            .collect()
    };
    json!({
        "OTIO_SCHEMA": "Timeline.1",
        "name": "bumper",
        "tracks": {
            "OTIO_SCHEMA": "Stack.1",
            "children": [
                { "OTIO_SCHEMA": "Track.1", "kind": "Video", "children": clips(false) },
                { "OTIO_SCHEMA": "Track.1", "kind": "Audio", "children": clips(true) },
            ],
        },
    })
}

fn rational(ms: u64, fps: f64) -> serde_json::Value {
    json!({
        "OTIO_SCHEMA": "RationalTime.1",
        "rate": fps,
        "value": ms as f64 * fps / 1000.0,
    })
}

/// The `tracks.audio` source for a non-Still segment, or `None` for its own audio.
///
/// A looped clip is several items and each is its own ffmpeg input, so a bed
/// restarts at every seam (one audio track per item). The fade-in is kept to
/// the first item and the fade-out to the last, each timed to that item's
/// own length.
fn audio_override(
    seg: &ResolvedSegment,
    bed: Option<&AudioSource>,
    len_ms: u64,
    (first, last): (bool, bool),
) -> Option<PlayoutItemSource> {
    match seg.segment.audio {
        SegmentAudio::Own => None,
        SegmentAudio::Mute => Some(PlayoutItemSource::lavfi(SILENCE.to_string())),
        SegmentAudio::Bed => Some(PlayoutItemSource::lavfi(match bed {
            Some(bed) => bed_graph(bed, len_ms, first, last),
            None => SILENCE.to_string(),
        })),
    }
}

const SILENCE: &str = "anullsrc=channel_layout=stereo:sample_rate=48000";

/// The lavfi graph for a Still segment.
///
/// One lavfi item per Still, measured in etv-station-7aa7.3 on a 10s Still
/// played through ETV-next: one discontinuity, 4/4/2s HLS segments, correct
/// program-date-time, and clean boundary frames in 4 of 4 runs. The other
/// form, repeated one-frame Local items, needed 10 ffmpeg spawns, 20 ffprobe
/// calls and 10 discontinuities per 10s; a single Local with an out-point past
/// the file's end emitted a 10s segment labelled EXTINF 1.0 and stamped every
/// later segment 9s early. `movie` alone runs at 25 fps, so `fps=` resamples
/// to the channel rate.
fn still_graph(image: &Path, fps: f64, seg: &ResolvedSegment, bed: Option<&AudioSource>) -> String {
    let audio = match (seg.segment.audio, bed) {
        (SegmentAudio::Bed, Some(bed)) => bed_graph(bed, seg.duration_ms, true, true),
        _ => SILENCE.to_string(),
    };
    format!(
        "movie={},loop=loop=-1:size=1,fps={}[out0];{audio}[out1]",
        quote_filter_arg(&image.to_string_lossy()),
        format_fps(fps),
    )
}

/// `amovie` with the bed's gain and fades. The bed loops so a segment longer
/// than the file keeps sound; the item's own span ends it.
fn bed_graph(bed: &AudioSource, duration_ms: u64, first: bool, last: bool) -> String {
    let mut graph = format!(
        "amovie={}:loop=0,volume={}dB",
        quote_filter_arg(&bed.path.to_string_lossy()),
        bed.gain_db,
    );
    if first && bed.fade_in_ms > 0 {
        graph.push_str(&format!(",afade=t=in:st=0:d={}", seconds(bed.fade_in_ms)));
    }
    if last && bed.fade_out_ms > 0 {
        let at = duration_ms.saturating_sub(bed.fade_out_ms);
        graph.push_str(&format!(
            ",afade=t=out:st={}:d={}",
            seconds(at),
            seconds(bed.fade_out_ms)
        ));
    }
    graph
}

fn overlay_for(overlay: Option<&OverlayVars>, root: &Path) -> ItemOverlay {
    match overlay {
        None => ItemOverlay::Disabled(OverlayNone::None),
        Some(o) => ItemOverlay::Script {
            script: root.join(&o.script).to_string_lossy().into_owned(),
            vars: Some(o.vars.clone().into_iter().collect()),
            extra: serde_json::Map::new(),
        },
    }
}

/// Single-quote a filter argument; ffmpeg's filter syntax gives `:` `,` `;`
/// `[` `]` and `\` meaning, and a quoted run protects all of them.
fn quote_filter_arg(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

fn seconds(ms: u64) -> String {
    format!("{}", ms as f64 / 1000.0)
}

fn format_fps(fps: f64) -> String {
    // NTSC rates are n*1000/1001; a rounded decimal would drift from the
    // channel's exact rate over a long Still.
    for n in [24u32, 30, 48, 60] {
        let exact = f64::from(n) * 1000.0 / 1001.0;
        if (fps - exact).abs() < 0.005 {
            return format!("{}/1001", n * 1000);
        }
    }
    if fps.fract() == 0.0 {
        format!("{}", fps as u64)
    } else {
        format!("{fps}")
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ersatztv_playout::playout::is_bumper_id;

    use super::*;
    use crate::bumper::timeline::{Duration as Dur, Segment, Timeline, fit};

    fn seg(name: &str, source: SegmentSource, duration: Dur) -> Segment {
        Segment {
            name: name.into(),
            source,
            duration,
            loop_: false,
            optional: false,
            priority: 0,
            overlay: None,
            audio: SegmentAudio::Own,
        }
    }

    fn ctx() -> EmitContext<'static> {
        EmitContext {
            channel: "7",
            fps: 30.0,
            overlay_root: Path::new("/station/overlays"),
            media_len_ms: &|_| None,
        }
    }

    fn start() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
    }

    fn emit(timeline: &Timeline, gap_ms: u64) -> Vec<PlayoutItem> {
        let resolved = fit(timeline, gap_ms, 30.0).unwrap();
        emit_timeline(&resolved, &ctx(), start())
    }

    fn lavfi_params(item: &PlayoutItem) -> &str {
        match item.source.as_ref().unwrap() {
            PlayoutItemSource::Lavfi { params, .. } => params,
            other => panic!("expected lavfi, got {other:?}"),
        }
    }

    fn mixed() -> Timeline {
        Timeline {
            segments: vec![
                seg("card", SegmentSource::black(), Dur::Fixed(2_000)),
                seg(
                    "clip",
                    SegmentSource::Clip {
                        path: "/m/a.mkv".into(),
                        in_ms: 5_000,
                        out_ms: 9_000,
                    },
                    Dur::Fixed(3_333),
                ),
                seg(
                    "still",
                    SegmentSource::Still {
                        image_path: "/m/art.png".into(),
                    },
                    Dur::Flex {
                        min_ms: 1_000,
                        max_ms: 20_000,
                        grow: 1.0,
                    },
                ),
            ],
            bed: None,
        }
    }

    #[test]
    fn items_are_consecutive_and_sum_to_the_resolved_length() {
        let timeline = mixed();
        let resolved = fit(&timeline, 10_001, 30.0).unwrap();
        let items = emit_timeline(&resolved, &ctx(), start());
        assert_eq!(items[0].start, start());
        for pair in items.windows(2) {
            assert_eq!(pair[0].finish, pair[1].start);
        }
        let total = items.last().unwrap().finish - items[0].start;
        assert_eq!(total.whole_milliseconds() as u64, resolved.total_ms());
        assert_eq!(resolved.total_ms(), 10_001);
    }

    #[test]
    fn every_id_is_a_bumper_id_carrying_channel_start_and_index() {
        let items = emit(&mixed(), 10_000);
        for (i, item) in items.iter().enumerate() {
            assert!(is_bumper_id(&item.id), "{}", item.id);
            assert_eq!(item.id, format!("bumper:7:1800000000000:{i}"));
        }
    }

    #[test]
    fn a_looping_clip_longer_than_its_window_becomes_repeats_and_a_tail() {
        let mut clip = seg(
            "clip",
            SegmentSource::Clip {
                path: "/m/a.mkv".into(),
                in_ms: 1_000,
                out_ms: 4_000,
            },
            Dur::Fixed(8_000),
        );
        clip.loop_ = true;
        let items = emit(
            &Timeline {
                segments: vec![clip],
                bed: None,
            },
            8_000,
        );
        let spans: Vec<(String, Option<u64>, Option<u64>)> = items
            .iter()
            .map(|i| match i.source.as_ref().unwrap() {
                PlayoutItemSource::Local {
                    in_point_ms,
                    out_point_ms,
                    ..
                } => (i.id.clone(), *in_point_ms, *out_point_ms),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            spans,
            vec![
                (
                    "bumper:7:1800000000000:0#0".into(),
                    Some(1_000),
                    Some(4_000)
                ),
                (
                    "bumper:7:1800000000000:0#1".into(),
                    Some(1_000),
                    Some(4_000)
                ),
                (
                    "bumper:7:1800000000000:0#2".into(),
                    Some(1_000),
                    Some(3_000)
                ),
            ]
        );
        assert_eq!(
            (items[2].finish - items[0].start).whole_milliseconds(),
            8_000
        );
    }

    #[test]
    fn a_clip_that_fits_is_one_unsuffixed_item_with_its_window() {
        let items = emit(&mixed(), 10_000);
        assert_eq!(items[1].id, "bumper:7:1800000000000:1");
        match items[1].source.as_ref().unwrap() {
            PlayoutItemSource::Local {
                path,
                in_point_ms,
                out_point_ms,
                ..
            } => {
                assert_eq!(path, "/m/a.mkv");
                assert_eq!(*in_point_ms, Some(5_000));
                assert_eq!(*out_point_ms, Some(8_333));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_clip_shorter_than_its_window_is_cut_at_the_file_end_and_padded_black() {
        let clip = seg(
            "clip",
            SegmentSource::Clip {
                path: "/m/a.mkv".into(),
                in_ms: 1_000,
                out_ms: 20_000,
            },
            Dur::Fixed(10_000),
        );
        let resolved = fit(
            &Timeline {
                segments: vec![clip],
                bed: None,
            },
            10_000,
            30.0,
        )
        .unwrap();
        let ctx = EmitContext {
            media_len_ms: &|_| Some(5_000),
            ..ctx()
        };
        let items = emit_timeline(&resolved, &ctx, start());
        assert_eq!(items.len(), 2);
        match items[0].source.as_ref().unwrap() {
            PlayoutItemSource::Local {
                in_point_ms,
                out_point_ms,
                ..
            } => assert_eq!((*in_point_ms, *out_point_ms), (Some(1_000), Some(5_000))),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            (items[0].finish - items[0].start).whole_milliseconds(),
            4_000
        );
        assert_eq!(items[1].id, "bumper:7:1800000000000:0#pad");
        assert_eq!(lavfi_params(&items[1]), "color=c=black");
        assert_eq!(items[0].finish, items[1].start);
        assert_eq!(
            (items[1].finish - items[0].start).whole_milliseconds(),
            10_000
        );
    }

    #[test]
    fn a_render_shorter_than_its_window_is_padded_and_a_long_one_is_untouched() {
        let render = |dur| {
            let resolved = fit(
                &Timeline {
                    segments: vec![seg(
                        "r",
                        SegmentSource::Render {
                            profile: "p".into(),
                            path: "/r/x.mp4".into(),
                        },
                        Dur::Fixed(dur),
                    )],
                    bed: None,
                },
                dur,
                30.0,
            )
            .unwrap();
            let ctx = EmitContext {
                media_len_ms: &|_| Some(6_000),
                ..ctx()
            };
            emit_timeline(&resolved, &ctx, start())
        };
        assert_eq!(render(10_000).len(), 2);
        let exact = render(6_000);
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].id, "bumper:7:1800000000000:0");
    }

    #[test]
    fn a_still_is_one_lavfi_item_at_the_channel_fps() {
        let items = emit(&mixed(), 10_000);
        assert_eq!(
            lavfi_params(&items[2]),
            "movie='/m/art.png',loop=loop=-1:size=1,fps=30[out0];\
             anullsrc=channel_layout=stereo:sample_rate=48000[out1]"
        );
        assert!(items[2].tracks.is_none());
    }

    #[test]
    fn lavfi_params_carry_no_duration() {
        let items = emit(&mixed(), 10_000);
        assert_eq!(lavfi_params(&items[0]), "color=c=black");
        assert_eq!(
            (items[0].finish - items[0].start).whole_milliseconds(),
            2_000
        );
    }

    fn bed() -> AudioSource {
        AudioSource {
            path: PathBuf::from("/beds/rain.wav"),
            gain_db: -12.0,
            fade_in_ms: 500,
            fade_out_ms: 1_000,
        }
    }

    #[test]
    fn bed_and_mute_items_opt_out_of_loudnorm_and_own_audio_does_not() {
        let mut card = seg("card", SegmentSource::black(), Dur::Fixed(4_000));
        card.audio = SegmentAudio::Bed;
        let mut quiet = seg("quiet", SegmentSource::black(), Dur::Fixed(1_000));
        quiet.audio = SegmentAudio::Mute;
        let own = seg("own", SegmentSource::black(), Dur::Fixed(1_000));
        assert_eq!(own.audio, SegmentAudio::Own);
        let items = emit(
            &Timeline {
                segments: vec![card, quiet, own],
                bed: Some(bed()),
            },
            6_000,
        );
        assert_eq!(items[0].normalize_loudness, Some(false));
        assert_eq!(items[1].normalize_loudness, Some(false));
        assert_eq!(items[2].normalize_loudness, None);
    }

    #[test]
    fn bed_audio_is_an_amovie_with_gain_and_fades_and_mute_is_anullsrc() {
        let mut card = seg("card", SegmentSource::black(), Dur::Fixed(4_000));
        card.audio = SegmentAudio::Bed;
        let mut quiet = seg("quiet", SegmentSource::black(), Dur::Fixed(1_000));
        quiet.audio = SegmentAudio::Mute;
        let items = emit(
            &Timeline {
                segments: vec![card, quiet],
                bed: Some(bed()),
            },
            5_000,
        );
        let audio = |i: &PlayoutItem| match i
            .tracks
            .as_ref()
            .unwrap()
            .audio
            .as_ref()
            .unwrap()
            .source
            .as_ref()
            .unwrap()
        {
            PlayoutItemSource::Lavfi { params, .. } => params.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            audio(&items[0]),
            "amovie='/beds/rain.wav':loop=0,volume=-12dB,afade=t=in:st=0:d=0.5,afade=t=out:st=3:d=1"
        );
        assert_eq!(
            audio(&items[1]),
            "anullsrc=channel_layout=stereo:sample_rate=48000"
        );
    }

    #[test]
    fn a_still_with_a_bed_takes_the_bed_as_its_audio_output() {
        let mut still = seg(
            "still",
            SegmentSource::Still {
                image_path: "/m/art.png".into(),
            },
            Dur::Fixed(4_000),
        );
        still.audio = SegmentAudio::Bed;
        let items = emit(
            &Timeline {
                segments: vec![still],
                bed: Some(bed()),
            },
            4_000,
        );
        assert!(
            lavfi_params(&items[0])
                .contains("fps=30[out0];amovie='/beds/rain.wav':loop=0,volume=-12dB")
        );
        assert!(lavfi_params(&items[0]).ends_with("[out1]"));
    }

    #[test]
    fn overlay_is_absolute_with_vars_or_none_and_metadata_marks_the_bumper() {
        let mut with = seg("card", SegmentSource::black(), Dur::Fixed(1_000));
        with.overlay = Some(OverlayVars {
            script: "shared/title_card.rhai".into(),
            vars: [("title".to_string(), json!("Alien"))]
                .into_iter()
                .collect(),
        });
        let without = seg("plain", SegmentSource::black(), Dur::Fixed(1_000));
        let items = emit(
            &Timeline {
                segments: vec![with, without],
                bed: None,
            },
            2_000,
        );
        match items[0].overlay.as_ref().unwrap() {
            ItemOverlay::Script { script, vars, .. } => {
                assert_eq!(script, "/station/overlays/shared/title_card.rhai");
                assert_eq!(vars.as_ref().unwrap()["title"], "Alien");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            items[1].overlay,
            Some(ItemOverlay::Disabled(OverlayNone::None))
        );
        let meta = items[0].metadata.as_ref().unwrap();
        assert_eq!(meta["bumper"], true);
        assert_eq!(meta["segment"], "card");
    }

    #[test]
    fn otio_has_a_video_and_an_audio_track_with_one_clip_per_segment() {
        let resolved = fit(&mixed(), 10_000, 30.0).unwrap();
        let doc = otio_json(&resolved, 30.0);
        let tracks = doc["tracks"]["children"].as_array().unwrap();
        assert_eq!(tracks.len(), 2);
        let video = tracks[0]["children"].as_array().unwrap();
        assert_eq!(video.len(), 3);
        assert_eq!(video[1]["media_reference"]["target_url"], "/m/a.mkv");
        assert_eq!(video[1]["source_range"]["start_time"]["value"], 150.0);
    }

    #[test]
    fn a_bed_under_a_looped_clip_fades_in_once_and_out_once_per_item_length() {
        let mut clip = seg(
            "clip",
            SegmentSource::Clip {
                path: "/m/a.mkv".into(),
                in_ms: 0,
                out_ms: 3_000,
            },
            Dur::Fixed(8_000),
        );
        clip.loop_ = true;
        clip.audio = SegmentAudio::Bed;
        let items = emit(
            &Timeline {
                segments: vec![clip],
                bed: Some(bed()),
            },
            8_000,
        );
        let graph = |i: &PlayoutItem| match i
            .tracks
            .as_ref()
            .unwrap()
            .audio
            .as_ref()
            .unwrap()
            .source
            .as_ref()
            .unwrap()
        {
            PlayoutItemSource::Lavfi { params, .. } => params.clone(),
            other => panic!("{other:?}"),
        };
        assert!(graph(&items[0]).contains("afade=t=in") && !graph(&items[0]).contains("t=out"));
        assert!(!graph(&items[1]).contains("afade"));
        assert!(
            graph(&items[2]).contains("afade=t=out:st=1:d=1") && !graph(&items[2]).contains("t=in")
        );
    }

    #[test]
    fn ntsc_rates_are_exact_fractions() {
        assert_eq!(format_fps(29.97), "30000/1001");
        assert_eq!(format_fps(23.976), "24000/1001");
        assert_eq!(format_fps(25.0), "25");
    }

    #[test]
    fn a_path_with_filter_syntax_characters_stays_one_argument() {
        assert_eq!(
            quote_filter_arg("/a b/it's,x:y.png"),
            r"'/a b/it'\''s,x:y.png'"
        );
    }
}

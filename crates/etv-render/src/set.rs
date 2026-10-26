//! The set plan: whole songs played one after another, each pair joined by a
//! seam recipe.
//!
//! A chapter plan cuts short hooks from stems; a set plan plays each song's full
//! mix from where it enters until the seam out of it begins. The plan names the
//! recipe, the tempo each song plays at and where every seam falls in the
//! outgoing song, and says nothing about why a plugin chose them. The format is
//! documented in `docs/schema.md` § Set plan.
//!
//! Three recipes join song A to song B, as the autodj spike's round 4 settled:
//!
//! * `bass_swap`: for `bars` bars A's low end is cut and B's is brought in over
//!   it, then B carries on alone. B plays at A's running tempo, so the beats
//!   stay locked through the overlap.
//! * `echo_out`: A's last bar rings out in an echo into B's entry, B at its own
//!   tempo.
//! * `drop_mix`: A stops on the phrase boundary and B starts on the beat, B at
//!   its own tempo.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::plan::{Card, PlanError};

/// Below this frequency a bass swap hands the low end from one song to the next.
const BASS_CROSSOVER_HZ: u32 = 250;
/// The top of the band A keeps while its bass is out, when the seam sweeps.
const SWEEP_CEILING_HZ: u32 = 1200;
/// A hard cut fades over this long so it does not click.
const CUT_FADE_S: f64 = 0.01;
const SHORT_FADE_S: f64 = 0.02;
/// The last song fades out over this long.
const LAST_FADE_S: f64 = 2.0;
/// How far a bass swap's incoming tempo may sit from the running tempo.
const RATIO_TOLERANCE: f64 = 0.002;

/// The `kind` a set plan carries; a plan without a `kind` is a chapter plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetKind {
    Set,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recipe {
    BassSwap,
    EchoOut,
    DropMix,
}

impl Recipe {
    pub fn as_str(self) -> &'static str {
        match self {
            Recipe::BassSwap => "bass_swap",
            Recipe::EchoOut => "echo_out",
            Recipe::DropMix => "drop_mix",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Track {
    pub entry_id: String,
    pub path: PathBuf,
    /// The song's own tempo.
    pub native_bpm: f64,
    /// Where in the file the song enters the set, in seconds: its start, or its
    /// landing bar.
    pub enter_s: f64,
    /// Source seconds played per set second; 1.0 is the song's own tempo.
    pub ratio: f64,
}

impl Track {
    /// Seconds in one bar of the file.
    fn source_bar_s(&self) -> f64 {
        240.0 / self.native_bpm
    }

    /// Seconds in one bar as the set plays it.
    fn bar_s(&self) -> f64 {
        self.source_bar_s() / self.ratio
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seam {
    pub recipe: Recipe,
    /// Where in the outgoing song, in its file's seconds, the seam's phrase
    /// ends: the last position of A played at full band before the recipe.
    pub out_s: f64,
    /// Bars a `bass_swap` lasts (even, 2 or more); 0 for the other recipes.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bars: u32,
    /// Whether a `bass_swap` also narrows A to a band while its bass is out.
    #[serde(default, skip_serializing_if = "is_false")]
    pub sweep: bool,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetPlan {
    pub kind: SetKind,
    /// The songs in air order.
    pub tracks: Vec<Track>,
    /// `seams[i]` joins `tracks[i]` to `tracks[i + 1]`.
    pub seams: Vec<Seam>,
    /// Where in the last song's file the set stops, in seconds.
    pub end_s: f64,
    /// Who is singing what, for the overlay. The renderer does not draw them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cards: Vec<Card>,
    /// Where the rendered set lands.
    pub output: PathBuf,
}

/// One excerpt of a song's audio on the set's timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioLayer {
    /// What an error calls it: `seam 2 (song-b) bass in`.
    pub name: String,
    pub track: usize,
    /// Where the excerpt starts in the song's file.
    pub src_start: f64,
    /// Where it starts in the set.
    pub out_start: f64,
    /// Seconds it lasts in the set, after stretching.
    pub out_len: f64,
    pub ratio: f64,
    /// ffmpeg audio filters applied after the stretch, fades included.
    pub filters: Vec<String>,
}

impl AudioLayer {
    /// Seconds of the file the excerpt reads.
    pub fn src_len(&self) -> f64 {
        self.out_len * self.ratio
    }
}

/// One stretch of a song's picture on the set's timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoSegment {
    pub name: String,
    pub track: usize,
    pub src_start: f64,
    pub out_start: f64,
    pub out_len: f64,
    pub ratio: f64,
}

/// Where everything of a set plan falls on its timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub audio: Vec<AudioLayer>,
    /// In air order, with no gap and no overlap.
    pub video: Vec<VideoSegment>,
    /// `seam_at_s[i]` is when `tracks[i + 1]` is playing alone after seam `i`.
    pub seam_at_s: Vec<f64>,
    pub duration_s: f64,
}

fn fade_in(start: f64, len: f64) -> String {
    format!("afade=t=in:st={start:.4}:d={len:.4}")
}

fn fade_out(start: f64, len: f64) -> String {
    format!("afade=t=out:st={:.4}:d={len:.4}", start.max(0.0))
}

impl SetPlan {
    pub fn parse(json: &str) -> Result<Self, PlanError> {
        let plan: SetPlan = serde_json::from_str(json)?;
        plan.validate()?;
        Ok(plan)
    }

    pub fn duration_s(&self) -> f64 {
        self.layout().duration_s
    }

    /// The media file a track's audio and picture are read from.
    pub fn track_path(&self, track: usize) -> &PathBuf {
        &self.tracks[track].path
    }

    /// Structure and arithmetic only; a named file that does not exist is the
    /// renderer's finding, recorded against the job.
    pub fn validate(&self) -> Result<(), PlanError> {
        let bad = |msg: String| Err(PlanError::Invalid(msg));
        if self.tracks.is_empty() {
            return bad("tracks is empty".into());
        }
        if self.seams.len() + 1 != self.tracks.len() {
            return bad(format!(
                "{} tracks need {} seams, got {}",
                self.tracks.len(),
                self.tracks.len() - 1,
                self.seams.len()
            ));
        }
        let mut ids = BTreeSet::new();
        for (i, t) in self.tracks.iter().enumerate() {
            let at = format!("tracks[{i}] ({})", t.entry_id);
            if !ids.insert(t.entry_id.as_str()) {
                return bad(format!("{at}: entry_id appears twice in tracks"));
            }
            if !(t.native_bpm.is_finite() && t.native_bpm > 0.0) {
                return bad(format!("{at}: native_bpm must be positive"));
            }
            if !(t.ratio.is_finite() && t.ratio > 0.0) {
                return bad(format!("{at}: ratio must be positive"));
            }
            if !(t.enter_s.is_finite() && t.enter_s >= 0.0) {
                return bad(format!("{at}: enter_s must not be negative"));
            }
        }
        for (k, seam) in self.seams.iter().enumerate() {
            let a = &self.tracks[k];
            let b = &self.tracks[k + 1];
            let at = format!("seams[{k}] ({} -> {})", a.entry_id, b.entry_id);
            if !seam.out_s.is_finite() || seam.out_s < a.enter_s + a.source_bar_s() {
                return bad(format!(
                    "{at}: out_s {} leaves less than one bar of {} after it enters at {}s",
                    seam.out_s, a.entry_id, a.enter_s
                ));
            }
            match seam.recipe {
                Recipe::BassSwap => {
                    if seam.bars < 2 || seam.bars % 2 != 0 {
                        return bad(format!(
                            "{at}: a bass_swap lasts an even number of bars, 2 or more, got {}",
                            seam.bars
                        ));
                    }
                    let half = f64::from(seam.bars) / 2.0 * a.bar_s();
                    let lead_in = half * b.ratio;
                    if b.enter_s + 1e-9 < lead_in {
                        return bad(format!(
                            "{at}: {} enters at {}s, which leaves {lead_in:.2}s of lead-in for the bass swap before the start of the file",
                            b.entry_id, b.enter_s
                        ));
                    }
                    let running = a.native_bpm * a.ratio;
                    let wanted = running / b.native_bpm;
                    if (b.ratio - wanted).abs() > RATIO_TOLERANCE {
                        return bad(format!(
                            "{at}: a bass_swap keeps the beats locked, so {}'s ratio must be {wanted:.4} ({running:.2} bpm over its {}), got {}",
                            b.entry_id, b.native_bpm, b.ratio
                        ));
                    }
                }
                Recipe::EchoOut => {
                    if seam.out_s < a.enter_s + 2.0 * a.source_bar_s() {
                        return bad(format!(
                            "{at}: an echo_out needs a bar of {} before out_s and the bar's echo after it",
                            a.entry_id
                        ));
                    }
                }
                Recipe::DropMix => {}
            }
            if seam.recipe != Recipe::BassSwap && (seam.bars != 0 || seam.sweep) {
                return bad(format!("{at}: bars and sweep belong to bass_swap only"));
            }
        }
        let last = self.tracks.last().expect("checked non-empty");
        if !self.end_s.is_finite() || self.end_s < last.enter_s + LAST_FADE_S * last.ratio {
            return bad(format!(
                "end_s {} leaves less than the {LAST_FADE_S}s closing fade after {} enters at {}s",
                self.end_s, last.entry_id, last.enter_s
            ));
        }
        if self.output.as_os_str().is_empty() {
            return bad("output is empty".into());
        }
        let total = self.duration_s();
        for (i, card) in self.cards.iter().enumerate() {
            if !(card.at_s.is_finite() && card.at_s >= 0.0 && card.at_s <= total) {
                return bad(format!(
                    "cards[{i}]: at_s {} is outside the set (0 to {total:.3}s)",
                    card.at_s
                ));
            }
        }
        Ok(())
    }

    /// The audio layers and picture of the set. Assumes [`SetPlan::validate`]
    /// passed.
    pub fn layout(&self) -> Layout {
        let mut audio = Vec::new();
        let mut video = Vec::new();
        let mut seam_at_s = Vec::new();
        let mut t = 0.0;
        for (k, seam) in self.seams.iter().enumerate() {
            let a = &self.tracks[k];
            let b = &self.tracks[k + 1];
            let ra = a.ratio;
            let pos = a.enter_s;
            let bl = a.bar_s();
            let p = seam.out_s;
            let body = (p - pos) / ra;
            let tp = t + body;
            let name = |what: &str| format!("seam {} ({}) {what}", k + 1, a.entry_id);
            let name_b = |what: &str| format!("seam {} ({}) {what}", k + 1, b.entry_id);
            let vseg = |track: usize, src_start: f64, out_start: f64, out_len: f64, ratio: f64| {
                VideoSegment {
                    name: format!("seam {} picture", k + 1),
                    track,
                    src_start,
                    out_start,
                    out_len,
                    ratio,
                }
            };
            video.push(vseg(k, pos, t, body, ra));
            match seam.recipe {
                Recipe::BassSwap => {
                    let half = f64::from(seam.bars) / 2.0 * bl;
                    let swap = tp + half;
                    let rb = b.ratio;
                    let through = body + half;
                    audio.push(AudioLayer {
                        name: name("body"),
                        track: k,
                        src_start: pos,
                        out_start: t,
                        out_len: through,
                        ratio: ra,
                        filters: vec![fade_out(through - SHORT_FADE_S, SHORT_FADE_S)],
                    });
                    let band = if seam.sweep {
                        format!("highpass=f={BASS_CROSSOVER_HZ},lowpass=f={SWEEP_CEILING_HZ}")
                    } else {
                        format!("highpass=f={BASS_CROSSOVER_HZ}")
                    };
                    audio.push(AudioLayer {
                        name: name("tail without bass"),
                        track: k,
                        src_start: p + half * ra,
                        out_start: swap,
                        out_len: half,
                        ratio: ra,
                        filters: vec![
                            band,
                            fade_in(0.0, SHORT_FADE_S),
                            fade_out(half / 2.0, half / 2.0),
                        ],
                    });
                    let bass_in = b.enter_s - half * rb;
                    audio.push(AudioLayer {
                        name: name_b("entering without bass"),
                        track: k + 1,
                        src_start: bass_in,
                        out_start: tp,
                        out_len: half,
                        ratio: rb,
                        filters: vec![
                            format!("highpass=f={BASS_CROSSOVER_HZ}"),
                            fade_in(0.0, bl.min(half)),
                            fade_out(half - SHORT_FADE_S, SHORT_FADE_S),
                        ],
                    });
                    // The picture cuts between the two songs once a bar, B's
                    // share of the bar growing as the swap goes on.
                    let beat = bl / 4.0;
                    let nbars = (seam.bars / 2).max(1);
                    let mut tt = tp;
                    for bar in 0..nbars {
                        let nb = (4 * (bar + 1) / (nbars + 1) + 1).min(4);
                        let a_beats = f64::from(4 - nb);
                        let bar_f = f64::from(bar);
                        if nb < 4 {
                            video.push(vseg(k, p + bar_f * bl * ra, tt, a_beats * beat, ra));
                        }
                        video.push(vseg(
                            k + 1,
                            bass_in + (bar_f * bl + a_beats * beat) * rb,
                            tt + a_beats * beat,
                            f64::from(nb) * beat,
                            rb,
                        ));
                        tt += bl;
                    }
                    t = swap;
                }
                Recipe::EchoOut => {
                    let bar_before = body - bl;
                    audio.push(AudioLayer {
                        name: name("body"),
                        track: k,
                        src_start: pos,
                        out_start: t,
                        out_len: bar_before,
                        ratio: ra,
                        filters: vec![fade_out(bar_before - SHORT_FADE_S, SHORT_FADE_S)],
                    });
                    let delay_ms = (bl / 4.0 * 0.75 * 1000.0) as i64;
                    audio.push(AudioLayer {
                        name: name("echo"),
                        track: k,
                        src_start: p - bl * ra,
                        out_start: tp - bl,
                        out_len: 3.0 * bl,
                        ratio: ra,
                        filters: vec![
                            format!("aecho=0.8:0.7:{delay_ms}:0.55"),
                            fade_in(0.0, SHORT_FADE_S),
                            fade_out(bl, 2.0 * bl),
                        ],
                    });
                    t = tp;
                }
                Recipe::DropMix => {
                    audio.push(AudioLayer {
                        name: name("body"),
                        track: k,
                        src_start: pos,
                        out_start: t,
                        out_len: body,
                        ratio: ra,
                        filters: vec![fade_out(body - CUT_FADE_S, CUT_FADE_S)],
                    });
                    t = tp;
                }
            }
            seam_at_s.push(t);
        }
        let last_at = self.tracks.len() - 1;
        let last = &self.tracks[last_at];
        let n = (self.end_s - last.enter_s) / last.ratio;
        audio.push(AudioLayer {
            name: format!("last song ({})", last.entry_id),
            track: last_at,
            src_start: last.enter_s,
            out_start: t,
            out_len: n,
            ratio: last.ratio,
            filters: vec![fade_out(n - LAST_FADE_S, LAST_FADE_S)],
        });
        video.push(VideoSegment {
            name: "last song picture".into(),
            track: last_at,
            src_start: last.enter_s,
            out_start: t,
            out_len: n,
            ratio: last.ratio,
        });
        video.sort_by(|x, y| x.out_start.total_cmp(&y.out_start));
        Layout {
            audio,
            video,
            seam_at_s,
            duration_s: t + n,
        }
    }
}

/// The ffmpeg filter graph that mixes `layout.audio` into `[aout]`. Input `i`
/// is `layout.audio[i]`, already cut to its excerpt, so each graph branch only
/// stretches, filters and delays it.
pub fn filter_graph(layout: &Layout, sample_rate: u32) -> String {
    let mut parts = Vec::new();
    let mut labels = String::new();
    for (i, l) in layout.audio.iter().enumerate() {
        let mut chain = format!(
            "[{i}:a]asetpts=PTS-STARTPTS,aresample={sample_rate},aformat=channel_layouts=stereo"
        );
        if (l.ratio - 1.0).abs() > 1e-9 {
            chain.push_str(&format!(",rubberband=tempo={:.6}", l.ratio));
        }
        for f in &l.filters {
            chain.push(',');
            chain.push_str(f);
        }
        let delay_ms = (l.out_start * 1000.0).round() as i64;
        chain.push_str(&format!(",adelay=delays={delay_ms}:all=1[a{i}]"));
        parts.push(chain);
        labels.push_str(&format!("[a{i}]"));
    }
    let total = layout.duration_s;
    parts.push(format!(
        "{labels}amix=inputs={}:normalize=0:duration=longest,apad=whole_dur={total:.4},atrim=end={total:.4},alimiter=limit=0.95[aout]",
        layout.audio.len()
    ));
    parts.join(";")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str, bpm: f64, enter_s: f64, ratio: f64) -> Track {
        Track {
            entry_id: id.into(),
            path: format!("/m/{id}.mp4").into(),
            native_bpm: bpm,
            enter_s,
            ratio,
        }
    }

    fn seam(recipe: Recipe, out_s: f64, bars: u32, sweep: bool) -> Seam {
        Seam {
            recipe,
            out_s,
            bars,
            sweep,
        }
    }

    /// 120 bpm songs have 2 s bars, so every number below is easy to check.
    fn plan(recipe: Recipe, bars: u32) -> SetPlan {
        SetPlan {
            kind: SetKind::Set,
            tracks: vec![track("a", 120.0, 16.0, 1.0), track("b", 120.0, 32.0, 1.0)],
            seams: vec![seam(recipe, 96.0, bars, false)],
            end_s: 80.0,
            cards: vec![],
            output: "/o.mp4".into(),
        }
    }

    fn assert_close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn a_drop_mix_cuts_a_at_its_phrase_and_starts_b_on_the_beat() {
        let p = plan(Recipe::DropMix, 0);
        p.validate().unwrap();
        let l = p.layout();
        // a plays 16 s to 96 s: 80 s. b plays 32 s to 80 s: 48 s.
        assert_close(l.seam_at_s[0], 80.0);
        assert_close(l.duration_s, 128.0);
        assert_eq!(l.audio.len(), 2);
        assert_close(l.audio[1].out_start, 80.0);
        assert_close(l.audio[1].src_start, 32.0);
    }

    #[test]
    fn a_bass_swap_overlaps_the_two_songs_for_its_bars_and_keeps_their_tempos_locked() {
        let p = plan(Recipe::BassSwap, 4);
        p.validate().unwrap();
        let l = p.layout();
        // Four bars of 2 s: the swap lasts 8 s, half of it (4 s) before B takes
        // over alone at 80 s + 4 s.
        assert_close(l.seam_at_s[0], 84.0);
        let names: Vec<&str> = l.audio.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "seam 1 (a) body",
                "seam 1 (a) tail without bass",
                "seam 1 (b) entering without bass",
                "last song (b)"
            ]
        );
        // a's body runs through the phrase and the half; its tail follows.
        assert_close(l.audio[0].out_len, 84.0);
        assert_close(l.audio[1].src_start, 100.0);
        // b's bass comes in 4 s of source before it enters at 32 s, at 80 s.
        assert_close(l.audio[2].src_start, 28.0);
        assert_close(l.audio[2].out_start, 80.0);
        assert!(l.audio[1].filters[0].starts_with("highpass=f=250"));
        assert_close(l.duration_s, 84.0 + 48.0);
    }

    #[test]
    fn a_sweeping_bass_swap_narrows_the_tail_to_a_band() {
        let mut p = plan(Recipe::BassSwap, 2);
        p.seams[0].sweep = true;
        let l = p.layout();
        assert_eq!(l.audio[1].filters[0], "highpass=f=250,lowpass=f=1200");
    }

    #[test]
    fn an_echo_out_rings_the_last_bar_into_the_next_song() {
        let p = plan(Recipe::EchoOut, 0);
        p.validate().unwrap();
        let l = p.layout();
        assert_close(l.seam_at_s[0], 80.0);
        // The body stops one bar (2 s) early; the echo covers that bar and two more.
        assert_close(l.audio[0].out_len, 78.0);
        assert_close(l.audio[1].src_start, 94.0);
        assert_close(l.audio[1].out_start, 78.0);
        assert_close(l.audio[1].out_len, 6.0);
        // A 2 s bar's beat is 0.5 s; the echo delay is three quarters of it.
        assert!(l.audio[1].filters[0].starts_with("aecho=0.8:0.7:375:0.55"));
    }

    #[test]
    fn a_songs_ratio_stretches_its_body_in_the_set() {
        let mut p = plan(Recipe::DropMix, 0);
        p.tracks[0].ratio = 1.25;
        p.seams[0].recipe = Recipe::DropMix;
        let l = p.layout();
        // 80 s of source at 1.25x is 64 s of set.
        assert_close(l.seam_at_s[0], 64.0);
        assert_close(l.audio[0].out_len, 64.0);
        assert_close(l.audio[0].src_len(), 80.0);
    }

    #[test]
    fn the_picture_has_no_gap_and_no_overlap_in_any_recipe() {
        for (recipe, bars) in [
            (Recipe::BassSwap, 2),
            (Recipe::BassSwap, 4),
            (Recipe::EchoOut, 0),
            (Recipe::DropMix, 0),
        ] {
            let l = plan(recipe, bars).layout();
            let mut end = 0.0;
            for v in &l.video {
                assert_close(v.out_start, end);
                assert!(v.out_len > 0.0, "{recipe:?}: empty segment {v:?}");
                end = v.out_start + v.out_len;
            }
            assert_close(end, l.duration_s);
        }
    }

    #[test]
    fn the_filter_graph_delays_stretches_and_mixes_every_layer() {
        let mut p = plan(Recipe::BassSwap, 4);
        p.tracks[1].ratio = 1.0;
        let l = p.layout();
        let g = filter_graph(&l, 44_100);
        assert!(g.contains("amix=inputs=4:normalize=0"));
        assert!(g.contains("adelay=delays=80000:all=1[a2]"));
        assert!(g.contains("apad=whole_dur=132.0000"));
        assert!(!g.contains("rubberband"), "{g}");
        let mut stretched = plan(Recipe::DropMix, 0);
        stretched.tracks[0].ratio = 1.1;
        let g = filter_graph(&stretched.layout(), 44_100);
        assert!(g.contains("rubberband=tempo=1.100000"));
    }

    fn rejects(edit: impl FnOnce(&mut SetPlan), needle: &str) {
        let mut p = plan(Recipe::BassSwap, 4);
        edit(&mut p);
        let err = p.validate().unwrap_err().to_string();
        assert!(err.contains(needle), "{err:?} lacks {needle:?}");
    }

    #[test]
    fn a_bass_swap_needs_lead_in_before_the_entry() {
        rejects(|p| p.tracks[1].enter_s = 2.0, "lead-in");
    }

    #[test]
    fn a_bass_swap_needs_the_incoming_song_at_the_running_tempo() {
        rejects(
            |p| {
                p.tracks[1].native_bpm = 130.0;
                p.tracks[1].ratio = 1.0;
            },
            "ratio must be",
        );
    }

    #[test]
    fn a_bass_swap_lasts_an_even_number_of_bars() {
        rejects(|p| p.seams[0].bars = 3, "even number of bars");
        rejects(|p| p.seams[0].bars = 0, "even number of bars");
    }

    #[test]
    fn the_other_recipes_take_no_bars() {
        rejects(
            |p| {
                p.seams[0].recipe = Recipe::DropMix;
            },
            "belong to bass_swap",
        );
    }

    #[test]
    fn seams_must_match_the_tracks() {
        rejects(|p| p.seams.clear(), "need 1 seams");
    }

    #[test]
    fn a_seam_must_fall_after_the_song_enters() {
        rejects(|p| p.seams[0].out_s = 17.0, "less than one bar");
    }

    #[test]
    fn a_song_cannot_air_twice() {
        rejects(|p| p.tracks[1].entry_id = "a".into(), "appears twice");
    }

    #[test]
    fn the_last_song_needs_room_for_its_closing_fade() {
        rejects(|p| p.end_s = 32.5, "closing fade");
    }

    #[test]
    fn an_unknown_field_is_refused() {
        let mut v = serde_json::to_value(plan(Recipe::DropMix, 0)).unwrap();
        v["tempo"] = 1.into();
        assert!(SetPlan::parse(&v.to_string()).is_err());
    }

    #[test]
    fn a_valid_plan_round_trips_through_json_with_its_kind() {
        let p = plan(Recipe::BassSwap, 2);
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"kind\":\"set\""), "{json}");
        assert_eq!(SetPlan::parse(&json).unwrap(), p);
    }

    #[test]
    fn a_card_outside_the_set_is_refused() {
        rejects(
            |p| {
                p.cards.push(Card {
                    at_s: 999.0,
                    artist: "x".into(),
                    title: "y".into(),
                    year: None,
                })
            },
            "outside the set",
        );
    }
}

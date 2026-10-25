//! The chapter's audio: layers cut from each hook's stems, stretched to the
//! chapter tempo, shifted to its key, levelled, faded, delayed to their bar and
//! mixed.
//!
//! Each hook contributes its vocal from its landing bar on. For the first
//! `vocal_over_bars` the previous hook's instrumental carries on underneath it;
//! after that the hook's own instrumental joins its vocal for `full_bars`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::RenderError;
use crate::plan::{Plan, REGION_BARS_BEFORE};
use crate::stems::{NO_VOCALS, VOCALS};
use crate::tools;

/// Mean level, in dB, the layers are brought to before mixing.
const VOCAL_DB: f64 = -20.0;
const SEAM_VOCAL_DB: f64 = -19.0;
const INSTRUMENTAL_DB: f64 = -17.0;
/// A stem quieter than this is silence, not music to be turned up.
const SILENT_DB: f64 = -70.0;
/// Rate the mix is written at.
const SAMPLE_RATE: u32 = 44_100;

/// One stem excerpt on the chapter's timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct Layer {
    /// What an error calls it: `hook 2 (song-b) vocals`.
    pub name: String,
    pub file: PathBuf,
    /// Where the excerpt starts in the stem file, and how long it is there.
    pub native_start: f64,
    pub native_len: f64,
    /// Where it starts in the chapter.
    pub out_start: f64,
    /// Chapter bpm over the song's own.
    pub tempo: f64,
    /// Frequency ratio for the semitone shift.
    pub pitch: f64,
    /// The mean level to bring the excerpt to, in dB.
    pub target_db: f64,
    pub fade_in: f64,
    pub fade_out: f64,
}

impl Layer {
    /// Seconds the excerpt lasts once stretched to the chapter tempo.
    pub fn out_len(&self) -> f64 {
        self.native_len / self.tempo
    }
}

/// Every layer of the chapter. `stem_dirs[i]` is hook `i`'s stems.
pub fn layers(plan: &Plan, stem_dirs: &[PathBuf]) -> Vec<Layer> {
    let bar = plan.bar_seconds;
    let mut layers = Vec::new();
    let mut out_bar = 0u32;
    for (i, hook) in plan.hooks.iter().enumerate() {
        let native_bar = 240.0 / hook.native_bpm;
        let tempo = plan.bpm / hook.native_bpm;
        let pitch = 2f64.powf(f64::from(hook.semitones) / 12.0);
        let name = |kind: &str| format!("hook {} ({}) {kind}", i + 1, hook.entry_id);
        let layer = |kind: &str,
                     dir: &Path,
                     file: &str,
                     from_bar: u32,
                     bars: u32,
                     at_bar: u32,
                     db: f64,
                     fi: f64,
                     fo: f64| Layer {
            name: name(kind),
            file: dir.join(file),
            native_start: (REGION_BARS_BEFORE + f64::from(from_bar)) * native_bar,
            native_len: f64::from(bars) * native_bar,
            out_start: f64::from(at_bar) * bar,
            tempo,
            pitch,
            target_db: db,
            fade_in: fi,
            fade_out: fo,
        };

        if i == 0 {
            layers.push(layer(
                "vocals",
                &stem_dirs[i],
                VOCALS,
                0,
                hook.full_bars,
                out_bar,
                VOCAL_DB,
                0.02,
                0.02,
            ));
            layers.push(layer(
                "instrumental",
                &stem_dirs[i],
                NO_VOCALS,
                0,
                hook.full_bars,
                out_bar,
                INSTRUMENTAL_DB,
                1.0,
                0.02,
            ));
        } else {
            let prev = &plan.hooks[i - 1];
            if hook.vocal_over_bars > 0 {
                // The previous song's instrumental keeps playing from where its
                // own bars ended, pitched and stretched as it was.
                let prev_native_bar = 240.0 / prev.native_bpm;
                layers.push(Layer {
                    name: format!(
                        "hook {} ({}) instrumental under the next vocal",
                        i, prev.entry_id
                    ),
                    file: stem_dirs[i - 1].join(NO_VOCALS),
                    native_start: (REGION_BARS_BEFORE
                        + f64::from(prev.vocal_over_bars + prev.full_bars))
                        * prev_native_bar,
                    native_len: f64::from(hook.vocal_over_bars) * prev_native_bar,
                    out_start: f64::from(out_bar) * bar,
                    tempo: plan.bpm / prev.native_bpm,
                    pitch: 2f64.powf(f64::from(prev.semitones) / 12.0),
                    target_db: INSTRUMENTAL_DB,
                    fade_in: 0.02,
                    fade_out: bar,
                });
                layers.push(layer(
                    "vocals over the previous song",
                    &stem_dirs[i],
                    VOCALS,
                    0,
                    hook.vocal_over_bars,
                    out_bar,
                    SEAM_VOCAL_DB,
                    0.5,
                    0.02,
                ));
            }
            let full_at = out_bar + hook.vocal_over_bars;
            layers.push(layer(
                "vocals",
                &stem_dirs[i],
                VOCALS,
                hook.vocal_over_bars,
                hook.full_bars,
                full_at,
                VOCAL_DB,
                0.02,
                0.02,
            ));
            layers.push(layer(
                "instrumental",
                &stem_dirs[i],
                NO_VOCALS,
                hook.vocal_over_bars,
                hook.full_bars,
                full_at,
                INSTRUMENTAL_DB,
                0.3,
                0.02,
            ));
        }
        out_bar += hook.vocal_over_bars + hook.full_bars;
    }
    layers
}

/// The ffmpeg filter graph that mixes `layers` into `[aout]`, `total_s` long.
/// `gains[i]` is the dB to apply to layer `i`.
pub fn filter_graph(layers: &[Layer], gains: &[f64], total_s: f64) -> String {
    let mut parts = Vec::new();
    let mut labels = String::new();
    for (i, (l, gain)) in layers.iter().zip(gains).enumerate() {
        let out_len = l.out_len();
        let fade_in = l.fade_in.min(out_len / 2.0);
        let fade_out = l.fade_out.min(out_len / 2.0);
        let delay_ms = (l.out_start * 1000.0).round() as i64;
        parts.push(format!(
            "[{i}:a]atrim=start={:.4}:end={:.4},asetpts=PTS-STARTPTS,aresample={SAMPLE_RATE},aformat=channel_layouts=stereo,\
             rubberband=tempo={:.6}:pitch={:.6},volume={gain:.3}dB,\
             afade=t=in:st=0:d={fade_in:.4},afade=t=out:st={:.4}:d={fade_out:.4},\
             adelay=delays={delay_ms}:all=1[a{i}]",
            l.native_start,
            l.native_start + l.native_len,
            l.tempo,
            l.pitch,
            out_len - fade_out,
        ));
        labels.push_str(&format!("[a{i}]"));
    }
    parts.push(format!(
        "{labels}amix=inputs={}:normalize=0:duration=longest,apad=whole_dur={total_s:.4},atrim=end={total_s:.4},alimiter=limit=0.95[aout]",
        layers.len()
    ));
    parts.join(";")
}

/// Mix the chapter's audio into `out_wav`.
pub fn mix(ffmpeg: &str, plan: &Plan, layers: &[Layer], out_wav: &Path) -> Result<(), RenderError> {
    let mut gains = Vec::with_capacity(layers.len());
    for layer in layers {
        let measured =
            tools::mean_volume_db(ffmpeg, &layer.file, layer.native_start, layer.native_len)
                .map_err(|e| RenderError::new(&layer.name, e))?;
        if measured < SILENT_DB {
            return Err(RenderError::new(
                &layer.name,
                format!(
                    "{} is silent at {:.1}s (mean {measured:.1} dB), so there is nothing to level",
                    layer.file.display(),
                    layer.native_start
                ),
            ));
        }
        gains.push(layer.target_db - measured);
    }
    let graph = filter_graph(layers, &gains, plan.duration_s());
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-y", "-v", "error"]);
    for layer in layers {
        cmd.arg("-i").arg(&layer.file);
    }
    cmd.args(["-filter_complex", &graph, "-map", "[aout]", "-ar"])
        .arg(SAMPLE_RATE.to_string())
        .args(["-ac", "2"])
        .arg(out_wav);
    tools::run(&mut cmd).map_err(|e| RenderError::new("audio mix", e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Cut, Hook, Plan};

    fn plan() -> Plan {
        let hook = |id: &str, bpm: f64, semis: i32, over: u32, full: u32| Hook {
            entry_id: id.into(),
            path: format!("/m/{id}.mp4").into(),
            landing_s: 30.0,
            native_bpm: bpm,
            semitones: semis,
            vocal_over_bars: over,
            full_bars: full,
        };
        Plan {
            bpm: 120.0,
            key: "A minor".into(),
            camelot: "8A".into(),
            bar_seconds: 2.0,
            hooks: vec![hook("a", 100.0, 0, 0, 4), hook("b", 120.0, -2, 2, 4)],
            broll: vec![],
            cuts: vec![Cut {
                entry_id: "a".into(),
                native_start_s: 0.0,
                out_start_s: 0.0,
                length_s: 20.0,
                ratio: 1.0,
            }],
            cards: vec![],
            output: "/o.mp4".into(),
        }
    }

    fn dirs() -> Vec<PathBuf> {
        vec!["/s/a".into(), "/s/b".into()]
    }

    #[test]
    fn the_first_hook_is_one_vocal_and_one_instrumental_layer() {
        let l = layers(&plan(), &dirs());
        assert_eq!(l[0].name, "hook 1 (a) vocals");
        assert_eq!(l[1].name, "hook 1 (a) instrumental");
        // 4 bars of a 100 bpm song, starting 4 native bars (9.6s) into its stem file.
        assert!((l[0].native_start - 9.6).abs() < 1e-9);
        assert!((l[0].native_len - 9.6).abs() < 1e-9);
        // Stretched to 120 bpm: 4 bars of 2s.
        assert!((l[0].out_len() - 8.0).abs() < 1e-9);
    }

    #[test]
    fn the_seam_carries_the_previous_instrumental_under_the_new_vocal() {
        let l = layers(&plan(), &dirs());
        let under = &l[2];
        assert_eq!(under.file, PathBuf::from("/s/a/no_vocals.wav"));
        // After a's 4 bars, 4 bars of region lead-in: 8 native bars of 2.4s.
        assert!((under.native_start - 8.0 * 2.4).abs() < 1e-9);
        assert!((under.out_start - 8.0).abs() < 1e-9);
        assert!((under.out_len() - 4.0).abs() < 1e-9);
        let vocal = &l[3];
        assert_eq!(vocal.file, PathBuf::from("/s/b/vocals.wav"));
        assert!((vocal.out_start - 8.0).abs() < 1e-9);
        assert!((vocal.out_len() - 4.0).abs() < 1e-9);
    }

    #[test]
    fn the_full_section_starts_after_the_seam_in_both_chapter_and_song() {
        let l = layers(&plan(), &dirs());
        let full_vocals = &l[4];
        assert_eq!(full_vocals.name, "hook 2 (b) vocals");
        assert!((full_vocals.out_start - 12.0).abs() < 1e-9);
        // b is at the chapter tempo: bars of 2s, 4 lead-in + 2 seam bars in.
        assert!((full_vocals.native_start - 6.0 * 2.0).abs() < 1e-9);
        assert!((full_vocals.out_len() - 8.0).abs() < 1e-9);
    }

    #[test]
    fn the_layers_cover_exactly_the_chapter() {
        let p = plan();
        let l = layers(&p, &dirs());
        let end = l
            .iter()
            .map(|x| x.out_start + x.out_len())
            .fold(0.0, f64::max);
        assert!((end - p.duration_s()).abs() < 1e-9, "{end}");
    }

    #[test]
    fn the_filter_graph_stretches_shifts_delays_and_mixes_every_layer() {
        let p = plan();
        let l = layers(&p, &dirs());
        let gains = vec![0.0; l.len()];
        let g = filter_graph(&l, &gains, p.duration_s());
        assert_eq!(g.matches("rubberband=").count(), l.len());
        assert!(g.contains("amix=inputs=6:normalize=0"));
        assert!(g.contains("adelay=delays=8000:all=1[a2]"));
        assert!(g.contains("apad=whole_dur=20.0000"));
        // b is two semitones down: 2^(-2/12).
        assert!(g.contains("pitch=0.890899"));
    }
}

//! The plan's picture: each cut rendered from its source at its ratio, then
//! the cuts joined and muxed with the mix.
//!
//! The title card is not drawn here. The channel's overlay draws it from the
//! plan's `cards`, so the rendered file stays clean of burned-in text.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::deps::Encoder;
use crate::error::RenderError;
use crate::plan::Plan;
use crate::settings::{FPS, HEIGHT, Settings, WIDTH};
use crate::tools;

/// Keyframe interval of a cut's intermediate file: every frame for a cut under
/// [`ALL_INTRA_UNDER_S`], so short cuts join exactly, else once a second.
const LONG_GOP: u32 = 30;
const ALL_INTRA_UNDER_S: f64 = 15.0;

/// One stretch of picture to render: `length_s` result seconds of `source` from
/// `native_start_s`, played at `ratio`, placed at `out_start_s`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CutSpec {
    /// What an error calls it: `cut 3 (a)`.
    pub layer: String,
    pub source: PathBuf,
    pub native_start_s: f64,
    pub out_start_s: f64,
    pub length_s: f64,
    pub ratio: f64,
}

/// Frames a span holds. Boundaries are rounded on the output's frame grid, so
/// the sum over a run of spans is exactly the run's frame count and no
/// per-span rounding accumulates into drift.
fn span_frames(out_start_s: f64, length_s: f64) -> u32 {
    let start = (out_start_s * f64::from(FPS)).round();
    let end = ((out_start_s + length_s) * f64::from(FPS)).round();
    (end - start).max(1.0) as u32
}

pub fn cut_frames(plan: &Plan) -> Vec<u32> {
    plan.cuts
        .iter()
        .map(|c| span_frames(c.out_start_s, c.out_len_s))
        .collect()
}

/// The filter chain that fits any source into the output frame.
fn fit_chain() -> String {
    format!(
        "fps={FPS},scale={WIDTH}:{HEIGHT}:force_original_aspect_ratio=decrease,\
         pad={WIDTH}:{HEIGHT}:(ow-iw)/2:(oh-ih)/2,setsar=1,format=yuv420p"
    )
}

/// Render every cut of the plan into `dir` and return the concat list's path.
pub fn render_cuts(plan: &Plan, settings: &Settings, dir: &Path) -> Result<PathBuf, RenderError> {
    let specs: Vec<CutSpec> = plan
        .cuts
        .iter()
        .enumerate()
        .map(|(i, cut)| CutSpec {
            layer: format!("cut {} ({})", i + 1, cut.source),
            source: plan
                .source_path(&cut.source)
                .expect("validated: every cut names a known source")
                .clone(),
            native_start_s: cut.src_start_s,
            out_start_s: cut.out_start_s,
            length_s: cut.out_len_s,
            ratio: cut.ratio,
        })
        .collect();
    render_specs(&specs, settings, dir)
}

/// Render `specs` in order into `dir`, one intermediate file each, and return
/// the concat list's path.
pub(crate) fn render_specs(
    specs: &[CutSpec],
    settings: &Settings,
    dir: &Path,
) -> Result<PathBuf, RenderError> {
    let mut list = String::new();
    for (i, spec) in specs.iter().enumerate() {
        let frames = span_frames(spec.out_start_s, spec.length_s);
        let gop = if spec.length_s < ALL_INTRA_UNDER_S {
            1
        } else {
            LONG_GOP
        };
        let segment = dir.join(format!("{i:04}.mp4"));
        // The segment holds the cut's source span stretched by `ratio`; `tpad`
        // holds the last frame if the source ends early, so every segment has
        // exactly its frame count and the output's length never depends on
        // how much picture a source had left.
        let vf = format!(
            "setpts=(PTS-STARTPTS)/{:.6},{},tpad=stop=-1:stop_mode=clone",
            spec.ratio,
            fit_chain()
        );
        tools::run(
            Command::new(&settings.ffmpeg)
                .args(["-y", "-v", "error", "-ss"])
                .arg(format!("{:.4}", spec.native_start_s))
                .arg("-t")
                .arg(format!("{:.4}", spec.length_s * spec.ratio + 0.5))
                .arg("-i")
                .arg(&spec.source)
                .args(["-an", "-vf", &vf, "-frames:v"])
                .arg(frames.to_string())
                .args([
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-crf",
                    "18",
                    "-g",
                ])
                .arg(gop.to_string())
                .arg(&segment),
        )
        .map_err(|e| RenderError::new(&spec.layer, e))?;
        list.push_str(&format!("file '{}'\n", segment.display()));
    }
    let list_path = dir.join("list.txt");
    fs::write(&list_path, list).map_err(|e| {
        RenderError::new(
            "video concat",
            format!("cannot write {}: {e}", list_path.display()),
        )
    })?;
    Ok(list_path)
}

/// Join the cuts, add the mixed audio, and write the first `duration_s` seconds to `out`.
pub fn mux(
    duration_s: f64,
    settings: &Settings,
    encoder: &Encoder,
    list: &Path,
    audio: &Path,
    out: &Path,
) -> Result<(), RenderError> {
    let mut cmd = Command::new(&settings.ffmpeg);
    cmd.args(["-y", "-v", "error"]);
    cmd.args(&encoder.input_args);
    cmd.args(["-f", "concat", "-safe", "0", "-i"]).arg(list);
    cmd.arg("-i").arg(audio);
    cmd.args(["-map", "0:v", "-map", "1:a"]);
    let vf = format!("fps={FPS}{}", encoder.filter_suffix);
    cmd.args(["-vf", &vf]);
    cmd.args(&encoder.codec_args);
    cmd.args(["-c:a", "aac", "-b:a", "192k", "-t"])
        .arg(format!("{duration_s:.4}"))
        .args(["-movflags", "+faststart", "-f", "mp4"])
        .arg(out);
    tools::run(&mut cmd)
        .map_err(|e| RenderError::new(format!("video mux ({})", encoder.name), e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Cut;
    use crate::plan::tests::sample;

    #[test]
    fn cut_frames_sum_to_the_plans_frame_count_with_no_drift() {
        // Thirds of a second round badly one at a time: boundaries, not
        // lengths, are rounded, so 24 thirds are exactly 240 frames at 30 fps.
        let mut plan = sample();
        plan.cuts = (0..24)
            .map(|i| Cut {
                source: "a".into(),
                src_start_s: 0.0,
                out_start_s: f64::from(i) / 3.0,
                out_len_s: 1.0 / 3.0,
                ratio: 1.0,
            })
            .collect();
        let total: u32 = cut_frames(&plan).iter().sum();
        assert_eq!(total, 240);
    }
}

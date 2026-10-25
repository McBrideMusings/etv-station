//! The chapter's picture: each cut rendered from its source at its ratio, then
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

/// Frames each cut holds. Boundaries are rounded on the chapter's frame grid,
/// so the sum is exactly the chapter's frame count and no per-cut rounding
/// accumulates into drift.
pub fn cut_frames(plan: &Plan) -> Vec<u32> {
    plan.cuts
        .iter()
        .map(|c| {
            let start = (c.out_start_s * f64::from(FPS)).round();
            let end = ((c.out_start_s + c.length_s) * f64::from(FPS)).round();
            (end - start).max(1.0) as u32
        })
        .collect()
}

/// The filter chain that fits any source into the output frame.
fn fit_chain() -> String {
    format!(
        "fps={FPS},scale={WIDTH}:{HEIGHT}:force_original_aspect_ratio=decrease,\
         pad={WIDTH}:{HEIGHT}:(ow-iw)/2:(oh-ih)/2,setsar=1,format=yuv420p"
    )
}

/// Render every cut into `dir` and return the concat list's path.
pub fn render_cuts(plan: &Plan, settings: &Settings, dir: &Path) -> Result<PathBuf, RenderError> {
    let frames = cut_frames(plan);
    let mut list = String::new();
    for (i, (cut, frames)) in plan.cuts.iter().zip(&frames).enumerate() {
        let layer = format!("cut {} ({})", i + 1, cut.entry_id);
        let source = plan
            .source_path(&cut.entry_id)
            .expect("validated: every cut names a known source");
        let segment = dir.join(format!("{i:04}.mp4"));
        // The segment holds the cut's source span stretched by `ratio`; `tpad`
        // holds the last frame if the source ends early, so every segment has
        // exactly its frame count and the chapter's length never depends on
        // how much picture a song had left.
        let vf = format!(
            "setpts=(PTS-STARTPTS)/{:.6},{},tpad=stop=-1:stop_mode=clone",
            cut.ratio,
            fit_chain()
        );
        tools::run(
            Command::new(&settings.ffmpeg)
                .args(["-y", "-v", "error", "-ss"])
                .arg(format!("{:.4}", cut.native_start_s))
                .arg("-t")
                .arg(format!("{:.4}", cut.length_s * cut.ratio + 0.5))
                .arg("-i")
                .arg(source)
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
                    "1",
                ])
                .arg(&segment),
        )
        .map_err(|e| RenderError::new(&layer, e))?;
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

/// Join the cuts, add the mixed audio, and write the chapter to `out`.
pub fn mux(
    plan: &Plan,
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
        .arg(format!("{:.4}", plan.duration_s()))
        .args(["-movflags", "+faststart", "-f", "mp4"])
        .arg(out);
    tools::run(&mut cmd)
        .map_err(|e| RenderError::new(format!("video mux ({})", encoder.name), e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Cut, Hook};

    fn plan_with_cuts(cuts: &[(f64, f64)]) -> Plan {
        Plan {
            bpm: 120.0,
            key: "A".into(),
            camelot: "8A".into(),
            bar_seconds: 2.0,
            hooks: vec![Hook {
                entry_id: "a".into(),
                path: "/a.mp4".into(),
                landing_s: 20.0,
                native_bpm: 120.0,
                semitones: 0,
                vocal_over_bars: 0,
                full_bars: 4,
            }],
            broll: vec![],
            cuts: cuts
                .iter()
                .map(|&(s, l)| Cut {
                    entry_id: "a".into(),
                    native_start_s: 0.0,
                    out_start_s: s,
                    length_s: l,
                    ratio: 1.0,
                })
                .collect(),
            cards: vec![],
            output: "/o.mp4".into(),
        }
    }

    #[test]
    fn cut_frames_sum_to_the_chapter_frame_count_with_no_drift() {
        // Thirds of a second round badly one at a time: 10 frames each of
        // 0.3333s would be 10 x 10 = 100, but 3.3333s is 100 frames at 30 fps
        // only if boundaries, not lengths, are rounded.
        let cuts: Vec<(f64, f64)> = (0..24)
            .map(|i| (f64::from(i) * 1.0 / 3.0, 1.0 / 3.0))
            .collect();
        let plan = plan_with_cuts(&cuts);
        let total: u32 = cut_frames(&plan).iter().sum();
        assert_eq!(total, 240);
    }
}

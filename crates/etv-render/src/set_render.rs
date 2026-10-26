//! Render a set plan: the audio layers mixed, the picture cut and joined, the
//! two muxed into the plan's output.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use crate::deps;
use crate::error::RenderError;
use crate::render::{Rendered, Timings, make_work_dir, require_tools};
use crate::set::{self, Layout, SetPlan};
use crate::settings::Settings;
use crate::tools;
use crate::video::{self, CutSpec};

const SAMPLE_RATE: u32 = 44_100;
/// A source may fall this short of what a layer reads before it is an error.
const SOURCE_SLACK_S: f64 = 0.25;
/// Keyframe interval of the intermediate picture files: a song's body is minutes
/// long, so they are not all-intra the way a chapter's short cuts are.
const SEGMENT_GOP: u32 = 30;

/// Render `plan`. The output appears only when complete.
pub fn render(plan: &SetPlan, settings: &Settings) -> Result<Rendered, RenderError> {
    let started = Instant::now();
    plan.validate()
        .map_err(|e| RenderError::new("plan", e.to_string()))?;
    let layout = plan.layout();
    check_sources(plan, &layout, settings)?;
    let encoder = require_tools(settings)?;
    tracing::info!(
        event = "render.start",
        kind = "set",
        output = %plan.output.display(),
        tracks = plan.tracks.len(),
        seconds = layout.duration_s,
        audio_layers = layout.audio.len(),
        picture_segments = layout.video.len(),
        encoder = encoder.name,
        "rendering a set"
    );
    let work = make_work_dir(settings, started)?;
    let result = render_in(plan, &layout, settings, &encoder, &work, started);
    let _ = fs::remove_dir_all(&work);
    result
}

fn render_in(
    plan: &SetPlan,
    layout: &Layout,
    settings: &Settings,
    encoder: &deps::Encoder,
    work: &Path,
    started: Instant,
) -> Result<Rendered, RenderError> {
    let mut timings = Timings::default();

    let t = Instant::now();
    let mix = work.join("set.wav");
    mix_audio(plan, layout, settings, &mix)?;
    timings.audio_s = t.elapsed().as_secs_f64();

    let t = Instant::now();
    let specs: Vec<CutSpec> = layout
        .video
        .iter()
        .enumerate()
        .map(|(i, v)| CutSpec {
            layer: format!(
                "picture {} ({}, {})",
                i + 1,
                plan.tracks[v.track].entry_id,
                v.name
            ),
            source: plan.track_path(v.track).clone(),
            native_start_s: v.src_start,
            out_start_s: v.out_start,
            length_s: v.out_len,
            ratio: v.ratio,
        })
        .collect();
    let list = video::render_specs(&specs, SEGMENT_GOP, settings, work)?;
    if let Some(parent) = plan.output.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            RenderError::new("output", format!("cannot create {}: {e}", parent.display()))
        })?;
    }
    let part = plan.output.with_extension("part");
    if let Err(e) = video::mux(layout.duration_s, settings, encoder, &list, &mix, &part) {
        let _ = fs::remove_file(&part);
        return Err(e);
    }
    fs::rename(&part, &plan.output).map_err(|e| {
        let _ = fs::remove_file(&part);
        RenderError::new(
            "output",
            format!("cannot move the render to {}: {e}", plan.output.display()),
        )
    })?;
    timings.video_s = t.elapsed().as_secs_f64();
    timings.total_s = started.elapsed().as_secs_f64();
    tracing::info!(
        event = "render.done",
        kind = "set",
        output = %plan.output.display(),
        audio_s = timings.audio_s,
        video_s = timings.video_s,
        total_s = timings.total_s,
        "set rendered"
    );
    Ok(Rendered {
        timings,
        stems: Vec::new(),
    })
}

/// Every song a layer reads must exist and reach as far as the layer does, so
/// the failure names the song instead of an ffmpeg exit code or a silent tail.
fn check_sources(plan: &SetPlan, layout: &Layout, settings: &Settings) -> Result<(), RenderError> {
    let mut needed: BTreeMap<usize, f64> = BTreeMap::new();
    for layer in &layout.audio {
        let end = layer.src_start + layer.src_len();
        let slot = needed.entry(layer.track).or_insert(0.0);
        *slot = slot.max(end);
    }
    for (i, track) in plan.tracks.iter().enumerate() {
        let layer = format!("track {} ({})", i + 1, track.entry_id);
        if !track.path.is_file() {
            return Err(RenderError::new(
                layer,
                format!("media file not found: {}", track.path.display()),
            ));
        }
        let have = tools::duration_s(&settings.ffprobe, &track.path)
            .map_err(|e| RenderError::new(&layer, e))?;
        let want = needed.get(&i).copied().unwrap_or(0.0);
        if have + SOURCE_SLACK_S < want {
            return Err(RenderError::new(
                layer,
                format!(
                    "{} lasts {have:.2}s but the set plays it to {want:.2}s: the song ends inside its seam",
                    track.path.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Mix the set's audio into `out_wav`. Each layer is its own input, read from
/// its start position for its length, so no song is decoded from the top more
/// than the part the set plays.
fn mix_audio(
    plan: &SetPlan,
    layout: &Layout,
    settings: &Settings,
    out_wav: &Path,
) -> Result<(), RenderError> {
    let graph = set::filter_graph(layout, SAMPLE_RATE);
    let mut cmd = Command::new(&settings.ffmpeg);
    cmd.args(["-y", "-v", "error"]);
    for layer in &layout.audio {
        cmd.arg("-ss")
            .arg(format!("{:.4}", layer.src_start))
            .arg("-t")
            .arg(format!("{:.4}", layer.src_len()))
            .arg("-i")
            .arg::<&PathBuf>(plan.track_path(layer.track));
    }
    cmd.args(["-filter_complex", &graph, "-map", "[aout]", "-ar"])
        .arg(SAMPLE_RATE.to_string())
        .args(["-ac", "2"])
        .arg(out_wav);
    tools::run(&mut cmd).map_err(|e| RenderError::new("audio mix", e))?;
    Ok(())
}

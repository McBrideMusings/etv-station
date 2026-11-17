//! Render one plan to its output file, in the foreground.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::audio;
use crate::deps;
use crate::error::RenderError;
use crate::plan::Plan;
use crate::settings::Settings;
use crate::stems;
use crate::video;

/// Where a render spent its time, in seconds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Timings {
    pub stems_s: f64,
    pub audio_s: f64,
    pub video_s: f64,
    pub total_s: f64,
}

/// What a render produced besides the file.
#[derive(Debug, Clone)]
pub struct Rendered {
    pub timings: Timings,
    /// Each separated region's directory, relative to the cache's `stems` folder.
    pub stems: Vec<String>,
}

/// Render `plan`. The output appears only when complete: it is written beside
/// its final name and renamed into place, so a reader never sees half a file.
pub fn render(plan: &Plan, settings: &Settings) -> Result<Rendered, RenderError> {
    let started = Instant::now();
    plan.validate()
        .map_err(|e| RenderError::new("plan", e.to_string()))?;
    check_sources(plan)?;
    let encoder = require_tools(settings)?;
    tracing::info!(
        event = "render.start",
        output = %plan.output.display(),
        seconds = plan.duration_s,
        sources = plan.sources.len(),
        layers = plan.layers.len(),
        cuts = plan.cuts.len(),
        encoder = encoder.name,
        "rendering a plan"
    );

    let work = make_work_dir(settings, started)?;
    let result = render_in(plan, settings, &encoder, &work, started);
    let _ = fs::remove_dir_all(&work);
    result
}

fn render_in(
    plan: &Plan,
    settings: &Settings,
    encoder: &deps::Encoder,
    work: &Path,
    started: Instant,
) -> Result<Rendered, RenderError> {
    let mut timings = Timings::default();

    let t = Instant::now();
    let mut stem_dirs = BTreeMap::new();
    for region in stems::regions(plan) {
        let layer = format!("stems of {}", region.source);
        let dir = stems::ensure(settings, &region, &layer)?;
        stem_dirs.insert(region.source.clone(), (region, dir));
    }
    timings.stems_s = t.elapsed().as_secs_f64();

    let t = Instant::now();
    let inputs = audio::inputs(plan, &stem_dirs);
    require_reach(plan, &inputs, settings)?;
    let mix = work.join("mix.wav");
    audio::mix(&settings.ffmpeg, plan, &inputs, &mix)?;
    timings.audio_s = t.elapsed().as_secs_f64();

    let t = Instant::now();
    let list = video::render_cuts(plan, settings, work)?;
    if let Some(parent) = plan.output.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            RenderError::new("output", format!("cannot create {}: {e}", parent.display()))
        })?;
    }
    let part = plan.output.with_extension("part");
    let muxed = video::mux(plan.duration_s, settings, encoder, &list, &mix, &part);
    if let Err(e) = muxed {
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
        output = %plan.output.display(),
        stems_s = timings.stems_s,
        audio_s = timings.audio_s,
        video_s = timings.video_s,
        total_s = timings.total_s,
        "plan rendered"
    );
    let stems = stem_dirs
        .values()
        .filter_map(|(_, d)| d.strip_prefix(settings.cache_dir.join("stems")).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    Ok(Rendered { timings, stems })
}

/// The encoder to finish with, after checking that this ffmpeg can render at
/// all: it needs the `rubberband` filter to stretch audio.
fn require_tools(settings: &Settings) -> Result<deps::Encoder, RenderError> {
    let filters = deps::ffmpeg_filters_text(settings)
        .map_err(|e| RenderError::new("dependency ffmpeg", e))?;
    if !deps::has_rubberband_filter(&filters) {
        return Err(RenderError::new(
            "dependency librubberband",
            format!(
                "librubberband is missing: {} was built without the rubberband filter",
                settings.ffmpeg
            ),
        ));
    }
    let encoders =
        deps::ffmpeg_encoders(settings).map_err(|e| RenderError::new("dependency ffmpeg", e))?;
    Ok(deps::pick_encoder(settings, &encoders))
}

/// A scratch directory under the cache for one render; the caller removes it.
fn make_work_dir(settings: &Settings, started: Instant) -> Result<std::path::PathBuf, RenderError> {
    let work = settings.cache_dir.join("work").join(format!(
        "{}-{}",
        std::process::id(),
        started.elapsed().as_nanos()
    ));
    fs::create_dir_all(&work).map_err(|e| {
        RenderError::new("work dir", format!("cannot create {}: {e}", work.display()))
    })?;
    Ok(work)
}

/// Every source the plan names must exist before any work starts, so the
/// failure names the file instead of an ffmpeg exit code.
fn check_sources(plan: &Plan) -> Result<(), RenderError> {
    for source in &plan.sources {
        if !source.path.is_file() {
            return Err(RenderError::new(
                format!("source {}", source.id),
                format!("media file not found: {}", source.path.display()),
            ));
        }
    }
    Ok(())
}

/// A source may fall this short of what a layer reads before it is an error.
const SOURCE_SLACK_S: f64 = 0.25;

/// Every layer's input must reach as far as the layer reads: a source that ends
/// inside a layer leaves a silent tail the mix would otherwise paper over.
fn require_reach(
    plan: &Plan,
    inputs: &[audio::Input],
    settings: &crate::settings::Settings,
) -> Result<(), RenderError> {
    let mut lengths: BTreeMap<&Path, f64> = BTreeMap::new();
    for (l, input) in plan.layers.iter().zip(inputs) {
        let have = match lengths.get(input.file.as_path()) {
            Some(d) => *d,
            None => {
                let d = crate::tools::duration_s(&settings.ffprobe, &input.file)
                    .map_err(|e| RenderError::new(&l.name, e))?;
                lengths.insert(input.file.as_path(), d);
                d
            }
        };
        let want = input.start_s + l.src_len_s();
        if have + SOURCE_SLACK_S < want {
            return Err(RenderError::new(
                &l.name,
                format!(
                    "{} holds {have:.2}s but the layer reads to {want:.2}s: the source ends inside the layer",
                    input.file.display()
                ),
            ));
        }
    }
    Ok(())
}

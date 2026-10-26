//! Render one chapter plan to its output file, in the foreground.

use std::fs;
use std::path::Path;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::audio;
use crate::deps;
use crate::document::Document;
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
    /// Each hook's stem directory, relative to the cache's `stems` folder.
    pub stems: Vec<String>,
}

/// Render `plan`. The output appears only when complete: it is written beside
/// its final name and renamed into place, so a reader never sees half a file.
pub fn render(plan: &Plan, settings: &Settings) -> Result<Rendered, RenderError> {
    let started = Instant::now();
    plan.validate()
        .map_err(|e| RenderError::new("plan", e.to_string()))?;
    check_files(plan)?;
    let encoder = require_tools(settings)?;
    tracing::info!(
        event = "render.start",
        output = %plan.output.display(),
        bars = plan.total_bars(),
        hooks = plan.hooks.len(),
        cuts = plan.cuts.len(),
        encoder = encoder.name,
        "rendering a chapter"
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
    let mut stem_dirs = Vec::new();
    for (i, hook) in plan.hooks.iter().enumerate() {
        let layer = format!("hook {} ({}) stems", i + 1, hook.entry_id);
        let dir = stems::ensure(settings, hook, &layer)?;
        require_region(&dir, plan, i, &layer, settings)?;
        stem_dirs.push(dir);
    }
    timings.stems_s = t.elapsed().as_secs_f64();

    let t = Instant::now();
    let layers = audio::layers(plan, &stem_dirs);
    let mix = work.join("chapter.wav");
    audio::mix(&settings.ffmpeg, plan, &layers, &mix)?;
    timings.audio_s = t.elapsed().as_secs_f64();

    let t = Instant::now();
    let list = video::render_cuts(plan, settings, work)?;
    if let Some(parent) = plan.output.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            RenderError::new("output", format!("cannot create {}: {e}", parent.display()))
        })?;
    }
    let part = plan.output.with_extension("part");
    let muxed = video::mux(plan.duration_s(), settings, encoder, &list, &mix, &part);
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
        "chapter rendered"
    );
    let stems = stem_dirs
        .iter()
        .filter_map(|d| d.strip_prefix(settings.cache_dir.join("stems")).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    Ok(Rendered { timings, stems })
}

/// The encoder to finish with, after checking that this ffmpeg can render at
/// all: it needs the `rubberband` filter to stretch audio.
pub(crate) fn require_tools(settings: &Settings) -> Result<deps::Encoder, RenderError> {
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
pub(crate) fn make_work_dir(
    settings: &Settings,
    started: Instant,
) -> Result<std::path::PathBuf, RenderError> {
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

/// Render any plan file's document in the foreground.
pub fn render_document(doc: &Document, settings: &Settings) -> Result<Rendered, RenderError> {
    match doc {
        Document::Chapter(plan) => render(plan, settings),
        Document::Set(plan) => crate::set_render::render(plan, settings),
    }
}

/// Every media file the plan names must exist before any work starts, so the
/// failure names the file instead of an ffmpeg exit code.
fn check_files(plan: &Plan) -> Result<(), RenderError> {
    for (i, hook) in plan.hooks.iter().enumerate() {
        if !hook.path.is_file() {
            return Err(RenderError::new(
                format!("hook {} ({})", i + 1, hook.entry_id),
                format!("media file not found: {}", hook.path.display()),
            ));
        }
    }
    for source in &plan.broll {
        if !source.path.is_file() {
            return Err(RenderError::new(
                format!("broll ({})", source.entry_id),
                format!("media file not found: {}", source.path.display()),
            ));
        }
    }
    Ok(())
}

/// The stems must reach as far as the hook's bars do; a song that ends inside
/// its hook leaves a silent tail the mix would otherwise paper over.
fn require_region(
    dir: &Path,
    plan: &Plan,
    index: usize,
    layer: &str,
    settings: &Settings,
) -> Result<(), RenderError> {
    let hook = &plan.hooks[index];
    let next_over = plan.hooks.get(index + 1).map_or(0, |h| h.vocal_over_bars);
    let bars = f64::from(hook.vocal_over_bars + hook.full_bars + next_over);
    let needed = (crate::plan::REGION_BARS_BEFORE + bars) * (240.0 / hook.native_bpm);
    for name in [stems::VOCALS, stems::NO_VOCALS] {
        let have = crate::tools::duration_s(&settings.ffprobe, &dir.join(name))
            .map_err(|e| RenderError::new(layer, e))?;
        if have + 0.05 < needed {
            return Err(RenderError::new(
                layer,
                format!(
                    "{} holds {have:.2}s but the hook needs {needed:.2}s: the song ends inside the hook",
                    dir.join(name).display()
                ),
            ));
        }
    }
    Ok(())
}

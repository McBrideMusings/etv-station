//! Channels › Graphics: a channel's overlay and bumper scripts rendered in the
//! page, so previewing one needs no checkout, cargo or VLC.
//!
//! `GET /api/channels/{ch}/graphics` lists the overlay scripts the channel's
//! resolved overlay names and the bumper scripts its `fill:` entries name.
//! `POST /api/channels/{ch}/graphics/render` queues one render and answers its
//! id; `GET /api/renders/{render}` answers 202 while it runs, then the result
//! (`url`, size, duration, the binaries' stderr), and `GET
//! /api/renders/{render}/file` serves the PNG or mp4.
//!
//! The render is the one the running station draws with: `etv-overlay
//! render-still` for a still, `etv-overlay run` over footage cut from an
//! airing for a clip, and `etv-overlay watch --item` over each item
//! `bumper::preview` emits for a bumper. Renders run one at a time on their
//! own worker thread, so no request waits on one. A render's id is a digest of
//! what decides it (the request, the files it reads and their mtimes, the
//! files beside each script, the renderer and ffmpeg binaries), so a saved script gets a new id and re-renders, and an
//! unchanged one is served from the renders directory.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Seek, SeekFrom};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use etv_overlay::OverlaySpec;
use sha2::{Digest, Sha256};

use super::lineup::lineup_keys;
use super::*;

/// The gap a bumper renders onto when the request names none.
const DEFAULT_GAP: &str = "45s";
/// The longest gap a bumper render accepts; its `watch` plays in real time.
const MAX_GAP_MS: u64 = 600_000;
/// Clip length when the request names none, and the longest it accepts.
const DEFAULT_CLIP_SECS: f64 = 6.0;
const MAX_CLIP_SECS: f64 = 20.0;
/// Longest a still may be asked for into the overlay's own clock.
const MAX_STILL_AT_SECS: f64 = 3600.0;
/// Seconds into the footage a clip's cut starts, when the airing is long enough.
const FOOTAGE_OFFSET_SECS: f64 = 60.0;
/// Most of a binary's stderr kept in a render's log.
const LOG_TAIL_BYTES: usize = 8192;
/// Past this a step is killed: a stale network mount blocks ffmpeg for good.
const STEP_SLACK: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Still,
    Clip,
    Bumper,
}

impl Kind {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "still" => Some(Self::Still),
            "clip" => Some(Self::Clip),
            "bumper" => Some(Self::Bumper),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Still => "still",
            Self::Clip => "clip",
            Self::Bumper => "bumper",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Still => "png",
            Self::Clip | Self::Bumper => "mp4",
        }
    }
}

/// Footage a clip is cut from.
struct Footage {
    path: PathBuf,
    offset_secs: f64,
}

/// What a bumper render needs beyond the overlay spec.
struct BumperJob {
    /// The fill entry's `config:`, handed to the script as `ctx.config`.
    config: Option<Value>,
    channel_name: String,
    channel_number: i64,
    overlay_root: PathBuf,
}

/// One queued render, owned so the worker thread needs nothing from the server.
struct Job {
    id: String,
    kind: Kind,
    channel: String,
    script: PathBuf,
    spec_yaml: String,
    size: (u32, u32),
    fps: u32,
    at_secs: f64,
    clip_secs: f64,
    gap: String,
    footage: Option<Footage>,
    bumper: Option<BumperJob>,
    fonts: Vec<PathBuf>,
    /// Every file the render reads, with its mtime, as the log and the id name them.
    inputs: Vec<(PathBuf, Option<u128>)>,
}

enum JobState {
    Pending(Instant),
    Failed { error: String, log: String },
}

/// The render worker and what it has not finished or failed. A finished render
/// lives on disk (`{id}.{ext}` and `{id}.json`), so it survives a restart.
pub(super) struct RenderQueue {
    pub(super) dir: PathBuf,
    jobs: Arc<Mutex<HashMap<String, JobState>>>,
    tx: mpsc::Sender<Job>,
}

impl RenderQueue {
    pub(super) fn new(dir: PathBuf) -> Self {
        let jobs: Arc<Mutex<HashMap<String, JobState>>> = Arc::default();
        let (tx, rx) = mpsc::channel::<Job>();
        let worker_jobs = Arc::clone(&jobs);
        let worker_dir = dir.clone();
        std::thread::Builder::new()
            .name("ui-renders".into())
            .spawn(move || {
                for job in rx {
                    let id = job.id.clone();
                    let began = Instant::now();
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        render(&worker_dir, &job)
                    }))
                    .unwrap_or_else(|_| Err(("the render thread panicked".into(), String::new())));
                    let ms = began.elapsed().as_millis() as u64;
                    let inputs = inputs_text(&job.inputs);
                    let mut jobs = worker_jobs.lock().unwrap_or_else(|p| p.into_inner());
                    match outcome {
                        Ok(()) => {
                            jobs.remove(&id);
                            tracing::info!(
                                event = "ui.render",
                                render = %id,
                                kind = job.kind.name(),
                                channel = %job.channel,
                                script = %job.script.display(),
                                inputs = %inputs,
                                cache = "miss",
                                ms,
                                outcome = "ok",
                                "render finished"
                            );
                        }
                        Err((error, log)) => {
                            tracing::warn!(
                                event = "ui.render",
                                render = %id,
                                kind = job.kind.name(),
                                channel = %job.channel,
                                script = %job.script.display(),
                                inputs = %inputs,
                                cache = "miss",
                                ms,
                                outcome = "failed",
                                error = %error,
                                stderr = %log,
                                "render failed"
                            );
                            jobs.insert(id, JobState::Failed { error, log });
                        }
                    }
                }
            })
            .expect("spawn the render thread");
        Self { dir, jobs, tx }
    }
}

fn inputs_text(inputs: &[(PathBuf, Option<u128>)]) -> String {
    inputs
        .iter()
        .map(|(p, m)| match m {
            Some(ns) => format!("{}@{ns}", p.display()),
            None => format!("{}@missing", p.display()),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn mtime_ns(path: &Path) -> Option<u128> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
}

/// Most files one render's id covers beyond the ones it names, and the deepest
/// directory it descends: a script's imports and images are found by the
/// script at run time, so every file beside it counts as read.
const TREE_MAX_FILES: usize = 500;
const TREE_MAX_DEPTH: usize = 4;

/// Every regular file under `dir`, appended to `out` (bounded).
fn collect_tree(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if out.len() >= TREE_MAX_FILES {
            return;
        }
        if path.is_dir() {
            if depth < TREE_MAX_DEPTH {
                collect_tree(&path, depth + 1, out);
            }
        } else if path.is_file() {
            out.push(path);
        }
    }
}

/// The `ffmpeg` the render steps spawn, resolved on `PATH`, so a new build is a new render.
fn ffmpeg_on_path() -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join("ffmpeg"))
        .find(|p| p.is_file())
}

/// A channel looked up in the lineup.
pub(super) enum Lookup {
    Loaded(Box<config::Station>, usize),
    Missing(String),
    Dropped {
        config_path: PathBuf,
        reason: String,
    },
}

pub(super) fn find_channel(state: &AppState, key: &str) -> Result<Lookup, String> {
    let path = &state.options.station;
    let station =
        config::load_for_inspection(path).map_err(|e| format!("load {}: {e}", path.display()))?;
    let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let keys = lineup_keys(&station.lineup, &base);
    let Some(position) = keys.iter().position(|k| k == key) else {
        return Ok(Lookup::Missing(format!(
            "no channel {key:?} in {}",
            path.display()
        )));
    };
    let config_path = station.lineup[position].clone();
    if let Some(dropped) = station
        .dropped
        .iter()
        .find(|d| d.config_path == config_path)
    {
        return Ok(Lookup::Dropped {
            config_path,
            reason: dropped.reason.to_string(),
        });
    }
    let index = station
        .channels
        .iter()
        .position(|c| c.config_path == config_path)
        .ok_or_else(|| format!("{key} is in the lineup but neither loaded nor dropped"))?;
    Ok(Lookup::Loaded(Box::new(station), index))
}

/// An overlay script the channel's resolved overlay names.
struct OverlayScript {
    script: PathBuf,
    name: Option<String>,
}

fn overlay_scripts(lc: &config::LoadedChannel) -> Vec<OverlayScript> {
    let mut out: Vec<OverlayScript> = Vec::new();
    let Some(base) = &lc.overlays.base else {
        return out;
    };
    for g in &base.graphics {
        if let Some(script) = &g.script
            && !out.iter().any(|o| o.script == *script)
        {
            out.push(OverlayScript {
                script: script.clone(),
                name: g.name.clone(),
            });
        }
    }
    out
}

/// The bumper scripts the channel's `fill:` entries name, as absolute paths,
/// each with the first entry's `config:`.
fn bumper_scripts(lc: &config::LoadedChannel) -> Vec<(PathBuf, config::FillEntry)> {
    let dir = lc.config_path.parent().unwrap_or(Path::new("."));
    let mut out: Vec<(PathBuf, config::FillEntry)> = Vec::new();
    for block in lc.config.rule.all_blocks() {
        for entry in &block.fill {
            let script = dir.join(&entry.script);
            if !out.iter().any(|(p, _)| *p == script) {
                out.push((script, entry.clone()));
            }
        }
    }
    out
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// `GET /api/channels/{ch}/graphics`: the scripts this channel can render.
pub(super) fn api_channel_graphics(state: &AppState, key: &str) -> Result<(u16, Value), String> {
    match find_channel(state, key)? {
        Lookup::Missing(why) => Ok((404, json!({ "error": why }))),
        Lookup::Dropped {
            config_path,
            reason,
        } => Ok((
            200,
            json!({
                "channel": key,
                "config_path": config_path.display().to_string(),
                "load_error": reason,
            }),
        )),
        Lookup::Loaded(station, index) => {
            let lc = &station.channels[index];
            let overlays: Vec<Value> = overlay_scripts(lc)
                .iter()
                .map(|o| {
                    json!({
                        "script": o.script.display().to_string(),
                        "name": file_name(&o.script),
                        "graphic": o.name,
                    })
                })
                .collect();
            let bumpers: Vec<Value> = bumper_scripts(lc)
                .iter()
                .map(|(script, entry)| {
                    json!({
                        "script": script.display().to_string(),
                        "name": file_name(script),
                        "min_gap": entry.min_gap.map(|d| humantime::format_duration(d).to_string()),
                        "max_gap": entry.max_gap.map(|d| humantime::format_duration(d).to_string()),
                    })
                })
                .collect();
            let size = lc
                .overlays
                .base
                .as_ref()
                .map(|s| json!({ "width": s.width, "height": s.height }));
            tracing::info!(
                event = "ui.graphics",
                channel = key,
                overlays = overlays.len(),
                bumpers = bumpers.len(),
                "graphics listed"
            );
            Ok((
                200,
                json!({
                    "channel": key,
                    "overlays": overlays,
                    "bumpers": bumpers,
                    "canvas": size,
                    "default_gap": DEFAULT_GAP,
                }),
            ))
        }
    }
}

/// `POST /api/channels/{ch}/graphics/render`: queue one render, or name the
/// finished one an identical request already made.
pub(super) fn api_graphics_render(
    state: &mut AppState,
    key: &str,
    body: &str,
) -> Result<(u16, Value), String> {
    let req: Value = serde_json::from_str(body).map_err(|e| format!("body is not JSON: {e}"))?;
    let kind = req
        .get("kind")
        .and_then(Value::as_str)
        .and_then(Kind::parse)
        .ok_or("kind must be one of still, clip, bumper")?;
    let script = PathBuf::from(
        req.get("script")
            .and_then(Value::as_str)
            .ok_or("script is required")?,
    );
    let at_secs = req.get("at").and_then(Value::as_f64).unwrap_or(0.0);
    if !(0.0..=MAX_STILL_AT_SECS).contains(&at_secs) {
        return Err(format!("at must be 0 to {MAX_STILL_AT_SECS} seconds"));
    }
    let clip_secs = req
        .get("secs")
        .and_then(Value::as_f64)
        .unwrap_or(DEFAULT_CLIP_SECS);
    if !(1.0..=MAX_CLIP_SECS).contains(&clip_secs) {
        return Err(format!("secs must be 1 to {MAX_CLIP_SECS} seconds"));
    }
    let gap = req
        .get("gap")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_GAP)
        .to_string();
    let gap_ms = crate::bumper::preview::parse_gap(&gap)?;
    if gap_ms > MAX_GAP_MS {
        return Err(format!("gap {gap:?} is over {} seconds", MAX_GAP_MS / 1000));
    }

    let (station, index) = match find_channel(state, key)? {
        Lookup::Missing(why) => return Ok((404, json!({ "error": why }))),
        Lookup::Dropped { reason, .. } => {
            return Err(format!("channel {key} fails to load: {reason}"));
        }
        Lookup::Loaded(station, index) => (station, index),
    };
    let lc = &station.channels[index];

    let mut spec = lc.overlays.base.clone();
    let mut bumper = None;
    let mut script_inputs = vec![script.clone()];
    match kind {
        Kind::Still | Kind::Clip => {
            if !overlay_scripts(lc).iter().any(|o| o.script == script) {
                return Err(format!(
                    "{} is not an overlay script of {key}; GET /api/channels/{key}/graphics lists them",
                    script.display()
                ));
            }
            // The chosen script's graphic, and the static ones (a logo) that draw beside it.
            if let Some(spec) = spec.as_mut() {
                spec.graphics
                    .retain(|g| g.script.is_none() || g.script.as_ref() == Some(&script));
            }
            script_inputs.extend(
                lc.overlays
                    .base
                    .iter()
                    .flat_map(|s| s.graphics.iter())
                    .filter_map(|g| g.script.clone()),
            );
        }
        Kind::Bumper => {
            let Some((_, entry)) = bumper_scripts(lc).into_iter().find(|(p, _)| *p == script)
            else {
                return Err(format!(
                    "{} is not a bumper script of {key}; GET /api/channels/{key}/graphics lists them",
                    script.display()
                ));
            };
            bumper = Some(BumperJob {
                config: entry.config.clone(),
                channel_name: lc.name.clone(),
                channel_number: lc.config.number,
                overlay_root: lc
                    .config_path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_default(),
            });
        }
    }
    let spec = spec.unwrap_or_else(|| {
        let video = &station.station.normalization.video;
        OverlaySpec {
            width: video.width.unwrap_or(1280),
            height: video.height.unwrap_or(720),
            framerate: 30,
            pixel_format: Default::default(),
            graphics: Vec::new(),
        }
    });
    let spec_yaml =
        serde_norway::to_string(&spec).map_err(|e| format!("serializing the overlay: {e}"))?;

    let footage = if kind == Kind::Clip {
        find_footage(state, &station, lc)
    } else {
        None
    };
    let mut paths: Vec<PathBuf> = vec![state.options.station.clone(), lc.config_path.clone()];
    paths.extend(footage.iter().map(|f| f.path.clone()));
    for script in &script_inputs {
        if let Some(dir) = script.parent() {
            collect_tree(dir, 0, &mut paths);
        }
    }
    paths.extend(script_inputs);
    paths.extend(ffmpeg_on_path());
    paths.extend(station.station.fonts.iter().cloned());
    paths.push(crate::overlay_supervisor::overlay_binary_path());
    paths.sort();
    paths.dedup();
    let inputs: Vec<(PathBuf, Option<u128>)> = paths
        .into_iter()
        .map(|p| (p.clone(), mtime_ns(&p)))
        .collect();

    let mut digest = Sha256::new();
    digest.update(
        json!([
            kind.name(),
            key,
            script.display().to_string(),
            at_secs,
            clip_secs,
            gap,
            spec_yaml,
        ])
        .to_string(),
    );
    for (p, m) in &inputs {
        digest.update(format!("{}@{m:?}\n", p.display()));
    }
    let id = format!(
        "{}-{}",
        kind.name(),
        &format!("{:x}", digest.finalize())[..16]
    );

    let queue = &state.review.renders;
    {
        let mut jobs = queue.jobs.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(jobs.get(&id), Some(JobState::Pending(_))) {
            return Ok((202, json!({ "render": id, "cached": false })));
        }
        if read_meta(&queue.dir, &id).is_some() {
            jobs.remove(&id);
            tracing::info!(
                event = "ui.render",
                render = %id,
                kind = kind.name(),
                channel = key,
                script = %script.display(),
                inputs = %inputs_text(&inputs),
                cache = "hit",
                ms = 0u64,
                outcome = "ok",
                "render served from cache"
            );
            return Ok((200, json!({ "render": id, "cached": true })));
        }
        jobs.insert(id.clone(), JobState::Pending(Instant::now()));
    }

    let job = Job {
        id: id.clone(),
        kind,
        channel: key.to_string(),
        script,
        size: (spec.width, spec.height),
        fps: spec.framerate.max(1),
        spec_yaml,
        at_secs,
        clip_secs,
        gap,
        footage,
        bumper,
        fonts: station.station.fonts.clone(),
        inputs,
    };
    if state.review.renders.tx.send(job).is_err() {
        state
            .review
            .renders
            .jobs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
        return Err("the render worker has stopped".to_string());
    }
    Ok((202, json!({ "render": id, "cached": false })))
}

/// The newest airing in the last two days whose media file exists here.
fn find_footage(
    state: &AppState,
    station: &config::Station,
    lc: &config::LoadedChannel,
) -> Option<Footage> {
    let since = time::OffsetDateTime::now_utc() - time::Duration::days(2);
    let history = real::open_history(&station.station.output_base).ok()?;
    let aired = real::read(
        lc,
        history.as_ref(),
        &state.catalog,
        since,
        time::Duration::minutes(30),
    )
    .ok()?;
    let remap = MediaRemap::from_env();
    for airing in aired.airings.iter().rev().take(40) {
        let Ok(sources) = state.catalog.sources_for(&airing.id) else {
            continue;
        };
        let Some(source) = crate::resolve::pick_playback_source(&sources) else {
            continue;
        };
        let path = PathBuf::from(crate::catalog::ingest::plex::translate_path(
            &source.playback_path,
            &remap.from,
            &remap.to,
        ));
        if path.is_file() {
            let long_enough = airing.duration.as_seconds_f64() > 2.0 * FOOTAGE_OFFSET_SECS;
            return Some(Footage {
                path,
                offset_secs: if long_enough {
                    FOOTAGE_OFFSET_SECS
                } else {
                    0.0
                },
            });
        }
    }
    None
}

/// A finished render's sidecar, when its media file is still there.
fn read_meta(dir: &Path, id: &str) -> Option<Value> {
    let meta: Value =
        serde_json::from_slice(&std::fs::read(dir.join(format!("{id}.json"))).ok()?).ok()?;
    let file = meta.get("file")?.as_str()?;
    dir.join(file).is_file().then_some(meta)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `GET /api/renders/{render}`: 202 while it runs, else what it made or why it failed.
pub(super) fn api_render(state: &AppState, id: &str) -> (u16, Value) {
    if !valid_id(id) {
        return (404, json!({ "error": format!("no render {id:?}") }));
    }
    let queue = &state.review.renders;
    // The job map before the sidecar: the worker writes the sidecar and then
    // drops the job, so a render is never in neither.
    {
        let jobs = queue.jobs.lock().unwrap_or_else(|p| p.into_inner());
        match jobs.get(id) {
            Some(JobState::Pending(since)) => {
                return (
                    202,
                    json!({
                        "state": "pending",
                        "render": id,
                        "elapsed_secs": since.elapsed().as_secs(),
                    }),
                );
            }
            Some(JobState::Failed { error, log }) => {
                return (
                    200,
                    json!({ "state": "failed", "render": id, "error": error, "log": log }),
                );
            }
            None => {}
        }
    }
    match read_meta(&queue.dir, id) {
        Some(mut meta) => {
            meta["state"] = json!("done");
            meta["render"] = json!(id);
            meta["url"] = json!(format!("/api/renders/{id}/file"));
            (200, meta)
        }
        None => (
            404,
            json!({ "error": format!("no render {id:?}; it may have been asked for before a restart") }),
        ),
    }
}

/// The file `GET /api/renders/{render}/file` serves, and its content type.
pub(super) fn render_file(state: &AppState, id: &str) -> Option<(PathBuf, &'static str)> {
    if !valid_id(id) {
        return None;
    }
    let dir = &state.review.renders.dir;
    let meta = read_meta(dir, id)?;
    let file = meta.get("file")?.as_str()?.to_string();
    let content_type = if file.ends_with(".png") {
        "image/png"
    } else {
        "video/mp4"
    };
    Some((dir.join(file), content_type))
}

/// Answers a file request on its own thread, so a slow viewer holds up its
/// own download and not the dashboard. Honours one `Range: bytes=a-b` (or
/// `bytes=-n`) so a browser can seek in a clip.
pub(super) fn respond_render_file(
    request: tiny_http::Request,
    path: &Path,
    content_type: &str,
    range: Option<&str>,
) {
    let (path, content_type, range) = (
        path.to_path_buf(),
        content_type.to_string(),
        range.map(str::to_string),
    );
    std::thread::spawn(move || serve_render_file(request, &path, &content_type, range.as_deref()));
}

fn serve_render_file(
    request: tiny_http::Request,
    path: &Path,
    content_type: &str,
    range: Option<&str>,
) {
    let header = |name: &str, value: &str| {
        tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("ascii header")
    };
    let opened = std::fs::File::open(path).and_then(|f| f.metadata().map(|m| (f, m.len())));
    let (mut file, len) = match opened {
        Ok(v) => v,
        Err(e) => {
            let _ = request.respond(
                tiny_http::Response::from_string(format!("{}: {e}", path.display()))
                    .with_status_code(404),
            );
            return;
        }
    };
    let span = range.and_then(|r| {
        let (a, b) = r.strip_prefix("bytes=")?.split_once('-')?;
        if a.is_empty() {
            let n: u64 = b.parse().ok()?;
            return Some(Some((len.saturating_sub(n), len.checked_sub(1)?)));
        }
        let start: u64 = a.parse().ok()?;
        let end: u64 = if b.is_empty() {
            len.checked_sub(1)?
        } else {
            b.parse().ok()?
        };
        Some((start <= end && start < len).then_some((start, end.min(len - 1))))
    });
    let response = match span {
        Some(Some((start, end))) => {
            let n = end - start + 1;
            let _ = file.seek(SeekFrom::Start(start));
            tiny_http::Response::new(
                tiny_http::StatusCode(206),
                vec![
                    header("Content-Type", content_type),
                    header("Accept-Ranges", "bytes"),
                    header("Content-Range", &format!("bytes {start}-{end}/{len}")),
                ],
                file.take(n),
                Some(n as usize),
                None,
            )
            .boxed()
        }
        Some(None) => tiny_http::Response::new(
            tiny_http::StatusCode(416),
            vec![header("Content-Range", &format!("bytes */{len}"))],
            std::io::empty(),
            Some(0),
            None,
        )
        .boxed(),
        None => tiny_http::Response::new(
            tiny_http::StatusCode(200),
            vec![
                header("Content-Type", content_type),
                header("Accept-Ranges", "bytes"),
            ],
            file,
            Some(len as usize),
            None,
        )
        .boxed(),
    };
    let _ = request.respond(response);
}

/// Runs one step, appending the command line and its stderr tail to `log`. A
/// step still running at `timeout` is asked to stop, then killed.
fn run_step(
    log: &mut String,
    program: &OsStr,
    args: &[OsString],
    stdout_to: Option<&Path>,
    timeout: Duration,
) -> Result<(), String> {
    let name = Path::new(program)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    log.push_str(&format!(
        "$ {} {}\n",
        program.to_string_lossy(),
        args.iter()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    ));
    let stdout = match stdout_to {
        Some(p) => Stdio::from(
            std::fs::File::create(p).map_err(|e| format!("create {}: {e}", p.display()))?,
        ),
        None => Stdio::null(),
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", program.to_string_lossy()))?;
    let mut pipe = child.stderr.take().expect("stderr was piped");
    let (tail_tx, tail_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut tail: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        while let Ok(n) = pipe.read(&mut chunk) {
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&chunk[..n]);
            if tail.len() > LOG_TAIL_BYTES {
                tail.drain(..tail.len() - LOG_TAIL_BYTES);
            }
        }
        let _ = tail_tx.send(String::from_utf8_lossy(&tail).into_owned());
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                // SIGTERM first: etv-overlay's handler stops its own ffmpeg.
                let _ = Command::new("kill")
                    .arg(child.id().to_string())
                    .stderr(Stdio::null())
                    .status();
                let grace = Instant::now() + Duration::from_secs(3);
                while Instant::now() < grace && matches!(child.try_wait(), Ok(None)) {
                    std::thread::sleep(Duration::from_millis(50));
                }
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!(
                    "{name} still running after {}s; stopped",
                    timeout.as_secs()
                ));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!("wait for {name}: {e}"));
            }
        }
    };
    // A grandchild that outlives the kill can hold the pipe open; the worker must not wait on it.
    let stderr = tail_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or_default();
    log.push_str(&strip_ansi(&stderr));
    if !log.ends_with('\n') {
        log.push('\n');
    }
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("{name} exited with {s}")),
        Err(e) => Err(e),
    }
}

/// A renderer's colour codes mean nothing in the page's log.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // CSI: ESC [ parameters, ended by a letter.
        if chars.next() == Some('[') {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    out
}

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn scale_filter((w, h): (u32, u32), fps: u32) -> String {
    format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps={fps}"
    )
}

/// Renders `job` into `dir/{id}.{ext}` and its sidecar; on failure, the error
/// and the log gathered so far.
fn render(dir: &Path, job: &Job) -> Result<(), (String, String)> {
    let mut log = String::new();
    render_into(dir, job, &mut log).map_err(|e| (e, log.clone()))?;
    Ok(())
}

fn render_into(dir: &Path, job: &Job, log: &mut String) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let work = dir.join(format!("work-{}", job.id));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| format!("create {}: {e}", work.display()))?;
    let result = render_in(&work, dir, job, log);
    let _ = std::fs::remove_dir_all(&work);
    result
}

fn render_in(work: &Path, dir: &Path, job: &Job, log: &mut String) -> Result<(), String> {
    let spec_path = work.join("spec.yaml");
    std::fs::write(&spec_path, &job.spec_yaml)
        .map_err(|e| format!("write {}: {e}", spec_path.display()))?;
    let overlay = crate::overlay_supervisor::overlay_binary_path();
    let ffmpeg = OsString::from("ffmpeg");
    let mut overlay_args: Vec<OsString> = Vec::new();
    for font in &job.fonts {
        overlay_args.push("--font".into());
        overlay_args.push(font.into());
    }
    let out = work.join(format!("out.{}", job.kind.extension()));
    let (w, h) = job.size;

    match job.kind {
        Kind::Still => {
            let mut args = overlay_args;
            args.extend(os(&["render-still", "--config"]));
            args.push(spec_path.into());
            args.extend(os(&["--output"]));
            args.push(out.clone().into());
            args.extend(os(&["--time", &job.at_secs.to_string()]));
            run_step(log, overlay.as_os_str(), &args, None, STEP_SLACK)?;
        }
        Kind::Clip => {
            let cut = work.join("cut.mp4");
            let secs = job.clip_secs.to_string();
            let mut args = os(&["-hide_banner", "-loglevel", "warning", "-y"]);
            match &job.footage {
                Some(f) => {
                    log.push_str(&format!(
                        "footage: {} from {}s\n",
                        f.path.display(),
                        f.offset_secs
                    ));
                    args.extend(os(&["-ss", &f.offset_secs.to_string(), "-t", &secs, "-i"]));
                    args.push(f.path.clone().into());
                    args.extend(os(&["-map", "0:v:0", "-map", "0:a:0?", "-vf"]));
                    args.push(scale_filter(job.size, job.fps).into());
                }
                None => {
                    log.push_str("footage: none of the channel's recent airings has a media file here; black\n");
                    args.extend(os(&["-f", "lavfi", "-i"]));
                    args.push(format!("color=c=black:s={w}x{h}:r={}:d={secs}", job.fps).into());
                    args.extend(os(&["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo"]));
                    args.extend(os(&[
                        "-map",
                        "0:v",
                        "-map",
                        "1:a",
                        "-shortest",
                        "-t",
                        &secs,
                    ]));
                }
            }
            args.extend(os(&[
                "-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p", "-c:a", "aac",
            ]));
            args.push(cut.clone().into());
            run_step(
                log,
                &ffmpeg,
                &args,
                None,
                STEP_SLACK + Duration::from_secs_f64(job.clip_secs * 4.0),
            )?;

            let mut args = overlay_args;
            args.extend(os(&["run", "--input"]));
            args.push(cut.into());
            args.extend(os(&["--config"]));
            args.push(spec_path.into());
            args.extend(os(&["--output"]));
            args.push(out.clone().into());
            args.extend(os(&["--fifo"]));
            args.push(work.join("overlay.fifo").into());
            run_step(
                log,
                overlay.as_os_str(),
                &args,
                None,
                STEP_SLACK + Duration::from_secs_f64(job.clip_secs * 3.0),
            )?;
        }
        Kind::Bumper => render_bumper(work, job, log, &spec_path, &overlay, &overlay_args, &out)?,
    }

    let (width, height, duration) = match job.kind {
        Kind::Still => {
            let (pw, ph) = png_size(&out)?;
            (pw, ph, None)
        }
        Kind::Clip | Kind::Bumper => probe_video(&out)?,
    };
    let file = format!("{}.{}", job.id, job.kind.extension());
    std::fs::rename(&out, dir.join(&file)).map_err(|e| format!("keep the render: {e}"))?;
    let meta = json!({
        "kind": job.kind.name(),
        "script": job.script.display().to_string(),
        "file": file,
        "width": width,
        "height": height,
        "duration": duration,
        "log": log,
    });
    std::fs::write(dir.join(format!("{}.json", job.id)), meta.to_string())
        .map_err(|e| format!("write the render's sidecar: {e}"))?;
    prune(dir, RENDER_DIR_MAX_RENDERS);
    Ok(())
}

/// Each item `bumper::preview` emits for the gap, played once by `etv-overlay
/// watch` into mpegts, remuxed to mp4, then joined in order.
fn render_bumper(
    work: &Path,
    job: &Job,
    log: &mut String,
    spec_path: &Path,
    overlay: &Path,
    overlay_args: &[OsString],
    out: &Path,
) -> Result<(), String> {
    use crate::bumper::hook::ChannelInfo;
    use crate::bumper::preview::{self, PreviewRequest};

    let b = job.bumper.as_ref().ok_or("a bumper render has no bumper")?;
    let gap_ms = preview::parse_gap(&job.gap)?;
    let now = time::OffsetDateTime::now_utc();
    let resolved = preview::preview(&PreviewRequest {
        script: &job.script,
        gap_ms,
        fps: f64::from(job.fps),
        next: None,
        upcoming: Vec::new(),
        config: b.config.clone(),
        channel: ChannelInfo {
            name: b.channel_name.clone(),
            number: b.channel_number,
            ..ChannelInfo::default()
        },
        overlay_root: &b.overlay_root,
        start: now.replace_nanosecond(0).unwrap_or(now),
    })?;
    log.push_str(&format!(
        "bumper {} on a {} gap: {} item(s)\n",
        job.script.display(),
        job.gap,
        resolved.items.len()
    ));
    if resolved.items.is_empty() {
        return Err("the bumper emitted no items for that gap".into());
    }
    let ffmpeg = OsString::from("ffmpeg");
    let mut parts: Vec<PathBuf> = Vec::new();
    for (n, item) in resolved.items.iter().enumerate() {
        let item_json = serde_json::to_string(item).map_err(|e| e.to_string())?;
        let length = (item.finish - item.start).as_seconds_f64().max(1.0);
        let ts = work.join(format!("item{n}.ts"));
        let mp4 = work.join(format!("item{n}.mp4"));
        let mut args = overlay_args.to_vec();
        args.extend(os(&["watch", "--item"]));
        args.push(item_json.into());
        args.extend(os(&["--config"]));
        args.push(spec_path.into());
        args.extend(os(&["--fifo"]));
        args.push(work.join(format!("item{n}.fifo")).into());
        run_step(
            log,
            overlay.as_os_str(),
            &args,
            Some(&ts),
            STEP_SLACK + Duration::from_secs_f64(length * 2.0),
        )?;
        let mut args = os(&["-hide_banner", "-loglevel", "warning", "-y", "-i"]);
        args.push(ts.into());
        args.extend(os(&["-c", "copy", "-movflags", "+faststart"]));
        args.push(mp4.clone().into());
        run_step(log, &ffmpeg, &args, None, STEP_SLACK)?;
        parts.push(mp4);
    }
    if let [only] = parts.as_slice() {
        std::fs::rename(only, out).map_err(|e| e.to_string())?;
        return Ok(());
    }
    let list = work.join("parts.txt");
    let listing: String = parts
        .iter()
        .map(|p| format!("file '{}'\n", p.display()))
        .collect();
    std::fs::write(&list, listing).map_err(|e| e.to_string())?;
    let mut args = os(&[
        "-hide_banner",
        "-loglevel",
        "warning",
        "-y",
        "-f",
        "concat",
        "-safe",
        "0",
        "-i",
    ]);
    args.push(list.into());
    args.extend(os(&["-c", "copy", "-movflags", "+faststart"]));
    args.push(out.into());
    run_step(log, &ffmpeg, &args, None, STEP_SLACK)
}

/// A PNG's size, from its IHDR chunk.
fn png_size(path: &Path) -> Result<(u32, u32), String> {
    let mut head = [0u8; 24];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    if &head[..8] != b"\x89PNG\r\n\x1a\n" {
        return Err(format!("{} is not a PNG", path.display()));
    }
    let n = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    Ok((n(&head[16..20]), n(&head[20..24])))
}

/// The first video stream's size and the container's duration in seconds.
fn probe_video(path: &Path) -> Result<(u32, u32, Option<f64>), String> {
    let mut args = os(&[
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=width,height:format=duration",
        "-of",
        "json",
    ]);
    args.push(path.into());
    match crate::ffprobe::run_bounded(&args, crate::ffprobe::PROBE_TIMEOUT) {
        Ok(crate::ffprobe::Bounded::Done {
            success: true,
            stdout,
            ..
        }) => {
            let v: Value = serde_json::from_slice(&stdout).map_err(|e| format!("ffprobe: {e}"))?;
            let dim = |k: &str| {
                v.pointer(&format!("/streams/0/{k}"))
                    .and_then(Value::as_u64)
            };
            let duration = v
                .pointer("/format/duration")
                .and_then(Value::as_str)
                .and_then(|d| d.parse::<f64>().ok());
            match (dim("width"), dim("height")) {
                (Some(w), Some(h)) => Ok((w as u32, h as u32, duration)),
                _ => Err(format!(
                    "ffprobe found no video stream in {}",
                    path.display()
                )),
            }
        }
        Ok(crate::ffprobe::Bounded::Done { stderr, .. }) => Err(format!(
            "ffprobe {}: {}",
            path.display(),
            String::from_utf8_lossy(&stderr).trim()
        )),
        Ok(crate::ffprobe::Bounded::TimedOut) => Err("ffprobe timed out".into()),
        Err(e) => Err(format!("spawn ffprobe: {e}")),
    }
}

/// The most renders the directory keeps; the oldest go first.
const RENDER_DIR_MAX_RENDERS: usize = 60;

fn prune(dir: &Path, max: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut sidecars: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    if sidecars.len() <= max {
        return;
    }
    sidecars.sort();
    for (_, sidecar) in &sidecars[..sidecars.len() - max] {
        let stem = sidecar.with_extension("");
        let _ = std::fs::remove_file(stem.with_extension("png"));
        let _ = std::fs::remove_file(stem.with_extension("mp4"));
        let _ = std::fs::remove_file(sidecar);
    }
}

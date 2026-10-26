//! The shot list of a video: where its scenes begin and end, how bright and how
//! busy each one is, and whether it is a card (near-black or near-uniform).
//!
//! A generic fact about a file, computed from ffmpeg alone: ffmpeg decodes the
//! first video stream to 64x36 grayscale frames, and [`Segmenter`] reads them
//! one at a time. A frame whose mean absolute difference from the one before it
//! is large starts a new shot. Nothing here knows what a plugin does with the
//! list; a plugin reads it as `ctx.probe(path).shots`.
//!
//! [`run_queue`] is the background pass that fills the catalog's `entry_shots`
//! table, one file at a time and off the generation path, so a backfill of the
//! whole library never delays a schedule.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::catalog::Catalog;

/// Width and height ffmpeg scales each frame to before the station reads it.
const FRAME_W: usize = 64;
const FRAME_H: usize = 36;
const FRAME_BYTES: usize = FRAME_W * FRAME_H;

/// A frame whose mean absolute difference from the previous one (0 to 255)
/// reaches this starts a shot. A cut between two scenes of similar brightness
/// can fall below it and is missed; fast motion inside one shot stays under it.
const CUT_THRESHOLD: f64 = 20.0;

/// A frame whose mean luma (0 to 1) is below this is dark. Limited-range
/// black is 16/255, which is 0.063.
const BLACK_LUMA: f64 = 0.08;
/// A frame whose pixel standard deviation (0 to 255) is below this is
/// near-uniform: a flat colour, a fade, a slate.
const UNIFORM_STDDEV: f64 = 3.0;
/// A dark frame is near-black, not a dark scene, only while its standard
/// deviation stays below this: noise on a black frame, not picture detail.
const BLACK_DETAIL_STDDEV: f64 = 6.0;
/// The share of a shot's frames that must be near-black or near-uniform for
/// the shot to be a card, so one flash inside a long black does not hide it
/// and one dark frame inside a scene does not make it one.
const CARD_FRAME_SHARE: f64 = 0.9;

/// The longest the analyzer lets one file run, whatever its length.
const FILE_TIMEOUT: Duration = Duration::from_secs(45 * 60);

/// How long the queue sleeps once nothing is left to analyze.
const IDLE_WAIT: Duration = Duration::from_secs(300);

/// One shot: `start` and `end` in seconds, `luma` the mean brightness 0 to 1,
/// `motion` the mean absolute frame difference 0 to 1, `card` true when nearly
/// every frame of the shot is near-black or near-uniform.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Shot {
    pub start: f64,
    pub end: f64,
    pub luma: f64,
    pub motion: f64,
    pub card: bool,
}

#[derive(Default)]
struct Open {
    first: u64,
    last: u64,
    luma: f64,
    card_frames: u64,
    motion: f64,
    motion_frames: u64,
}

/// Turns a stream of equally sized grayscale frames into shots.
pub struct Segmenter {
    fps: f64,
    frame: u64,
    prev: Vec<u8>,
    open: Option<Open>,
    shots: Vec<Shot>,
}

impl Segmenter {
    pub fn new(fps: f64) -> Self {
        Self {
            fps,
            frame: 0,
            prev: Vec::new(),
            open: None,
            shots: Vec::new(),
        }
    }

    /// Feed the next frame.
    pub fn push(&mut self, frame: &[u8]) {
        let n = frame.len().max(1) as f64;
        let mean = frame.iter().map(|&v| f64::from(v)).sum::<f64>() / n;
        let var = frame
            .iter()
            .map(|&v| (f64::from(v) - mean).powi(2))
            .sum::<f64>()
            / n;
        let diff = if self.prev.len() == frame.len() {
            Some(
                frame
                    .iter()
                    .zip(&self.prev)
                    .map(|(&a, &b)| f64::from(a.abs_diff(b)))
                    .sum::<f64>()
                    / n,
            )
        } else {
            None
        };
        let cut = diff.is_some_and(|d| self.is_cut(d));
        if cut {
            self.close();
        }
        let open = self.open.get_or_insert_with(|| Open {
            first: self.frame,
            ..Open::default()
        });
        open.last = self.frame;
        open.luma += mean;
        let stddev = var.sqrt();
        if stddev < UNIFORM_STDDEV || (mean / 255.0 < BLACK_LUMA && stddev < BLACK_DETAIL_STDDEV) {
            open.card_frames += 1;
        }
        if let (false, Some(d)) = (cut, diff) {
            open.motion += d;
            open.motion_frames += 1;
        }
        self.prev.clear();
        self.prev.extend_from_slice(frame);
        self.frame += 1;
    }

    fn is_cut(&self, diff: f64) -> bool {
        diff >= CUT_THRESHOLD
    }

    fn close(&mut self) {
        let Some(open) = self.open.take() else {
            return;
        };
        let frames = (open.last - open.first + 1) as f64;
        let luma = open.luma / frames / 255.0;
        let card = open.card_frames as f64 / frames >= CARD_FRAME_SHARE;
        let motion = if open.motion_frames == 0 {
            0.0
        } else {
            open.motion / open.motion_frames as f64 / 255.0
        };
        self.shots.push(Shot {
            start: round(open.first as f64 / self.fps, 1000.0),
            end: round((open.last + 1) as f64 / self.fps, 1000.0),
            luma: round(luma, 10_000.0),
            motion: round(motion, 10_000.0),
            card,
        });
    }

    /// The shots of every frame pushed.
    pub fn finish(mut self) -> Vec<Shot> {
        self.close();
        self.shots
    }
}

fn round(v: f64, scale: f64) -> f64 {
    (v * scale).round() / scale
}

/// Why a file has no shot list. A permanent failure is about the file and is
/// stored, so the file is not tried again until its path changes; a transient
/// one is about the machine (no ffmpeg, a stale mount, a timeout, a stop) and
/// is retried on the next pass.
#[derive(Debug, Clone, PartialEq)]
pub enum DetectError {
    Permanent(String),
    Transient(String),
}

impl std::fmt::Display for DetectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (Self::Permanent(m) | Self::Transient(m)) = self;
        f.write_str(m)
    }
}

/// The first video stream's average frame rate, or why there is none. The
/// average is the frame count over the duration, so a variable-frame-rate file
/// places its last shot at its true end; `r_frame_rate` is the fastest rate the
/// stream declares and compresses every time toward the start. A stream that
/// reports no average falls back to `r_frame_rate`.
fn frame_rate(path: &Path) -> Result<f64, DetectError> {
    let args: Vec<OsString> = [
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=avg_frame_rate,r_frame_rate",
        "-of",
        "default=noprint_wrappers=1",
        "-protocol_whitelist",
        "file",
        "-i",
    ]
    .iter()
    .map(Into::into)
    .chain([path.as_os_str().to_owned()])
    .collect();
    let out = match crate::ffprobe::run_bounded(&args, crate::ffprobe::PROBE_TIMEOUT)
        .map_err(|e| DetectError::Transient(format!("cannot run ffprobe: {e}")))?
    {
        crate::ffprobe::Bounded::Done {
            success: true,
            stdout,
            ..
        } => String::from_utf8_lossy(&stdout).trim().to_string(),
        crate::ffprobe::Bounded::Done { stderr, .. } => {
            return Err(DetectError::Permanent(format!(
                "ffprobe rejected the file: {}",
                String::from_utf8_lossy(&stderr).trim()
            )));
        }
        crate::ffprobe::Bounded::TimedOut => {
            return Err(DetectError::Transient("ffprobe timed out".into()));
        }
    };
    if out.is_empty() {
        return Err(DetectError::Permanent("no video stream".into()));
    }
    pick_frame_rate(&out)
}

/// The usable rate in `key=num/den` lines: `avg_frame_rate` first, else
/// `r_frame_rate`.
fn pick_frame_rate(lines: &str) -> Result<f64, DetectError> {
    let value = |key: &str| {
        lines
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .and_then(parse_rate)
    };
    value("avg_frame_rate")
        .or_else(|| value("r_frame_rate"))
        .ok_or_else(|| DetectError::Permanent(format!("unusable frame rate {lines:?}")))
}

/// `30000/1001` or `25` as frames per second; `None` for zero or garbage.
fn parse_rate(text: &str) -> Option<f64> {
    let (num, den) = text.trim().split_once('/').unwrap_or((text.trim(), "1"));
    let (num, den): (f64, f64) = (num.parse().ok()?, den.parse().ok()?);
    (num > 0.0 && den > 0.0).then(|| num / den)
}

/// The program every shot-analysis ffmpeg spawn runs: the station's
/// `ffmpeg.ffmpeg_path`, else `ffmpeg` on `PATH`. The same value ETV-next is
/// given for its transcodes, so one setting decides which ffmpeg the station
/// uses.
pub fn ffmpeg_program(configured: Option<&Path>) -> OsString {
    match configured {
        Some(p) if !p.as_os_str().is_empty() => p.as_os_str().to_owned(),
        _ => OsString::from("ffmpeg"),
    }
}

/// Whether `program` can be run: a path with a separator must be a file, a bare
/// name must be on `PATH`.
pub fn ffmpeg_found(program: &OsString) -> bool {
    crate::audio::locate(&program.to_string_lossy()).is_some()
}

/// Detect the shots of `path`.
///
/// A reader thread owns ffmpeg's pipe and hands frames over a channel, so a
/// decode stalled on a dead network mount cannot hold this call past `stop` or
/// [`FILE_TIMEOUT`]: the caller gives up, kills ffmpeg and leaves the reader to
/// end when the pipe closes.
pub fn detect(path: &Path, ffmpeg: &OsString, stop: &AtomicBool) -> Result<Vec<Shot>, DetectError> {
    use std::sync::mpsc::{RecvTimeoutError, sync_channel};

    let fps = frame_rate(path)?;
    let mut child = Command::new(ffmpeg)
        .args([
            "-nostdin",
            "-v",
            "error",
            "-protocol_whitelist",
            "file",
            "-i",
        ])
        .arg(path)
        .args(["-map", "0:v:0", "-an", "-sn", "-dn", "-vf"])
        .arg(format!("scale={FRAME_W}:{FRAME_H}:flags=area,format=gray"))
        .args(["-fps_mode", "passthrough", "-f", "rawvideo", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| DetectError::Transient(format!("cannot run ffmpeg: {e}")))?;
    let mut stdout = child.stdout.take().expect("stdout was piped");
    let (tx, rx) = sync_channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        loop {
            let mut buf = vec![0u8; FRAME_BYTES];
            if stdout.read_exact(&mut buf).is_err() || tx.send(buf).is_err() {
                return;
            }
        }
    });
    let deadline = Instant::now() + FILE_TIMEOUT;
    let mut seg = Segmenter::new(fps);
    let outcome = loop {
        if stop.load(Ordering::Relaxed) {
            break Err(DetectError::Transient("stopped".into()));
        }
        if Instant::now() >= deadline {
            break Err(DetectError::Transient(format!(
                "ffmpeg ran past {}s",
                FILE_TIMEOUT.as_secs()
            )));
        }
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(frame) => seg.push(&frame),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break Ok(()),
        }
    };
    if outcome.is_err() {
        let _ = child.kill();
    }
    drop(rx);
    // A killed ffmpeg on a dead mount may not reap; do not wait on it here.
    if outcome.is_err() {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        return outcome.map(|()| Vec::new());
    }
    let status = child
        .wait()
        .map_err(|e| DetectError::Transient(format!("waiting on ffmpeg: {e}")))?;
    if !status.success() {
        return Err(DetectError::Permanent(format!(
            "ffmpeg exited with {status}"
        )));
    }
    let shots = seg.finish();
    if shots.is_empty() {
        return Err(DetectError::Permanent(
            "the video stream decoded no frames".into(),
        ));
    }
    Ok(shots)
}

/// Why [`entry_facts`] has no answer: the id names no entry, or the catalog
/// could not be read.
#[derive(Debug, Clone, PartialEq)]
pub enum FactsError {
    UnknownEntry(String),
    Catalog(String),
}

impl std::fmt::Display for FactsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownEntry(id) => write!(f, "no catalog entry {id:?}"),
            Self::Catalog(e) => f.write_str(e),
        }
    }
}

/// The facts the catalog holds about one entry, as the JSON `etv-station facts`
/// prints and the dashboard serves: `shots` is `null` for an entry never
/// analyzed, else the row with its `error` when the analysis failed.
pub fn entry_facts(catalog: &Catalog, entry_id: &str) -> Result<serde_json::Value, FactsError> {
    let catalog_err = |e: crate::catalog::CatalogError| FactsError::Catalog(e.to_string());
    let entry = catalog
        .entry(entry_id)
        .map_err(catalog_err)?
        .ok_or_else(|| FactsError::UnknownEntry(entry_id.to_string()))?;
    let shots = catalog.shots_for(entry_id).map_err(catalog_err)?;
    Ok(serde_json::json!({
        "entry_id": entry_id,
        "title": entry.title,
        "shots": shots.map(|row| serde_json::json!({
            "source_path": row.source_path,
            "analyzed_at": row.analyzed_at,
            "error": row.error,
            "shots": row.shots,
        })),
    }))
}

/// Analyze every entry that has no shot list for its live file, one at a time,
/// and return how many were analyzed. Stops between files when `stop` is set.
pub fn analyze_pending(catalog: &Catalog, ffmpeg: &OsString, stop: &AtomicBool) -> usize {
    let pending = match catalog.entries_needing_shots() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(
                event = "shots.pending_failed",
                error = %e,
                "could not list the entries without a shot list",
            );
            return 0;
        }
    };
    let mut done = 0;
    for (entry_id, path) in pending {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let started = Instant::now();
        let result = detect(Path::new(&path), ffmpeg, stop);
        if let Err(DetectError::Transient(e)) = &result {
            tracing::warn!(
                event = "shots.deferred",
                entry = %entry_id,
                path = %path,
                error = %e,
                "could not analyze now; leaving the entry pending and ending this pass",
            );
            break;
        }
        let ms = started.elapsed().as_millis() as u64;
        let stored = match &result {
            Ok(shots) => {
                tracing::info!(
                    event = "shots.analyzed",
                    entry = %entry_id,
                    path = %path,
                    shots = shots.len(),
                    cards = shots.iter().filter(|s| s.card).count(),
                    ms,
                    "stored the shot list",
                );
                catalog.set_shots(&entry_id, &path, shots, None)
            }
            Err(e) => {
                tracing::warn!(
                    event = "shots.failed",
                    entry = %entry_id,
                    path = %path,
                    error = %e,
                    ms,
                    "could not list the shots; recorded the failure and will not retry until the file changes",
                );
                catalog.set_shots(&entry_id, &path, &[], Some(&e.to_string()))
            }
        };
        if let Err(e) = stored {
            tracing::error!(
                event = "shots.store_failed",
                entry = %entry_id,
                error = %e,
                "could not write the shot list to the catalog",
            );
            break;
        }
        done += 1;
    }
    done
}

/// The shot-analysis queue: analyze what is pending on a handle of its own to
/// the catalog at `catalog_path`, wait, and look again, until `stop` fires.
/// Each file runs on a blocking thread, so the queue never holds a runtime
/// worker or a channel's catalog reader.
pub async fn run_queue(catalog_path: PathBuf, ffmpeg: OsString, stop: Arc<Notify>) {
    let halt = Arc::new(AtomicBool::new(false));
    loop {
        let pass = {
            let path = catalog_path.clone();
            let halt = Arc::clone(&halt);
            let ffmpeg = ffmpeg.clone();
            tokio::task::spawn_blocking(move || match Catalog::open(&path) {
                Ok(catalog) => analyze_pending(&catalog, &ffmpeg, &halt),
                Err(e) => {
                    tracing::error!(
                        event = "shots.open_failed",
                        path = %path.display(),
                        error = %e,
                        "could not open the catalog for shot analysis",
                    );
                    0
                }
            })
        };
        tokio::pin!(pass);
        let stopped = tokio::select! {
            _ = stop.notified() => {
                halt.store(true, Ordering::Relaxed);
                let _ = (&mut pass).await;
                true
            }
            done = &mut pass => {
                if let Ok(n) = done && n > 0 {
                    tracing::info!(event = "shots.pass_done", analyzed = n, "shot analysis pass finished");
                }
                false
            }
        };
        if stopped {
            return;
        }
        tokio::select! {
            _ = stop.notified() => return,
            _ = tokio::time::sleep(IDLE_WAIT) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(v: u8) -> Vec<u8> {
        vec![v; FRAME_BYTES]
    }

    /// A frame of shot `shot`: a fixed texture per shot plus a one-level
    /// jitter per frame, so a shot has motion of about one level and two shots
    /// differ by far more.
    fn shot_frame(base: u8, shot: u32, frame: u32) -> Vec<u8> {
        let tex = |i: usize| (shot.wrapping_mul(2_654_435_761) >> (i % 24)) as u8 % 40;
        (0..FRAME_BYTES)
            .map(|i| {
                base.saturating_add(tex(i))
                    .saturating_add((frame % 2) as u8)
            })
            .collect()
    }

    #[test]
    fn a_hard_cut_splits_the_shots_at_its_frame() {
        let mut seg = Segmenter::new(10.0);
        for i in 0..20 {
            seg.push(&shot_frame(40, 1, i));
        }
        for i in 0..30 {
            seg.push(&shot_frame(180, 2, i));
        }
        let shots = seg.finish();
        assert_eq!(shots.len(), 2, "{shots:?}");
        assert_eq!((shots[0].start, shots[0].end), (0.0, 2.0));
        assert_eq!((shots[1].start, shots[1].end), (2.0, 5.0));
        assert!(shots[1].luma > shots[0].luma);
        assert!(!shots[0].card && !shots[1].card);
    }

    #[test]
    fn a_black_lead_in_is_a_card_and_the_picture_after_it_is_not() {
        let mut seg = Segmenter::new(10.0);
        for _ in 0..30 {
            seg.push(&flat(16));
        }
        for i in 0..20 {
            seg.push(&shot_frame(90, 3, i));
        }
        let shots = seg.finish();
        assert_eq!(shots.len(), 2, "{shots:?}");
        assert_eq!((shots[0].start, shots[0].end), (0.0, 3.0));
        assert!(shots[0].card);
        assert!(!shots[1].card);
    }

    #[test]
    fn a_flat_bright_slate_is_a_card_by_uniformity() {
        let mut seg = Segmenter::new(10.0);
        for _ in 0..10 {
            seg.push(&flat(200));
        }
        let shots = seg.finish();
        assert_eq!(shots.len(), 1);
        assert!(shots[0].card);
        assert!(shots[0].luma > 0.7);
    }

    #[test]
    fn steady_motion_inside_a_shot_is_not_a_cut() {
        let mut seg = Segmenter::new(10.0);
        for i in 0..40 {
            seg.push(&shot_frame(100, 4, i));
        }
        let shots = seg.finish();
        assert_eq!(shots.len(), 1, "{shots:?}");
        assert!(shots[0].motion > 0.0);
    }

    #[test]
    fn a_long_dark_scene_with_picture_detail_is_not_a_card() {
        let mut seg = Segmenter::new(10.0);
        // Alternating 0 and 30: mean luma 0.059 (dark), standard deviation 15.
        let dark: Vec<u8> = (0..FRAME_BYTES)
            .map(|i| if i % 2 == 0 { 0 } else { 30 })
            .collect();
        for _ in 0..100 {
            seg.push(&dark);
        }
        let shots = seg.finish();
        assert_eq!(shots.len(), 1);
        assert!(shots[0].luma < BLACK_LUMA);
        assert!(!shots[0].card);
    }

    #[test]
    fn a_black_shot_with_sensor_noise_is_still_a_card() {
        let mut seg = Segmenter::new(10.0);
        let noisy: Vec<u8> = (0..FRAME_BYTES)
            .map(|i| if i % 2 == 0 { 14 } else { 20 })
            .collect();
        for _ in 0..30 {
            seg.push(&noisy);
        }
        assert!(seg.finish()[0].card);
    }

    #[test]
    fn the_frame_rate_is_the_average_one_and_falls_back_to_the_declared_one() {
        let rate = |t: &str| pick_frame_rate(t).unwrap();
        assert_eq!(rate("r_frame_rate=60/1\navg_frame_rate=25/1"), 25.0);
        assert_eq!(
            rate("avg_frame_rate=0/0\nr_frame_rate=30000/1001"),
            30000.0 / 1001.0
        );
        assert!(pick_frame_rate("avg_frame_rate=0/0\nr_frame_rate=0/0").is_err());
    }

    #[test]
    fn the_configured_ffmpeg_wins_and_the_default_is_path_lookup() {
        assert_eq!(
            ffmpeg_program(Some(Path::new("/opt/ff/ffmpeg"))),
            "/opt/ff/ffmpeg"
        );
        assert_eq!(ffmpeg_program(Some(Path::new(""))), "ffmpeg");
        assert_eq!(ffmpeg_program(None), "ffmpeg");
    }

    fn make_fixture(dir: &Path, filter: &str, inputs: &[&str]) -> PathBuf {
        let out = dir.join("fixture.mp4");
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-nostdin", "-v", "error", "-y"]);
        for i in inputs {
            cmd.args(["-f", "lavfi", "-i", i]);
        }
        let ok = cmd
            .args(["-filter_complex", filter, "-map", "[v]"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "5"])
            .arg(&out)
            .status()
            .unwrap()
            .success();
        assert!(ok, "ffmpeg could not build the fixture");
        out
    }

    #[test]
    fn detect_finds_each_hard_cut_of_a_real_file_within_one_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_fixture(
            dir.path(),
            "[0][1][2]concat=n=3:v=1:a=0[v]",
            &[
                "testsrc2=s=320x180:r=25:d=2",
                "color=c=red:s=320x180:r=25:d=1",
                "smptebars=s=320x180:r=25:d=2",
            ],
        );
        let shots = detect(&path, &ffmpeg_program(None), &AtomicBool::new(false)).unwrap();
        let cuts: Vec<f64> = shots.iter().skip(1).map(|s| s.start).collect();
        let one_frame = 1.0 / 25.0;
        for expected in [2.0, 3.0] {
            assert!(
                cuts.iter()
                    .any(|c| (c - expected).abs() <= one_frame + 1e-9),
                "no cut near {expected}s in {shots:?}"
            );
        }
    }

    #[test]
    fn detect_marks_a_three_second_black_lead_in_as_a_card() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_fixture(
            dir.path(),
            "[0][1]concat=n=2:v=1:a=0[v]",
            &[
                "color=c=black:s=320x180:r=25:d=3",
                "testsrc2=s=320x180:r=25:d=3",
            ],
        );
        let shots = detect(&path, &ffmpeg_program(None), &AtomicBool::new(false)).unwrap();
        let first = &shots[0];
        assert!(first.card, "{shots:?}");
        assert_eq!(first.start, 0.0);
        assert!((first.end - 3.0).abs() <= 1.0 / 25.0 + 1e-9, "{shots:?}");
        assert!(shots.iter().skip(1).any(|s| !s.card), "{shots:?}");
    }

    #[test]
    fn detect_refuses_a_file_with_no_video() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.wav");
        let ok = Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-f", "lavfi", "-i", "sine=d=1"])
            .arg(&path)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let err = detect(&path, &ffmpeg_program(None), &AtomicBool::new(false)).unwrap_err();
        assert_eq!(err, DetectError::Permanent("no video stream".into()));
    }
}

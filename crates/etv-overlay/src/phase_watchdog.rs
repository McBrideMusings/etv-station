//! The overlay half of the two-clock instrumentation.
//!
//! When a channel freezes, the outside view is symmetric: ffmpeg's `out_time`
//! stops advancing and nothing is logged. That looks identical whether the
//! overlay stopped producing frames (starving ffmpeg) or ffmpeg stopped
//! consuming them (blocking the overlay inside `write_all`). Telling those apart
//! needs to know what the overlay's frame loop was doing at the moment the other
//! clock stopped.
//!
//! The frame loop marks which [`Phase`] it is in. A watchdog thread samples that
//! mark and reports any phase that outlives its budget, so a stall names the
//! phase it happened in rather than showing up as generic silence.
//!
//! Cost in the hot path is two relaxed atomic stores per phase transition. All
//! I/O — the log line and the heartbeat file — happens on the watchdog thread,
//! never on the thread that has to hold 30fps.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Where the frame loop is. Ordered as the loop runs them.
///
/// The distinction the whole diagnosis rests on is [`Phase::WriteFifo`] versus
/// everything else. Blocked in `WriteFifo` means ffmpeg is not draining the
/// pipe, so ffmpeg stopped first. Blocked in any other phase means the overlay
/// stopped first and ffmpeg is starving downstream of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Between frames, sleeping to hold the target framerate. Expected to be the
    /// resting phase; a long stay here is normal.
    Pacing,
    /// `OverlayTimelineSource::refresh` — stats the config, may re-read it.
    TimelineRefresh,
    /// Rebuilding the Rhai engine and layers after a timeline block change.
    EngineReload,
    /// `ProgramContextSource::refresh` + `current_at`. Re-reads and re-parses
    /// every playout chunk file whenever the folder mtime moved.
    ProgramContext,
    /// Rhai script evaluation for this frame.
    Evaluate,
    /// Vello render of this frame.
    Render,
    /// `write_all` of the frame into the fifo. Blocks while the reader is behind.
    WriteFifo,
    /// Waiting for a new reader to attach after a BrokenPipe.
    ReopenWait,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Phase::Pacing => "pacing",
            Phase::TimelineRefresh => "timeline_refresh",
            Phase::EngineReload => "engine_reload",
            Phase::ProgramContext => "program_context",
            Phase::Evaluate => "evaluate",
            Phase::Render => "render",
            Phase::WriteFifo => "write_fifo",
            Phase::ReopenWait => "reopen_wait",
        }
    }

    fn from_index(i: usize) -> Phase {
        match i {
            1 => Phase::TimelineRefresh,
            2 => Phase::EngineReload,
            3 => Phase::ProgramContext,
            4 => Phase::Evaluate,
            5 => Phase::Render,
            6 => Phase::WriteFifo,
            7 => Phase::ReopenWait,
            _ => Phase::Pacing,
        }
    }

    fn index(self) -> usize {
        match self {
            Phase::Pacing => 0,
            Phase::TimelineRefresh => 1,
            Phase::EngineReload => 2,
            Phase::ProgramContext => 3,
            Phase::Evaluate => 4,
            Phase::Render => 5,
            Phase::WriteFifo => 6,
            Phase::ReopenWait => 7,
        }
    }

    /// How long this phase may run before the watchdog calls it stalled.
    ///
    /// `Pacing` and `ReopenWait` are legitimately long: pacing sleeps between
    /// frames, and a reader gap at an item boundary is expected and already
    /// bounded by the overlay's own idle timeout. The rest are per-frame work
    /// that has to fit inside a frame period, so a full second in any of them is
    /// already far outside normal.
    fn budget(self) -> Option<Duration> {
        match self {
            Phase::Pacing | Phase::ReopenWait => None,
            _ => Some(Duration::from_secs(1)),
        }
    }
}

/// Shared mark the frame loop writes and the watchdog thread reads.
pub struct PhaseWatch {
    phase: AtomicUsize,
    /// Unix milliseconds at which the current phase was entered.
    entered_ms: AtomicU64,
    /// Frames written since start; lets a reader tell "stuck" from "slow".
    frames: AtomicU64,
    stop: AtomicBool,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl PhaseWatch {
    pub fn new() -> Arc<Self> {
        Arc::new(PhaseWatch {
            phase: AtomicUsize::new(Phase::Pacing.index()),
            entered_ms: AtomicU64::new(now_ms()),
            frames: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        })
    }

    /// Mark the loop as having entered `phase`. Two relaxed stores; safe to call
    /// on every transition at framerate.
    pub fn enter(&self, phase: Phase) {
        self.phase.store(phase.index(), Ordering::Relaxed);
        self.entered_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// Count a frame that made it into the fifo.
    pub fn frame_written(&self) {
        self.frames.fetch_add(1, Ordering::Relaxed);
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    fn snapshot(&self) -> (Phase, u64, u64) {
        (
            Phase::from_index(self.phase.load(Ordering::Relaxed)),
            self.entered_ms.load(Ordering::Relaxed),
            self.frames.load(Ordering::Relaxed),
        )
    }
}

/// One line of JSON describing where the frame loop is, rewritten every
/// `HEARTBEAT_INTERVAL`.
///
/// This exists so the state is readable from outside the process while it is
/// wedged — a log line is only emitted when a budget is already blown, but the
/// capture script needs to be able to ask "what is it doing *right now*" at any
/// moment, including from another container or over ssh.
fn write_heartbeat(path: &Path, phase: Phase, age_ms: u64, frames: u64) {
    let body = format!(
        r#"{{"phase":"{}","phase_age_ms":{},"frames_written":{},"unix_ms":{}}}"#,
        phase.as_str(),
        age_ms,
        frames,
        now_ms()
    );
    // Write-then-rename so a reader never catches a half-written file.
    let tmp = path.with_extension("heartbeat.tmp");
    if std::fs::write(&tmp, body.as_bytes()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);

/// Start the watchdog thread. It samples the phase mark, writes the heartbeat
/// file, and logs a warning the first time a phase exceeds its budget (and
/// again, at a decaying cadence, while it stays over).
pub fn spawn(watch: Arc<PhaseWatch>, heartbeat_path: PathBuf) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut last_heartbeat = std::time::Instant::now() - HEARTBEAT_INTERVAL;
        // Which phase-entry we have already warned about, and at what age, so a
        // single long stall logs on a widening interval instead of every sample.
        let mut warned_entry: Option<u64> = None;
        let mut next_warn_secs: u64 = 1;

        while !watch.stop.load(Ordering::SeqCst) {
            let (phase, entered_ms, frames) = watch.snapshot();
            let age_ms = now_ms().saturating_sub(entered_ms);

            if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
                write_heartbeat(&heartbeat_path, phase, age_ms, frames);
                last_heartbeat = std::time::Instant::now();
            }

            match phase.budget() {
                Some(budget) if Duration::from_millis(age_ms) > budget => {
                    let fresh = warned_entry != Some(entered_ms);
                    if fresh {
                        warned_entry = Some(entered_ms);
                        next_warn_secs = 1;
                    }
                    if age_ms / 1000 >= next_warn_secs {
                        tracing::warn!(
                            event = "overlay.phase_stall",
                            phase = phase.as_str(),
                            age_ms,
                            frames_written = frames,
                            "overlay frame loop has been in one phase past its budget; \
                             if ffmpeg's out_time froze at the same moment, the phase named \
                             here is the side that stopped first",
                        );
                        // 1s, 2s, 4s, 8s ... so a 60s stall logs 7 lines, not 240.
                        next_warn_secs = (next_warn_secs * 2).max(1);
                    }
                }
                _ => {
                    warned_entry = None;
                    next_warn_secs = 1;
                }
            }

            thread::sleep(SAMPLE_INTERVAL);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_index_round_trips() {
        for p in [
            Phase::Pacing,
            Phase::TimelineRefresh,
            Phase::EngineReload,
            Phase::ProgramContext,
            Phase::Evaluate,
            Phase::Render,
            Phase::WriteFifo,
            Phase::ReopenWait,
        ] {
            assert_eq!(
                Phase::from_index(p.index()),
                p,
                "round trip failed for {p:?}"
            );
        }
    }

    /// The two phases that are legitimately long must not be budgeted, or a
    /// normal item boundary would log a stall every time.
    #[test]
    fn only_per_frame_phases_carry_a_budget() {
        assert!(Phase::Pacing.budget().is_none());
        assert!(Phase::ReopenWait.budget().is_none());
        assert!(Phase::WriteFifo.budget().is_some());
        assert!(Phase::ProgramContext.budget().is_some());
    }

    #[test]
    fn heartbeat_file_is_written_and_parseable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overlay.heartbeat");
        write_heartbeat(&path, Phase::WriteFifo, 4321, 99);
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains(r#""phase":"write_fifo""#), "got: {body}");
        assert!(body.contains(r#""phase_age_ms":4321"#), "got: {body}");
        assert!(body.contains(r#""frames_written":99"#), "got: {body}");
    }

    #[test]
    fn watchdog_reports_a_stalled_phase_and_keeps_the_heartbeat_current() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overlay.heartbeat");
        let watch = PhaseWatch::new();
        let handle = spawn(Arc::clone(&watch), path.clone());

        // Enter a budgeted phase and stay there past its budget.
        watch.enter(Phase::WriteFifo);
        thread::sleep(Duration::from_millis(1600));

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains(r#""phase":"write_fifo""#), "got: {body}");
        let age: u64 = body
            .split(r#""phase_age_ms":"#)
            .nth(1)
            .and_then(|s| s.split(',').next())
            .and_then(|s| s.parse().ok())
            .expect("phase_age_ms present");
        assert!(
            age >= 1000,
            "expected the stall to be visible, got age {age}ms"
        );

        watch.stop();
        handle.join().unwrap();
    }
}

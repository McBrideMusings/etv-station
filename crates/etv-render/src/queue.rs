//! The queue directory: plans waiting to render, and one state file per job.
//!
//! ```text
//! <queue>/<id>.plan.json     the plan, written first
//! <queue>/<id>.state.json    the job's state, written last
//! ```
//!
//! A job exists once its state file does, and `enqueue` writes that file last,
//! so the sidecar never sees a state without its plan. Both are written to a
//! temporary name and renamed into place. The job's id is a hash of the plan,
//! so enqueueing the same chapter twice is one job.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::cache;
use crate::plan::Plan;
use crate::render::{self, Timings};
use crate::settings::Settings;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Running,
    Done,
    Failed,
    /// Rendered, then removed from the cache to stay under its size cap.
    Evicted,
}

impl Status {
    pub const ALL: [Status; 5] = [
        Status::Queued,
        Status::Running,
        Status::Done,
        Status::Failed,
        Status::Evicted,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Queued => "queued",
            Status::Running => "running",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Evicted => "evicted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobState {
    pub id: String,
    pub status: Status,
    pub output: PathBuf,
    /// How long the rendered chapter plays.
    pub duration_s: f64,
    #[serde(with = "time::serde::rfc3339")]
    pub enqueued_at: OffsetDateTime,
    /// When the chapter airs. Eviction needs it; a job without one is never evicted.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub air_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub started_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    /// The part of the chapter that stopped the last attempt.
    #[serde(default)]
    pub failed_layer: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub timings: Option<Timings>,
    /// Stem directories the render used, relative to `<cache>/stems`.
    #[serde(default)]
    pub stems: Vec<String>,
    #[serde(default)]
    pub output_bytes: Option<u64>,
    pub attempts: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error(transparent)]
    Plan(#[from] crate::plan::PlanError),
    #[error("queue {}: {message}", dir.display())]
    Io { dir: PathBuf, message: String },
}

#[derive(Debug, Clone)]
pub struct Queue {
    pub dir: PathBuf,
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut f = fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::rename(&tmp, path)
}

impl Queue {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn io(&self, message: impl std::fmt::Display) -> QueueError {
        QueueError::Io {
            dir: self.dir.clone(),
            message: message.to_string(),
        }
    }

    pub fn plan_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.plan.json"))
    }

    fn state_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.state.json"))
    }

    pub fn read(&self, id: &str) -> Option<JobState> {
        let bytes = fs::read(self.state_path(id)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn write(&self, state: &JobState) -> Result<(), QueueError> {
        let json = serde_json::to_vec_pretty(state).map_err(|e| self.io(e))?;
        write_atomic(&self.state_path(&state.id), &json).map_err(|e| self.io(e))
    }

    /// Every job, oldest enqueue first. A state file that does not parse is
    /// logged and skipped: one damaged file must not hide the rest.
    pub fn list(&self) -> Vec<JobState> {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut jobs: Vec<JobState> = entries
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".state.json"))
            .filter_map(|e| match fs::read(e.path()) {
                Ok(bytes) => match serde_json::from_slice(&bytes) {
                    Ok(state) => Some(state),
                    Err(err) => {
                        tracing::warn!(event = "render.state_unreadable", file = %e.path().display(), error = %err, "skipping a job state that does not parse");
                        None
                    }
                },
                Err(_) => None,
            })
            .collect();
        jobs.sort_by(|a, b| a.enqueued_at.cmp(&b.enqueued_at).then(a.id.cmp(&b.id)));
        jobs
    }

    /// Put `plan` on the queue and return at once with its job. A plan already
    /// queued, running or done is left as it is (its air time is updated when
    /// given); one that failed or was evicted is queued again.
    pub fn enqueue(
        &self,
        plan: &Plan,
        air_at: Option<OffsetDateTime>,
        now: OffsetDateTime,
    ) -> Result<JobState, QueueError> {
        plan.validate()?;
        fs::create_dir_all(&self.dir).map_err(|e| self.io(e))?;
        let json = serde_json::to_vec_pretty(plan).map_err(|e| self.io(e))?;
        let id = job_id(plan);

        if let Some(mut existing) = self.read(&id) {
            if matches!(
                existing.status,
                Status::Queued | Status::Running | Status::Done
            ) {
                if air_at.is_some() && existing.air_at != air_at {
                    existing.air_at = air_at;
                    self.write(&existing)?;
                }
                return Ok(existing);
            }
            let attempts = existing.attempts;
            let state = fresh_state(&id, plan, air_at, now, attempts);
            write_atomic(&self.plan_path(&id), &json).map_err(|e| self.io(e))?;
            self.write(&state)?;
            tracing::info!(event = "render.enqueued", id = %id, retry = true, "requeued a chapter");
            return Ok(state);
        }

        write_atomic(&self.plan_path(&id), &json).map_err(|e| self.io(e))?;
        let state = fresh_state(&id, plan, air_at, now, 0);
        self.write(&state)?;
        tracing::info!(event = "render.enqueued", id = %id, output = %plan.output.display(), "queued a chapter");
        Ok(state)
    }

    /// A job left `running` by a sidecar that died goes back to `queued`.
    pub fn recover_interrupted(&self) -> usize {
        let mut n = 0;
        for mut job in self
            .list()
            .into_iter()
            .filter(|j| j.status == Status::Running)
        {
            job.status = Status::Queued;
            job.started_at = None;
            if self.write(&job).is_ok() {
                n += 1;
            }
        }
        n
    }

    fn next_queued(&self) -> Option<JobState> {
        self.list().into_iter().find(|j| j.status == Status::Queued)
    }
}

fn fresh_state(
    id: &str,
    plan: &Plan,
    air_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
    attempts: u32,
) -> JobState {
    JobState {
        id: id.to_string(),
        status: Status::Queued,
        output: plan.output.clone(),
        duration_s: plan.duration_s(),
        enqueued_at: now,
        air_at,
        started_at: None,
        finished_at: None,
        failed_layer: None,
        last_error: None,
        timings: None,
        stems: Vec::new(),
        output_bytes: None,
        attempts,
    }
}

/// Sixteen hex characters of the plan's SHA-256.
pub fn job_id(plan: &Plan) -> String {
    let bytes = serde_json::to_vec(plan).expect("a plan serializes");
    let digest = Sha256::digest(bytes);
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// Render the oldest queued job. Returns its final state, or `None` when
/// nothing was waiting.
pub fn process_next(
    queue: &Queue,
    settings: &Settings,
    cap_bytes: u64,
    now: impl Fn() -> OffsetDateTime,
) -> Option<JobState> {
    let mut job = queue.next_queued()?;
    job.status = Status::Running;
    job.started_at = Some(now());
    job.attempts += 1;
    job.last_error = None;
    job.failed_layer = None;
    if queue.write(&job).is_err() {
        return None;
    }
    tracing::info!(event = "render.job_start", id = %job.id, attempt = job.attempts, "job running");

    let outcome = match fs::read_to_string(queue.plan_path(&job.id)) {
        Ok(text) => match Plan::parse(&text) {
            Ok(plan) => render::render(&plan, settings),
            Err(e) => Err(crate::RenderError::new("plan", e.to_string())),
        },
        Err(e) => Err(crate::RenderError::new(
            "plan",
            format!("cannot read {}: {e}", queue.plan_path(&job.id).display()),
        )),
    };
    job.finished_at = Some(now());
    match outcome {
        Ok(done) => {
            job.status = Status::Done;
            job.timings = Some(done.timings);
            job.stems = done.stems;
            job.output_bytes = fs::metadata(&job.output).ok().map(|m| m.len());
            tracing::info!(event = "render.job_done", id = %job.id, output = %job.output.display(), "job done");
        }
        Err(e) => {
            job.status = Status::Failed;
            job.failed_layer = Some(e.layer.clone());
            job.last_error = Some(e.to_string());
            tracing::error!(event = "render.job_failed", id = %job.id, layer = %e.layer, error = %e.message, "job failed");
        }
    }
    let _ = queue.write(&job);
    let evicted = cache::enforce_cap(queue, settings, cap_bytes, now());
    if !evicted.is_empty() {
        tracing::info!(event = "render.evicted", ids = ?evicted, "evicted chapters over the cache cap");
    }
    Some(job)
}

/// Drain the queue until `stop` is set, one job at a time.
pub fn run_sidecar(
    queue: &Queue,
    settings: &Settings,
    cap_bytes: u64,
    stop: &AtomicBool,
    poll: Duration,
) {
    let recovered = queue.recover_interrupted();
    tracing::info!(event = "render.sidecar_start", queue = %queue.dir.display(), cache = %settings.cache_dir.display(), cap_bytes, recovered, "chapter renderer watching its queue");
    let now = OffsetDateTime::now_utc;
    cache::enforce_cap(queue, settings, cap_bytes, now());
    while !stop.load(Ordering::Relaxed) {
        if process_next(queue, settings, cap_bytes, now).is_none() {
            std::thread::sleep(poll);
        }
    }
}

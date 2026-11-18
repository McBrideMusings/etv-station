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
//! so enqueueing the same plan twice is one job.
//!
//! The queue follows the schedule ([`Queue::sync`]): a file the schedule airs
//! within the lead is queued from the plan its pending segment names, and a
//! queued job the schedule no longer airs there is superseded.

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
    /// Its air time came before the sidecar reached it, so it was never rendered.
    Missed,
    /// Queued, then the schedule stopped airing its file within the lead: a
    /// newer generation replaced it. Queued again if the schedule names it again.
    Superseded,
}

impl Status {
    pub const ALL: [Status; 7] = [
        Status::Queued,
        Status::Running,
        Status::Done,
        Status::Failed,
        Status::Evicted,
        Status::Missed,
        Status::Superseded,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Queued => "queued",
            Status::Running => "running",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Evicted => "evicted",
            Status::Missed => "missed",
            Status::Superseded => "superseded",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobState {
    pub id: String,
    pub status: Status,
    pub output: PathBuf,
    /// How long the rendered file plays.
    pub duration_s: f64,
    #[serde(with = "time::serde::rfc3339")]
    pub enqueued_at: OffsetDateTime,
    /// When the render airs. Eviction needs it; a job without one is never evicted.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub air_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub started_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    /// The part of the plan that stopped the last attempt.
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

    /// Delete a job: its state first, so no reader ever finds a state without
    /// its plan, then its plan.
    fn remove(&self, id: &str) -> Result<(), QueueError> {
        fs::remove_file(self.state_path(id)).map_err(|e| self.io(e))?;
        fs::remove_file(self.plan_path(id)).map_err(|e| self.io(e))
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
    /// given); one that failed, was evicted or was missed is queued again. A
    /// new or requeued plan whose air time has already come is recorded as
    /// missed, not queued; a queued one given a past air time is marked missed
    /// by the sidecar's next pass.
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
        let past = is_past(air_at, now);

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
            if past && existing.status == Status::Missed && existing.air_at == air_at {
                return Ok(existing);
            }
            let attempts = existing.attempts;
            let state = fresh_state(&id, plan, air_at, now, attempts);
            write_atomic(&self.plan_path(&id), &json).map_err(|e| self.io(e))?;
            self.write(&state)?;
            tracing::info!(event = "render.enqueued", id = %id, retry = true, status = state.status.as_str(), "requeued a render");
            return Ok(state);
        }

        write_atomic(&self.plan_path(&id), &json).map_err(|e| self.io(e))?;
        let state = fresh_state(&id, plan, air_at, now, 0);
        self.write(&state)?;
        tracing::info!(event = "render.enqueued", id = %id, output = %plan.output.display(), status = state.status.as_str(), "queued a plan");
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

    /// Mark every queued job whose air time has come as missed, returning their
    /// ids. Rendering one would help nobody: the station swaps a render in
    /// only before it airs.
    pub fn mark_missed(&self, jobs: &mut [JobState], now: OffsetDateTime) -> Vec<String> {
        let mut missed = Vec::new();
        for job in jobs.iter_mut() {
            if job.status != Status::Queued || !is_past(job.air_at, now) {
                continue;
            }
            // An enqueue may have moved its air time since `list` read it.
            match self.read(&job.id) {
                Some(current)
                    if current.status == Status::Queued && is_past(current.air_at, now) =>
                {
                    *job = current;
                }
                Some(current) => {
                    *job = current;
                    continue;
                }
                None => continue,
            }
            job.status = Status::Missed;
            let written = self.write(job).map_err(|e| e.to_string()).err();
            tracing::warn!(event = "render.missed", id = %job.id, air_at = ?job.air_at, write_error = ?written, "air time came before the plan rendered; not rendering it");
            if written.is_none() {
                missed.push(job.id.clone());
            }
        }
        missed
    }
}

/// A file the schedule airs that a plan makes: what [`Queue::sync`] reads.
#[derive(Debug, Clone, PartialEq)]
pub struct Wanted {
    pub output: PathBuf,
    /// The plan that makes `output`; `None` when the schedule names a pending
    /// file nothing here renders.
    pub plan: Option<PathBuf>,
    pub air_at: OffsetDateTime,
}

/// What one [`Queue::sync`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SyncCounts {
    /// Files the schedule airs within the lead.
    pub wanted: usize,
    pub enqueued: usize,
    pub superseded: usize,
    /// Superseded jobs deleted because their air time has passed.
    pub removed: usize,
    /// Plans that could not be read or did not make the file that names them.
    pub unreadable_plans: usize,
}

/// Plans already found unreadable, with the modification time they had then,
/// so one is read again only after it changes.
pub type UnreadablePlans = std::collections::HashMap<PathBuf, Option<std::time::SystemTime>>;

impl Queue {
    /// Make the queue follow the schedule up to `horizon`. `wanted` is every
    /// file the schedule airs between `now` and `horizon`.
    ///
    /// - A queued job with an air time still ahead whose file is not in
    ///   `wanted` is superseded: the schedule replaced it, or it airs past the
    ///   horizon and is queued again once the horizon reaches it. A queued job
    ///   whose air time has come is left for [`Queue::mark_missed`].
    /// - A superseded job whose air time has passed is deleted, plan and state.
    /// - A wanted file with a plan and no queued, running, done or failed job
    ///   is queued from that plan. A failed job is not retried here, so a plan
    ///   that cannot render does not loop.
    pub fn sync(
        &self,
        wanted: &[Wanted],
        now: OffsetDateTime,
        unreadable: &mut UnreadablePlans,
    ) -> SyncCounts {
        let mut counts = SyncCounts {
            wanted: wanted.len(),
            ..SyncCounts::default()
        };
        let airs: std::collections::HashSet<&Path> =
            wanted.iter().map(|w| w.output.as_path()).collect();
        let mut covered: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for mut job in self.list() {
            if job.status == Status::Superseded && is_past(job.air_at, now) {
                // Its air time is gone, so no schedule can name it again.
                if self.remove(&job.id).is_ok() {
                    counts.removed += 1;
                }
                continue;
            }
            if job.status == Status::Queued
                && job.air_at.is_some_and(|air| air > now)
                && !airs.contains(job.output.as_path())
            {
                job.status = Status::Superseded;
                if self.write(&job).is_ok() {
                    counts.superseded += 1;
                    tracing::info!(event = "render.superseded", id = %job.id, output = %job.output.display(), air_at = ?job.air_at, "the schedule no longer airs this file within the lead; not rendering it");
                }
                continue;
            }
            if matches!(
                job.status,
                Status::Queued | Status::Running | Status::Done | Status::Failed
            ) {
                covered.insert(job.output);
            }
        }
        for w in wanted {
            let Some(plan_path) = &w.plan else { continue };
            if covered.contains(&w.output) {
                continue;
            }
            let modified = fs::metadata(plan_path).and_then(|m| m.modified()).ok();
            if unreadable.get(plan_path) == Some(&modified) {
                continue;
            }
            let plan = fs::read_to_string(plan_path)
                .map_err(|e| e.to_string())
                .and_then(|text| Plan::parse(&text).map_err(|e| e.to_string()))
                .and_then(|plan| {
                    if plan.output == w.output {
                        Ok(plan)
                    } else {
                        Err(format!(
                            "it makes {}, not the scheduled {}",
                            plan.output.display(),
                            w.output.display()
                        ))
                    }
                });
            match plan.and_then(|plan| {
                self.enqueue(&plan, Some(w.air_at), now)
                    .map_err(|e| e.to_string())
            }) {
                Ok(_) => {
                    unreadable.remove(plan_path);
                    covered.insert(w.output.clone());
                    counts.enqueued += 1;
                }
                Err(error) => {
                    unreadable.insert(plan_path.clone(), modified);
                    counts.unreadable_plans += 1;
                    tracing::warn!(event = "render.plan_unreadable", plan = %plan_path.display(), output = %w.output.display(), error = %error, "cannot queue the plan a scheduled file names; skipping it until the plan changes");
                }
            }
        }
        counts
    }
}

fn is_past(air_at: Option<OffsetDateTime>, now: OffsetDateTime) -> bool {
    air_at.is_some_and(|air| air <= now)
}

/// The queued job to render next: the one airing soonest, then any with no
/// air time; ties go to the one enqueued first. A job whose air time has come
/// is never picked.
pub fn pick_next(jobs: &[JobState], now: OffsetDateTime) -> Option<&JobState> {
    jobs.iter()
        .filter(|j| j.status == Status::Queued && !is_past(j.air_at, now))
        .min_by(|a, b| {
            (a.air_at.is_none(), a.air_at, a.enqueued_at, &a.id).cmp(&(
                b.air_at.is_none(),
                b.air_at,
                b.enqueued_at,
                &b.id,
            ))
        })
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
        status: if is_past(air_at, now) {
            Status::Missed
        } else {
            Status::Queued
        },
        output: plan.output.clone(),
        duration_s: plan.duration_s,
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

/// Render the queued job airing soonest, after marking any whose air time has
/// come as missed. Returns its final state, or `None` when nothing was waiting.
pub fn process_next(
    queue: &Queue,
    settings: &Settings,
    cap_bytes: u64,
    now: impl Fn() -> OffsetDateTime,
) -> Option<JobState> {
    let mut jobs = queue.list();
    let at = now();
    queue.mark_missed(&mut jobs, at);
    let mut job = pick_next(&jobs, at)?.clone();
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
        tracing::info!(event = "render.evicted", ids = ?evicted, "evicted renders over the cache cap");
    }
    Some(job)
}

/// What the schedule airs from a given instant through the sidecar's lead,
/// or why it could not be read.
pub type Schedule<'a> = dyn Fn(OffsetDateTime) -> Result<Vec<Wanted>, String> + 'a;

/// Drain the queue until `stop` is set, one job at a time, bringing the queue
/// in line with `schedule` at most once every `sync_every`.
pub fn run_sidecar(
    queue: &Queue,
    settings: &Settings,
    cap_bytes: u64,
    stop: &AtomicBool,
    poll: Duration,
    schedule: &Schedule<'_>,
    sync_every: Duration,
) {
    let recovered = queue.recover_interrupted();
    tracing::info!(event = "render.sidecar_start", queue = %queue.dir.display(), cache = %settings.cache_dir.display(), cap_bytes, recovered, "renderer watching its queue");
    let now = OffsetDateTime::now_utc;
    cache::enforce_cap(queue, settings, cap_bytes, now());
    let mut unreadable = UnreadablePlans::new();
    let mut synced: Option<std::time::Instant> = None;
    while !stop.load(Ordering::Relaxed) {
        if synced.is_none_or(|t| t.elapsed() >= sync_every) {
            synced = Some(std::time::Instant::now());
            let at = now();
            match schedule(at) {
                Ok(wanted) => {
                    let c = queue.sync(&wanted, at, &mut unreadable);
                    if c.enqueued + c.superseded + c.removed + c.unreadable_plans > 0 {
                        tracing::info!(
                            event = "render.sync",
                            wanted = c.wanted,
                            enqueued = c.enqueued,
                            superseded = c.superseded,
                            removed = c.removed,
                            unreadable_plans = c.unreadable_plans,
                            "brought the queue in line with the schedule"
                        );
                    }
                }
                Err(error) => {
                    tracing::warn!(event = "render.schedule_unreadable", error = %error, "cannot read the schedule; the queue is left as it is until the next pass");
                }
            }
        }
        if process_next(queue, settings, cap_bytes, now).is_none() {
            std::thread::sleep(poll);
        }
    }
}

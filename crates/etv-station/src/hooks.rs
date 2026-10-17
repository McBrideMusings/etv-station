//! Event hooks (ADR 0021): code the station runs at an event, which cannot
//! change the schedule. A hook's `run` is a Rhai script (`.rhai`, run on a
//! blocking thread with only the functions its `grants` name) or any other
//! executable (run as a child process, the event as JSON on its stdin).
//!
//! The station keeps no record of executions beyond the log line each run
//! writes (`event = "hook.run"`), so a script that starts outside work owns
//! its own deduplication.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ersatztv_channel::playback_events::PlaybackEvent;
use rhai::{Array, Dynamic, Engine, Map, Scope};
use time::OffsetDateTime;
use tokio::sync::Notify;

use crate::config::{HookDecl, HookGrant, HookTrigger, Station};

/// Longest a `http()` grant waits for a response.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest one run may take — an executable, or a script together with every
/// program it `exec`s — before it is killed. A run that outlives its interval
/// already skips the next one; this bounds how long a hung run can do that.
const RUN_TIMEOUT: Duration = Duration::from_secs(600);

/// Most bytes of a child's stdout or stderr kept in memory.
const OUTPUT_CAP: usize = 1 << 20;

/// Bytes of stderr carried into a failed run's log line.
const STDERR_LOGGED: usize = 1000;

/// Rhai operation budget for one script run: a `loop {}` ends with an error
/// instead of holding a worker thread forever.
const MAX_OPERATIONS: u64 = 100_000_000;

/// Where a hook was declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookScope {
    Station,
    Channel(String),
}

/// One configured hook with the context it needs to run.
#[derive(Debug, Clone)]
pub struct HookSpec {
    pub scope: HookScope,
    pub index: usize,
    pub decl: HookDecl,
    /// Directory of the config file that declared it; a relative `run`
    /// resolves against it.
    pub base_dir: PathBuf,
}

impl HookSpec {
    pub fn run_path(&self) -> PathBuf {
        self.decl.resolved_run(&self.base_dir)
    }

    /// The name a log line and the listing use: `station#0` or `channel:<name>#1`.
    pub fn label(&self) -> String {
        match &self.scope {
            HookScope::Station => format!("station#{}", self.index),
            HookScope::Channel(n) => format!("channel:{n}#{}", self.index),
        }
    }

    pub fn kind(&self) -> &'static str {
        if self.decl.is_rhai() { "rhai" } else { "exec" }
    }
}

/// Every hook the loaded config declares: the station's, then each channel's.
pub fn collect(station: &Station) -> Vec<HookSpec> {
    let mut specs = Vec::new();
    let station_dir = station
        .config_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_default();
    for (index, decl) in station.station.hooks.iter().enumerate() {
        specs.push(HookSpec {
            scope: HookScope::Station,
            index,
            decl: decl.clone(),
            base_dir: station_dir.clone(),
        });
    }
    for ch in &station.channels {
        let dir = ch
            .config_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_default();
        for (index, decl) in ch.config.hooks.iter().enumerate() {
            specs.push(HookSpec {
                scope: HookScope::Channel(ch.name.clone()),
                index,
                decl: decl.clone(),
                base_dir: dir.clone(),
            });
        }
    }
    specs
}

/// One line per hook, for `--list-hooks`.
pub fn describe(specs: &[HookSpec]) -> String {
    let mut out = String::new();
    for s in specs {
        let every = s
            .decl
            .every
            .map(|d| humantime::format_duration(d).to_string())
            .unwrap_or_else(|| "-".into());
        let grants = if s.decl.grants.is_empty() {
            "-".to_string()
        } else {
            s.decl
                .grants
                .iter()
                .map(|g| g.as_str())
                .collect::<Vec<_>>()
                .join(",")
        };
        out.push_str(&format!(
            "{}\ton={}\tevery={}\t{}\tgrants={}\trun={}\n",
            s.label(),
            s.decl.on.as_str(),
            every,
            s.kind(),
            grants,
            s.run_path().display(),
        ));
    }
    out
}

/// What one run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Process exit code; `0` for a Rhai script that finished; `-1` when the
    /// run never produced one (spawn failure, killed by a signal, script error).
    pub exit: i32,
    /// Why it failed, when it did.
    pub error: Option<String>,
}

impl Outcome {
    fn ok() -> Self {
        Outcome {
            exit: 0,
            error: None,
        }
    }
    fn failed(exit: i32, error: impl Into<String>) -> Self {
        Outcome {
            exit,
            error: Some(error.into()),
        }
    }
}

/// One firing of a trigger: what a hook is told when it runs.
#[derive(Debug, Clone)]
pub struct HookEvent {
    /// The channel the event happened on; `None` for a daemon-wide event
    /// (`startup`, `config_reload`, `cron`).
    pub channel: Option<String>,
    /// Trigger-specific fields, handed to the hook as `data`.
    pub data: serde_json::Value,
    pub fired_at: String,
}

impl HookEvent {
    pub fn now(channel: Option<String>, data: serde_json::Value) -> Self {
        HookEvent {
            channel,
            data,
            fired_at: OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
        }
    }
}

/// The event JSON an executable reads on stdin and a script sees as `event`.
/// `channel` is the channel the event happened on, else the channel that
/// declared the hook, else null.
fn event_json(spec: &HookSpec, event: &HookEvent) -> serde_json::Value {
    let (scope, declared) = match &spec.scope {
        HookScope::Station => ("station", None),
        HookScope::Channel(n) => ("channel", Some(n.clone())),
    };
    let channel = event.channel.clone().or(declared);
    serde_json::json!({
        "hook": spec.label(),
        "trigger": spec.decl.on.as_str(),
        "scope": scope,
        "channel": channel,
        "fired_at": event.fired_at,
        "data": event.data,
    })
}

/// Run a hook once, blocking, and log it. Callers on the async runtime go
/// through `spawn_blocking`.
pub fn execute(spec: &HookSpec, fired: &HookEvent) -> Outcome {
    let started = Instant::now();
    let event = event_json(spec, fired);
    let outcome = if spec.decl.is_rhai() {
        run_rhai(spec, &event)
    } else {
        run_exec(spec, &event)
    };
    let duration_ms = started.elapsed().as_millis() as u64;
    match &outcome.error {
        None => tracing::info!(
            event = "hook.run",
            hook = %spec.label(),
            trigger = spec.decl.on.as_str(),
            duration_ms,
            exit = outcome.exit,
            "hook finished",
        ),
        Some(err) => tracing::warn!(
            event = "hook.run",
            hook = %spec.label(),
            trigger = spec.decl.on.as_str(),
            duration_ms,
            exit = outcome.exit,
            error = %err,
            "hook failed",
        ),
    }
    outcome
}

fn run_exec(spec: &HookSpec, event: &serde_json::Value) -> Outcome {
    let path = spec.run_path();
    let mut cmd = Command::new(&path);
    match capture(&mut cmd, Some(event.to_string()), RUN_TIMEOUT) {
        Err(e) => Outcome::failed(-1, format!("spawn {}: {e}", path.display())),
        Ok(out) => {
            if !out.stdout.trim().is_empty() {
                tracing::debug!(event = "hook.stdout", hook = %spec.label(), stdout = %out.stdout.trim());
            }
            if out.timed_out {
                return Outcome::failed(-1, format!("killed after {}s", RUN_TIMEOUT.as_secs()));
            }
            match out.code {
                Some(0) => Outcome::ok(),
                Some(c) => Outcome::failed(c, format!("exited {c}: {}", tail(&out.stderr))),
                None => Outcome::failed(-1, format!("killed by a signal: {}", tail(&out.stderr))),
            }
        }
    }
}

/// The last `STDERR_LOGGED` bytes of a stream, trimmed, for an error message.
fn tail(text: &str) -> String {
    let t = text.trim();
    let start = t.len().saturating_sub(STDERR_LOGGED);
    let start = (start..=t.len())
        .find(|i| t.is_char_boundary(*i))
        .unwrap_or(t.len());
    t[start..].to_string()
}

struct Captured {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

/// Run `cmd` with optional stdin, collecting stdout and stderr (each capped at
/// `OUTPUT_CAP` bytes) on reader threads so a chatty child never blocks on a
/// full pipe, and kill it once `timeout` passes.
fn capture(
    cmd: &mut Command,
    stdin: Option<String>,
    timeout: Duration,
) -> std::io::Result<Captured> {
    use std::io::Read;
    let mut child = cmd
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // A hook that never reads its stdin closes the pipe early; that is not
        // a failure of the run.
        std::thread::spawn(move || {
            let _ = pipe.write_all(text.as_bytes());
        });
    }
    fn drain(mut r: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            while let Ok(n) = r.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                if buf.len() < OUTPUT_CAP {
                    buf.extend_from_slice(&chunk[..n]);
                }
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    }
    let out = child.stdout.take().map(drain);
    let err = child.stderr.take().map(drain);
    let deadline = Instant::now() + timeout;
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (Some(status), false);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break (None, true);
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    // After a kill, a grandchild may still hold the pipes open; joining the
    // readers would wait for it. Leave them to finish on their own.
    let collect = |h: Option<std::thread::JoinHandle<String>>| match h {
        Some(h) if !timed_out => h.join().unwrap_or_default(),
        _ => String::new(),
    };
    Ok(Captured {
        code: status.and_then(|s| s.code()),
        stdout: collect(out),
        stderr: collect(err),
        timed_out,
    })
}

fn run_rhai(spec: &HookSpec, event: &serde_json::Value) -> Outcome {
    let path = spec.run_path();
    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => return Outcome::failed(-1, format!("read {}: {e}", path.display())),
    };
    let engine = hook_engine(&spec.decl.grants, spec.label());
    let mut scope = Scope::new();
    match rhai::serde::to_dynamic(event) {
        Ok(d) => {
            scope.push_constant("event", d);
        }
        Err(e) => return Outcome::failed(-1, format!("event: {e}")),
    }
    match engine.run_with_scope(&mut scope, &source) {
        Ok(()) => Outcome::ok(),
        Err(e) => Outcome::failed(-1, format!("{}: {e}", path.display())),
    }
}

/// The engine a hook script runs in: `log(text)` and `parse_json(text)`
/// always; `exec` and `http` only when granted. A call to an ungranted
/// function is a script error ("function not found"), not a silent no-op.
fn hook_engine(grants: &[HookGrant], label: String) -> Engine {
    let mut engine = Engine::new();
    engine.set_max_expr_depths(128, 64);
    engine.set_max_operations(MAX_OPERATIONS);
    let log_label = label;
    engine.register_fn("log", move |text: &str| {
        tracing::info!(event = "hook.log", hook = %log_label, "{text}");
    });
    engine.register_fn("parse_json", crate::plugin_http::parse_json);
    if grants.contains(&HookGrant::Exec) {
        engine.register_fn("exec", exec_grant);
    }
    if grants.contains(&HookGrant::Http) {
        engine.register_fn("http", http_grant);
    }
    engine
}

/// `exec(program, args)` -> `#{ code, stdout, stderr }`.
fn exec_grant(program: &str, args: Array) -> Result<Map, Box<rhai::EvalAltResult>> {
    Ok(exec_call(program, args, RUN_TIMEOUT)?)
}

/// Run `program` with `args` and no stdin, killing it after `timeout`. The one
/// procedure behind a hook's granted `exec` and a bumper's `ctx.exec`. A spawn
/// failure and a timeout are errors; a nonzero exit is a result the script reads.
pub(crate) fn exec_call(program: &str, args: Array, timeout: Duration) -> Result<Map, String> {
    let args: Vec<String> = args.into_iter().map(|a| a.to_string()).collect();
    let out = capture(Command::new(program).args(&args), None, timeout)
        .map_err(|e| format!("exec {program}: {e}"))?;
    if out.timed_out {
        return Err(format!(
            "exec {program}: killed after {}s",
            timeout.as_secs()
        ));
    }
    let mut m = Map::new();
    m.insert("code".into(), Dynamic::from(out.code.unwrap_or(-1) as i64));
    m.insert("stdout".into(), Dynamic::from(out.stdout));
    m.insert("stderr".into(), Dynamic::from(out.stderr));
    Ok(m)
}

/// `http(url, method, body)` -> response body text. A 4xx/5xx is an error.
fn http_grant(url: &str, method: &str, body: &str) -> Result<String, Box<rhai::EvalAltResult>> {
    Ok(crate::plugin_http::call(url, method, body, HTTP_TIMEOUT)?)
}

/// The cron loop for one hook: sleep one `every`, run, repeat until `stop`.
/// A run still going when the next interval arrives makes that interval skip
/// (logged), so a slow hook never stacks up copies of itself.
pub async fn run_cron(spec: HookSpec, stop: Arc<Notify>) {
    let Some(every) = spec.decl.every else {
        return;
    };
    let spec = Arc::new(spec);
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut running: Option<tokio::task::JoinHandle<Outcome>> = None;
    loop {
        tokio::select! {
            biased;
            _ = stop.notified() => {
                // A reload starts a fresh loop for this hook; let the run in
                // flight finish first so the two never overlap.
                if let Some(h) = running.take() {
                    let _ = h.await;
                }
                break;
            }
            _ = interval.tick() => {
                if running.as_ref().is_some_and(|h| !h.is_finished()) {
                    tracing::warn!(
                        event = "hook.skipped",
                        hook = %spec.label(),
                        trigger = spec.decl.on.as_str(),
                        "previous run still going, skipping this interval",
                    );
                    continue;
                }
                let fired = HookEvent::now(None, serde_json::Value::Null);
                let s = spec.clone();
                running = Some(tokio::task::spawn_blocking(move || execute(&s, &fired)));
            }
        }
    }
}

/// Every hook of one config generation, and which of them are mid-run.
///
/// The daemon fires an event into it; each hook declared `on:` that trigger
/// runs on a blocking thread. A hook still running when its trigger fires
/// again waits its turn: firings queue and run one after another, so a slow
/// hook never runs two copies of itself and no boundary or file is lost. A daemon-wide event (no channel) reaches every hook of that
/// trigger; a channel event reaches the station's hooks and that channel's.
#[derive(Default)]
pub struct HookBus {
    slots: Vec<Slot>,
}

struct Slot {
    spec: Arc<HookSpec>,
    queue: Arc<Mutex<SlotQueue>>,
}

/// Firings waiting for a hook's current run to finish. `running` is true from
/// the moment a worker is spawned until it finds the queue empty.
#[derive(Default)]
struct SlotQueue {
    running: bool,
    pending: VecDeque<HookEvent>,
}

/// Most firings queued behind one slow hook; further ones are dropped (logged).
const QUEUE_CAP: usize = 1000;

impl HookBus {
    pub fn new(specs: Vec<HookSpec>) -> Self {
        HookBus {
            slots: specs
                .into_iter()
                .map(|spec| Slot {
                    spec: Arc::new(spec),
                    queue: Arc::default(),
                })
                .collect(),
        }
    }

    fn matches(slot: &Slot, trigger: HookTrigger, channel: Option<&str>) -> bool {
        slot.spec.decl.on == trigger
            && match (&slot.spec.scope, channel) {
                (_, None) | (HookScope::Station, _) => true,
                (HookScope::Channel(declared), Some(on)) => declared == on,
            }
    }

    /// Whether any hook would run for this trigger on this channel.
    pub fn wants(&self, trigger: HookTrigger, channel: Option<&str>) -> bool {
        self.slots
            .iter()
            .any(|s| Self::matches(s, trigger, channel))
    }

    /// Run every matching hook, each on its own blocking thread. Must be
    /// called inside the tokio runtime.
    pub fn fire(&self, trigger: HookTrigger, channel: Option<&str>, data: serde_json::Value) {
        for slot in self
            .slots
            .iter()
            .filter(|s| Self::matches(s, trigger, channel))
        {
            let event = HookEvent::now(channel.map(str::to_owned), data.clone());
            let start_worker = {
                let mut q = slot.queue.lock().unwrap();
                if q.pending.len() >= QUEUE_CAP {
                    tracing::warn!(
                        event = "hook.dropped",
                        hook = %slot.spec.label(),
                        trigger = trigger.as_str(),
                        queued = q.pending.len(),
                        "hook is far behind its events, dropping this firing",
                    );
                    continue;
                }
                q.pending.push_back(event);
                !std::mem::replace(&mut q.running, true)
            };
            if start_worker {
                let spec = slot.spec.clone();
                let queue = slot.queue.clone();
                tokio::task::spawn_blocking(move || {
                    loop {
                        let next = {
                            let mut q = queue.lock().unwrap();
                            let next = q.pending.pop_front();
                            if next.is_none() {
                                q.running = false;
                            }
                            next
                        };
                        let Some(event) = next else { break };
                        execute(&spec, &event);
                    }
                });
            }
        }
    }

    /// Drop firings still waiting behind a run, so a generation ending does
    /// not wait out a backlog.
    pub fn discard_queued(&self) {
        for slot in &self.slots {
            slot.queue.lock().unwrap().pending.clear();
        }
    }

    /// Wait for every run in flight, and anything queued, to finish. A reload
    /// calls this before the next generation starts, so two generations' runs
    /// of one hook never overlap.
    pub async fn drain(&self) {
        while self.slots.iter().any(|s| s.queue.lock().unwrap().running) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// One scheduled item as the schedule watcher sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Airing {
    pub id: String,
    pub start: OffsetDateTime,
    pub finish: OffsetDateTime,
    pub title: Option<String>,
}

impl Airing {
    fn json(&self) -> serde_json::Value {
        let rfc = &time::format_description::well_known::Rfc3339;
        serde_json::json!({
            "id": self.id,
            "title": self.title,
            "start": self.start.format(rfc).unwrap_or_default(),
            "finish": self.finish.format(rfc).unwrap_or_default(),
        })
    }
}

/// The item airing at `now` (latest start wins when chunks overlap) and the
/// next instant the answer can change: the soonest start or finish after `now`.
fn airing_at(items: &[Airing], now: OffsetDateTime) -> (Option<Airing>, Option<OffsetDateTime>) {
    let current = items
        .iter()
        .filter(|i| i.start <= now && now < i.finish)
        .max_by_key(|i| i.start)
        .cloned();
    let next = items
        .iter()
        .flat_map(|i| [i.start, i.finish])
        .filter(|t| *t > now)
        .min();
    (current, next)
}

/// Longest the watcher sleeps between looks at the playout files, so a
/// regenerated schedule is noticed even when the old next boundary was far off.
const WATCH_RECHECK: Duration = Duration::from_secs(30);

/// The items of the playout files around `now`: the chunks containing it and
/// the one after.
async fn read_airings(folder: &std::path::Path, now: OffsetDateTime) -> Vec<Airing> {
    let Ok(files) = crate::scan::scan_output_folder(folder).await else {
        return Vec::new();
    };
    let following = files.iter().find(|f| f.start > now).map(|f| f.path.clone());
    let mut paths: Vec<PathBuf> = files
        .iter()
        .filter(|f| f.start <= now && f.finish >= now)
        .map(|f| f.path.clone())
        .collect();
    paths.extend(following);
    let mut out = Vec::new();
    for path in paths {
        let Ok(bytes) = tokio::fs::read(&path).await else {
            continue;
        };
        let Ok(playout) = serde_json::from_slice::<ersatztv_playout::playout::Playout>(&bytes)
        else {
            continue;
        };
        out.extend(playout.items.into_iter().map(|i| Airing {
            id: i.id,
            start: i.start,
            finish: i.finish,
            title: i.program.and_then(|p| p.title),
        }));
    }
    out
}

/// Fire `schedule_item_change` each time the station clock passes an item
/// boundary on `channel`, whether or not anyone is watching. The item airing
/// when the watcher starts is the baseline and fires nothing.
pub async fn run_schedule_watch(
    bus: Arc<HookBus>,
    channel: String,
    folder: PathBuf,
    stop: Arc<Notify>,
) {
    let mut last: Option<Airing> = None;
    let mut baselined = false;
    loop {
        let now = OffsetDateTime::now_utc();
        let items = read_airings(&folder, now).await;
        let (current, next) = airing_at(&items, now);
        if let Some(cur) = &current {
            let changed = last
                .as_ref()
                .is_none_or(|l| (&l.id, l.start) != (&cur.id, cur.start));
            if baselined && changed {
                tracing::info!(
                    event = "schedule.item_change",
                    channel = %channel,
                    id = %cur.id,
                    start = %cur.start,
                    "station clock passed an item boundary",
                );
                bus.fire(
                    HookTrigger::ScheduleItemChange,
                    Some(&channel),
                    serde_json::json!({
                        "item": cur.json(),
                        "previous": last.as_ref().map(Airing::json),
                    }),
                );
            }
            last = Some(cur.clone());
        }
        baselined = true;
        let wait = next
            .map(|t| Duration::try_from(t - now).unwrap_or_default() + Duration::from_millis(50))
            .unwrap_or(WATCH_RECHECK)
            .min(WATCH_RECHECK);
        tokio::select! {
            biased;
            _ = stop.notified() => break,
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

/// How often the playback watcher looks for new lines in the worker's log.
const PLAYBACK_POLL: Duration = Duration::from_millis(500);

/// The complete lines of the playback log past `offset`, decoded, and the
/// offset after them. A log shorter than `offset` was dropped by the worker
/// and is read from its start; a missing log reads as empty. A trailing line
/// without its newline is left for the next look.
fn read_playback_events(path: &std::path::Path, offset: u64) -> (Vec<PlaybackEvent>, u64) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return (Vec::new(), 0);
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let offset = if len < offset { 0 } else { offset };
    let mut buf = Vec::new();
    if file.seek(SeekFrom::Start(offset)).is_err() || file.read_to_end(&mut buf).is_err() {
        return (Vec::new(), offset);
    }
    let Some(end) = buf.iter().rposition(|b| *b == b'\n') else {
        return (Vec::new(), offset);
    };
    let events = buf[..end]
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .filter_map(|line| match serde_json::from_slice(line) {
            Ok(event) => Some(event),
            Err(e) => {
                tracing::warn!(
                    event = "playback.bad_line",
                    path = %path.display(),
                    error = %e,
                    line = %String::from_utf8_lossy(line),
                    "skipping an unreadable playback log line",
                );
                None
            }
        })
        .collect();
    (events, offset + end as u64 + 1)
}

/// The trigger a playback log line fires, the event name it logs under, and
/// the hook's `data`.
fn playback_hook(event: &PlaybackEvent) -> (HookTrigger, &'static str, serde_json::Value) {
    let rfc = &time::format_description::well_known::Rfc3339;
    let stamp = |t: &OffsetDateTime| t.format(rfc).unwrap_or_default();
    match event {
        PlaybackEvent::SessionStarted { at, .. } => (
            HookTrigger::ViewerStart,
            "session_started",
            serde_json::json!({ "at": stamp(at) }),
        ),
        PlaybackEvent::SessionStopped { at, .. } => (
            HookTrigger::ViewerStop,
            "session_stopped",
            serde_json::json!({ "at": stamp(at) }),
        ),
        PlaybackEvent::ItemStarted {
            at,
            item_id,
            start,
            finish,
            title,
            ..
        } => (
            HookTrigger::PlaybackItemChange,
            "item_started",
            serde_json::json!({
                "at": stamp(at),
                "item": {
                    "id": item_id,
                    "title": title,
                    "start": stamp(start),
                    "finish": stamp(finish),
                },
            }),
        ),
    }
}

/// Tail the playback log ETV-next's channel worker writes into `folder` and
/// fire `playback_item_change`, `viewer_start` and `viewer_stop` for each new
/// line. Lines already in the log when the watcher starts are history and
/// fire nothing.
pub async fn run_playback_watch(
    bus: Arc<HookBus>,
    channel: String,
    folder: PathBuf,
    stop: Arc<Notify>,
) {
    let path = folder.join(ersatztv_core::PLAYBACK_EVENTS_FILE_NAME);
    let mut offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    loop {
        let (events, next) = read_playback_events(&path, offset);
        offset = next;
        for event in &events {
            let (trigger, name, data) = playback_hook(event);
            tracing::info!(
                event = "playback.event",
                channel = %channel,
                kind = name,
                data = %data,
                "channel worker reported playback",
            );
            bus.fire(trigger, Some(&channel), data);
        }
        tokio::select! {
            biased;
            _ = stop.notified() => break,
            _ = tokio::time::sleep(PLAYBACK_POLL) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HookTrigger;

    fn spec(dir: &std::path::Path, run: &str, grants: Vec<HookGrant>) -> HookSpec {
        HookSpec {
            scope: HookScope::Channel("news".into()),
            index: 1,
            decl: HookDecl {
                on: HookTrigger::Cron,
                every: Some(Duration::from_secs(60)),
                run: run.into(),
                grants,
            },
            base_dir: dir.to_path_buf(),
        }
    }

    fn ev(fired_at: &str) -> HookEvent {
        HookEvent {
            channel: None,
            data: serde_json::Value::Null,
            fired_at: fired_at.into(),
        }
    }

    #[cfg(unix)]
    fn write_script(path: &std::path::Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// `exec_call` kills a program that outlives its timeout and reports an error.
    #[test]
    fn exec_call_kills_a_program_past_its_timeout() {
        let started = Instant::now();
        let err = exec_call(
            "/bin/sh",
            vec![
                Dynamic::from("-c".to_string()),
                Dynamic::from("sleep 30".to_string()),
            ],
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert!(err.contains("killed after"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// An executable gets the event JSON on stdin and its exit code is reported.
    #[cfg(unix)]
    #[test]
    fn an_executable_reads_the_event_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("seen.json");
        write_script(
            &dir.path().join("h.sh"),
            &format!("#!/bin/sh\ncat > {}\nexit 3\n", out.display()),
        );
        let o = execute(
            &spec(dir.path(), "h.sh", vec![]),
            &ev("2026-01-01T00:00:00Z"),
        );
        assert_eq!(o.exit, 3);
        let seen: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(out).unwrap()).unwrap();
        assert_eq!(seen["hook"], "channel:news#1");
        assert_eq!(seen["trigger"], "cron");
        assert_eq!(seen["channel"], "news");
        assert_eq!(seen["fired_at"], "2026-01-01T00:00:00Z");
    }

    /// A Rhai script sees `event`, and `exec` runs only when granted.
    #[cfg(unix)]
    #[test]
    fn a_rhai_script_sees_the_event_and_exec_needs_its_grant() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("marker");
        std::fs::write(
            dir.path().join("h.rhai"),
            format!(
                "let r = exec(\"/bin/sh\", [\"-c\", \"printf '%s' \" + event.hook + \" > {}\"]);\nif r.code != 0 {{ throw \"bad\"; }}",
                out.display()
            ),
        )
        .unwrap();
        let denied = execute(&spec(dir.path(), "h.rhai", vec![]), &ev("t"));
        assert_eq!(denied.exit, -1);
        assert!(!out.exists());
        let granted = execute(&spec(dir.path(), "h.rhai", vec![HookGrant::Exec]), &ev("t"));
        assert_eq!(granted, Outcome::ok());
        assert_eq!(std::fs::read_to_string(out).unwrap(), "channel:news#1");
    }

    /// A run past the timeout is killed, and a failed run's stderr reaches the
    /// error that is logged.
    #[cfg(unix)]
    #[test]
    fn a_hung_program_is_killed_and_stderr_is_reported() {
        let mut hung = Command::new("/bin/sleep");
        hung.arg("30");
        let started = Instant::now();
        let out = capture(&mut hung, None, Duration::from_millis(200)).unwrap();
        assert!(out.timed_out);
        assert!(started.elapsed() < Duration::from_secs(5));

        let dir = tempfile::tempdir().unwrap();
        write_script(
            &dir.path().join("h.sh"),
            "#!/bin/sh\necho boom >&2\nexit 2\n",
        );
        let o = execute(&spec(dir.path(), "h.sh", vec![]), &ev("t"));
        assert_eq!(o.exit, 2);
        assert!(o.error.unwrap().contains("boom"));
    }

    /// An endless script ends at the operation budget instead of holding its
    /// worker thread.
    #[test]
    fn an_endless_script_is_stopped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("h.rhai"), "loop { }").unwrap();
        let o = execute(&spec(dir.path(), "h.rhai", vec![]), &ev("t"));
        assert_eq!(o.exit, -1);
        assert!(o.error.unwrap().contains("perations"));
    }

    fn on(trigger: HookTrigger, scope: HookScope, run: &str, dir: &std::path::Path) -> HookSpec {
        HookSpec {
            scope,
            index: 0,
            decl: HookDecl {
                on: trigger,
                every: None,
                run: run.into(),
                grants: vec![],
            },
            base_dir: dir.to_path_buf(),
        }
    }

    /// A daemon-wide event reaches hooks of every scope; a channel event
    /// reaches the station's hooks and only that channel's own.
    #[test]
    fn a_channel_event_reaches_only_its_channels_hooks() {
        let d = std::path::Path::new("/cfg");
        let bus = HookBus::new(vec![
            on(HookTrigger::ChunkWritten, HookScope::Station, "a.sh", d),
            on(
                HookTrigger::ChunkWritten,
                HookScope::Channel("news".into()),
                "b.sh",
                d,
            ),
            on(
                HookTrigger::ChunkWritten,
                HookScope::Channel("movies".into()),
                "c.sh",
                d,
            ),
            on(
                HookTrigger::Startup,
                HookScope::Channel("movies".into()),
                "d.sh",
                d,
            ),
        ]);
        let hits = |t, ch| {
            bus.slots
                .iter()
                .filter(|s| HookBus::matches(s, t, ch))
                .map(|s| s.spec.decl.run.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            hits(HookTrigger::ChunkWritten, Some("news")),
            ["a.sh", "b.sh"]
        );
        assert_eq!(hits(HookTrigger::Startup, None), ["d.sh"]);
        assert!(!bus.wants(HookTrigger::ScheduleItemChange, Some("news")));
    }

    /// A fired event runs the executable with the trigger, channel and data on
    /// stdin, and `drain` returns once it has finished.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_fired_event_reaches_the_executable_with_its_data() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("seen.json");
        write_script(
            &dir.path().join("h.sh"),
            &format!("#!/bin/sh\ncat > {}\n", out.display()),
        );
        let bus = HookBus::new(vec![on(
            HookTrigger::ChunkWritten,
            HookScope::Station,
            "h.sh",
            dir.path(),
        )]);
        bus.fire(
            HookTrigger::ChunkWritten,
            Some("news"),
            serde_json::json!({"file": "x.json"}),
        );
        bus.drain().await;
        let seen: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(out).unwrap()).unwrap();
        assert_eq!(seen["trigger"], "chunk_written");
        assert_eq!(seen["scope"], "station");
        assert_eq!(seen["channel"], "news");
        assert_eq!(seen["data"]["file"], "x.json");
    }

    fn airing(id: &str, start: i64, finish: i64) -> Airing {
        Airing {
            id: id.into(),
            start: OffsetDateTime::from_unix_timestamp(start).unwrap(),
            finish: OffsetDateTime::from_unix_timestamp(finish).unwrap(),
            title: None,
        }
    }

    #[test]
    fn the_airing_item_and_next_boundary_follow_the_clock() {
        let items = [
            airing("a", 0, 100),
            airing("b", 100, 250),
            airing("a", 100, 250),
        ];
        let at = |t| OffsetDateTime::from_unix_timestamp(t).unwrap();
        let (cur, next) = airing_at(&items, at(50));
        assert_eq!(cur.unwrap().id, "a");
        assert_eq!(next, Some(at(100)));
        let (cur, next) = airing_at(&items, at(100));
        assert_eq!(cur.unwrap().start, at(100));
        assert_eq!(next, Some(at(250)));
        assert_eq!(airing_at(&items, at(250)), (None, None));
    }

    /// The watcher fires once per boundary and stays silent for the item that
    /// is airing when it starts.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_watcher_fires_when_the_clock_passes_a_boundary() {
        use ersatztv_playout::playout::DATE_FORMAT;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("out");
        std::fs::create_dir(&folder).unwrap();
        let now = OffsetDateTime::now_utc();
        let t = |secs: i64| now + time::Duration::seconds(secs);
        let item = |id: &str, a: OffsetDateTime, b: OffsetDateTime| {
            serde_json::json!({
                "id": id,
                "start": a.format(&time::format_description::well_known::Rfc3339).unwrap(),
                "finish": b.format(&time::format_description::well_known::Rfc3339).unwrap(),
            })
        };
        let (from, to) = (t(-60), t(60));
        let name = format!(
            "{}_{}.json",
            from.format(&DATE_FORMAT).unwrap(),
            to.format(&DATE_FORMAT).unwrap()
        );
        let body = serde_json::json!({
            "version": "https://ersatztv.org/playout/version/0.0.1",
            "items": [item("one", t(-60), t(2)), item("two", t(2), t(60))],
        });
        std::fs::write(folder.join(name), body.to_string()).unwrap();
        let out = dir.path().join("log");
        write_script(
            &dir.path().join("h.sh"),
            &format!(
                "#!/bin/sh\ncat >> {}\necho >> {}\n",
                out.display(),
                out.display()
            ),
        );
        let bus = Arc::new(HookBus::new(vec![on(
            HookTrigger::ScheduleItemChange,
            HookScope::Station,
            "h.sh",
            dir.path(),
        )]));
        let stop = Arc::new(Notify::new());
        let watch = tokio::spawn(run_schedule_watch(
            bus.clone(),
            "news".into(),
            folder,
            stop.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(4000)).await;
        stop.notify_one();
        watch.await.unwrap();
        bus.drain().await;
        let text = std::fs::read_to_string(out).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 1, "{text}");
        assert_eq!(lines[0]["data"]["item"]["id"], "two");
        assert_eq!(lines[0]["data"]["previous"]["id"], "one");
        assert_eq!(lines[0]["channel"], "news");
    }

    fn playback_lines(ids: &[&str]) -> String {
        ids.iter()
            .map(|id| {
                format!(
                    "{{\"event\":\"item_started\",\"at\":\"2026-01-01T12:00:00Z\",\"channel\":\"7\",\"item_id\":\"{id}\",\"start\":\"2026-01-01T12:00:00Z\",\"finish\":\"2026-01-01T12:30:00Z\"}}\n"
                )
            })
            .collect()
    }

    /// The reader returns only lines past its offset, leaves a line still
    /// being written, and starts over when the worker dropped the log.
    #[test]
    fn the_playback_reader_resumes_at_its_offset_and_survives_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".playback-events.jsonl");
        assert_eq!(read_playback_events(&path, 0), (vec![], 0));

        let first = playback_lines(&["a"]);
        std::fs::write(&path, format!("{first}{{\"event\":\"item_st")).unwrap();
        let (events, offset) = read_playback_events(&path, 0);
        assert_eq!(events.len(), 1);
        assert_eq!(offset, first.len() as u64);

        std::fs::write(&path, format!("{first}{}", playback_lines(&["b"]))).unwrap();
        let (events, after) = read_playback_events(&path, offset);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], PlaybackEvent::ItemStarted { item_id, .. } if item_id == "b"));

        std::fs::write(&path, playback_lines(&["c"])).unwrap();
        let (events, _) = read_playback_events(&path, after);
        assert!(matches!(&events[0], PlaybackEvent::ItemStarted { item_id, .. } if item_id == "c"));
    }

    /// A line the worker appends after the watcher starts reaches a
    /// `playback_item_change` hook with the item in `data`; a line already in
    /// the log at start does not.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_worker_line_fires_playback_item_change_but_history_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("seen.jsonl");
        write_script(
            &dir.path().join("h.sh"),
            &format!(
                "#!/bin/sh\ncat >> {}\necho >> {}\n",
                out.display(),
                out.display()
            ),
        );
        let folder = dir.path().join("playout");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(
            folder.join(ersatztv_core::PLAYBACK_EVENTS_FILE_NAME),
            playback_lines(&["old"]),
        )
        .unwrap();
        let bus = Arc::new(HookBus::new(vec![on(
            HookTrigger::PlaybackItemChange,
            HookScope::Station,
            "h.sh",
            dir.path(),
        )]));
        let stop = Arc::new(Notify::new());
        let watch = tokio::spawn(run_playback_watch(
            bus.clone(),
            "news".into(),
            folder.clone(),
            stop.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        ersatztv_channel::playback_events::append(
            &folder,
            &serde_json::from_str(playback_lines(&["new"]).trim()).unwrap(),
        );
        tokio::time::sleep(Duration::from_millis(1200)).await;
        stop.notify_one();
        watch.await.unwrap();
        bus.drain().await;
        let text = std::fs::read_to_string(out).unwrap();
        let seen: Vec<serde_json::Value> = text
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(seen.len(), 1, "{text}");
        assert_eq!(seen[0]["trigger"], "playback_item_change");
        assert_eq!(seen[0]["channel"], "news");
        assert_eq!(seen[0]["data"]["item"]["id"], "new");
        assert_eq!(seen[0]["data"]["item"]["finish"], "2026-01-01T12:30:00Z");
    }

    #[test]
    fn describe_lists_every_hook() {
        let dir = std::path::Path::new("/cfg");
        let text = describe(&[spec(dir, "h.rhai", vec![HookGrant::Http])]);
        assert!(text.contains("channel:news#1"));
        assert!(text.contains("every=1m"));
        assert!(text.contains("rhai"));
        assert!(text.contains("grants=http"));
        assert!(text.contains("/cfg/h.rhai"));
    }
}

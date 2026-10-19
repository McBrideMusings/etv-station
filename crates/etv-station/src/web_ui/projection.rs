//! The Schedule's projections: a channel's schedule simulated forward from
//! the frontier of its real playout, cached by what the simulation depends on.
//!
//! A projection is never started by a page load alone. The request thread
//! hashes the inputs ([`digests`]) into a [`Request`] and asks
//! [`SimCache::lookup`], which answers from memory or from the cache directory,
//! or queues the job and answers [`Lookup::Pending`]. The worker thread
//! ([`SimCache::spawn`]) runs queued jobs one at a time.
//!
//! The key is a content hash of: the channel file, every `.rhai` script it
//! names plus the scripts those import (and the size and mtime of the
//! datastores a pool is granted, too large to read), `station.yaml`, the state
//! the projection starts from (the `.resume` sidecar and the recent airings),
//! the channel's resolved candidate set (entry ids and durations) and
//! [`crate::simulate::SIMULATOR_REVISION`]. No file mtime outside the datastore
//! stamp, no catalog mtime and no binary build time is in it, so the same
//! inputs give the same projection across deploys and across midnight. The
//! cache directory lives on the data volume beside the catalog.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration as StdDuration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::catalog::Catalog;
use crate::config::{self, ChannelConfig};
use crate::resume::ResumeMap;
use crate::simulate::{Ledger, SIMULATOR_REVISION, Simulation};

/// How many days past the frontier the Project ahead control offers.
pub const DAY_CHOICES: [u32; 4] = [1, 3, 7, 14];

/// How many projections stay in memory; the rest are read back from disk.
const MEMORY_SLOTS: usize = 8;

/// The most projection files the cache directory keeps.
const DISK_MAX_FILES: usize = 400;

/// The names of the inputs a projection is keyed on, in display order.
pub const INPUT_NAMES: [&str; 6] = [
    "config",
    "scripts",
    "station",
    "frontier",
    "candidates",
    "simulator",
];

/// Input name -> hex digest of its content.
pub type Digests = BTreeMap<String, String>;

/// One cached projection: the simulation plus what it was made from.
#[derive(Debug, Serialize, Deserialize)]
pub struct Projection {
    pub key: String,
    pub days: u32,
    /// The instant the projection starts from, unix seconds.
    pub frontier: i64,
    pub inputs: Digests,
    /// The inputs that differed from the channel's previous projection of the
    /// same length when this one was made; empty for the first.
    pub changed: Vec<String>,
    pub simulation: Simulation,
}

/// Everything one projection starts from, gathered by the request thread.
pub struct Request {
    pub channel: String,
    pub config_path: PathBuf,
    pub days: u32,
    pub frontier: OffsetDateTime,
    /// The channel's last airings, oldest first.
    pub tail: Vec<String>,
    pub cursor: BTreeMap<String, String>,
    pub show_cursor: BTreeMap<String, String>,
    pub resume: ResumeMap,
    pub digests: Digests,
}

impl Request {
    /// The cache key: the same inputs and length always give the same key.
    pub fn key(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.channel.as_bytes());
        h.update(format!("\0{}\0", self.days));
        for (name, digest) in &self.digests {
            h.update(format!("{name}={digest}\0"));
        }
        format!("{:x}", h.finalize())
    }
}

pub enum Lookup {
    Ready(Arc<Projection>),
    /// The projection ended in this error. Reported once; the next lookup retries.
    Failed(String),
    /// Queued or running; ask again shortly.
    Pending,
}

struct Job {
    key: String,
    request: Request,
    changed: Vec<String>,
}

#[derive(Default)]
struct Inner {
    /// Oldest first; at most [`MEMORY_SLOTS`].
    slots: VecDeque<Arc<Projection>>,
    failed: HashMap<String, String>,
    queue: VecDeque<Job>,
    running: Option<String>,
    /// The inputs of the last finished projection per (channel, days).
    last: HashMap<(String, u32), Digests>,
    /// Why the worker stopped for good; every lookup that misses reports it.
    dead: Option<String>,
}

pub struct SimCache {
    station_path: PathBuf,
    catalog_path: PathBuf,
    dir: PathBuf,
    inner: Mutex<Inner>,
    wake: Condvar,
}

/// What `GET /api/health` reports about the cache.
pub struct Stats {
    pub in_memory: usize,
    pub queued: usize,
    pub running: Option<String>,
}

fn sha_hex(parts: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p);
    }
    format!("{:x}", h.finalize())
}

fn mtime_ns(path: &Path) -> Option<u128> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
}

/// Every string in `value` that names a `.rhai` script.
fn collect_scripts(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) if s.ends_with(".rhai") => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|v| collect_scripts(v, out)),
        Value::Object(o) => o.values().for_each(|v| collect_scripts(v, out)),
        _ => {}
    }
}

/// Every `path` of a `datastores:` grant in `value`.
fn collect_datastores(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Array(a) => a.iter().for_each(|v| collect_datastores(v, out)),
        Value::Object(o) => {
            for (k, v) in o {
                if k == "datastores" {
                    if let Value::Array(grants) = v {
                        out.extend(
                            grants
                                .iter()
                                .filter_map(|g| g.get("path")?.as_str().map(str::to_string)),
                        );
                    }
                } else {
                    collect_datastores(v, out);
                }
            }
        }
        _ => {}
    }
}

/// The module names a script's `import "name"` statements load.
fn imported_modules(source: &str) -> Vec<String> {
    source
        .lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix("import")?;
            let rest = rest.trim_start().strip_prefix('"')?;
            Some(rest[..rest.find('"')?].to_string())
        })
        .collect()
}

/// `scripts` plus every module they import, transitively. rhai's file
/// resolver reads `<name>.rhai` relative to the working directory; the
/// channel file's directory is covered too.
fn with_imports(base: &Path, scripts: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut all: Vec<PathBuf> = Vec::new();
    let mut queue = scripts;
    while let Some(path) = queue.pop() {
        if all.contains(&path) {
            continue;
        }
        if let Ok(source) = std::fs::read_to_string(&path) {
            for name in imported_modules(&source) {
                let file = format!("{name}.rhai");
                queue.push(PathBuf::from(&file));
                queue.push(base.join(&file));
            }
        }
        all.push(path);
    }
    all
}

/// The `.rhai` scripts a channel names plus the scripts they import, sorted.
fn script_files(config_path: &Path, config: &ChannelConfig) -> Vec<PathBuf> {
    let base = config_path.parent().unwrap_or(Path::new("."));
    let mut names = Vec::new();
    if let Ok(v) = serde_json::to_value(config) {
        collect_scripts(&v, &mut names);
    }
    let mut scripts = with_imports(
        base,
        names
            .iter()
            .map(|n| crate::score::resolve_plugin_path(base, Path::new(n)))
            .collect(),
    );
    scripts.sort();
    scripts.dedup();
    scripts
}

/// The datastores (plexdb snapshots) a channel's pools are granted, sorted.
fn datastore_files(config: &ChannelConfig) -> Vec<PathBuf> {
    let mut grants = Vec::new();
    if let Ok(v) = serde_json::to_value(config) {
        collect_datastores(&v, &mut grants);
    }
    // Grants are stored as written (`${VAR}`); the plexdb file is the expanded path.
    let mut out: Vec<PathBuf> = grants
        .into_iter()
        .map(|p| PathBuf::from(config::expand_process_env(&p).unwrap_or(p)))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// What a projection starts from, as one digest: where it starts, the
/// `.resume` sidecar's bytes, the recent airings and the two cursors.
pub fn frontier_digest(
    frontier: OffsetDateTime,
    resume_bytes: &[u8],
    tail: &[String],
    cursor: &BTreeMap<String, String>,
    show_cursor: &BTreeMap<String, String>,
) -> String {
    let state = serde_json::json!({ "tail": tail, "cursor": cursor, "show_cursor": show_cursor });
    sha_hex(&[
        &frontier.unix_timestamp().to_le_bytes(),
        resume_bytes,
        state.to_string().as_bytes(),
    ])
}

/// The channel's resolved candidate set as one digest: entry ids with their
/// durations, in id order.
pub fn candidates_digest(mut candidates: Vec<(String, Option<i64>)>) -> String {
    candidates.sort();
    let text: String = candidates
        .iter()
        .map(|(id, ms)| format!("{id}:{}\n", ms.map(|m| m.to_string()).unwrap_or_default()))
        .collect();
    sha_hex(&[text.as_bytes()])
}

/// Every input of a projection, hashed by content. A missing file hashes as
/// absent, so creating or deleting one changes the digest too.
pub fn digests(
    station_path: &Path,
    config_path: &Path,
    config: &ChannelConfig,
    frontier: String,
    candidates: String,
) -> Digests {
    let file = |p: &Path| match std::fs::read(p) {
        Ok(bytes) => sha_hex(&[b"file", &bytes]),
        Err(_) => sha_hex(&[b"absent"]),
    };
    let mut scripts: Vec<String> = script_files(config_path, config)
        .iter()
        .map(|p| {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
            format!("{}={}", name.unwrap_or_default(), file(p))
        })
        .collect();
    scripts.extend(datastore_files(config).iter().map(|p| {
        let len = std::fs::metadata(p).map(|m| m.len()).ok();
        format!("{}={len:?}@{:?}", p.display(), mtime_ns(p))
    }));
    let scripts = sha_hex(&[scripts.join("\n").as_bytes()]);
    BTreeMap::from([
        ("config".to_string(), file(config_path)),
        ("scripts".to_string(), scripts),
        ("station".to_string(), file(station_path)),
        ("frontier".to_string(), frontier),
        ("candidates".to_string(), candidates),
        (
            "simulator".to_string(),
            sha_hex(&[&SIMULATOR_REVISION.to_le_bytes()]),
        ),
    ])
}

/// The input names whose digests differ between `old` and `new`, in display order.
pub fn changed_inputs(old: &Digests, new: &Digests) -> Vec<String> {
    INPUT_NAMES
        .iter()
        .filter(|name| old.get(**name) != new.get(**name))
        .map(|name| name.to_string())
        .collect()
}

impl SimCache {
    pub fn new(station_path: PathBuf, catalog_path: PathBuf, dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            station_path,
            catalog_path,
            dir,
            inner: Mutex::new(Inner::default()),
            wake: Condvar::new(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn disk_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    fn last_path(&self, channel: &str, days: u32) -> PathBuf {
        self.dir.join("last").join(format!(
            "{}.json",
            sha_hex(&[channel.as_bytes(), &days.to_le_bytes()])
        ))
    }

    /// The inputs of the last projection finished for (`channel`, `days`).
    fn last(&self, channel: &str, days: u32) -> Option<Digests> {
        if let Some(d) = self.lock().last.get(&(channel.to_string(), days)) {
            return Some(d.clone());
        }
        let bytes = std::fs::read(self.last_path(channel, days)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// The projection for `req` if it is cached; otherwise queue it and say so.
    pub fn lookup(&self, req: Request) -> Lookup {
        let key = req.key();
        {
            let mut g = self.lock();
            if let Some(p) = g.slots.iter().find(|p| p.key == key) {
                return Lookup::Ready(p.clone());
            }
            if let Some(msg) = g.failed.remove(&key) {
                return Lookup::Failed(msg);
            }
        }
        if let Some(p) = self.read_disk(&key) {
            self.remember(p.clone());
            return Lookup::Ready(p);
        }
        let changed = self
            .last(&req.channel, req.days)
            .map(|old| changed_inputs(&old, &req.digests))
            .unwrap_or_default();
        let mut g = self.lock();
        if let Some(why) = &g.dead {
            return Lookup::Failed(why.clone());
        }
        if g.running.as_deref() != Some(&key) && !g.queue.iter().any(|j| j.key == key) {
            g.queue.push_back(Job {
                key,
                request: req,
                changed,
            });
        }
        self.wake.notify_all();
        Lookup::Pending
    }

    fn read_disk(&self, key: &str) -> Option<Arc<Projection>> {
        let path = self.disk_path(key);
        let bytes = std::fs::read(&path).ok()?;
        match serde_json::from_slice::<Projection>(&bytes) {
            Ok(p) => Some(Arc::new(p)),
            Err(e) => {
                tracing::warn!(event = "ui.projection.disk_unreadable", path = %path.display(), error = %e, "projection cache file unreadable; dropped");
                let _ = std::fs::remove_file(&path);
                None
            }
        }
    }

    fn remember(&self, p: Arc<Projection>) {
        let mut g = self.lock();
        g.slots.retain(|s| s.key != p.key);
        g.slots.push_back(p);
        while g.slots.len() > MEMORY_SLOTS {
            g.slots.pop_front();
        }
    }

    /// Record a finished projection: memory, the cache directory, and the
    /// channel's "last inputs" pointer the next change notice compares against.
    fn store(&self, job: Job, result: Result<Simulation, String>) {
        match result {
            Ok(simulation) => {
                let projection = Arc::new(Projection {
                    key: job.key,
                    days: job.request.days,
                    frontier: job.request.frontier.unix_timestamp(),
                    inputs: job.request.digests.clone(),
                    changed: job.changed,
                    simulation,
                });
                self.write_disk(&projection);
                self.lock().last.insert(
                    (job.request.channel.clone(), job.request.days),
                    job.request.digests.clone(),
                );
                self.write_last(&job.request.channel, job.request.days, &job.request.digests);
                self.remember(projection);
            }
            Err(e) => {
                self.lock().failed.insert(job.key, e);
            }
        }
    }

    fn write_atomic(&self, path: &Path, bytes: Vec<u8>) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())
    }

    fn write_disk(&self, p: &Projection) {
        let path = self.disk_path(&p.key);
        let result = serde_json::to_vec(p)
            .map_err(|e| e.to_string())
            .and_then(|bytes| self.write_atomic(&path, bytes));
        match result {
            Ok(()) => super::channel::prune_still_dir(&self.dir, DISK_MAX_FILES),
            Err(e) => {
                tracing::warn!(event = "ui.projection.disk_write_failed", path = %path.display(), error = %e, "projection not cached on disk")
            }
        }
    }

    fn write_last(&self, channel: &str, days: u32, digests: &Digests) {
        let path = self.last_path(channel, days);
        if let Err(e) = serde_json::to_vec(digests)
            .map_err(|e| e.to_string())
            .and_then(|bytes| self.write_atomic(&path, bytes))
        {
            tracing::warn!(event = "ui.projection.disk_write_failed", path = %path.display(), error = %e, "projection pointer not written");
        }
    }

    pub fn stats(&self) -> Stats {
        let g = self.lock();
        Stats {
            in_memory: g.slots.len(),
            queued: g.queue.len(),
            running: g.running.clone(),
        }
    }

    /// Start the worker thread.
    pub fn spawn(self: &Arc<Self>) {
        let me = self.clone();
        let spawned = std::thread::Builder::new()
            .name("ui-projection".into())
            .spawn(move || me.run());
        if let Err(e) = spawned {
            tracing::error!(event = "ui.projection.spawn_failed", error = %e, "projection worker did not start");
        }
    }

    fn run(&self) {
        let catalog = match Catalog::open_readonly(&self.catalog_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(event = "ui.projection.catalog_failed", error = %e, "projection worker: catalog did not open");
                self.lock().dead = Some(format!("the projection worker has no catalog: {e}"));
                return;
            }
        };
        loop {
            let job = {
                let mut g = self.lock();
                loop {
                    if let Some(job) = g.queue.pop_front() {
                        g.running = Some(job.key.clone());
                        break job;
                    }
                    g = self
                        .wake
                        .wait_timeout(g, StdDuration::from_secs(30))
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
            };
            self.execute(job, &catalog);
            self.lock().running = None;
        }
    }

    fn execute(&self, job: Job, catalog: &Catalog) {
        let began = std::time::Instant::now();
        let req = &job.request;
        let result = self.simulate(req, catalog);
        tracing::info!(
            event = "ui.projection.done",
            channel = %req.channel,
            days = req.days,
            key = %job.key,
            ok = result.is_ok(),
            error = result.as_ref().err().map(String::as_str),
            changed = ?job.changed,
            ms = began.elapsed().as_millis() as u64,
            "projection finished"
        );
        self.store(job, result);
    }

    fn simulate(&self, req: &Request, catalog: &Catalog) -> Result<Simulation, String> {
        let station = config::load_for_inspection(&self.station_path)
            .map_err(|e| format!("load {}: {e}", self.station_path.display()))?;
        let lc = station
            .channels
            .iter()
            .find(|c| c.config_path == req.config_path)
            .ok_or_else(|| format!("{} is no longer in the lineup", req.channel))?;
        let tz = crate::tz::parse(&station.station.tz).map_err(|e| e.to_string())?;
        let account_id = super::tune::channel_account_id(&lc.config)?;
        let ledger = Ledger::seeded(
            req.tail.clone(),
            req.cursor.clone(),
            req.show_cursor.clone(),
        );
        // A panicking plugin fails this projection, not the worker.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::simulate::simulate_from(
                &lc.config,
                &lc.config_path,
                catalog,
                account_id,
                req.days,
                req.frontier,
                Some(tz),
                ledger,
                req.resume.clone(),
            )
        }))
        .unwrap_or_else(|_| Err("the projection panicked; see the log".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(tag: &str) -> Arc<SimCache> {
        let dir = std::env::temp_dir().join(format!("etv-projection-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        SimCache::new("station.yaml".into(), "catalog.db".into(), dir)
    }

    fn digests_of(config: &str) -> Digests {
        INPUT_NAMES
            .iter()
            .map(|n| {
                (
                    n.to_string(),
                    if *n == "config" { config } else { "x" }.to_string(),
                )
            })
            .collect()
    }

    fn request(channel: &str, days: u32, config: &str) -> Request {
        Request {
            channel: channel.into(),
            config_path: format!("{channel}.yaml").into(),
            days,
            frontier: OffsetDateTime::UNIX_EPOCH,
            tail: Vec::new(),
            cursor: BTreeMap::new(),
            show_cursor: BTreeMap::new(),
            resume: ResumeMap::new(),
            digests: digests_of(config),
        }
    }

    fn empty_sim() -> Simulation {
        Simulation {
            start: OffsetDateTime::UNIX_EPOCH,
            generations: Vec::new(),
        }
    }

    fn finish(c: &SimCache, req: Request) {
        let job = c.lock().queue.pop_front().expect("a queued job");
        assert_eq!(job.key, req.key());
        c.store(job, Ok(empty_sim()));
    }

    #[test]
    fn a_miss_queues_once_and_a_stored_result_serves() {
        let c = cache("queue");
        assert!(matches!(c.lookup(request("a", 7, "c1")), Lookup::Pending));
        assert!(matches!(c.lookup(request("a", 7, "c1")), Lookup::Pending));
        assert_eq!(c.lock().queue.len(), 1);
        finish(&c, request("a", 7, "c1"));
        assert!(matches!(c.lookup(request("a", 7, "c1")), Lookup::Ready(_)));
        let _ = std::fs::remove_dir_all(&c.dir);
    }

    #[test]
    fn the_key_depends_on_content_and_length_only() {
        assert_eq!(request("a", 7, "c1").key(), request("a", 7, "c1").key());
        assert_ne!(request("a", 7, "c1").key(), request("a", 7, "c2").key());
        assert_ne!(request("a", 7, "c1").key(), request("a", 3, "c1").key());
        assert_ne!(request("a", 7, "c1").key(), request("b", 7, "c1").key());
    }

    #[test]
    fn a_result_survives_a_restart_through_the_cache_directory() {
        let c = cache("disk");
        assert!(matches!(c.lookup(request("a", 7, "c1")), Lookup::Pending));
        finish(&c, request("a", 7, "c1"));
        let fresh = SimCache::new("station.yaml".into(), "catalog.db".into(), c.dir.clone());
        assert!(matches!(
            fresh.lookup(request("a", 7, "c1")),
            Lookup::Ready(_)
        ));
        let _ = std::fs::remove_dir_all(&c.dir);
    }

    #[test]
    fn a_changed_input_is_named_on_the_new_projection() {
        let c = cache("changed");
        assert!(matches!(c.lookup(request("a", 7, "c1")), Lookup::Pending));
        finish(&c, request("a", 7, "c1"));
        assert!(matches!(c.lookup(request("a", 7, "c2")), Lookup::Pending));
        finish(&c, request("a", 7, "c2"));
        let Lookup::Ready(p) = c.lookup(request("a", 7, "c2")) else {
            panic!("expected the new projection");
        };
        assert_eq!(p.changed, ["config"]);
        let Lookup::Ready(first) = c.lookup(request("a", 7, "c1")) else {
            panic!("expected the first projection");
        };
        assert!(first.changed.is_empty());
        let _ = std::fs::remove_dir_all(&c.dir);
    }

    #[test]
    fn imports_are_found_and_datastores_collected() {
        assert_eq!(
            imported_modules("import \"shared/util\" as u;\n  import \"b\" as b;\nlet x = 1;"),
            ["shared/util", "b"]
        );
        let v: Value =
            serde_json::json!({"pools":[{"datastores":[{"name":"d","path":"/x/plex.db"}]}]});
        let mut out = Vec::new();
        collect_datastores(&v, &mut out);
        assert_eq!(out, ["/x/plex.db"]);
    }

    #[test]
    fn a_failure_is_reported_once_then_retried() {
        let c = cache("fail");
        assert!(matches!(c.lookup(request("a", 7, "c1")), Lookup::Pending));
        let job = c.lock().queue.pop_front().unwrap();
        c.store(job, Err("boom".into()));
        assert!(matches!(c.lookup(request("a", 7, "c1")), Lookup::Failed(m) if m == "boom"));
        assert!(matches!(c.lookup(request("a", 7, "c1")), Lookup::Pending));
    }
}

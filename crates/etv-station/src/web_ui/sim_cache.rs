//! The Review view's simulation cache and the worker thread that fills it.
//!
//! Replaying a channel's schedule costs seconds to minutes (001-for-you ~217s,
//! 034-syfy ~18s), and the server answers requests on one thread. So the
//! request thread never simulates: it asks [`SimCache::lookup`], which answers
//! from memory or from the cache directory under the OS temp dir, or queues
//! the channel at the front of the worker's list and answers
//! [`Lookup::Pending`]. The worker thread ([`SimCache::spawn`]) simulates
//! queued channels first and otherwise the whole lineup in `station.yaml`
//! order, so most channels are ready before anyone opens them.
//!
//! A result is valid for one [`SimKey`] (channel file, days, start date) and
//! one [`Fingerprint`]: the mtimes of `station.yaml`, the channel file, every
//! `.rhai` script its config names plus the scripts those import, every
//! datastore (plexdb snapshot) a pool is granted, and the catalog (with its
//! WAL). An edit to any of them makes the next lookup miss.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration as StdDuration;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::catalog::Catalog;
use crate::config::{self, ChannelConfig};
use crate::simulate::Simulation;

/// How many simulations stay in memory; the rest are read back from disk.
const MEMORY_SLOTS: usize = 8;

/// The most simulation files the cache directory keeps.
const DISK_MAX_FILES: usize = 200;

/// How long the idle worker sleeps before it looks for edited files again.
const IDLE_RESCAN: StdDuration = StdDuration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SimKey {
    pub config_path: PathBuf,
    pub days: u32,
    /// The simulated span's first instant, unix seconds (a local midnight).
    pub start: i64,
}

/// File name and modified time (ns since the epoch; `None` for a missing
/// file) of everything a simulation reads.
pub type Fingerprint = Vec<(PathBuf, Option<u128>)>;

pub enum Lookup {
    Ready(Arc<Simulation>),
    /// The simulation ended in this error. Reported once; the next lookup retries.
    Failed(String),
    /// Queued for the worker; ask again shortly.
    Pending,
}

struct Slot {
    fingerprint: Fingerprint,
    sim: Arc<Simulation>,
}

#[derive(Default)]
struct Inner {
    /// Oldest first; at most [`MEMORY_SLOTS`].
    slots: VecDeque<(SimKey, Slot)>,
    failed: HashMap<SimKey, (Fingerprint, String)>,
    /// Channels a request asked for, next to simulate first.
    wanted: VecDeque<SimKey>,
    running: Option<SimKey>,
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

pub fn fingerprint(
    station_path: &Path,
    config_path: &Path,
    config: &ChannelConfig,
    catalog_path: &Path,
) -> Fingerprint {
    let base = config_path.parent().unwrap_or(Path::new("."));
    let value = serde_json::to_value(config).ok();
    let mut names = Vec::new();
    let mut datastores = Vec::new();
    if let Some(v) = &value {
        collect_scripts(v, &mut names);
        collect_datastores(v, &mut datastores);
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
    // Grants are stored as written (`${VAR}`); the plexdb file is the expanded path.
    let mut datastores: Vec<PathBuf> = datastores
        .into_iter()
        .map(|p| PathBuf::from(config::expand_process_env(&p).unwrap_or(p)))
        .collect();
    datastores.sort();
    datastores.dedup();
    let mut wal = catalog_path.as_os_str().to_owned();
    wal.push("-wal");

    // Position 1 must stay the channel file: `SimCache::step` reads its mtime from there.
    let mut paths = vec![station_path.to_path_buf(), config_path.to_path_buf()];
    paths.extend(scripts);
    paths.extend(datastores);
    paths.push(catalog_path.to_path_buf());
    paths.push(PathBuf::from(wal));
    paths
        .into_iter()
        .map(|p| {
            let m = mtime_ns(&p);
            (p, m)
        })
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

    fn disk_path(&self, key: &SimKey, fp: &Fingerprint) -> PathBuf {
        let digest = Sha256::digest(format!("{key:?}{fp:?}").as_bytes());
        self.dir.join(format!("{digest:x}.json"))
    }

    /// The simulation for `key` if it is cached under `fp`; otherwise queue
    /// the channel ahead of the background fill and say so.
    pub fn lookup(&self, key: &SimKey, fp: &Fingerprint) -> Lookup {
        {
            let mut g = self.lock();
            if let Some((_, slot)) = g
                .slots
                .iter()
                .find(|(k, s)| k == key && s.fingerprint == *fp)
            {
                return Lookup::Ready(slot.sim.clone());
            }
            if g.failed.get(key).is_some_and(|(f, _)| f == fp) {
                let (_, msg) = g.failed.remove(key).expect("checked above");
                return Lookup::Failed(msg);
            }
        }
        if let Some(sim) = self.read_disk(key, fp) {
            self.remember(key.clone(), fp.clone(), sim.clone());
            return Lookup::Ready(sim);
        }
        let mut g = self.lock();
        if let Some(why) = &g.dead {
            return Lookup::Failed(why.clone());
        }
        if g.running.as_ref() != Some(key) {
            g.wanted.retain(|k| k != key);
            g.wanted.push_front(key.clone());
        }
        self.wake.notify_all();
        Lookup::Pending
    }

    fn read_disk(&self, key: &SimKey, fp: &Fingerprint) -> Option<Arc<Simulation>> {
        let path = self.disk_path(key, fp);
        let bytes = std::fs::read(&path).ok()?;
        match serde_json::from_slice::<Simulation>(&bytes) {
            Ok(sim) => Some(Arc::new(sim)),
            Err(e) => {
                tracing::warn!(event = "ui.sim.disk_unreadable", path = %path.display(), error = %e, "simulation cache file unreadable; dropped");
                let _ = std::fs::remove_file(&path);
                None
            }
        }
    }

    fn remember(&self, key: SimKey, fp: Fingerprint, sim: Arc<Simulation>) {
        let mut g = self.lock();
        g.slots
            .retain(|(k, _)| !(k.config_path == key.config_path && k.days == key.days));
        g.slots.push_back((
            key,
            Slot {
                fingerprint: fp,
                sim,
            },
        ));
        while g.slots.len() > MEMORY_SLOTS {
            g.slots.pop_front();
        }
    }

    /// Record a finished simulation: memory, and the cache directory.
    fn store(&self, key: SimKey, fp: Fingerprint, result: Result<Simulation, String>) {
        match result {
            Ok(sim) => {
                self.write_disk(&key, &fp, &sim);
                self.remember(key, fp, Arc::new(sim));
            }
            Err(e) => {
                self.lock().failed.insert(key, (fp, e));
            }
        }
    }

    fn write_disk(&self, key: &SimKey, fp: &Fingerprint, sim: &Simulation) {
        let path = self.disk_path(key, fp);
        let result = (|| -> Result<(), String> {
            std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
            let bytes = serde_json::to_vec(sim).map_err(|e| e.to_string())?;
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
            std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
        })();
        match result {
            Ok(()) => super::channel::prune_still_dir(&self.dir, DISK_MAX_FILES),
            Err(e) => {
                tracing::warn!(event = "ui.sim.disk_write_failed", path = %path.display(), error = %e, "simulation not cached on disk")
            }
        }
    }

    /// True when `key` needs no work under `fp`: held in memory, failed, or on disk.
    fn has(&self, key: &SimKey, fp: &Fingerprint) -> bool {
        {
            let g = self.lock();
            if g.slots
                .iter()
                .any(|(k, s)| k == key && s.fingerprint == *fp)
                || g.failed.get(key).is_some_and(|(f, _)| f == fp)
            {
                return true;
            }
        }
        self.disk_path(key, fp).is_file()
    }

    pub fn stats(&self) -> Stats {
        let g = self.lock();
        Stats {
            in_memory: g.slots.len(),
            queued: g.wanted.len(),
            running: g
                .running
                .as_ref()
                .map(|k| k.config_path.display().to_string()),
        }
    }

    /// Start the worker thread.
    pub fn spawn(self: &Arc<Self>, default_days: u32) {
        let me = self.clone();
        let spawned = std::thread::Builder::new()
            .name("ui-sim".into())
            .spawn(move || me.run(default_days));
        if let Err(e) = spawned {
            tracing::error!(event = "ui.sim.spawn_failed", error = %e, "simulation worker did not start");
        }
    }

    fn run(&self, default_days: u32) {
        let catalog = match Catalog::open_readonly(&self.catalog_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(event = "ui.sim.catalog_failed", error = %e, "simulation worker: catalog did not open");
                self.lock().dead = Some(format!("the simulation worker has no catalog: {e}"));
                return;
            }
        };
        let mut accounts: HashMap<PathBuf, (Option<u128>, Option<i64>)> = HashMap::new();
        loop {
            if self.step(&catalog, default_days, &mut accounts) {
                continue;
            }
            let g = self.lock();
            if g.wanted.is_empty() {
                let _ = self.wake.wait_timeout(g, IDLE_RESCAN);
            }
        }
    }

    /// Simulate one channel: the one a request asked for, else the first
    /// lineup channel without a result. False when there is nothing to do.
    fn step(
        &self,
        catalog: &Catalog,
        default_days: u32,
        accounts: &mut HashMap<PathBuf, (Option<u128>, Option<i64>)>,
    ) -> bool {
        let wanted = self.lock().wanted.pop_front();
        let station = match config::load_for_inspection(&self.station_path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(event = "ui.sim.station_failed", error = %e, "simulation worker: station config did not load");
                return false;
            }
        };
        let Ok(tz) = crate::tz::parse(&station.station.tz) else {
            return false;
        };
        let today = crate::tz::local_midnight_at_or_before(time::OffsetDateTime::now_utc(), tz);
        let default_key = |path: &Path| SimKey {
            config_path: path.to_path_buf(),
            days: default_days,
            start: today.unix_timestamp(),
        };

        let (key, lc, fp) = match wanted {
            Some(key) => {
                let Some(lc) = station
                    .channels
                    .iter()
                    .find(|c| c.config_path == key.config_path)
                else {
                    return true;
                };
                let fp = fingerprint(
                    &self.station_path,
                    &lc.config_path,
                    &lc.config,
                    &self.catalog_path,
                );
                (key, lc, fp)
            }
            None => {
                let mut found = None;
                for path in &station.lineup {
                    let Some(lc) = station.channels.iter().find(|c| c.config_path == *path) else {
                        continue;
                    };
                    let fp = fingerprint(
                        &self.station_path,
                        &lc.config_path,
                        &lc.config,
                        &self.catalog_path,
                    );
                    let key = default_key(path);
                    if !self.has(&key, &fp) {
                        found = Some((key, lc, fp));
                        break;
                    }
                }
                match found {
                    Some(f) => f,
                    None => return false,
                }
            }
        };
        if self.has(&key, &fp) {
            return true;
        }

        self.lock().running = Some(key.clone());
        let began = std::time::Instant::now();
        let channel_mtime = fp.get(1).and_then(|(_, m)| *m);
        let account = match accounts.get(&lc.config_path) {
            Some((m, id)) if *m == channel_mtime => Ok(*id),
            _ => super::pools::channel_account_id(&lc.config).inspect(|id| {
                accounts.insert(lc.config_path.clone(), (channel_mtime, *id));
            }),
        };
        let result = account.and_then(|account_id| {
            let start =
                time::OffsetDateTime::from_unix_timestamp(key.start).map_err(|e| e.to_string())?;
            // A panicking plugin fails this channel, not the worker.
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::simulate::simulate(
                    &lc.config,
                    &lc.config_path,
                    catalog,
                    account_id,
                    key.days,
                    start,
                    Some(tz),
                )
            }))
            .unwrap_or_else(|_| Err("the simulation panicked; see the log".to_string()))
        });
        tracing::info!(
            event = "ui.sim.done",
            channel = %super::lineup::channel_key(&key.config_path),
            days = key.days,
            ok = result.is_ok(),
            error = result.as_ref().err().map(String::as_str),
            ms = began.elapsed().as_millis() as u64,
            "simulation finished"
        );
        self.store(key, fp, result);
        self.lock().running = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(tag: &str) -> Arc<SimCache> {
        let dir = std::env::temp_dir().join(format!("etv-simcache-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        SimCache::new("station.yaml".into(), "catalog.db".into(), dir)
    }

    fn key(name: &str) -> SimKey {
        SimKey {
            config_path: name.into(),
            days: 14,
            start: 1_000,
        }
    }

    fn fp(mtime: u128) -> Fingerprint {
        vec![("channel.yaml".into(), Some(mtime))]
    }

    fn empty_sim() -> Simulation {
        Simulation {
            start: time::OffsetDateTime::UNIX_EPOCH,
            generations: Vec::new(),
        }
    }

    #[test]
    fn a_miss_queues_the_channel_and_a_jumped_to_one_moves_to_the_front() {
        let c = cache("queue");
        assert!(matches!(c.lookup(&key("a"), &fp(1)), Lookup::Pending));
        assert!(matches!(c.lookup(&key("b"), &fp(1)), Lookup::Pending));
        assert!(matches!(c.lookup(&key("a"), &fp(1)), Lookup::Pending));
        let order: Vec<_> = c
            .lock()
            .wanted
            .iter()
            .map(|k| k.config_path.clone())
            .collect();
        assert_eq!(order, [PathBuf::from("a"), PathBuf::from("b")]);
    }

    #[test]
    fn a_stored_result_serves_until_a_file_mtime_changes() {
        let c = cache("fp");
        c.store(key("a"), fp(1), Ok(empty_sim()));
        assert!(matches!(c.lookup(&key("a"), &fp(1)), Lookup::Ready(_)));
        assert!(matches!(c.lookup(&key("a"), &fp(2)), Lookup::Pending));
        let _ = std::fs::remove_dir_all(&c.dir);
    }

    #[test]
    fn a_result_survives_a_restart_through_the_cache_directory() {
        let c = cache("disk");
        c.store(key("a"), fp(1), Ok(empty_sim()));
        let fresh = SimCache::new("station.yaml".into(), "catalog.db".into(), c.dir.clone());
        assert!(matches!(fresh.lookup(&key("a"), &fp(1)), Lookup::Ready(_)));
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
        c.store(key("a"), fp(1), Err("boom".into()));
        assert!(matches!(c.lookup(&key("a"), &fp(1)), Lookup::Failed(m) if m == "boom"));
        assert!(matches!(c.lookup(&key("a"), &fp(1)), Lookup::Pending));
    }
}

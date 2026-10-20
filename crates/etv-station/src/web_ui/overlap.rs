//! What the review's overlap section reads: every lineup channel's candidate
//! entry ids (its fingerprint, resolved from its config with no scorer run)
//! and the catalog's movie and episode ids.
//!
//! Resolving a fingerprint costs a few hundred milliseconds a channel, so a
//! whole lineup costs seconds. A request never resolves one. The worker thread
//! ([`OverlapCache::spawn`]) fills the cache as soon as the server starts, then
//! sleeps until a lookup finds a channel file newer than its fingerprint, or a
//! failure older than [`FAILED_RETRY`]. A lookup ([`OverlapCache::lookup`]) answers from
//! the cache or says how far the worker has got.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration as StdDuration, Instant, SystemTime};

use crate::catalog::Catalog;
use crate::config;

/// How long after a pass a failed fingerprint or library waits before a
/// lookup wakes the worker to retry it, so a channel that cannot resolve does
/// not cost a whole pass per page view.
const FAILED_RETRY: StdDuration = StdDuration::from_secs(60);

/// A channel's candidate entry ids.
pub type Fingerprint = Arc<HashSet<String>>;

/// Every movie id and every episode id in the catalog, for telling a channel
/// that draws from the whole library apart from one that doesn't.
pub type Library = Arc<(HashSet<String>, HashSet<String>)>;

#[derive(Default)]
struct Inner {
    library: Option<Result<Library, String>>,
    /// Keyed by channel file, with the file's mtime when it was resolved.
    fingerprints: HashMap<PathBuf, (SystemTime, Result<Fingerprint, String>)>,
    /// A lookup found something stale since the worker's last pass.
    wanted: bool,
    running: bool,
    /// When the worker's last pass finished.
    last_pass: Option<Instant>,
    /// Why the worker stopped for good.
    dead: Option<String>,
}

pub struct OverlapCache {
    station_path: PathBuf,
    catalog_path: PathBuf,
    inner: Mutex<Inner>,
    wake: Condvar,
}

/// What [`OverlapCache::lookup`] found for the channels it was asked about.
pub enum Lookup {
    /// Every channel's fingerprint (or its failure) at its file's current
    /// mtime, in the order asked.
    Ready {
        library: Library,
        fingerprints: Vec<Result<Fingerprint, String>>,
    },
    /// The worker has not resolved them all yet: `ready` of `total` are.
    Pending {
        ready: usize,
        total: usize,
    },
    Failed(String),
}

/// What `GET /api/health` reports about the cache.
pub struct Stats {
    pub fingerprints: usize,
    pub running: bool,
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl OverlapCache {
    pub fn new(station_path: PathBuf, catalog_path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            station_path,
            catalog_path,
            inner: Mutex::new(Inner::default()),
            wake: Condvar::new(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The library and each of `channels`' fingerprints, or how far the
    /// worker has got. A missing or stale entry wakes the worker, and so does
    /// a failure once [`FAILED_RETRY`] has passed since the last pass.
    pub fn lookup(&self, channels: &[&Path]) -> Lookup {
        let mtimes: Vec<Option<SystemTime>> = channels.iter().map(|p| file_mtime(p)).collect();
        let mut g = self.lock();
        if let Some(why) = &g.dead {
            return Lookup::Failed(why.clone());
        }
        // A channel file that cannot be read has no fingerprint to wait for.
        let current: Vec<Option<Result<Fingerprint, String>>> = channels
            .iter()
            .zip(&mtimes)
            .map(|(path, mtime)| match (mtime, g.fingerprints.get(*path)) {
                (None, _) => Some(Err(format!("cannot read {}", path.display()))),
                (Some(m), Some((at, result))) if at == m => Some(result.clone()),
                _ => None,
            })
            .collect();
        let ready = current.iter().filter(|c| c.is_some()).count();
        let stale = ready < channels.len() || g.library.is_none();
        let any_failed =
            current.iter().any(|c| matches!(c, Some(Err(_)))) || matches!(g.library, Some(Err(_)));
        let retry_due = g.last_pass.is_none_or(|t| t.elapsed() >= FAILED_RETRY);
        if stale || (any_failed && retry_due) {
            g.wanted = true;
            self.wake.notify_all();
        }
        match &g.library {
            Some(Ok(library)) if !stale => Lookup::Ready {
                library: library.clone(),
                fingerprints: current.into_iter().flatten().collect(),
            },
            Some(Err(e)) => Lookup::Failed(e.clone()),
            _ => Lookup::Pending {
                ready,
                total: channels.len(),
            },
        }
    }

    pub fn stats(&self) -> Stats {
        let g = self.lock();
        Stats {
            fingerprints: g.fingerprints.len(),
            running: g.running,
        }
    }

    /// Start the worker thread. Its first pass fills the cache for the whole
    /// lineup. A panic in it marks the cache dead, so lookups report it
    /// rather than wait.
    pub fn spawn(self: &Arc<Self>) {
        let me = self.clone();
        let spawned = std::thread::Builder::new()
            .name("ui-overlap".into())
            .spawn(move || {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| me.run())).is_err() {
                    tracing::error!(event = "ui.overlap.panicked", "overlap worker panicked");
                    let mut g = me.lock();
                    g.running = false;
                    g.dead = Some("the overlap worker panicked".into());
                }
            });
        if let Err(e) = spawned {
            tracing::error!(event = "ui.overlap.spawn_failed", error = %e, "overlap worker did not start");
            self.lock().dead = Some(format!("the overlap worker did not start: {e}"));
        }
    }

    fn run(&self) {
        let catalog = match Catalog::open_readonly(&self.catalog_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(event = "ui.overlap.catalog_failed", error = %e, "overlap worker: catalog did not open");
                self.lock().dead = Some(format!("the overlap worker has no catalog: {e}"));
                return;
            }
        };
        loop {
            self.lock().running = true;
            self.pass(&catalog);
            let mut g = self.lock();
            g.running = false;
            while !g.wanted {
                g = self
                    .wake
                    .wait_timeout(g, StdDuration::from_secs(30))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            g.wanted = false;
        }
    }

    /// Resolve the library if it is missing or failed, then every lineup
    /// channel whose fingerprint is missing, stale or failed.
    fn pass(&self, catalog: &Catalog) {
        let began = Instant::now();
        let result = self.resolve_lineup(catalog);
        self.lock().last_pass = Some(Instant::now());
        match result {
            Ok((channels, resolved, failed)) => tracing::info!(
                event = "ui.overlap.pass",
                channels,
                resolved,
                failed,
                ms = began.elapsed().as_millis() as u64,
                "overlap worker: fingerprints resolved"
            ),
            Err(e) => {
                tracing::warn!(event = "ui.overlap.failed", error = %e, "overlap worker: station did not load")
            }
        }
    }

    /// The pass's work: how many lineup channels there were, how many were
    /// resolved and how many of those failed.
    fn resolve_lineup(&self, catalog: &Catalog) -> Result<(usize, usize, usize), String> {
        // A channel file changed at or after this instant may differ from the
        // config just loaded, so its result is not cached under its mtime.
        let loaded_at = SystemTime::now();
        let station = config::load_for_inspection(&self.station_path).map_err(|e| e.to_string())?;
        if !matches!(self.lock().library, Some(Ok(_))) {
            let library = resolve_library(catalog);
            if let Err(e) = &library {
                tracing::warn!(event = "ui.overlap.failed", error = %e, "overlap worker: library ids did not resolve");
            }
            self.lock().library = Some(library);
        }
        let mut resolved = 0;
        let mut failed = 0;
        for lc in &station.channels {
            let Some(mtime) = file_mtime(&lc.config_path) else {
                continue;
            };
            let fresh = matches!(
                self.lock().fingerprints.get(&lc.config_path),
                Some((at, Ok(_))) if *at == mtime
            );
            if fresh {
                continue;
            }
            let result = crate::resolve::resolve_channel_fingerprint_ids(
                &lc.config,
                &lc.config_path,
                &[],
                None,
                Some(catalog),
            )
            .map(|ids| Arc::new(ids.into_iter().collect::<HashSet<_>>()))
            .map_err(|e| e.to_string());
            resolved += 1;
            if let Err(e) = &result {
                failed += 1;
                tracing::warn!(event = "ui.overlap.channel_failed", channel = %lc.config_path.display(), error = %e, "overlap worker: fingerprint did not resolve");
            }
            let mut g = self.lock();
            if mtime >= loaded_at {
                g.wanted = true;
            } else {
                g.fingerprints
                    .insert(lc.config_path.clone(), (mtime, result));
            }
        }
        Ok((station.channels.len(), resolved, failed))
    }
}

fn resolve_library(catalog: &Catalog) -> Result<Library, String> {
    let movies = catalog
        .resolve_query(r#"item.type == "movie""#)
        .map_err(|e| e.to_string())?;
    let episodes = catalog
        .resolve_query(r#"item.type == "episode""#)
        .map_err(|e| e.to_string())?;
    Ok(Arc::new((
        movies.into_iter().collect(),
        episodes.into_iter().collect(),
    )))
}

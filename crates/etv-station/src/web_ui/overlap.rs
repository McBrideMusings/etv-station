//! What the review's overlap section reads: every lineup channel's candidate
//! entry ids (its fingerprint, resolved from its config with no scorer run)
//! and the catalog's movie and episode ids.
//!
//! Resolving a fingerprint costs a few hundred milliseconds a channel, so a
//! whole lineup costs seconds. A request never resolves one. The worker thread
//! ([`OverlapCache::spawn`]) fills the cache as soon as the server starts, then
//! sleeps until a lookup finds a channel file newer than its fingerprint or a
//! fingerprint that failed. A lookup ([`OverlapCache::lookup`]) answers from
//! the cache or says how far the worker has got.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration as StdDuration, SystemTime};

use crate::catalog::Catalog;
use crate::config;

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
    Pending { ready: usize, total: usize },
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
    /// worker has got. A stale or failed entry wakes the worker.
    pub fn lookup(&self, channels: &[&Path]) -> Lookup {
        let mtimes: Vec<Option<SystemTime>> = channels.iter().map(|p| file_mtime(p)).collect();
        let mut g = self.lock();
        if let Some(why) = &g.dead {
            return Lookup::Failed(why.clone());
        }
        let current: Vec<Option<&Result<Fingerprint, String>>> = channels
            .iter()
            .zip(&mtimes)
            .map(|(path, mtime)| match g.fingerprints.get(*path) {
                Some((at, result)) if Some(*at) == *mtime => Some(result),
                _ => None,
            })
            .collect();
        let ready = current.iter().filter(|c| c.is_some()).count();
        let any_failed = current.iter().any(|c| matches!(c, Some(Err(_))))
            || matches!(g.library, Some(Err(_)));
        let answer = match &g.library {
            Some(Ok(library)) if ready == channels.len() => Lookup::Ready {
                library: library.clone(),
                fingerprints: current
                    .into_iter()
                    .map(|c| c.expect("all ready").clone())
                    .collect(),
            },
            Some(Err(e)) => Lookup::Failed(e.clone()),
            _ => Lookup::Pending {
                ready,
                total: channels.len(),
            },
        };
        if !matches!(answer, Lookup::Ready { .. }) || any_failed {
            g.wanted = true;
            self.wake.notify_all();
        }
        answer
    }

    pub fn stats(&self) -> Stats {
        let g = self.lock();
        Stats {
            fingerprints: g.fingerprints.len(),
            running: g.running,
        }
    }

    /// Start the worker thread. Its first pass fills the cache for the whole
    /// lineup.
    pub fn spawn(self: &Arc<Self>) {
        let me = self.clone();
        let spawned = std::thread::Builder::new()
            .name("ui-overlap".into())
            .spawn(move || me.run());
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
        let began = std::time::Instant::now();
        let station = match config::load_for_inspection(&self.station_path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(event = "ui.overlap.failed", error = %e, "overlap worker: station did not load");
                return;
            }
        };
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
            self.lock()
                .fingerprints
                .insert(lc.config_path.clone(), (mtime, result));
        }
        tracing::info!(
            event = "ui.overlap.pass",
            channels = station.channels.len(),
            resolved,
            failed,
            ms = began.elapsed().as_millis() as u64,
            "overlap worker: fingerprints resolved"
        );
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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ersatztv_playout::playout::OverlaySpec as PlayoutOverlaySpec;
use time::OffsetDateTime;
use time_tz::Tz;
use tokio::select;
use tokio::sync::Notify;
use tracing::Instrument;

use crate::anchor;
use crate::catalog::Catalog;
use crate::config::{LoadedChannel, Station};
use crate::duration::DurationCache;
use crate::emit::emit_window;
use crate::errors::{ConfigError, StationError};
use crate::overlay_supervisor;
use crate::rule::LoopForever;
use crate::scan;
use crate::tz as tzmod;

pub async fn run(station: Station) -> Result<(), StationError> {
    let config_path = station.config_path.clone();
    let shutdown = Arc::new(Notify::new());
    let reload = Arc::new(Notify::new());
    spawn_signal_listener(shutdown.clone(), reload.clone());

    // Each pass of this loop runs one "generation" of channel + overlay tasks
    // against the current config. SIGHUP re-reads the config from disk and
    // starts a fresh generation; SIGTERM/SIGINT stops the current generation
    // and exits. The first generation runs the config passed in at startup.
    let mut station = Arc::new(station);
    // Open + populate the station-wide catalog once, before the first generation,
    // so it survives reloads (#96). A catalog-free station keeps working: query /
    // non-`manual` channels just error in `resolve_channel` as before.
    let catalog = open_and_ingest_catalog(&station).await?;
    // What the catalog was opened against — the catalog is opened once and NOT
    // reopened on reload (#96), so a later reload that changes these diverges.
    let opened_catalog_path = station.station.catalog_path.clone();
    let opened_source_roots = station.station.source_roots.clone();
    // The last config that prepared cleanly. A `prepare_generation` failure on a
    // reload reverts to this instead of killing a daemon that's streaming fine.
    // See #90 and docs/adr/0001-reload-generation-revert.md.
    let mut last_good: Option<Arc<Station>> = None;
    loop {
        let tz = match prepare_generation(&station).await {
            Ok(tz) => {
                last_good = Some(station.clone());
                tz
            }
            // First generation (nothing good yet) → fail loud at startup. A
            // reload whose config won't prepare reverts to the last-known-good
            // and re-spawns it. If the config we reverted TO also fails to
            // prepare (same Arc), the environment is unrecoverable → exit.
            Err(e) => match &last_good {
                Some(good) if !Arc::ptr_eq(good, &station) => {
                    tracing::error!(
                        event = "config.reload_reverted",
                        error = %e,
                        "generation failed to prepare on reload; reverting to last-known-good config",
                    );
                    station = good.clone();
                    continue;
                }
                _ => return Err(e),
            },
        };

        // Spawn the generation, run it until a signal, then tear it down. The
        // whole generation is joined before we return here, which is what lets
        // the `station` Arc be swapped safely below (no task still reads it).
        let (do_reload, first_err) =
            run_generation(&station, tz, catalog.as_ref(), &shutdown, &reload).await;

        // On shutdown a channel that failed on its own becomes the daemon's exit
        // error. `channel_loop` only returns `Err` from its startup section
        // (duration probing, anchor load, catch-up emit); roll-tick errors are
        // logged and retried, never returned. On reload we do NOT treat that
        // error as fatal — the failing channel gets another startup attempt next
        // generation, so a transient probe/media error must not tear down an
        // otherwise-healthy daemon (it was already logged in `run_generation`).
        if !do_reload {
            return first_err.map_or(Ok(()), Err);
        }

        // SIGHUP: re-read the config from disk. A malformed edit that won't even
        // parse keeps the previous config running. A config that parses but
        // can't be prepared (bad tz/overlay, uncreatable folder) is caught the
        // next iteration by `prepare_generation`, which reverts — so the
        // runnable-check lives in exactly one place.
        match crate::config::load(&config_path) {
            Ok(s) => {
                if catalog.is_some()
                    && (s.station.catalog_path != opened_catalog_path
                        || s.station.source_roots != opened_source_roots)
                {
                    tracing::warn!(
                        event = "config.reload_catalog_divergent",
                        "reload changes catalog_path/source_roots, but the catalog is opened once at startup and is not reopened; the running catalog and its path index still reflect the config it was opened with — restart to apply",
                    );
                }
                tracing::info!(event = "config.reload", config = %config_path.display(), "configuration reloaded");
                station = Arc::new(s);
            }
            Err(e) => {
                tracing::error!(
                    event = "config.reload_failed",
                    error = %e,
                    config = %config_path.display(),
                    "configuration reload failed to parse; keeping previous config running",
                );
            }
        }
    }
}

/// Open the station catalog (if `catalog_path` is set) and populate it with a
/// full ingest pass at startup, returning a shareable handle for the channel
/// tasks. `None` → the station is catalog-free and only inline `manual` channels
/// resolve. Opening is fatal (a broken db must not be silently ignored); an
/// ingest failure is logged and the daemon continues with whatever was written —
/// a Plex outage or a bad media root shouldn't take playout down. Delta sync,
/// the refresh TTL, and a manual re-ingest trigger are follow-ups (#91/#96).
/// The station catalog shared into every channel task: the `Catalog` behind a
/// `Mutex` (it is `Send` but `!Sync`, and only ever locked for a synchronous
/// resolve) plus the canonical-path → `entry_id` index built **once** after
/// ingest (the catalog is immutable afterwards), so channels don't each rebuild
/// it under the lock.
struct SharedCatalog {
    catalog: Mutex<Catalog>,
    path_index: HashMap<String, String>,
}

async fn open_and_ingest_catalog(
    station: &Station,
) -> Result<Option<Arc<SharedCatalog>>, StationError> {
    let Some(path) = station
        .station
        .catalog_path
        .as_deref()
        .filter(|p| !p.trim().is_empty())
    else {
        return Ok(None);
    };

    let mut catalog = Catalog::open(path)?;
    let source_roots = &station.station.source_roots;

    // Local filesystem: scan the media roots (identity is canonicalised against
    // the same roots).
    let fs_roots: Vec<PathBuf> = source_roots.iter().map(PathBuf::from).collect();
    match crate::catalog::ingest::fs::ingest_roots(&catalog, &fs_roots, source_roots).await {
        Ok(stats) => tracing::info!(
            event = "catalog.ingest.fs",
            entries = stats.entries_written,
            sources = stats.sources_written,
            "local-fs catalog ingest complete",
        ),
        Err(e) => {
            tracing::error!(event = "catalog.ingest.fs_failed", error = %e, "local-fs catalog ingest failed; continuing")
        }
    }

    // Plex: only when both env vars are set — an unconfigured Plex is normal, not
    // an error. The client is blocking (`ureq`), so run it on a blocking thread
    // (moving the catalog in and back out) rather than stalling the async
    // runtime. `spawn_blocking` works on any runtime flavor, unlike
    // `block_in_place`.
    if std::env::var_os("PLEX_URL").is_some() && std::env::var_os("PLEX_TOKEN").is_some() {
        let roots = source_roots.clone();
        let (returned, plex) = tokio::task::spawn_blocking(move || {
            let result = crate::catalog::ingest::plex::ingest_from_env(&catalog, &roots);
            (catalog, result)
        })
        .await
        .expect("plex ingest task panicked");
        catalog = returned;
        match plex {
            Ok(stats) => tracing::info!(
                event = "catalog.ingest.plex",
                entries = stats.entries_written,
                sources = stats.sources_written,
                "plex catalog ingest complete",
            ),
            Err(e) => {
                tracing::error!(event = "catalog.ingest.plex_failed", error = %e, "plex catalog ingest failed; continuing")
            }
        }
    }

    // Build the path-match index once, now that the catalog is fully ingested.
    let roots: Vec<&str> = source_roots.iter().map(String::as_str).collect();
    let path_index = crate::catalog::ingest::canonical_index(&catalog, &roots)?;

    Ok(Some(Arc::new(SharedCatalog {
        catalog: Mutex::new(catalog),
        path_index,
    })))
}

/// Spawn one generation's channel + overlay tasks against `station`, run until a
/// shutdown or reload signal, then stop every task and join it. Returns
/// `(do_reload, first_err)`: whether the signal was a reload (vs. shutdown), and
/// the first channel startup error seen (logged here regardless of which). The
/// whole generation is joined before returning, so the caller can safely swap
/// the `station` Arc for the next generation with no task still reading it.
/// `run` is the sole caller and sole waiter on both signals, so `notify_one`'s
/// stored permit makes the wait race-free without an explicit `enable()`.
async fn run_generation(
    station: &Arc<Station>,
    tz: &'static Tz,
    catalog: Option<&Arc<SharedCatalog>>,
    shutdown: &Notify,
    reload: &Notify,
) -> (bool, Option<StationError>) {
    let mut handles = Vec::new();
    let mut supervisor_handles = Vec::new();
    // One stop signal per spawned task. `notify_one` stores a permit if the task
    // is not yet parked, so a reload that races a slow generation startup
    // (duration probing, catch-up emit) is never lost — unlike `notify_waiters`,
    // which only wakes already-parked waiters.
    let mut stops: Vec<Arc<Notify>> = Vec::new();
    for idx in 0..station.channels.len() {
        if let Some(ctx) = build_overlay_context(&station.channels[idx]) {
            let stop = Arc::new(Notify::new());
            stops.push(stop.clone());
            let name = station.channels[idx].name.clone();
            supervisor_handles.push(tokio::spawn(async move {
                tracing::info!(
                    event = "overlay.start",
                    channel = %name,
                    config = %ctx.overlay_config.display(),
                    fifo = %ctx.fifo_path.display(),
                    "starting overlay supervisor",
                );
                overlay_supervisor::run(ctx, stop).await;
            }));
        }
        let s = station.clone();
        let cat = catalog.cloned();
        let stop = Arc::new(Notify::new());
        stops.push(stop.clone());
        let channel_name = station.channels[idx].name.clone();
        // One span per channel wraps the whole channel loop, so every event it
        // emits (roll ticks, chunk writes, retention deletes) carries the channel
        // in its span context for correlation.
        let span = tracing::info_span!("channel", channel = %channel_name);
        handles.push(tokio::spawn(
            async move {
                let ch = &s.channels[idx];
                let result =
                    channel_loop(ch, tz, &s.station.source_roots, cat.as_deref(), stop).await;
                // A channel_loop error is fatal to THIS channel. Task handles
                // are only joined at shutdown (see below), so without logging
                // here a startup failure — a failed duration probe, unreadable
                // media, anchor or emit error — would stay invisible until the
                // daemon stops: the channel silently emits nothing while the
                // rest of the daemon reports healthy. Log it at the point of
                // failure so it is immediately diagnosable.
                if let Err(err) = &result {
                    tracing::error!(
                        event = "channel.failed",
                        channel = %ch.name,
                        error = %err,
                        "channel loop exited with error; this channel will emit no further output until the daemon reloads or restarts",
                    );
                }
                (ch.name.clone(), result)
            }
            .instrument(span),
        ));
    }

    // `biased` makes shutdown win if both signals are pending.
    let do_reload = select! {
        biased;
        _ = shutdown.notified() => false,
        _ = reload.notified() => true,
    };

    // Stop this generation and wait for every task to wind down: channel loops
    // return Ok on the stop branch; overlay supervisors kill their subprocess and
    // remove the fifo + ready marker.
    for stop in &stops {
        stop.notify_one();
    }

    let mut first_err: Option<StationError> = None;
    for h in handles {
        match h.await {
            Ok((name, Ok(()))) => {
                tracing::info!(event = "channel.exit", channel = %name, "channel loop exited cleanly");
            }
            Ok((_name, Err(e))) => {
                // Already logged at the point of failure inside the task
                // (event = "channel.failed"); here we only capture the first
                // error to surface through the daemon's exit code.
                first_err.get_or_insert(e);
            }
            Err(e) => {
                tracing::error!(event = "channel.panic", error = %e, "channel task panicked");
                first_err.get_or_insert_with(|| StationError::Task(format!("{e}")));
            }
        }
    }
    for h in supervisor_handles {
        if let Err(e) = h.await {
            tracing::warn!(event = "overlay.supervisor_error", error = %e, "overlay supervisor task ended with error");
        }
    }

    (do_reload, first_err)
}

/// Install the signal handlers that drive the daemon: SIGTERM/SIGINT request
/// shutdown, SIGHUP requests a config reload. Both notify the single waiter in
/// `run` via `notify_one`, so a signal delivered before `run` parks is not lost.
fn spawn_signal_listener(shutdown: Arc<Notify>, reload: Arc<Notify>) {
    use tokio::signal::unix::{SignalKind, signal};

    tokio::spawn(async move {
        // `./tools/kill-dev.sh` and most container orchestrators send SIGTERM,
        // not SIGINT — handle both so the generation's stop path always runs and
        // cleans up the etv-overlay subprocess + its fifo.
        let mut sigterm = signal(SignalKind::terminate())
            .map_err(|e| {
                tracing::error!(event = "signal.handler_failed", signal = "SIGTERM", error = %e, "failed to install SIGTERM handler; relying on SIGINT only");
            })
            .ok();
        let mut sighup = signal(SignalKind::hangup())
            .map_err(|e| {
                tracing::error!(event = "signal.handler_failed", signal = "SIGHUP", error = %e, "failed to install SIGHUP handler; config reload via signal disabled");
            })
            .ok();

        loop {
            select! {
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!(event = "signal.shutdown", signal = "SIGINT", "ctrl-c received, shutting down");
                    shutdown.notify_one();
                    return;
                }
                _ = recv_signal(sigterm.as_mut()) => {
                    tracing::info!(event = "signal.shutdown", signal = "SIGTERM", "sigterm received, shutting down");
                    shutdown.notify_one();
                    return;
                }
                _ = recv_signal(sighup.as_mut()) => {
                    tracing::info!(event = "signal.reload", signal = "SIGHUP", "sighup received, reloading config");
                    reload.notify_one();
                }
            }
        }
    });
}

/// Await one delivery of an optional Unix signal. A `None` handler (one that
/// failed to install) never fires, so the corresponding `select!` arm is inert.
async fn recv_signal(sig: Option<&mut tokio::signal::unix::Signal>) {
    match sig {
        Some(s) => {
            s.recv().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// Run every check and side effect a generation needs before its channel and
/// overlay tasks spawn: parse the station tz, validate each channel's overlay
/// spec, and create each channel's `output_folder`. The single home for the "is
/// this config runnable" gate — `run` calls it once per generation on both the
/// startup and reload paths, so a check added here can never be silently skipped
/// on one path (the split that let the #34 mkdir slip past the reload gate; see
/// #90 and docs/adr/0001-reload-generation-revert.md).
async fn prepare_generation(station: &Station) -> Result<&'static Tz, StationError> {
    let tz = tzmod::parse(&station.station.tz)?;
    validate_overlay_configs(station)?;
    // Create every channel's output_folder before any task spawns — a fresh
    // deploy on empty volumes needs it in place before etv-next's canonicalize
    // reads it and before the overlay supervisor opens its fifo underneath (#34).
    for channel in &station.channels {
        let output = &channel.output_folder;
        tokio::fs::create_dir_all(output)
            .await
            .map_err(|source| StationError::Io {
                path: output.clone(),
                source,
            })?;
    }
    Ok(tz)
}

async fn wipe_emitted_playout(channel: &LoadedChannel) -> Result<(), StationError> {
    let files = scan::scan_output_folder(&channel.output_folder).await?;
    let count = files.len();
    for f in &files {
        if let Err(source) = tokio::fs::remove_file(&f.path).await
            && source.kind() != std::io::ErrorKind::NotFound
        {
            return Err(StationError::Io {
                path: f.path.clone(),
                source,
            });
        }
    }
    if count > 0 {
        tracing::info!(
            event = "playout.wipe",
            channel = %channel.name,
            removed = count,
            "wiped existing playout JSON on startup; will regenerate from config",
        );
    }
    Ok(())
}

/// Resolve the (overlay_config_path, fifo_path) pair for a channel, if it has
/// an overlay configured. Both `build_overlay_context` and
/// `load_overlay_playout_spec` need the same resolution.
fn resolve_overlay_paths(channel: &LoadedChannel) -> Option<(PathBuf, PathBuf)> {
    let cfg = channel.config.overlay.as_ref()?;
    let overlay_config =
        overlay_supervisor::resolve_overlay_config(&channel.config_path, &cfg.config);
    let fifo_path =
        overlay_supervisor::resolve_fifo_path(&channel.output_folder, cfg.fifo_path.as_deref());
    Some((overlay_config, fifo_path))
}

fn build_overlay_context(channel: &LoadedChannel) -> Option<overlay_supervisor::OverlayContext> {
    let (overlay_config, fifo_path) = resolve_overlay_paths(channel)?;
    Some(overlay_supervisor::OverlayContext {
        channel_name: channel.name.clone(),
        output_folder: channel.output_folder.clone(),
        overlay_config,
        fifo_path,
    })
}

/// Parse every channel's overlay config up front so a malformed TOML fails the
/// daemon at startup instead of silently emitting playout JSON without an
/// overlay spec while the supervisor crash-loops on the same broken file.
fn validate_overlay_configs(station: &Station) -> Result<(), StationError> {
    for channel in &station.channels {
        let Some((overlay_config_path, _)) = resolve_overlay_paths(channel) else {
            continue;
        };
        etv_overlay::overlay_spec::OverlaySpec::from_path(&overlay_config_path).map_err(|e| {
            ConfigError::Validation {
                path: overlay_config_path.clone(),
                message: format!("overlay config for channel '{}': {e}", channel.name),
            }
        })?;
    }
    Ok(())
}

fn load_overlay_playout_spec(channel: &LoadedChannel) -> Option<PlayoutOverlaySpec> {
    let (overlay_config_path, fifo_path) = resolve_overlay_paths(channel)?;
    match etv_overlay::overlay_spec::OverlaySpec::from_path(&overlay_config_path) {
        Ok(spec) => Some(PlayoutOverlaySpec {
            fifo_path: fifo_path.to_string_lossy().into_owned(),
            pixel_format: String::from(spec.pixel_format.ffmpeg_arg()),
            width: spec.width,
            height: spec.height,
            framerate: spec.framerate,
            x: 0,
            y: 0,
        }),
        Err(e) => {
            // validate_overlay_configs parsed this at startup, so we only land
            // here if the file changed between startup and channel init.
            tracing::error!(
                event = "overlay.spec_error",
                channel = %channel.name,
                error = %e,
                config = %overlay_config_path.display(),
                "overlay config re-parse failed after startup validation; emitting playout without overlay spec",
            );
            None
        }
    }
}

async fn channel_loop(
    channel: &LoadedChannel,
    tz: &'static Tz,
    source_roots: &[String],
    catalog: Option<&SharedCatalog>,
    shutdown: Arc<Notify>,
) -> Result<(), StationError> {
    // `run` creates every channel's output_folder synchronously before spawning
    // this task (see #34), so it exists by the time we scan/emit into it here.
    let output = &channel.output_folder;

    // A pattern channel advances its pools every generation, so its resolved
    // list is different each time and the loop-a-fixed-list-from-an-anchor
    // model doesn't apply. It runs its own emission loop instead (#72).
    if channel.config.is_pattern() {
        return pattern_channel_loop(channel, tz, source_roots, catalog, shutdown).await;
    }

    // Resolve this channel's items. With a catalog, `query` entries and
    // non-`manual` order resolve and manual items path-match onto catalog
    // identities; without one, only inline-item `manual` channels resolve (others
    // error clearly in `resolve_channel`). The lock is held only across the
    // synchronous resolve — never an await — so `std::sync::Mutex` is fine. The
    // guard lives inside this block, so it is dropped before the first await below.
    let items = {
        // Recover from a poisoned lock rather than panicking: nothing is ever
        // written through this mutex (the catalog is fully ingested before it is
        // shared), so the guarded data is still valid if another channel task
        // panicked while resolving. Asserting here instead would cascade one
        // panic into every channel — and, since the catalog outlives reloads,
        // into every future generation too. The pre-built `path_index` needs no
        // lock (immutable after ingest).
        let guard = catalog.map(|sc| sc.catalog.lock().unwrap_or_else(|e| e.into_inner()));
        crate::resolve::resolve_channel(
            &channel.config,
            &channel.config_path,
            source_roots,
            catalog.map(|sc| &sc.path_index),
            guard.as_deref(),
        )?
    };

    let mut cache = DurationCache::load(output).await?;
    let (durations, stats) = cache.resolve_all(&items).await?;
    cache.save(output).await?;
    tracing::info!(
        event = "durations.resolve",
        channel = %channel.name,
        from_cache = stats.from_cache,
        from_probe = stats.from_probe,
        from_config = stats.from_config,
        items = items.len(),
        "resolved item durations",
    );

    let now_utc = OffsetDateTime::now_utc();
    let anchor_state = anchor::load_or_initialize(output, &items, now_utc, tz).await?;
    if anchor_state.initialized_now {
        tracing::info!(
            event = "anchor.init",
            channel = %channel.name,
            anchor = %anchor_state.anchor_utc,
            "anchored at first run",
        );
    } else if anchor_state.re_anchored {
        tracing::warn!(
            event = "anchor.reanchor",
            channel = %channel.name,
            anchor = %anchor_state.anchor_utc,
            "items changed; re-anchored",
        );
    } else {
        tracing::info!(
            event = "anchor.load",
            channel = %channel.name,
            anchor = %anchor_state.anchor_utc,
            "loaded anchor",
        );
    }

    let overlay_spec = load_overlay_playout_spec(channel);
    let rule = LoopForever::new(&items, &durations).with_overlay(overlay_spec);

    // SHARP EDGE: Wipe every emitted playout JSON on startup and regenerate
    // from the (possibly updated) channel config. This catches changes to the
    // overlay spec, item list, or any other field that flows into the JSON.
    // See https://github.com/McBrideMusings/etv-station/issues/53 for the
    // proper fix (in-place rewrite or change-detection).
    wipe_emitted_playout(channel).await?;

    // Startup catch-up: emit from max(now, highest_existing_finish) up to now+window_days.
    emit_catch_up(channel, &rule, anchor_state.anchor_utc, tz, "startup").await?;

    let mut interval = tokio::time::interval(channel.config.roll_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await; // consume immediate tick

    loop {
        select! {
            _ = shutdown.notified() => {
                tracing::info!(event = "channel.shutdown", channel = %channel.name, "shutdown received");
                return Ok(());
            }
            _ = interval.tick() => {
                // One span per roll tick; chunk emission opens a sub-span inside
                // emit_catch_up, so chunk.write events correlate to their tick.
                let tick = async {
                    if let Err(err) =
                        emit_catch_up(channel, &rule, anchor_state.anchor_utc, tz, "roll").await
                    {
                        tracing::error!(
                            event = "roll.error",
                            channel = %channel.name,
                            error = %err,
                            "roll tick failed; will retry next interval",
                        );
                    }
                };
                tick.instrument(tracing::info_span!("roll_tick")).await;
            }
        }
    }
}

/// Bound on how many generations one catch-up will chain before giving up, so
/// a pathological config (a sequence with no wall-clock length) can't spin.
const MAX_GENERATIONS_PER_TICK: usize = 512;

/// Delete emitted playout files that begin at or after `from`, leaving anything
/// already airing or aired in place. Returns how many were removed.
async fn wipe_playout_from(
    channel: &LoadedChannel,
    from: OffsetDateTime,
) -> Result<usize, StationError> {
    let files = scan::scan_output_folder(&channel.output_folder).await?;
    let mut removed = 0;
    for f in files.iter().filter(|f| f.start >= from) {
        match tokio::fs::remove_file(&f.path).await {
            Ok(()) => removed += 1,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(StationError::Io {
                    path: f.path.clone(),
                    source,
                });
            }
        }
    }
    Ok(removed)
}

/// The emission loop for a pattern channel (#72): **materialize forward**.
///
/// The looping model can't work here. `LoopForever` repeats one fixed list from
/// a stable anchor, and `.anchor` re-anchors whenever the item list changes —
/// but a pool with `advance = "resume"` produces a *different* list every
/// generation by design, so an anchored loop would restart the schedule on
/// every roll tick.
///
/// So the emitted chunk JSON becomes the durable timeline instead. Each pass
/// resolves from the stored resume map, lays the resulting sequence end-to-end
/// after the last thing already written, and stores where it got to. Nothing
/// already written is ever rewritten — which is also why this path skips the
/// startup wipe that `LoopForever` channels use to pick up config changes
/// (#53): for a forward-materialized channel the past is a record, not a
/// rendering of the current config.
async fn pattern_channel_loop(
    channel: &LoadedChannel,
    tz: &'static Tz,
    source_roots: &[String],
    catalog: Option<&SharedCatalog>,
    shutdown: Arc<Notify>,
) -> Result<(), StationError> {
    let (resume, how) = crate::resume::load(&channel.output_folder).await?;
    match &how {
        crate::resume::ResumeLoad::Fresh => tracing::info!(
            event = "resume.init",
            channel = %channel.name,
            "no resume sidecar; starting every pool from the top",
        ),
        crate::resume::ResumeLoad::Loaded => tracing::info!(
            event = "resume.load",
            channel = %channel.name,
            pools = resume.pools.len(),
            "loaded resume map",
        ),
        crate::resume::ResumeLoad::Discarded(reason) => tracing::warn!(
            event = "resume.discard",
            channel = %channel.name,
            reason = %reason,
            "resume sidecar unusable; starting every pool from the top",
        ),
    }
    let mut resume = resume;

    // Startup: throw away the future this channel had already written and
    // generate it again from the config as it stands now.
    //
    // A `LoopForever` channel gets this from `wipe_emitted_playout` — its whole
    // output is a pure function of (anchor, items), so wiping and re-emitting
    // is free. A pattern channel can't wipe wholesale: its output depends on
    // where the pools had advanced to, and that state is gone once consumed.
    // The checkpoint trail is what makes the same thing possible here — rewind
    // the pools to the start of the earliest unaired generation, drop exactly
    // the files from that instant on, and regenerate. What has already aired,
    // or is airing now, is untouched. Without this, a config or overlay edit
    // wouldn't reach a pattern channel until its entire written window had
    // played out (#53).
    let now = OffsetDateTime::now_utc();
    if let Some(regen_from) = resume.rewind_to_unaired(now) {
        let removed = wipe_playout_from(channel, regen_from).await?;
        tracing::info!(
            event = "resume.rewind",
            channel = %channel.name,
            from = %regen_from,
            removed = removed,
            "rewound to the earliest unaired generation; regenerating it from the current config",
        );
    }

    resume = pattern_catch_up(channel, tz, source_roots, catalog, resume, "startup").await?;

    let mut interval = tokio::time::interval(channel.config.roll_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await; // consume immediate tick

    loop {
        select! {
            _ = shutdown.notified() => {
                tracing::info!(event = "channel.shutdown", channel = %channel.name, "shutdown received");
                return Ok(());
            }
            _ = interval.tick() => {
                let tick = async {
                    match pattern_catch_up(channel, tz, source_roots, catalog, resume.clone(), "roll").await {
                        Ok(next) => resume = next,
                        Err(err) => tracing::error!(
                            event = "roll.error",
                            channel = %channel.name,
                            error = %err,
                            "roll tick failed; will retry next interval",
                        ),
                    }
                };
                tick.instrument(tracing::info_span!("roll_tick")).await;
            }
        }
    }
}

/// Generate and emit forward until the window through `now + window_days` is
/// covered, chaining one generation into the next through the resume map.
/// Returns the map to carry into the next tick.
async fn pattern_catch_up(
    channel: &LoadedChannel,
    tz: &'static Tz,
    source_roots: &[String],
    catalog: Option<&SharedCatalog>,
    mut resume: crate::resume::ResumeMap,
    phase: &'static str,
) -> Result<crate::resume::ResumeMap, StationError> {
    let output = &channel.output_folder;
    let now = OffsetDateTime::now_utc();
    let target = now + window_duration(channel.config.window_days);
    let overlay_spec = load_overlay_playout_spec(channel);
    let mut cache = DurationCache::load(output).await?;

    // Pick up exactly where the written record ends. Unlike the looping path
    // this is deliberately NOT snapped to a chunk boundary: a forward-
    // materialized channel continues from the last item's finish, so the
    // timeline stays gapless across the seam.
    let existing = scan::scan_output_folder(output).await?;
    let mut from = scan::highest_finish(&existing).unwrap_or(now).max(now);

    let mut generations = 0;
    while from < target {
        if generations >= MAX_GENERATIONS_PER_TICK {
            tracing::warn!(
                event = "pattern.generation_cap",
                channel = %channel.name,
                phase = phase,
                generations = generations,
                covered_through = %from,
                target = %target,
                "hit the per-tick generation cap; the window is only covered this far and will extend on the next roll tick",
            );
            break;
        }

        // Record the state entering this generation before anything consumes
        // it, so the span it is about to write stays regenerable while it is
        // still in the future.
        resume.checkpoint(from);

        let (items, resume_out) = {
            let guard = catalog.map(|sc| sc.catalog.lock().unwrap_or_else(|e| e.into_inner()));
            crate::resolve::resolve_channel_with_resume(
                &channel.config,
                &channel.config_path,
                source_roots,
                catalog.map(|sc| &sc.path_index),
                guard.as_deref(),
                &resume,
            )?
        };

        if items.is_empty() {
            // Every pool has dropped out — the channel has played all the
            // content it had. Not an error (see `resolve_channel_with_resume`):
            // say so once per catch-up and stop, rather than failing every
            // roll tick forever.
            tracing::info!(
                event = "pattern.exhausted",
                channel = %channel.name,
                phase = phase,
                covered_through = %from,
                "every pool has run out under wrap = \"drop\"; nothing further to schedule until new content appears",
            );
            resume.checkpoints.pop();
            break;
        }

        let (durations, _) = cache.resolve_all(&items).await?;
        let rule = crate::rule::Sequential::new(&items, &durations)
            .with_overlay(overlay_spec.as_ref().map(clone_overlay_spec));
        let span = rule.total_duration();
        if span <= time::Duration::ZERO {
            // Zero wall-clock length would never advance `from`. Stop rather
            // than spin, and say so — silently emitting nothing would look
            // exactly like a healthy idle channel.
            tracing::error!(
                event = "pattern.zero_length",
                channel = %channel.name,
                phase = phase,
                items = items.len(),
                "generation produced no playable duration; nothing further can be emitted",
            );
            resume.checkpoints.pop();
            break;
        }

        // Emit the generation *whole*, even when it reaches past the target.
        // Clamping to the target would drop the sequence's tail while the
        // resume map still recorded those items as played, skipping them
        // permanently. Overshooting the window by less than one generation
        // costs nothing; a hole in the schedule is unrecoverable.
        let to = from + span;
        let written = emit_window(
            output,
            &rule,
            from,
            tz,
            channel.config.chunk_hours,
            from,
            to,
        )
        .await?;
        log_emission(&channel.name, phase, &written, from, to);

        // `resume_out` carries only pool state; the checkpoint trail is the
        // daemon's, so it rides across rather than being replaced.
        let checkpoints = std::mem::take(&mut resume.checkpoints);
        resume = resume_out;
        resume.checkpoints = checkpoints;
        resume.prune_elapsed(now);
        crate::resume::save(output, &resume).await?;
        from += span;
        generations += 1;
    }

    if generations == 0 {
        tracing::info!(
            event = "chunk.skip",
            channel = %channel.name,
            phase = phase,
            "window already materialized through {target}",
        );
    }
    cache.save(output).await?;

    let removed = scan::sweep_retention(output, channel.config.retention_days, now).await;
    if removed > 0 {
        tracing::info!(
            event = "retention.sweep",
            channel = %channel.name,
            phase = phase,
            removed = removed,
            retention_days = channel.config.retention_days,
            "retention sweep pruned playout files",
        );
    }
    Ok(resume)
}

/// `OverlaySpec` (an ETV-next type) is not `Clone`, and each generation in a
/// catch-up builds its own rule.
fn clone_overlay_spec(spec: &PlayoutOverlaySpec) -> PlayoutOverlaySpec {
    PlayoutOverlaySpec {
        fifo_path: spec.fifo_path.clone(),
        pixel_format: spec.pixel_format.clone(),
        width: spec.width,
        height: spec.height,
        framerate: spec.framerate,
        x: spec.x,
        y: spec.y,
    }
}

/// Emit any chunks needed to bring the channel's output folder up through
/// `now + window_days`. Skips emission if everything is already materialized.
async fn emit_catch_up(
    channel: &LoadedChannel,
    rule: &LoopForever<'_>,
    anchor_utc: OffsetDateTime,
    tz: &'static Tz,
    phase: &'static str,
) -> Result<(), StationError> {
    let output = &channel.output_folder;
    let now = OffsetDateTime::now_utc();
    let to = now + window_duration(channel.config.window_days);
    let existing = scan::scan_output_folder(output).await?;
    // Snap `from` to the previous local chunk boundary so the first emitted file
    // is always a full-size chunk, never a sliver `[now, boundary)`. When
    // continuing from an existing boundary (the common roll-tick case) this is a
    // no-op — `chunk_boundary_at_or_before` returns a boundary unchanged. On a
    // fresh run, or after the window has fully elapsed (highest finish < now,
    // clamped to now), it lands on the boundary at-or-before now. See #28.
    let resume = scan::highest_finish(&existing).unwrap_or(now).max(now);
    let from = crate::tz::chunk_boundary_at_or_before(resume, channel.config.chunk_hours, tz);

    if from >= to {
        tracing::info!(
            event = "chunk.skip",
            channel = %channel.name,
            phase = phase,
            "window already materialized through {to}",
        );
    } else {
        // Sub-span for chunk emission, parented by the roll-tick span (or the
        // channel span on startup). The chunk.write event is logged inside it so
        // it correlates to its tick via the span list.
        async {
            let written = emit_window(
                output,
                rule,
                anchor_utc,
                tz,
                channel.config.chunk_hours,
                from,
                to,
            )
            .await?;
            log_emission(&channel.name, phase, &written, from, to);
            Ok::<(), StationError>(())
        }
        .instrument(tracing::info_span!("chunk_emit", phase = phase))
        .await?;
    }

    // Prune fully-elapsed playout files past the retention horizon. Runs on both
    // startup and every roll tick, even when nothing new was emitted, so old
    // chunks don't accumulate as the window advances. Housekeeping-grade: never
    // fatal, so a failed prune can't tear down the channel on startup.
    let removed = scan::sweep_retention(output, channel.config.retention_days, now).await;
    if removed > 0 {
        tracing::info!(
            event = "retention.sweep",
            channel = %channel.name,
            phase = phase,
            removed = removed,
            retention_days = channel.config.retention_days,
            "retention sweep pruned playout files",
        );
    }
    Ok(())
}

fn window_duration(window_days: u32) -> time::Duration {
    time::Duration::seconds(window_days as i64 * 24 * 3600)
}

fn log_emission(
    channel: &str,
    phase: &'static str,
    written: &[PathBuf],
    from: OffsetDateTime,
    to: OffsetDateTime,
) {
    tracing::info!(
        event = "chunk.write",
        channel = %channel,
        phase = phase,
        files = written.len(),
        from = %from,
        to = %to,
        "emitted playout files",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHANNEL_BODY: &str = r#"
window_days = 1
chunk_hours = 6
roll_interval = "60s"
retention_days = 1

[[rule.blocks]]
mode = "all"
order = "manual"

[[rule.blocks.entries]]
kind = "item"
in_point = "0s"
out_point = "30s"

[rule.blocks.entries.source]
kind = "lavfi"
params = "testsrc=size=1280x720:rate=30 [out0]"
"#;

    /// Write a station.toml (with the given tz) plus a lavfi channel into a
    /// fresh tempdir and return the dir handle and the station path. The
    /// channel's output_folder points inside the tempdir so `prepare_generation`
    /// mkdir's there rather than polluting the crate directory.
    fn write_station(tz: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let out_base = dir.path().join("out");
        let station = format!(
            "tz = {:?}\noutput_base = {:?}\nchannels = [\"channel.toml\"]\n",
            tz,
            out_base.to_string_lossy(),
        );
        // No output_folder — the channel's identity is its file stem
        // ("channel"), so it writes to {out_base}/channel inside the tempdir.
        std::fs::write(dir.path().join("station.toml"), station).unwrap();
        std::fs::write(dir.path().join("channel.toml"), CHANNEL_BODY).unwrap();
        let path = dir.path().join("station.toml");
        (dir, path)
    }

    #[tokio::test]
    async fn prepare_generation_accepts_valid_config() {
        let (_dir, path) = write_station("America/Chicago");
        let station = crate::config::load(&path).expect("valid config should load");
        prepare_generation(&station)
            .await
            .expect("valid config should prepare");
    }

    #[tokio::test]
    async fn catalog_disabled_when_no_path() {
        // A station without `catalog_path` stays catalog-free — today's behavior.
        let (_dir, path) = write_station("UTC");
        let station = crate::config::load(&path).unwrap();
        assert!(open_and_ingest_catalog(&station).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn catalog_opens_and_ingests_when_path_set() {
        // Hermetic: ignore any ambient Plex creds (the dev shell exports them) so
        // this never makes a real network call. Only `open_and_ingest_catalog`
        // reads these, and `catalog_disabled_when_no_path` returns before it does,
        // so clearing them here can't affect another test.
        unsafe {
            std::env::remove_var("PLEX_URL");
            std::env::remove_var("PLEX_TOKEN");
        }
        let dir = tempfile::tempdir().unwrap();
        let out_base = dir.path().join("out");
        let db = dir.path().join("catalog.db");
        let station_toml = format!(
            "tz = \"UTC\"\noutput_base = {:?}\ncatalog_path = {:?}\nchannels = [\"channel.toml\"]\n",
            out_base.to_string_lossy(),
            db.to_string_lossy(),
        );
        std::fs::write(dir.path().join("station.toml"), station_toml).unwrap();
        std::fs::write(dir.path().join("channel.toml"), CHANNEL_BODY).unwrap();
        let station = crate::config::load(&dir.path().join("station.toml")).unwrap();

        // No source_roots + no Plex env → a clean, empty ingest that still opens
        // the db and returns a shareable handle.
        let catalog = open_and_ingest_catalog(&station).await.unwrap();
        assert!(catalog.is_some());
        assert!(db.exists());
    }

    #[tokio::test]
    async fn prepare_generation_rejects_invalid_timezone() {
        // A non-empty-but-bogus tz passes `config::load`'s `validate_station`
        // (which only checks non-empty) and is caught by the timezone parse in
        // `prepare_generation` — the gate that, on reload, reverts to the
        // previous config instead of running a broken one. tz is parsed before
        // the mkdir, so this never touches the filesystem.
        let (_dir, path) = write_station("Totally/Bogus/Zone");
        let station = crate::config::load(&path).expect("bogus tz still parses as config");
        assert!(prepare_generation(&station).await.is_err());
    }
}

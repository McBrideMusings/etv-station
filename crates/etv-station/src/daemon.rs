use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ersatztv_playout::playout::OverlaySpec as PlayoutOverlaySpec;
use time::OffsetDateTime;
use time_tz::Tz;
use tokio::select;
use tokio::sync::Notify;
use tracing::Instrument;

use crate::catalog::Catalog;
use crate::config::{ChannelConfig, LoadedChannel, ScoringConfig, Station};
use crate::duration::DurationCache;
use crate::emit::emit_window;
use crate::errors::{ConfigError, StationError};
use crate::overlay_supervisor;
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
        // (duration probing, sidecar load, catch-up emit); roll-tick errors are
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
                // media, sidecar or emit error — would stay invisible until the
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
    forward_channel_loop(channel, tz, source_roots, catalog, shutdown).await
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

/// The emission loop for every channel: **materialize forward**.
///
/// Each pass resolves the channel, lays the resulting sequence end-to-end after
/// the last thing already written, and stores where it got to. The emitted
/// chunk JSON is the durable timeline; the `.resume` sidecar holds only the
/// seam. Nothing already written is ever rewritten — the past is a record, not
/// a rendering of the current config — so config edits arrive through the
/// checkpoint rewind below rather than a wholesale wipe (#53).
///
/// This replaced an anchor-and-loop model that resolved one list at startup and
/// repeated it forever off an `.anchor` sidecar. That model could not express a
/// channel whose list changes between generations: a pool with
/// `advance = "resume"` produces a different list by design, and `.anchor`
/// re-anchored on every change, restarting the schedule. It also could not
/// deliver what an unseeded `order = "random"` channel advertises — resolving
/// once per process meant one shuffle replayed until the daemon restarted,
/// never a fresh one per pass.
///
/// Nothing was lost by dropping it. A channel whose list happens never to change
/// resolves the same list every generation, and those laid end-to-end *are* the
/// loop. So there is one emission model rather than two.
async fn forward_channel_loop(
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

    // The play-history ledger (#70) — the single record of what this channel
    // has aired, and the thing each series' resume position is derived from.
    // Loaded once here and carried across every tick, so a catch-up chaining
    // many generations never re-reads it.
    let (mut ledger, skipped) = crate::history::load(&channel.output_folder).await?;
    if skipped > 0 {
        tracing::warn!(
            event = "history.partial",
            channel = %channel.name,
            skipped = skipped,
            "skipped unparseable play-history lines; affected series resume from an earlier position",
        );
    }
    tracing::info!(
        event = "history.load",
        channel = %channel.name,
        airings = ledger.len(),
        "loaded play history",
    );

    // Startup: throw away the future this channel had already written and
    // generate it again from the config as it stands now.
    //
    // A wholesale wipe is not available: the output depends on where the pools
    // had advanced to, and that state is gone once consumed.
    // The checkpoint trail is what makes the same thing possible here — rewind
    // the pools to the start of the earliest unaired generation, drop exactly
    // the files from that instant on, and regenerate. What has already aired,
    // or is airing now, is untouched. Without this, a config or overlay edit
    // wouldn't reach a pattern channel until its entire written window had
    // played out (#53).
    let now = OffsetDateTime::now_utc();
    if let Some(regen_from) = resume.rewind_to_unaired(now) {
        let removed = wipe_playout_from(channel, regen_from).await?;
        // Those airings are no longer scheduled, so they are no longer history.
        // Because the resume position is a projection of the ledger, dropping
        // them is also what rewinds each series — the two cannot disagree.
        ledger.truncate_from(regen_from);
        crate::history::save(&channel.output_folder, &mut ledger).await?;
        tracing::info!(
            event = "resume.rewind",
            channel = %channel.name,
            from = %regen_from,
            removed = removed,
            airings = ledger.len(),
            "rewound to the earliest unaired generation; regenerating it from the current config",
        );
    }

    resume = pattern_catch_up(
        channel,
        tz,
        source_roots,
        catalog,
        resume,
        &mut ledger,
        "startup",
    )
    .await?;

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
                    match pattern_catch_up(channel, tz, source_roots, catalog, resume.clone(), &mut ledger, "roll").await {
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
/// Hint a scorer plugin about how many items this generation needs.
///
/// Sized to **one chunk**, not to the whole remaining window. A generation lays
/// the plugin's returned list end-to-end, so a hint covering a 30-day window
/// would push a single generation to materialize the entire month in one pass.
/// Near the end of the window the remaining span is smaller than a chunk, and
/// that smaller span wins.
///
/// Clamped at both ends: at least one item, so a nearly-covered window still
/// asks for something rather than handing a plugin a target of zero, and capped
/// so no configuration can ask a plugin to rank an entire library.
fn target_count(config: &ChannelConfig, from: OffsetDateTime, target: OffsetDateTime) -> usize {
    const MAX: i64 = 500;
    let per_item = config
        .scoring
        .as_ref()
        .map(|s| s.nominal_item_secs)
        .unwrap_or_else(|| ScoringConfig::default().nominal_item_secs)
        .max(1) as i64;
    let chunk = i64::from(config.chunk_hours) * 3600;
    let remaining = (target - from).whole_seconds().max(0);
    remaining.min(chunk).div_euclid(per_item).clamp(1, MAX) as usize
}

/// Returns the map to carry into the next tick.
async fn pattern_catch_up(
    channel: &LoadedChannel,
    tz: &'static Tz,
    source_roots: &[String],
    catalog: Option<&SharedCatalog>,
    mut resume: crate::resume::ResumeMap,
    ledger: &mut crate::history::Ledger,
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
    // Only the first generation of a channel with nothing written yet joins its
    // list mid-way from a past `anchor`; see the phase calculation below.
    let mut first_generation = existing.is_empty();

    // Watch history is read once per tick, not once per generation: a catch-up
    // chains many generations in a row and they all share the same "what has
    // been watched lately". Empty when Tautulli is unset or unreachable, which
    // degrades a scorer's ranking rather than failing the tick (#74).
    // The HTTP half runs on a blocking thread — `ureq` is synchronous and would
    // otherwise stall this runtime worker for the request timeout, exactly as
    // the Plex ingest above avoids. The catalog join is local work and stays
    // here, where the mutex is already reachable.
    let history = match catalog {
        Some(sc) => {
            let rows = tokio::task::spawn_blocking(crate::tautulli::fetch_rows_from_env)
                .await
                .unwrap_or_default();
            let guard = sc.catalog.lock().unwrap_or_else(|e| e.into_inner());
            crate::tautulli::join(&guard, rows)
        }
        None => Vec::new(),
    };

    // How deep a recently-aired tail this channel's scorer sees. Read once —
    // it cannot change inside a tick.
    let recent_depth = channel
        .config
        .scoring
        .as_ref()
        .map(|s| s.recent_depth)
        .unwrap_or_else(|| ScoringConfig::default().recent_depth);

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

        // Where each series left off comes from the ledger, not from a cursor
        // of the sidecar's own (#70): one record, projected on demand.
        let state = crate::resume::GenerationState {
            resume: resume.clone(),
            cursor: ledger.series_cursor(),
            tail: ledger.tail(channel.config.adjacency_reach()),
        };

        // Sized to one chunk, not to the whole remaining window: the generation
        // lays whatever the plugin returns end-to-end, so asking for a month's
        // worth would make a single generation try to cover the month.
        let scoring = crate::score::ScoreInputs {
            target_count: target_count(&channel.config, from, target),
            history: history.clone(),
            recent: ledger.tail(recent_depth),
            now: now.unix_timestamp(),
        };

        let (items, resume_out, show_ids) = {
            let guard = catalog.map(|sc| sc.catalog.lock().unwrap_or_else(|e| e.into_inner()));
            let (items, resume_out) = crate::resolve::resolve_channel_with_resume(
                &channel.config,
                &channel.config_path,
                source_roots,
                catalog.map(|sc| &sc.path_index),
                guard.as_deref(),
                &state,
                &scoring,
            )?;
            // The ledger needs each airing's show, and only the catalog knows
            // it. One query for the whole generation, under the lock we already
            // hold.
            let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
            let show_ids = match guard.as_deref() {
                Some(cat) => cat.show_ids_for(&ids)?,
                None => HashMap::new(),
            };
            (items, resume_out, show_ids)
        };

        // No "the channel ran out" branch: every series loops, so a pattern
        // channel cannot play itself empty. An empty resolve means an empty
        // *set*, which `resolve_channel_with_resume` has already raised as the
        // config error it is.

        let (durations, _) = cache.resolve_all(&items).await?;

        // A channel with an `anchor` in the past joins its list where elapsed
        // time says it should be, rather than at item 0 — "this station has been
        // broadcasting since 2020". Only on the very first generation: after
        // that the written timeline is the phase, and re-deriving it would fight
        // the resume map. Skipped items aren't lost, just not aired this pass —
        // the next generation resolves the list afresh.
        let (items_slice, durations_slice, seq_start) =
            match channel.config.anchor.filter(|_| first_generation) {
                Some(anchor) => {
                    let (skip, into_item) = crate::rule::phase_at(anchor, from, &durations);
                    if skip > 0 || !into_item.is_zero() {
                        tracing::info!(
                            event = "anchor.join",
                            channel = %channel.name,
                            anchor = %anchor,
                            skipped_items = skip,
                            "joined the sequence mid-list from the configured anchor",
                        );
                    }
                    (&items[skip..], &durations[skip..], from - into_item)
                }
                None => (&items[..], &durations[..], from),
            };
        first_generation = false;

        let rule = crate::rule::Sequential::new(items_slice, durations_slice)
            .with_overlay(overlay_spec.as_ref().map(clone_overlay_spec));
        let span = rule.total_duration() - (from - seq_start);
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
            seq_start,
            tz,
            channel.config.chunk_hours,
            from,
            to,
        )
        .await?;
        log_emission(&channel.name, phase, &written, from, to);

        // One ledger row per scheduled airing, in schedule order. The times are
        // the same walk `Sequential` just emitted: items laid end to end from
        // `from`, which is why the whole generation is emitted rather than
        // clamped — a row must correspond to something actually on disk.
        //
        // `written_at` is read per generation rather than reusing the tick's
        // `now`: a catch-up can chain many generations, and stamping them all
        // with the moment the tick began would misreport when each was
        // actually scheduled.
        let written_at = OffsetDateTime::now_utc();
        let mut airing = from;
        let records: Vec<crate::history::PlayRecord> = items
            .iter()
            .zip(durations.iter())
            .map(|(item, dur)| {
                let start = airing;
                airing += time::Duration::seconds_f64(dur.as_secs_f64());
                crate::history::PlayRecord {
                    entry_id: item.id.clone(),
                    show_id: show_ids.get(&item.id).cloned(),
                    start,
                    played_at: written_at,
                }
            })
            .collect();
        ledger.extend(records);
        crate::history::save(output, ledger).await?;

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

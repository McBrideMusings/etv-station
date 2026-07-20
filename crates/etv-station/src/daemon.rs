use std::path::PathBuf;
use std::sync::Arc;

use ersatztv_playout::playout::OverlaySpec as PlayoutOverlaySpec;
use time::OffsetDateTime;
use time_tz::Tz;
use tokio::select;
use tokio::sync::Notify;
use tracing::Instrument;

use crate::anchor;
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
        let (do_reload, first_err) = run_generation(&station, tz, &shutdown, &reload).await;

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
                let result = channel_loop(ch, tz, stop).await;
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
    shutdown: Arc<Notify>,
) -> Result<(), StationError> {
    // `run` creates every channel's output_folder synchronously before spawning
    // this task (see #34), so it exists by the time we scan/emit into it here.
    let output = &channel.output_folder;

    // The station-wide catalog is not yet opened by the daemon (#71 follow-up);
    // until then only inline-item, `manual`-order channels resolve. Query
    // entries and non-`manual` order error clearly via `resolve_channel`.
    let items = crate::resolve::resolve_channel(&channel.config, &channel.config_path, None)?;

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
id = "bars-30s"
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

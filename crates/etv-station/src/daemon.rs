use std::path::PathBuf;
use std::sync::Arc;

use time::OffsetDateTime;
use time_tz::Tz;
use tokio::select;
use tokio::sync::Notify;

use crate::anchor;
use crate::config::{LoadedChannel, Station};
use crate::duration::DurationCache;
use crate::emit::emit_window;
use crate::errors::StationError;
use crate::rule::LoopForever;
use crate::scan;
use crate::tz as tzmod;

pub async fn run(station: Station) -> Result<(), StationError> {
    let tz = tzmod::parse(&station.station.tz)?;
    let shutdown = Arc::new(Notify::new());

    let shutdown_signal = shutdown.clone();
    tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            tracing::info!("ctrl-c received, shutting down");
            shutdown_signal.notify_waiters();
        }
    });

    let mut handles = Vec::new();
    let station = Arc::new(station);
    for idx in 0..station.channels.len() {
        let s = station.clone();
        let sd = shutdown.clone();
        handles.push(tokio::spawn(async move {
            let ch = &s.channels[idx];
            let result = channel_loop(ch, tz, sd).await;
            (ch.name.clone(), result)
        }));
    }

    let mut first_err: Option<StationError> = None;
    for h in handles {
        match h.await {
            Ok((name, Ok(()))) => {
                tracing::info!(channel = %name, "channel loop exited cleanly");
            }
            Ok((name, Err(e))) => {
                tracing::error!(channel = %name, error = %e, "channel loop failed");
                first_err.get_or_insert(e);
            }
            Err(e) => {
                tracing::error!(error = %e, "channel task panicked");
                first_err.get_or_insert_with(|| StationError::Task(format!("{e}")));
            }
        }
    }

    first_err.map_or(Ok(()), Err)
}

async fn channel_loop(
    channel: &LoadedChannel,
    tz: &'static Tz,
    shutdown: Arc<Notify>,
) -> Result<(), StationError> {
    let output = &channel.config.output_folder;

    tokio::fs::create_dir_all(output)
        .await
        .map_err(|source| StationError::Io {
            path: output.clone(),
            source,
        })?;

    let items = channel.config.rule.items();

    let mut cache = DurationCache::load(output).await?;
    let (durations, stats) = cache.resolve_all(items).await?;
    cache.save(output).await?;
    tracing::info!(
        channel = %channel.name,
        from_cache = stats.from_cache,
        from_probe = stats.from_probe,
        from_config = stats.from_config,
        items = items.len(),
        "resolved item durations",
    );

    let now_utc = OffsetDateTime::now_utc();
    let anchor_state = anchor::load_or_initialize(output, items, now_utc, tz).await?;
    if anchor_state.initialized_now {
        tracing::info!(
            channel = %channel.name,
            anchor = %anchor_state.anchor_utc,
            "anchored at first run",
        );
    } else if anchor_state.re_anchored {
        tracing::warn!(
            channel = %channel.name,
            anchor = %anchor_state.anchor_utc,
            "items changed; re-anchored",
        );
    } else {
        tracing::info!(
            channel = %channel.name,
            anchor = %anchor_state.anchor_utc,
            "loaded anchor",
        );
    }

    let rule = LoopForever::new(items, &durations);

    // Startup catch-up: emit from max(now, highest_existing_finish) up to now+window_days.
    emit_catch_up(channel, &rule, anchor_state.anchor_utc, tz, "startup").await?;

    let mut interval = tokio::time::interval(channel.config.roll_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await; // consume immediate tick

    loop {
        select! {
            _ = shutdown.notified() => {
                tracing::info!(channel = %channel.name, "shutdown received");
                return Ok(());
            }
            _ = interval.tick() => {
                if let Err(err) =
                    emit_catch_up(channel, &rule, anchor_state.anchor_utc, tz, "roll").await
                {
                    tracing::error!(
                        channel = %channel.name,
                        error = %err,
                        "roll tick failed; will retry next interval",
                    );
                }
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
    let output = &channel.config.output_folder;
    let now = OffsetDateTime::now_utc();
    let to = now + window_duration(channel.config.window_days);
    let existing = scan::scan_output_folder(output).await?;
    let from = scan::highest_finish(&existing).unwrap_or(now).max(now);

    if from >= to {
        tracing::info!(
            channel = %channel.name,
            phase = phase,
            "window already materialized through {to}",
        );
        return Ok(());
    }

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
        channel = %channel,
        phase = phase,
        files = written.len(),
        from = %from,
        to = %to,
        "emitted playout files",
    );
}

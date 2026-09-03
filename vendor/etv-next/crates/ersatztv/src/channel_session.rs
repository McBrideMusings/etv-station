use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ersatztv::error::LineupError;
use ersatztv_core::{
    HEARTBEAT_FILE_NAME, HEARTBEAT_FILE_TIMEOUT, READY_FILE_NAME, STALL_EXIT_CODE, reap_run_folders,
};
use tokio::sync::{Mutex, watch};

use crate::channel_health::HealthMap;
use crate::channel_model::ChannelModel;

/// Whether a viewer was still attached, judged the same way the channel worker
/// judges it: the heartbeat exists and was touched within the timeout. A missing
/// or unreadable file means nobody is watching, which is the safe reading — it
/// counts nothing rather than blaming the channel for an ordinary idle exit.
///
/// `pub(crate)` so `main`'s periodic run-folder reap sweep can use the exact
/// same freshness rule this module uses at exit time — a channel that goes
/// idle between viewers must be judged consistently by both call sites, or a
/// sweep timed just wrong could reap a run folder the exit-time check would
/// have kept.
pub(crate) async fn heartbeat_is_fresh(heartbeat_file: &Path) -> bool {
    let Ok(metadata) = tokio::fs::metadata(heartbeat_file).await else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    match modified.elapsed() {
        Ok(age) => age < HEARTBEAT_FILE_TIMEOUT,
        // A heartbeat stamped in the future (clock skew) is not evidence of
        // absence, so treat it as fresh.
        Err(_) => true,
    }
}

pub struct ChannelSession {
    ready_receiver: watch::Receiver<bool>,
}

impl ChannelSession {
    pub fn spawn(
        channel: &ChannelModel,
        active: Arc<Mutex<HashMap<String, ChannelSession>>>,
        health: Arc<Mutex<HealthMap>>,
    ) -> Result<Self, LineupError> {
        let mut child = tokio::process::Command::new(channel_binary_path()?)
            .arg("run")
            .arg("--output-folder")
            .arg(channel.output_folder())
            .arg("--number")
            .arg(channel.number())
            .arg("--name")
            .arg(channel.name())
            .arg(channel.config_path())
            .args(channel.overlay_paths())
            .spawn()
            .map_err(LineupError::Io)?;

        let (ready_sender, ready_receiver) = watch::channel(false);
        let output_folder = channel.output_folder().to_owned();
        let ready_file = channel.output_folder().join(READY_FILE_NAME);
        let heartbeat_file = channel.output_folder().join(HEARTBEAT_FILE_NAME);
        let channel_number = channel.number().to_owned();

        tokio::spawn(async move {
            let ready_file_clone = ready_file.clone();
            let watcher = tokio::spawn(async move {
                loop {
                    if tokio::fs::metadata(&ready_file_clone).await.is_ok() {
                        let _ = ready_sender.send(true);
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            });

            // Deliberately NOT clearing the failure count on ready: a channel
            // that dies every cycle reaches ready every cycle, so doing so
            // pinned it at one failure and it was never declared failed. Only
            // the uptime measured below decides whether a run was healthy.
            let started_at = Instant::now();

            let status = child.wait().await;
            watcher.abort();
            match &status {
                Ok(s) if s.success() => {
                    log::info!(
                        "channel {channel_number} exited cleanly (idle shutdown or normal stop)"
                    );
                }
                Ok(s) => {
                    log::warn!("channel {channel_number} exited with status {s}");
                }
                Err(e) => {
                    log::error!("channel {channel_number} wait failed: {e}");
                }
            }

            // The worker's own verdict that the stream stopped reaching the
            // viewer. Uptime cannot stand in for it — the stall watchdog only
            // fires 60s after the last segment, so a session that wedges a few
            // minutes in outlives HEALTHY_UPTIME while showing a frozen picture.
            let stalled = matches!(&status, Ok(s) if s.code() == Some(STALL_EXIT_CODE));

            // Sample the heartbeat BEFORE the cleanup below removes it. It is
            // the only thing distinguishing "died while someone was watching"
            // from an ordinary idle exit, and once the file is gone every exit
            // looks unwatched — so nothing would ever be counted.
            let viewer_attached = heartbeat_is_fresh(&heartbeat_file).await;
            // Record and read back under one lock, so the count logged is the
            // count stored — two locks could straddle another channel's update.
            let uptime = started_at.elapsed();
            let failures = {
                let mut guard = health.lock().await;
                guard.record_exit(
                    &channel_number,
                    viewer_attached,
                    stalled,
                    uptime,
                    Instant::now(),
                );
                guard.get(&channel_number).consecutive_failures
            };
            if viewer_attached {
                log::warn!(
                    "channel {channel_number} exited while a viewer was watching; \
                     consecutive failures now {failures}",
                );
            }

            // Reap this run's segment folder — the segments and .vtt
            // sidecars PlaylistManager's trim never reached, because it only
            // drops a segment once a *later* one pushes it outside the
            // two-minute playlist window, and no later segment ever arrives
            // once the worker has exited. Left alone, that tail (about two
            // minutes of segments) sits on disk until this channel happens to
            // be watched again — which, for an idle channel, may be never.
            // Done for every exit route: this point is reached whether the
            // worker returned cleanly, hit an error, or was killed out from
            // under it, since `child.wait()` above resolves either way.
            //
            // Per-run segment folders (etv-station-262) mean this can target
            // the run that just exited instead of wiping the whole channel
            // folder: `viewer_attached` (sampled above, before this cleanup)
            // decides whether that folder is kept — a client mid-playback
            // still holds a playlist naming segments in it. A viewer that has
            // not yet detached at this instant is exactly the case
            // `keep_newest` protects; one that detaches later is caught by
            // the periodic reap sweep in `main`, since nothing at exit time
            // can see a detach that has not happened yet.
            if let Err(err) = reap_run_folders(&output_folder, viewer_attached).await {
                log::warn!("failed to reap run folders for channel {channel_number}: {err}");
            }

            // Release the slot in `active` so `session_middleware` can spawn a
            // replacement worker. Per-run segment folders remove the race this
            // ordering used to guard against: a fresh worker computes its own
            // new run-folder name and never writes into a path this reap could
            // have just removed, so releasing the slot before or after the
            // reap above no longer matters for correctness. Kept after it
            // anyway, since there is no reason to invite a respawn before this
            // exit has finished tidying up after itself.
            active.lock().await.remove(&channel_number);

            if ready_file.exists() {
                let _ = tokio::fs::remove_file(&ready_file).await;
            }

            if heartbeat_file.exists() {
                let _ = tokio::fs::remove_file(&heartbeat_file).await;
            }
        });

        Ok(ChannelSession { ready_receiver })
    }

    pub fn subscribe_ready(&self) -> watch::Receiver<bool> {
        self.ready_receiver.clone()
    }
}

fn channel_binary_path() -> Result<PathBuf, LineupError> {
    let mut path = std::env::current_exe()?
        .parent()
        .ok_or(LineupError::ChannelBinaryNotFound)?
        .to_path_buf();
    path.push(format!("ersatztv-channel{}", std::env::consts::EXE_SUFFIX));

    if path.is_file() {
        Ok(path)
    } else {
        Err(LineupError::ChannelBinaryNotFoundAtPath(
            path.to_string_lossy().to_string(),
        ))
    }
}

#[cfg(test)]
mod reap_tests {
    use tempfile::TempDir;

    use super::{HEARTBEAT_FILE_NAME, heartbeat_is_fresh, reap_run_folders};

    /// Exercises the exact call the exit-time cleanup in `spawn` makes:
    /// `keep_newest` decided by `heartbeat_is_fresh` on a heartbeat sampled
    /// before cleanup. No heartbeat at all reads as "nobody watching", so the
    /// dead run's folder — the one that just exited — is removed.
    #[tokio::test]
    async fn a_dead_run_is_reaped_when_no_heartbeat_is_fresh() {
        let dir = TempDir::new().unwrap();
        let channel_root = dir.path();

        let dead_run = channel_root.join("r0000000000001-0000");
        tokio::fs::create_dir(&dead_run).await.unwrap();
        tokio::fs::write(dead_run.join("live000000.ts"), b"segment")
            .await
            .unwrap();

        let heartbeat_file = channel_root.join(HEARTBEAT_FILE_NAME);
        let viewer_attached = heartbeat_is_fresh(&heartbeat_file).await;
        assert!(!viewer_attached, "no heartbeat file exists yet");

        reap_run_folders(channel_root, viewer_attached)
            .await
            .unwrap();

        assert!(
            !dead_run.exists(),
            "a dead run with nobody watching must be reaped"
        );
    }

    /// The retention half of the same call: a fresh heartbeat means a client
    /// mid-playback still holds a playlist naming segments in the run that
    /// just exited, so the reap must leave it alone.
    #[tokio::test]
    async fn a_run_is_kept_when_its_heartbeat_is_fresh() {
        let dir = TempDir::new().unwrap();
        let channel_root = dir.path();

        let live_run = channel_root.join("r0000000000001-0000");
        tokio::fs::create_dir(&live_run).await.unwrap();
        tokio::fs::write(live_run.join("live000000.ts"), b"segment")
            .await
            .unwrap();

        let heartbeat_file = channel_root.join(HEARTBEAT_FILE_NAME);
        tokio::fs::write(&heartbeat_file, b"").await.unwrap();

        let viewer_attached = heartbeat_is_fresh(&heartbeat_file).await;
        assert!(viewer_attached, "a just-touched heartbeat must read fresh");

        reap_run_folders(channel_root, viewer_attached)
            .await
            .unwrap();

        assert!(
            live_run.exists(),
            "a run a viewer is still attached to must survive the reap"
        );
        assert!(live_run.join("live000000.ts").exists());
    }
}

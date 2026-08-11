use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ersatztv::error::LineupError;
use ersatztv_core::{HEARTBEAT_FILE_NAME, HEARTBEAT_FILE_TIMEOUT, READY_FILE_NAME};
use tokio::sync::{Mutex, watch};

use crate::channel_health::HealthMap;
use crate::channel_model::ChannelModel;

/// Whether a viewer was still attached, judged the same way the channel worker
/// judges it: the heartbeat exists and was touched within the timeout. A missing
/// or unreadable file means nobody is watching, which is the safe reading — it
/// counts nothing rather than blaming the channel for an ordinary idle exit.
async fn heartbeat_is_fresh(heartbeat_file: &Path) -> bool {
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
            .arg(channel.config_path())
            .args(channel.overlay_paths())
            .spawn()
            .map_err(LineupError::Io)?;

        let (ready_sender, ready_receiver) = watch::channel(false);
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
                    log::debug!("channel {channel_number} exited cleanly");
                }
                Ok(s) => {
                    log::warn!("channel {channel_number} exited with status {s}");
                }
                Err(e) => {
                    log::error!("channel {channel_number} wait failed: {e}");
                }
            }

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
                guard.record_exit(&channel_number, viewer_attached, uptime, Instant::now());
                guard.get(&channel_number).consecutive_failures
            };
            if viewer_attached {
                log::warn!(
                    "channel {channel_number} exited while a viewer was watching; \
                     consecutive failures now {failures}",
                );
            }

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

//! Per-channel overlay subprocess lifecycle.
//!
//! When a channel's `overlay` config is set, the station daemon spawns an
//! `etv-overlay pipe` subprocess that writes RGBA frames to a fifo. The fifo
//! is what the etv-next channel encoder reads as its overlay input.
//!
//! For the spike the supervisor eager-spawns the overlay process at startup
//! and respawns it on unexpected exit until daemon shutdown. The fifo is
//! pre-created (and re-created on respawn) so the channel encoder's ffmpeg
//! can always open it for read.
//!
//! TODO(v2): defer spawn until a viewer is actually watching, by watching the
//! `.heartbeat` file etv-next writes on every HLS segment fetch
//! (etv-next/crates/ersatztv/src/main.rs:259-277). This requires either
//! handling ffmpeg's "no fifo writer yet" state cleanly, or having etv-next
//! touch a "channel-spawning" file the supervisor can pick up earlier than
//! the segment-fetch heartbeat.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use nix::sys::stat::Mode;
use nix::unistd::mkfifo;
use std::os::unix::fs::FileTypeExt;
use tokio::process::{Child, Command};
use tokio::sync::Notify;
use tokio::time;

const HEARTBEAT_FILE_NAME: &str = ".heartbeat";
const READY_FILE_NAME: &str = ".overlay-ready";
const POLL_INTERVAL: Duration = Duration::from_secs(5);

pub struct OverlayContext {
    pub channel_name: String,
    pub output_folder: PathBuf,
    pub overlay_config: PathBuf,
    pub fifo_path: PathBuf,
}

impl OverlayContext {
    #[allow(dead_code)]
    pub fn heartbeat_path(&self) -> PathBuf {
        self.output_folder.join(HEARTBEAT_FILE_NAME)
    }

    pub fn ready_path(&self) -> PathBuf {
        self.output_folder.join(READY_FILE_NAME)
    }
}

/// Resolve the fifo path: explicit if set, else `{output_folder}/overlay.fifo`.
pub fn resolve_fifo_path(output_folder: &Path, configured: Option<&Path>) -> PathBuf {
    configured
        .map(Path::to_path_buf)
        .unwrap_or_else(|| output_folder.join("overlay.fifo"))
}

/// Resolve the overlay config path: absolute as-is, or relative to the
/// channel config file's directory.
pub fn resolve_overlay_config(channel_config_path: &Path, configured: &Path) -> PathBuf {
    if configured.is_absolute() {
        configured.to_path_buf()
    } else {
        match channel_config_path.parent() {
            Some(parent) => parent.join(configured),
            None => configured.to_path_buf(),
        }
    }
}

/// Pre-create the fifo on disk so etv-next can open it for reading any time
/// without racing the overlay process startup.
pub fn ensure_fifo(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        let meta = std::fs::metadata(path)?;
        if !meta.file_type().is_fifo() {
            std::fs::remove_file(path)?;
        } else {
            return Ok(());
        }
    }
    mkfifo(
        path,
        Mode::S_IRUSR | Mode::S_IWUSR | Mode::S_IRGRP | Mode::S_IWGRP,
    )
    .map_err(std::io::Error::other)?;
    Ok(())
}

/// Run the supervisor loop for a single channel. Eagerly spawns the overlay
/// at startup and restarts it if it dies; only exits on shutdown notify.
pub async fn run(ctx: OverlayContext, shutdown: Arc<Notify>) {
    if let Err(err) = ensure_fifo(&ctx.fifo_path) {
        tracing::warn!(
            channel = %ctx.channel_name,
            error = %err,
            fifo = %ctx.fifo_path.display(),
            "failed to create overlay fifo at startup; will retry on first spawn",
        );
    }

    let mut child: Option<Child> = None;
    let mut ready_observed = false;

    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                if let Some(mut c) = child.take() {
                    tracing::info!(channel = %ctx.channel_name, "shutting down overlay process");
                    let _ = c.kill().await;
                }
                let _ = std::fs::remove_file(&ctx.fifo_path);
                let _ = std::fs::remove_file(ctx.ready_path());
                return;
            }
            _ = time::sleep(POLL_INTERVAL) => {
                if let Some(c) = child.as_mut()
                    && let Ok(Some(status)) = c.try_wait()
                {
                    tracing::warn!(
                        channel = %ctx.channel_name,
                        status = %status,
                        "overlay process exited; will respawn",
                    );
                    child = None;
                    ready_observed = false;
                    let _ = std::fs::remove_file(ctx.ready_path());
                }
                if child.is_none() {
                    if let Err(err) = ensure_fifo(&ctx.fifo_path) {
                        tracing::warn!(
                            channel = %ctx.channel_name,
                            error = %err,
                            "fifo not available; deferring overlay spawn",
                        );
                        continue;
                    }
                    // Remove any stale ready marker from a prior run so we
                    // don't mistake it for the new process being ready.
                    let _ = std::fs::remove_file(ctx.ready_path());
                    match spawn_overlay(&ctx) {
                        Ok(new_child) => {
                            tracing::info!(
                                channel = %ctx.channel_name,
                                fifo = %ctx.fifo_path.display(),
                                "spawned overlay process",
                            );
                            child = Some(new_child);
                        }
                        Err(err) => {
                            tracing::warn!(
                                channel = %ctx.channel_name,
                                error = %err,
                                "failed to spawn overlay process; will retry next tick",
                            );
                        }
                    }
                } else if !ready_observed && ctx.ready_path().exists() {
                    tracing::info!(
                        channel = %ctx.channel_name,
                        "overlay reported ready (first frame written)",
                    );
                    ready_observed = true;
                }
            }
        }
    }
}

fn spawn_overlay(ctx: &OverlayContext) -> std::io::Result<Child> {
    let binary = overlay_binary_path();
    Command::new(binary)
        .arg("pipe")
        .arg("--config")
        .arg(&ctx.overlay_config)
        .arg("--fifo")
        .arg(&ctx.fifo_path)
        .arg("--ready-file")
        .arg(ctx.ready_path())
        // Source of truth for "what's airing now" — the overlay reads the
        // station-emitted chunk JSON to populate the per-frame Rhai context
        // (title, next_title, item_elapsed, item_remaining).
        .arg("--playout-folder")
        .arg(&ctx.output_folder)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
}

fn overlay_binary_path() -> PathBuf {
    if let Ok(path) = std::env::var("ETV_OVERLAY_BIN") {
        return PathBuf::from(path);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        let candidate = parent.join("etv-overlay");
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from("etv-overlay")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_fifo_uses_explicit_when_set() {
        let explicit = PathBuf::from("/tmp/custom.fifo");
        let resolved = resolve_fifo_path(Path::new("/some/output"), Some(&explicit));
        assert_eq!(resolved, explicit);
    }

    #[test]
    fn resolve_fifo_defaults_under_output_folder() {
        let resolved = resolve_fifo_path(Path::new("/some/output"), None);
        assert_eq!(resolved, PathBuf::from("/some/output/overlay.fifo"));
    }

    #[test]
    fn resolve_overlay_config_relative_to_channel_dir() {
        let channel_path = Path::new("/etc/etv/channels/test.toml");
        let cfg = Path::new("overlays/watermark.toml");
        let resolved = resolve_overlay_config(channel_path, cfg);
        assert_eq!(
            resolved,
            PathBuf::from("/etc/etv/channels/overlays/watermark.toml")
        );
    }

    #[test]
    fn ensure_fifo_creates_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("test.fifo");
        ensure_fifo(&fifo).unwrap();
        assert!(fifo.metadata().unwrap().file_type().is_fifo());
    }

    #[test]
    fn ensure_fifo_idempotent_on_existing_fifo() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("test.fifo");
        ensure_fifo(&fifo).unwrap();
        ensure_fifo(&fifo).unwrap();
        assert!(fifo.metadata().unwrap().file_type().is_fifo());
    }
}

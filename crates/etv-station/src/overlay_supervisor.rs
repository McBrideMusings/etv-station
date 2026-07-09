//! Per-channel overlay subprocess lifecycle.
//!
//! When a channel's `overlay` config is set, the station daemon can spawn an
//! `etv-overlay pipe` subprocess that writes RGBA frames to a fifo. The fifo
//! is what the etv-next channel encoder reads as its overlay input.
//!
//! Spawn is **demand-driven**, mirroring etv-next's own channel lifecycle: an
//! overlay process runs only while its channel is actually being watched, so
//! the daemon holds zero overlay processes (and zero idle GPU contexts) at
//! boot and for unwatched channels.
//!
//! The handshake uses two marker files in the shared `output_folder`:
//! - `.overlay-wanted` — etv-next's channel worker touches it before opening
//!   the fifo for read (see `OVERLAY_WANTED_FILE_NAME` in ersatztv-core). Its
//!   presence is the spawn trigger, and it closes the chicken-and-egg where
//!   ffmpeg would block opening a fifo that has no writer.
//! - `.overlay-ready` — this supervisor passes it as the overlay's
//!   `--ready-file`; the overlay writes it once warm. etv-next waits on it
//!   before opening the fifo.
//!
//! Despawn is owned by the overlay process itself: it exits once it has had no
//! reader for its idle timeout (the same 90s etv-next's worker uses to stop a
//! channel), so the overlay winds down in lockstep with the channel it serves.
//! This supervisor just reaps that exit — clearing `.overlay-wanted` on a clean
//! idle exit so the process isn't immediately respawned, and respawning only on
//! a crash while the channel is still wanted.

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

const READY_FILE_NAME: &str = ".overlay-ready";
const WANTED_FILE_NAME: &str = ".overlay-wanted";
const POLL_INTERVAL: Duration = Duration::from_secs(5);

pub struct OverlayContext {
    pub channel_name: String,
    pub output_folder: PathBuf,
    pub overlay_config: PathBuf,
    pub fifo_path: PathBuf,
}

impl OverlayContext {
    pub fn ready_path(&self) -> PathBuf {
        self.output_folder.join(READY_FILE_NAME)
    }

    pub fn wanted_path(&self) -> PathBuf {
        self.output_folder.join(WANTED_FILE_NAME)
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

/// Run the supervisor loop for a single channel. Spawns the overlay only while
/// the channel is being watched — signalled by etv-next touching
/// `.overlay-wanted` before it opens the fifo — and lets the overlay exit on
/// its own idle timeout when watching stops. Only exits on shutdown notify.
pub async fn run(ctx: OverlayContext, shutdown: Arc<Notify>) {
    // Pre-create the fifo so etv-next can open it for read the instant a viewer
    // arrives, without racing overlay spawn.
    if let Err(err) = ensure_fifo(&ctx.fifo_path) {
        tracing::warn!(
            event = "overlay.fifo_failed",
            channel = %ctx.channel_name,
            error = %err,
            fifo = %ctx.fifo_path.display(),
            "failed to create overlay fifo at startup; will retry on spawn",
        );
    }
    // Clear stale markers from a prior run so we neither mistake an old ready
    // marker for this run nor spawn for a channel nobody is watching yet.
    let _ = std::fs::remove_file(ctx.ready_path());
    let _ = std::fs::remove_file(ctx.wanted_path());

    let mut child: Option<Child> = None;
    let mut ready_observed = false;

    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                if let Some(mut c) = child.take() {
                    tracing::info!(event = "overlay.shutdown", channel = %ctx.channel_name, "shutting down overlay process");
                    let _ = c.kill().await;
                }
                let _ = std::fs::remove_file(&ctx.fifo_path);
                let _ = std::fs::remove_file(ctx.ready_path());
                let _ = std::fs::remove_file(ctx.wanted_path());
                return;
            }
            _ = time::sleep(POLL_INTERVAL) => {
                match child.as_mut() {
                    // Running: reap the process if it has exited. A clean exit
                    // (code 0) is the overlay's own idle timeout — the channel
                    // stopped being watched — so clear `.overlay-wanted` to
                    // avoid an immediate respawn; a returning viewer's worker
                    // re-touches it. A non-zero exit is a crash: leave
                    // `.overlay-wanted` so the next tick respawns while watched.
                    Some(c) => {
                        if let Ok(Some(status)) = c.try_wait() {
                            child = None;
                            ready_observed = false;
                            let _ = std::fs::remove_file(ctx.ready_path());
                            if status.success() {
                                tracing::info!(
                                    event = "overlay.despawn",
                                    channel = %ctx.channel_name,
                                    "overlay exited on idle (channel no longer watched)",
                                );
                                let _ = std::fs::remove_file(ctx.wanted_path());
                            } else {
                                tracing::warn!(
                                    event = "overlay.exit",
                                    channel = %ctx.channel_name,
                                    status = %status,
                                    "overlay process crashed; will respawn while watched",
                                );
                            }
                        } else if !ready_observed && ctx.ready_path().exists() {
                            tracing::info!(
                                event = "overlay.ready",
                                channel = %ctx.channel_name,
                                "overlay reported ready (first frame written)",
                            );
                            ready_observed = true;
                        }
                    }
                    // Not running: spawn when a viewer's worker has requested it.
                    None => {
                        if ctx.wanted_path().exists() {
                            if let Err(err) = ensure_fifo(&ctx.fifo_path) {
                                tracing::warn!(
                                    event = "overlay.fifo_failed",
                                    channel = %ctx.channel_name,
                                    error = %err,
                                    "fifo not available; deferring overlay spawn",
                                );
                                continue;
                            }
                            // Drop any stale ready marker so we don't mistake it
                            // for the new process being ready.
                            let _ = std::fs::remove_file(ctx.ready_path());
                            match spawn_overlay(&ctx) {
                                Ok(new_child) => {
                                    tracing::info!(
                                        event = "overlay.spawn",
                                        channel = %ctx.channel_name,
                                        fifo = %ctx.fifo_path.display(),
                                        "channel watched; spawned overlay process",
                                    );
                                    child = Some(new_child);
                                    ready_observed = false;
                                }
                                Err(err) => {
                                    tracing::warn!(
                                        event = "overlay.spawn_failed",
                                        channel = %ctx.channel_name,
                                        error = %err,
                                        "failed to spawn overlay process; will retry next tick",
                                    );
                                }
                            }
                        }
                    }
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

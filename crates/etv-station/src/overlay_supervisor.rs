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
//! reader for its own idle timeout (60s — deliberately *under* the 90s
//! `HEARTBEAT_FILE_TIMEOUT` etv-next's worker uses to stop a channel, which
//! `etv-overlay` asserts at compile time), so the overlay winds down in lockstep
//! with the channel it serves. This supervisor reaps that exit, and decides from
//! it whether the channel has actually gone cold.
//!
//! That decision cannot be read directly. An ffmpeg blocked in `open()` on the
//! fifo — which is what etv-next leaves behind when it replaces a failed item
//! mid-stream — is indistinguishable from no reader at all, unless something
//! opens the fifo for write. So the supervisor probes by respawning: a clean
//! exit *after* the overlay had served a reader leaves `.overlay-wanted` in
//! place and brings a fresh overlay up, which unblocks any parked ffmpeg on its
//! first open. Only when an overlay lives its whole idle timeout without ever
//! seeing a reader — no `.overlay-ready` — is the channel treated as cold. A
//! crash always respawns while the channel is wanted.
//!
//! Going cold does not delete `.overlay-wanted`. The file is written by
//! etv-next, and one process retracting another's assertion is what let a
//! channel wedge: the retraction could land while a replacement ffmpeg was
//! already blocked opening the fifo, and nothing was left to respawn the writer
//! it waited on. Instead the supervisor remembers the mtime of the request it
//! served (etv-next rewrites the marker before every item, so it doubles as a
//! heartbeat) and stays asleep until a newer one appears. The startup and
//! shutdown paths still clear the marker — those are process boundaries where
//! the station is resetting its own view, and keeping the clear there is what
//! holds the "zero overlay processes at boot" property above.

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

// Both marker names come from ersatztv-core, the crate that also defines them
// for the etv-next side of the handshake. Importing rather than redeclaring is
// the point: these two filenames ARE the protocol, and a copy here would let a
// rename over there compile cleanly and fail silently at runtime — the station
// polling for a file the channel worker no longer writes, so no overlay ever
// spawns and every overlay channel's ffmpeg blocks forever on a fifo with no
// writer.
use ersatztv_core::{OVERLAY_READY_FILE_NAME, OVERLAY_WANTED_FILE_NAME};

const POLL_INTERVAL: Duration = Duration::from_secs(5);

pub struct OverlayContext {
    pub channel_name: String,
    pub output_folder: PathBuf,
    pub overlay_config: PathBuf,
    pub fifo_path: PathBuf,
}

impl OverlayContext {
    pub fn ready_path(&self) -> PathBuf {
        self.output_folder.join(OVERLAY_READY_FILE_NAME)
    }

    pub fn wanted_path(&self) -> PathBuf {
        self.output_folder.join(OVERLAY_WANTED_FILE_NAME)
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
    // The mtime of the last watch request we served all the way to a cold idle
    // exit. `None` means "not cold" — any request is live. Set instead of
    // deleting the marker, so a request newer than this wakes us again.
    let mut cold_at: Option<std::time::SystemTime> = None;
    // The `.overlay-wanted` mtime as it stood when the running overlay was
    // spawned — the request that overlay is actually serving. Going cold must
    // record THIS, not the mtime at the moment of reaping: etv-next rewrites the
    // marker before every item, so a viewer arriving during the overlay's idle
    // wind-down leaves a fresher mtime that the reap would otherwise capture as
    // "already served", and the supervisor would sleep through a live request.
    // That is the 2026-08-16 011-madison failure: marker touched at 06:08:39,
    // despawn recorded it at 06:08:40, and no overlay came back until the worker
    // rewrote the marker 25s later — 6s after it had given up and blacked out
    // the item.
    let mut served_mtime: Option<std::time::SystemTime> = None;

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
                    // Running: reap the process if it has exited. A non-zero
                    // exit is a crash: leave `.overlay-wanted` so the next tick
                    // respawns while watched. A clean exit (code 0) is the
                    // overlay's own idle timeout, and what that means depends on
                    // whether it ever had a reader — see the module docs.
                    Some(c) => {
                        if let Ok(Some(status)) = c.try_wait() {
                            // The overlay touches its ready file only after a
                            // frame has reached an attached reader, so this is
                            // the record of whether this instance ever served
                            // one. Check the file as well as the flag: an
                            // instance can come and go inside a single poll
                            // interval, and the flag is only set by a poll that
                            // finds it still running.
                            let had_reader = ready_observed || ctx.ready_path().exists();
                            child = None;
                            ready_observed = false;
                            let _ = std::fs::remove_file(ctx.ready_path());
                            match reap_decision(status.success(), had_reader) {
                                Reap::Crashed => tracing::warn!(
                                    event = "overlay.exit",
                                    channel = %ctx.channel_name,
                                    status = %status,
                                    "overlay process crashed; will respawn while watched",
                                ),
                                Reap::Reprobe => tracing::info!(
                                    event = "overlay.reprobe",
                                    channel = %ctx.channel_name,
                                    "overlay idled out after serving a reader; respawning to check for a waiting reader",
                                ),
                                Reap::Retire => {
                                    tracing::info!(
                                        event = "overlay.despawn",
                                        channel = %ctx.channel_name,
                                        "overlay saw no reader before idling out (channel no longer watched)",
                                    );
                                    // Go cold by remembering the request we just
                                    // served, NOT by deleting etv-next's file.
                                    // See the module docs: whoever writes the
                                    // marker owns it, and a returning viewer is
                                    // signalled by a fresher mtime.
                                    cold_at = served_mtime;
                                }
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
                    // Not running: spawn when a viewer's worker has requested it
                    // and we haven't already served that same request.
                    None => {
                        if wants_overlay(&ctx, cold_at) {
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
                                    cold_at = None;
                                    served_mtime = wanted_mtime(&ctx);
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

/// Modification time of the `.overlay-wanted` marker, or `None` if it isn't
/// there (or can't be stat'd).
fn wanted_mtime(ctx: &OverlayContext) -> Option<std::time::SystemTime> {
    std::fs::metadata(ctx.wanted_path())
        .and_then(|m| m.modified())
        .ok()
}

/// Whether a live watch request is outstanding.
///
/// etv-next rewrites `.overlay-wanted` before every playout item, refreshing its
/// mtime, so the marker doubles as a heartbeat. `cold_at` records the request we
/// already served to a no-reader idle exit; a marker no newer than that is the
/// same dead request and must not respawn us. Anything newer — a returning
/// viewer's worker touching it again — is a fresh request.
///
/// Reading the mtime rather than deleting the file keeps the marker owned by the
/// single process that writes it. The station used to delete it on going cold,
/// which raced etv-next's own restart: if a replacement ffmpeg was already
/// waiting on the fifo, the retraction meant nothing ever respawned the writer
/// it was blocked on.
fn wants_overlay(ctx: &OverlayContext, cold_at: Option<std::time::SystemTime>) -> bool {
    match (wanted_mtime(ctx), cold_at) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(now), Some(cold)) => now > cold,
    }
}

/// What reaping an exited overlay means for `.overlay-wanted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reap {
    /// Non-zero exit. Keep the marker; the next tick respawns.
    Crashed,
    /// Clean exit after serving a reader. Indistinguishable from the outside:
    /// the channel may have gone cold, or an ffmpeg may be parked in `open()`
    /// on the fifo waiting for the writer that just exited. Keep the marker and
    /// respawn — the fresh overlay's own open is the probe.
    Reprobe,
    /// Clean exit having never served a reader, i.e. a whole idle timeout with
    /// nothing attached. Nobody is watching; retire the marker.
    Retire,
}

fn reap_decision(exited_cleanly: bool, had_reader: bool) -> Reap {
    match (exited_cleanly, had_reader) {
        (false, _) => Reap::Crashed,
        (true, true) => Reap::Reprobe,
        (true, false) => Reap::Retire,
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

    // Reaping a clean exit used to retire `.overlay-wanted` unconditionally.
    // When the overlay exited while etv-next was mid-restart — a replacement
    // ffmpeg already blocked in open() on the fifo — that retired the marker,
    // so nothing ever respawned the writer that ffmpeg was waiting on and the
    // channel served an empty playlist forever. An overlay that had a reader
    // must leave the marker alone and be respawned.
    #[test]
    fn clean_exit_after_serving_a_reader_respawns_rather_than_retiring() {
        assert_eq!(reap_decision(true, true), Reap::Reprobe);
    }

    #[test]
    fn clean_exit_without_ever_seeing_a_reader_retires_the_marker() {
        assert_eq!(reap_decision(true, false), Reap::Retire);
    }

    #[test]
    fn crash_respawns_regardless_of_whether_it_served_a_reader() {
        assert_eq!(reap_decision(false, true), Reap::Crashed);
        assert_eq!(reap_decision(false, false), Reap::Crashed);
    }

    fn ctx_in(dir: &Path) -> OverlayContext {
        OverlayContext {
            channel_name: "test".into(),
            output_folder: dir.to_path_buf(),
            overlay_config: dir.join("overlay.toml"),
            fifo_path: dir.join("overlay.fifo"),
        }
    }

    // The two filenames ARE the protocol with etv-next, so pin what actually
    // lands on disk. Sourcing them from ersatztv-core stops a rename drifting
    // silently, but an import can still name the wrong one of the pair — and
    // swapping ready for wanted would compile, pass every other test here, and
    // deadlock the handshake at runtime.
    #[test]
    fn the_marker_paths_render_the_names_etv_next_looks_for() {
        let ctx = ctx_in(Path::new("/playout/chan"));
        assert_eq!(
            ctx.ready_path(),
            PathBuf::from("/playout/chan/.overlay-ready")
        );
        assert_eq!(
            ctx.wanted_path(),
            PathBuf::from("/playout/chan/.overlay-wanted")
        );
    }

    #[test]
    fn no_marker_means_no_overlay_wanted() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!wants_overlay(&ctx_in(dir.path()), None));
    }

    #[test]
    fn a_marker_with_no_cold_record_is_a_live_request() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        std::fs::write(ctx.wanted_path(), b"").unwrap();
        assert!(wants_overlay(&ctx, None));
    }

    // Going cold records the request just served instead of deleting the file,
    // so the same request must not immediately respawn the overlay.
    #[test]
    fn the_request_we_already_served_does_not_respawn() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        std::fs::write(ctx.wanted_path(), b"").unwrap();
        let cold = wanted_mtime(&ctx);
        assert!(cold.is_some());
        assert!(!wants_overlay(&ctx, cold));
    }

    // A returning viewer's worker rewrites the marker; the fresher mtime is the
    // signal to wake up, replacing the old delete-and-recreate handshake.
    #[test]
    fn a_newer_request_wakes_a_cold_channel() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        std::fs::write(ctx.wanted_path(), b"").unwrap();
        let cold = wanted_mtime(&ctx);
        // Filesystem mtime resolution varies; set it explicitly rather than
        // sleeping and hoping the clock moved far enough to notice.
        let later = cold.unwrap() + Duration::from_secs(30);
        std::fs::File::open(ctx.wanted_path())
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert!(wants_overlay(&ctx, cold));
    }

    /// The 2026-08-16 011-madison failure. A viewer arrives while the overlay is
    /// winding down: the worker rewrites `.overlay-wanted` at 06:08:39, the
    /// overlay is reaped at 06:08:40, and the reap records *that* fresh mtime as
    /// the request it served. The live request then looks already-served, no
    /// overlay comes back, and the worker blacks the item out after its 20s wait.
    ///
    /// Recording the mtime captured at SPAWN keeps the viewer's newer request
    /// distinguishable, so the next poll respawns.
    #[test]
    fn a_request_arriving_during_wind_down_is_not_recorded_as_served() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());

        // The marker as it stood when the overlay was spawned.
        std::fs::write(ctx.wanted_path(), b"").unwrap();
        let served_mtime = wanted_mtime(&ctx);

        // A viewer arrives just before the reap; the worker rewrites the marker.
        let arrival = served_mtime.unwrap() + Duration::from_secs(60);
        std::fs::File::open(ctx.wanted_path())
            .unwrap()
            .set_modified(arrival)
            .unwrap();

        // Reaping records what the overlay served, NOT what the marker says now.
        let cold = served_mtime;
        assert!(
            wants_overlay(&ctx, cold),
            "a request that landed after the overlay spawned must still be live"
        );

        // The bug: recording the marker's mtime at reap time swallows it.
        let cold_at_reap = wanted_mtime(&ctx);
        assert!(
            !wants_overlay(&ctx, cold_at_reap),
            "guards the regression — this is what the old code recorded"
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

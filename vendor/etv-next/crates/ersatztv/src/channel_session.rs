use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ersatztv::error::LineupError;
use ersatztv_core::{
    EXIT_REASON_FILE_NAME, HEARTBEAT_FILE_NAME, HEARTBEAT_FILE_TIMEOUT, READY_FILE_NAME,
    STALL_EXIT_CODE, reap_run_folders, reap_unreferenced_run_folders,
};
use tokio::sync::{Mutex, watch};

use crate::channel_health::{ExitVerdict, FAILURE_THRESHOLD, HealthMap};
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

/// Decide and reap one channel's run folders under a single hold of `active`,
/// so a spawn and the periodic sweep can never interleave.
///
/// `session_middleware` and the ready-wait path in `main` both take `active`
/// *before* calling [`ChannelSession::spawn`] and hold it across the spawn and
/// the `insert` that follows. Holding the same lock here for the whole
/// decide-then-reap step makes the two orderings exhaustive: either a spawn
/// wins the lock first — it lands in `active` before this function ever reads
/// it, so `has_active_worker` sees the entry and `keep_newest` is `true`, and
/// the spawn's own brand-new run folder is the newest one anyway, which
/// `reap_unreferenced_run_folders` never removes — or this function wins the lock first —
/// the spawn then blocks at `active.lock().await` until the reap below has
/// returned, so the run folder it would create does not exist yet for the
/// reap to see or delete. There is no window where a folder is created after
/// the decision but before the removal runs.
pub(crate) async fn reap_channel_run_folders(
    number: &str,
    output_folder: &Path,
    active: &Mutex<HashMap<String, ChannelSession>>,
) -> Result<usize, std::io::Error> {
    let guard = active.lock().await;
    let has_active_worker = guard.contains_key(number);
    let heartbeat_file = output_folder.join(HEARTBEAT_FILE_NAME);
    let keep_newest = has_active_worker || heartbeat_is_fresh(&heartbeat_file).await;
    let result = reap_unreferenced_run_folders(output_folder, keep_newest).await;
    drop(guard);
    result
}

/// Why a worker exited, in one line: its exit status, plus the error text it
/// left in [`EXIT_REASON_FILE_NAME`] if it left one. The file is removed here so
/// the next run's exit can never be blamed on this one's cause.
async fn exit_cause(
    status: &std::io::Result<std::process::ExitStatus>,
    exit_reason_file: &Path,
) -> String {
    let reason = tokio::fs::read_to_string(exit_reason_file).await.ok();
    if reason.is_some() {
        let _ = tokio::fs::remove_file(exit_reason_file).await;
    }
    let status = match status {
        Ok(s) => s.to_string(),
        Err(e) => format!("wait failed: {e}"),
    };
    match reason.as_deref().map(str::trim) {
        Some(reason) if !reason.is_empty() => format!("{status}: {reason}"),
        _ => status,
    }
}

/// One line per change in a channel's health, so a channel heading for the
/// 503 guard is visible on every step there rather than only once it arrives.
fn log_verdict(number: &str, name: &str, verdict: ExitVerdict, cause: &str, uptime: Duration) {
    let up = uptime.as_secs();
    match verdict {
        ExitVerdict::Failure {
            consecutive,
            crossed,
            retry_in,
        } => {
            let backoff = match retry_in {
                Some(wait) => format!("; next spawn allowed in {}s", wait.as_secs()),
                None => String::new(),
            };
            log::warn!(
                "channel {number} ({name}) failed with a viewer watching, {up}s after spawn: \
                 {cause}; consecutive failures {consecutive}/{FAILURE_THRESHOLD}{backoff}"
            );
            if crossed {
                log::error!(
                    "channel {number} ({name}) is now FAILED after {consecutive} consecutive \
                     failures; /channel/{number}.m3u8 answers 503 until a spawn is allowed in {}s",
                    retry_in.unwrap_or_default().as_secs()
                );
            }
        }
        ExitVerdict::Recovered { cleared } => log::warn!(
            "channel {number} ({name}) is no longer failed: a run of {up}s exited without \
             failing under a viewer, clearing {cleared} consecutive failures"
        ),
        ExitVerdict::Cleared { cleared } => log::info!(
            "channel {number} ({name}) exited healthily after {up}s; cleared {cleared} \
             consecutive failures"
        ),
        ExitVerdict::Healthy => {}
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
        // A reason left by a run this server never reaped — it died between the
        // worker's write and `exit_cause` — must not be blamed on this run's
        // exit if this one dies to a signal and writes nothing.
        let exit_reason_file = channel.output_folder().join(EXIT_REASON_FILE_NAME);
        let _ = std::fs::remove_file(&exit_reason_file);

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
        let channel_name = channel.name().to_owned();

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
            let cause = exit_cause(&status, &exit_reason_file).await;
            let uptime = started_at.elapsed();
            let verdict = health.lock().await.record_exit(
                &channel_number,
                viewer_attached,
                stalled,
                uptime,
                Instant::now(),
                cause.clone(),
            );
            log_verdict(&channel_number, &channel_name, verdict, &cause, uptime);

            // Reap this run's segment folder — the segments and .vtt
            // sidecars PlaylistManager's trim never reached, because it only
            // drops a segment once it is older than SEGMENT_RETENTION while
            // the worker is still running, and nothing trims once the worker
            // has exited. A folder written to within SEGMENT_RETENTION is
            // kept here (a stalled viewer may still come back for it) and
            // collected by the periodic sweep in `main` once it ages out.
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
            // replacement worker. The ordering matters again now that
            // `reap_channel_run_folders` holds this same lock across its own
            // decide-then-reap step: while this key is still in `active`, that
            // sweep sees a live worker and keeps every run folder untouched, so
            // a respawn racing the sweep can never be starved by it. Removing
            // the key before the reap above finished would open exactly the
            // window that helper exists to close.
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
    use std::collections::HashMap;
    use std::sync::Arc;

    use tempfile::TempDir;
    use tokio::sync::{Mutex, watch};

    use super::{
        ChannelSession, HEARTBEAT_FILE_NAME, heartbeat_is_fresh, reap_channel_run_folders,
        reap_run_folders,
    };

    /// Backdate `path`'s mtime past `SEGMENT_RETENTION`, so a reap treats it
    /// as a run nobody can still be coming back for.
    fn aged(path: &std::path::Path) {
        let then = std::time::SystemTime::now()
            - ersatztv_core::SEGMENT_RETENTION
            - std::time::Duration::from_secs(60);
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(then)).unwrap();
    }

    // The worker's own error text is the cause; reading it must also consume
    // it, or the next run's exit — a signal, say, which writes nothing — would
    // be logged with this run's reason.
    #[cfg(unix)]
    #[tokio::test]
    async fn exit_cause_names_the_workers_reason_once() {
        use std::os::unix::process::ExitStatusExt;

        let dir = TempDir::new().unwrap();
        let file = dir.path().join(super::EXIT_REASON_FILE_NAME);
        std::fs::write(&file, "channel 32 terminated after ffmpeg stall\n").unwrap();
        let stalled = Ok(std::process::ExitStatus::from_raw(75 << 8));

        let cause = super::exit_cause(&stalled, &file).await;
        assert!(
            cause.ends_with(": channel 32 terminated after ffmpeg stall"),
            "{cause}"
        );
        assert!(cause.contains("75"), "{cause}");
        assert!(!file.exists(), "the reason must not outlive its run");

        let again = super::exit_cause(&stalled, &file).await;
        assert!(!again.contains("ffmpeg stall"), "{again}");
    }

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
        aged(&dead_run);

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

    /// Regression test for the reap-sweep race (etv-station-262.1): without
    /// `reap_channel_run_folders` holding `active` across the whole
    /// decide-then-reap step, a spawn landing between the "no active worker"
    /// read and the reap's own directory listing loses its brand-new run
    /// folder to that reap — the sweep lists the directory after the folder
    /// exists but reaps as though nobody was there when it decided.
    ///
    /// Looped: the failure depends on the sweep task yielding (at
    /// `heartbeat_is_fresh`'s metadata read) before the racing spawn task
    /// runs, which a single current-thread `#[tokio::test]` schedules
    /// deterministically in practice but is not guaranteed by the language —
    /// every iteration surviving is the evidence the lock, not luck, is what
    /// protects the folder.
    #[tokio::test]
    async fn a_spawn_racing_the_sweep_never_loses_its_run_folder() {
        for _ in 0..50 {
            let dir = TempDir::new().unwrap();
            let channel_root = dir.path().to_path_buf();

            let dead_run = channel_root.join("r0000000000001-0000");
            tokio::fs::create_dir(&dead_run).await.unwrap();
            tokio::fs::write(dead_run.join("live000000.ts"), b"segment")
                .await
                .unwrap();

            let active: Arc<Mutex<HashMap<String, ChannelSession>>> =
                Arc::new(Mutex::new(HashMap::new()));

            let root_for_sweep = channel_root.clone();
            let active_for_sweep = Arc::clone(&active);
            let sweep = tokio::spawn(async move {
                reap_channel_run_folders("1", &root_for_sweep, &active_for_sweep)
                    .await
                    .unwrap();
            });

            // Give the sweep a chance to acquire `active` and reach its first
            // real await (the heartbeat metadata read) before the spawn below
            // starts racing it for the same lock — the same ordering
            // `session_middleware` and the ready-wait path produce against
            // the real sweep in `main`.
            tokio::task::yield_now().await;

            let root_for_spawn = channel_root.clone();
            let active_for_spawn = Arc::clone(&active);
            let spawn = tokio::spawn(async move {
                let mut guard = active_for_spawn.lock().await;
                let new_run = root_for_spawn.join("r0000000000002-0000");
                tokio::fs::create_dir(&new_run).await.unwrap();
                tokio::fs::write(new_run.join("live000000.ts"), b"segment")
                    .await
                    .unwrap();
                let (_ready_sender, ready_receiver) = watch::channel(false);
                guard.insert("1".to_owned(), ChannelSession { ready_receiver });
            });

            sweep.await.unwrap();
            spawn.await.unwrap();

            let new_run = channel_root.join("r0000000000002-0000");
            assert!(
                new_run.exists(),
                "a run folder created by a spawn racing the sweep must survive it"
            );
            assert!(new_run.join("live000000.ts").exists());
        }
    }

    /// Regression test for etv-station-262.1's remaining defect: the periodic
    /// sweep must keep a dead run's folder as long as the served playlist
    /// still names it, not just because it happens to be the newest one.
    ///
    /// Walks the real respawn shape: run folder A (dead, still named by
    /// `live.m3u8`) and run folder B (the replacement worker, active in
    /// `active`). The first sweep must leave both standing — this fails on
    /// the old `reap_run_folders` behavior, which pops the newest (B) and
    /// deletes A regardless of what the playlist says. Once `live.m3u8` is
    /// rewritten to name only B, the same sweep must collect A.
    #[tokio::test]
    async fn sweep_keeps_a_dead_run_folder_the_live_playlist_still_names() {
        let dir = TempDir::new().unwrap();
        let channel_root = dir.path();

        let dead_run = channel_root.join("r0000000000001-0000");
        let new_run = channel_root.join("r0000000000002-0000");
        tokio::fs::create_dir(&dead_run).await.unwrap();
        tokio::fs::create_dir(&new_run).await.unwrap();
        tokio::fs::write(dead_run.join("live000000.ts"), b"segment")
            .await
            .unwrap();
        tokio::fs::write(new_run.join("live000000.ts"), b"segment")
            .await
            .unwrap();
        aged(&dead_run);

        // The client's in-hand playlist: still points at the dead run's
        // segments, which have not scrolled out of the window yet.
        tokio::fs::write(
            channel_root.join("live.m3u8"),
            "r0000000000001-0000/live000000.ts\n",
        )
        .await
        .unwrap();

        let active: Arc<Mutex<HashMap<String, ChannelSession>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (_ready_sender, ready_receiver) = watch::channel(false);
        active
            .lock()
            .await
            .insert("1".to_owned(), ChannelSession { ready_receiver });

        reap_channel_run_folders("1", channel_root, &active)
            .await
            .unwrap();

        assert!(
            dead_run.exists(),
            "a run folder the live playlist still names must survive the sweep"
        );
        assert!(new_run.exists(), "the active worker's run folder survives");
        assert!(
            channel_root.join("live.m3u8").exists(),
            "the playlist file itself is never a reap candidate"
        );

        // Advance the respawn: the current playlist now names only the new
        // run's segments, so nothing protects the dead one any more.
        tokio::fs::write(
            channel_root.join("live.m3u8"),
            "r0000000000002-0000/live000000.ts\n",
        )
        .await
        .unwrap();

        reap_channel_run_folders("1", channel_root, &active)
            .await
            .unwrap();

        assert!(
            !dead_run.exists(),
            "once no playlist names it, the dead run is collected"
        );
        assert!(new_run.exists());
    }
}

//! Regression coverage for the `watch` subcommand's ffmpeg-child cleanup
//! (`KillOnDrop` in `src/bin/etv-overlay.rs`).
//!
//! `watch` spawns ffmpeg with `-stream_loop -1`, which never exits on its
//! own — the only thing that stops it is `watch` itself killing it. Before
//! the fix, a shutdown signal arriving while the process was still parked in
//! `fifo.open_for_writing` (blocked because nothing had opened the *read*
//! side of the overlay fifo yet) returned early and skipped the
//! `child.kill()` at the bottom of the function entirely, leaking ffmpeg for
//! good.
//!
//! To make that race deterministic rather than a timing gamble against
//! ffmpeg's own startup speed, `--input` here is a *second*, writer-less
//! fifo rather than a real media file: ffmpeg blocks forever on opening its
//! own first `-i` argument and so can never reach its second `-i` (the
//! overlay fifo) to become its reader — guaranteeing `watch`'s
//! `open_for_writing` is still genuinely stuck when SIGTERM arrives, not
//! racing to get there first.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::sys::stat::Mode;
use nix::unistd::{Pid, mkfifo};

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn is_alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

/// The ffmpeg child's pid, found by scanning `ps` for a process whose parent
/// is `parent_pid` — portable enough across macOS/BSD and Linux `ps`, unlike
/// GNU-only `--ppid`.
///
/// Returns a PID only after confirming it is alive. This closes the race where
/// the process might exit between the `ps` scan and the caller's check.
fn find_child_pid(parent_pid: i32, deadline: Instant) -> Option<i32> {
    while Instant::now() < deadline {
        let out = Command::new("ps").args(["-Ao", "pid,ppid"]).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines().skip(1) {
            let mut cols = line.split_whitespace();
            let (Some(pid), Some(ppid)) = (cols.next(), cols.next()) else {
                continue;
            };
            if ppid.parse::<i32>() == Ok(parent_pid) {
                if let Ok(pid_i32) = pid.parse::<i32>() {
                    // Confirm the PID is actually alive before returning it.
                    // If it's dead, the loop will retry and find it again if it was
                    // respawned, or timeout if the child process has truly exited.
                    if is_alive(pid_i32) {
                        return Some(pid_i32);
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[test]
fn sigterm_during_reader_wait_kills_the_ffmpeg_child_too() {
    if !have_ffmpeg() {
        eprintln!("skipping: no ffmpeg on PATH");
        return;
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let config = manifest_dir.join("fixtures/title_chyron.yaml");
    assert!(
        config.exists(),
        "missing overlay fixture: {}",
        config.display()
    );

    let bin = PathBuf::from(env!("CARGO_BIN_EXE_etv-overlay"));
    let work_dir = tempfile::tempdir().unwrap();
    let overlay_fifo = work_dir.path().join("overlay.fifo");
    // Never opened for writing by anything — ffmpeg's own "-i" on this path
    // blocks forever, so it can never reach the overlay fifo below.
    let bg_fifo = work_dir.path().join("bg.fifo");
    mkfifo(&bg_fifo, Mode::S_IRUSR | Mode::S_IWUSR).expect("mkfifo bg_fifo");

    let mut child = Command::new(&bin)
        .args(["watch", "--input"])
        .arg(&bg_fifo)
        .arg("--config")
        .arg(&config)
        .arg("--fifo")
        .arg(&overlay_fifo)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn etv-overlay watch");

    let watch_pid = child.id() as i32;
    let ffmpeg_pid = find_child_pid(watch_pid, Instant::now() + Duration::from_secs(5))
        .expect("etv-overlay watch never spawned a live ffmpeg child within 5s");

    assert!(
        is_alive(watch_pid),
        "sanity: watch process should be running"
    );

    kill(Pid::from_raw(watch_pid), Signal::SIGTERM).expect("send SIGTERM");

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && (is_alive(watch_pid) || is_alive(ffmpeg_pid)) {
        std::thread::sleep(Duration::from_millis(50));
    }

    // Reap so a lingering zombie doesn't fail teardown elsewhere.
    let _ = child.wait();

    assert!(
        !is_alive(watch_pid),
        "watch process should have exited after SIGTERM"
    );
    assert!(
        !is_alive(ffmpeg_pid),
        "ffmpeg child leaked past its parent's shutdown — this is the bug KillOnDrop fixes"
    );
}

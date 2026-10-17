//! The one place that decides which `ffprobe` the station spawns, and a
//! deadline-bounded runner for callers that cannot await.

use std::ffi::OsString;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Environment variable naming the `ffprobe` binary; unset means `ffprobe`
/// on `PATH`.
pub const FFPROBE_ENV: &str = "ETV_STATION_FFPROBE";

/// The program every `ffprobe` spawn runs.
pub fn program() -> OsString {
    match std::env::var_os(FFPROBE_ENV) {
        Some(p) if !p.is_empty() => p,
        _ => OsString::from("ffprobe"),
    }
}

/// How a bounded run ended.
pub enum Bounded {
    /// The process exited before the deadline.
    Done { success: bool, stdout: Vec<u8> },
    /// The deadline passed; the process was killed.
    TimedOut,
}

/// Run `ffprobe` with `args`, killing it after `timeout`. A stale network
/// mount leaves ffprobe blocked in an uninterruptible read, so the wait polls
/// rather than blocking on the child.
pub fn run_bounded(args: &[OsString], timeout: Duration) -> std::io::Result<Bounded> {
    run_program(&program(), args, timeout)
}

fn run_program(
    program: &OsString,
    args: &[OsString],
    timeout: Duration,
) -> std::io::Result<Bounded> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut pipe = child.stdout.take().expect("stdout was piped");
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            let stdout = reader.join().unwrap_or_default();
            return Ok(Bounded::Done {
                success: status.success(),
                stdout,
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            // The reader ends once the killed child's pipe closes; a process
            // stuck in the kernel may keep it open, so do not join it.
            return Ok(Bounded::TimedOut);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hung_process_is_killed_at_the_deadline() {
        let started = Instant::now();
        let out = run_program(
            &OsString::from("sleep"),
            &[OsString::from("30")],
            Duration::from_millis(200),
        )
        .unwrap();
        assert!(matches!(out, Bounded::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_finished_process_returns_its_stdout() {
        let out = run_program(
            &OsString::from("echo"),
            &[OsString::from("hi")],
            Duration::from_secs(10),
        )
        .unwrap();
        match out {
            Bounded::Done { success, stdout } => {
                assert!(success);
                assert_eq!(stdout, b"hi\n");
            }
            Bounded::TimedOut => panic!("echo timed out"),
        }
    }
}

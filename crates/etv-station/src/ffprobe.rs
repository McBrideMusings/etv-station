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

/// Longest any caller waits for one `ffprobe`; a stale network mount otherwise
/// blocks the caller indefinitely.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// How a bounded run ended.
pub enum Bounded {
    /// The process exited before the deadline. `stderr` is the last 4096 bytes
    /// it wrote there, for the caller to put in an error message.
    Done {
        success: bool,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    /// The deadline passed; the process was killed.
    TimedOut,
}

/// Run `ffprobe` with `args`, killing it after `timeout`. A stale network
/// mount leaves ffprobe blocked in an uninterruptible read, so the wait polls
/// rather than blocking on the child.
pub fn run_bounded(args: &[OsString], timeout: Duration) -> std::io::Result<Bounded> {
    run_program(&program(), args, timeout)
}

/// The argv that probes one media file as JSON, format and streams. Only the
/// `file` protocol is allowed, so a path cannot name a URL.
pub fn media_args(path: &std::path::Path) -> Vec<OsString> {
    [
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_format",
        "-show_streams",
        "-protocol_whitelist",
        "file",
        "-i",
    ]
    .iter()
    .map(Into::into)
    .chain([path.as_os_str().to_owned()])
    .collect()
}

/// `ffprobe` output for `path` as the map `ctx.probe` returns. Only the `file`
/// protocol is allowed, so a path cannot name a URL. `Ok(None)` when
/// ffprobe runs but rejects the file; `Err` when ffprobe cannot be run.
pub fn probe_media(path: &std::path::Path) -> Result<Option<serde_json::Value>, String> {
    use serde_json::{Map as JsonMap, Value, json};

    let args = media_args(path);
    let stdout =
        match run_bounded(&args, PROBE_TIMEOUT).map_err(|e| format!("cannot run ffprobe: {e}"))? {
            Bounded::Done {
                success: true,
                stdout,
                ..
            } => stdout,
            Bounded::Done { stderr, .. } => {
                tracing::info!(
                    event = "ffprobe.rejected",
                    path = %path.display(),
                    stderr = %String::from_utf8_lossy(&stderr).trim(),
                    "ffprobe could not read the file"
                );
                return Ok(None);
            }
            Bounded::TimedOut => {
                tracing::warn!(
                    event = "ffprobe.timeout",
                    path = %path.display(),
                    timeout_secs = PROBE_TIMEOUT.as_secs(),
                    "ffprobe did not finish; ctx.probe returns ()"
                );
                return Ok(None);
            }
        };
    let raw: Value = serde_json::from_slice(&stdout)
        .map_err(|e| format!("ffprobe printed invalid JSON: {e}"))?;

    let tags = |v: &Value| -> Value {
        let mut out = JsonMap::new();
        if let Some(obj) = v.get("tags").and_then(Value::as_object) {
            for (k, val) in obj {
                if let Some(s) = val.as_str() {
                    out.insert(k.to_ascii_lowercase(), json!(s));
                }
            }
        }
        Value::Object(out)
    };
    let duration_ms = raw["format"]["duration"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|secs| (secs * 1000.0).round() as i64);
    let streams: Vec<Value> = raw["streams"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|s| {
            let stream_tags = tags(s);
            json!({
                "index": s["index"].as_i64(),
                "kind": s["codec_type"].as_str().unwrap_or("other"),
                "codec": s["codec_name"].as_str().unwrap_or(""),
                "lang": stream_tags["language"].as_str().filter(|l| *l != "und"),
                "width": s["width"].as_i64(),
                "height": s["height"].as_i64(),
                "channels": s["channels"].as_i64(),
                "tags": stream_tags,
            })
        })
        .collect();
    Ok(Some(json!({
        "duration_ms": duration_ms,
        "format": raw["format"]["format_name"].as_str().unwrap_or(""),
        "tags": tags(&raw["format"]),
        "streams": streams,
    })))
}

/// [`run_bounded`] for async callers: the polling wait runs on the blocking
/// pool, so it does not hold a runtime worker.
pub async fn run_bounded_async(args: Vec<OsString>) -> std::io::Result<Bounded> {
    tokio::task::spawn_blocking(move || run_bounded(&args, PROBE_TIMEOUT))
        .await
        .map_err(std::io::Error::other)?
}

fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

/// Most of a child's stderr kept for an error message; the tail is where
/// ffprobe puts the line that says why it failed.
const STDERR_KEEP: usize = 4096;

/// Read `pipe` to the end but keep only its last [`STDERR_KEEP`] bytes, so a
/// file that makes ffprobe warn for megabytes cannot grow the buffer.
fn drain_tail(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut tail: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let n = match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            tail.extend_from_slice(&chunk[..n]);
            if tail.len() > STDERR_KEEP {
                tail.drain(..tail.len() - STDERR_KEEP);
            }
        }
        tail
    })
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
        .stderr(Stdio::piped())
        .spawn()?;
    let reader = drain(child.stdout.take().expect("stdout was piped"));
    let err_reader = drain_tail(child.stderr.take().expect("stderr was piped"));
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            let stdout = reader.join().unwrap_or_default();
            let stderr = err_reader.join().unwrap_or_default();
            return Ok(Bounded::Done {
                success: status.success(),
                stdout,
                stderr,
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            // SIGKILL does not land while a stale mount holds the process in
            // an uninterruptible read, so reap it on a thread rather than
            // block the caller past the deadline.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            // The readers end once the killed child's pipe closes; a process
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
    fn stderr_keeps_only_its_tail() {
        let out = run_program(
            &OsString::from("sh"),
            &[
                OsString::from("-c"),
                OsString::from("yes warning | head -c 1000000 >&2; echo LAST >&2"),
            ],
            Duration::from_secs(10),
        )
        .unwrap();
        match out {
            Bounded::Done { stderr, .. } => {
                assert_eq!(stderr.len(), STDERR_KEEP);
                assert!(stderr.ends_with(b"LAST\n"));
            }
            Bounded::TimedOut => panic!("timed out"),
        }
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
            Bounded::Done {
                success, stdout, ..
            } => {
                assert!(success);
                assert_eq!(stdout, b"hi\n");
            }
            Bounded::TimedOut => panic!("echo timed out"),
        }
    }
}

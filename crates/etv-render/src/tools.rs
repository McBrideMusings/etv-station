//! Running ffmpeg, ffprobe and Python, and turning their failures into text a
//! person can act on.

use std::process::{Command, Output};

/// How many trailing stderr lines a failure keeps.
const STDERR_LINES: usize = 12;

/// Run `cmd` to completion. A failure to start names the program; a non-zero
/// exit carries the last lines of its stderr.
pub fn run(cmd: &mut Command) -> Result<Output, String> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let out = cmd
        .output()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    if out.status.success() {
        return Ok(out);
    }
    Err(format!(
        "{program} exited with {}: {}",
        out.status,
        tail(&out.stderr)
    ))
}

pub fn tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let from = lines.len().saturating_sub(STDERR_LINES);
    lines[from..].join(" | ")
}

/// A media file's duration in seconds, from ffprobe.
pub fn duration_s(ffprobe: &str, path: &std::path::Path) -> Result<f64, String> {
    let out = run(Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(path))?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .map_err(|e| format!("ffprobe gave no duration for {}: {e}", path.display()))
}

/// The mean volume (RMS, in dB) of `len` seconds of `path` from `start`.
pub fn mean_volume_db(
    ffmpeg: &str,
    path: &std::path::Path,
    start: f64,
    len: f64,
) -> Result<f64, String> {
    let out = run(Command::new(ffmpeg)
        .args(["-hide_banner", "-nostats", "-ss"])
        .arg(format!("{start:.4}"))
        .arg("-t")
        .arg(format!("{len:.4}"))
        .arg("-i")
        .arg(path)
        .args(["-vn", "-af", "volumedetect", "-f", "null", "-"]))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    stderr
        .lines()
        .find_map(|l| l.split("mean_volume:").nth(1))
        .and_then(|rest| {
            rest.trim()
                .trim_end_matches("dB")
                .trim()
                .parse::<f64>()
                .ok()
        })
        .ok_or_else(|| {
            format!(
                "ffmpeg volumedetect printed no mean_volume for {}",
                path.display()
            )
        })
}

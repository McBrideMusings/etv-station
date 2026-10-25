//! Fixtures shared by the renderer's tests: a source video, pre-separated
//! click-track stems, and the checked-in plan with its paths filled in.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use etv_render::plan::{Hook, Plan};
use etv_render::{Settings, stems};

pub const RATE: u32 = 44_100;

/// Settings for a cache under `dir`, using the ffmpeg on `PATH`.
pub fn settings(dir: &Path) -> Settings {
    Settings::from_lookup(dir.join("cache"), |_| None)
}

/// A 40-second 320x240 test picture, without audio.
pub fn make_source_video(path: &Path) {
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=25",
            "-t",
            "40",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(path)
        .status()
        .expect("ffmpeg must be installed to run the renderer tests");
    assert!(status.success(), "ffmpeg could not make the source video");
}

/// A stereo WAV with a short decaying tone on every beat of `bpm`.
pub fn write_click_wav(path: &Path, bpm: f64, seconds: f64, freq: f64) {
    let frames = (seconds * f64::from(RATE)) as usize;
    let beat = 60.0 / bpm;
    let mut data = Vec::with_capacity(frames * 4);
    for n in 0..frames {
        let t = n as f64 / f64::from(RATE);
        let since = t - (t / beat).floor() * beat;
        let v = if since < 0.02 {
            (2.0 * std::f64::consts::PI * freq * since).sin() * (1.0 - since / 0.02) * 0.8
        } else {
            0.0
        };
        let s = (v * 32767.0) as i16;
        data.extend_from_slice(&s.to_le_bytes());
        data.extend_from_slice(&s.to_le_bytes());
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 4).to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, out).unwrap();
}

/// Put a hook's stems where the renderer looks for them, so Demucs is not run.
pub fn seed_stems(settings: &Settings, hook: &Hook) {
    let dir = stems::stem_dir(settings, hook);
    let (_, len) = stems::region(hook);
    write_click_wav(&dir.join(stems::VOCALS), hook.native_bpm, len, 1500.0);
    write_click_wav(&dir.join(stems::NO_VOCALS), hook.native_bpm, len, 600.0);
}

/// The checked-in fixture plan with `@DIR@` replaced by `dir`.
pub fn fixture_plan(dir: &Path) -> Plan {
    let text =
        include_str!("../fixtures/chapter.plan.json").replace("@DIR@", &dir.display().to_string());
    Plan::parse(&text).expect("the fixture plan is valid")
}

/// Decode a file's audio to mono samples at 44.1 kHz.
pub fn decode_mono(path: &Path) -> Vec<f32> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-vn", "-ac", "1", "-ar", "44100", "-f", "f32le", "-"])
        .output()
        .expect("ffmpeg runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

/// Seconds at which a beat starts: the first sample over a quarter of the peak,
/// with a 250 ms refractory gap.
pub fn onsets(samples: &[f32]) -> Vec<f64> {
    let peak = samples.iter().fold(0f32, |m, s| m.max(s.abs()));
    let threshold = peak * 0.25;
    let gap = (0.25 * f64::from(RATE)) as usize;
    let mut out = Vec::new();
    let mut last = None::<usize>;
    for (i, s) in samples.iter().enumerate() {
        if s.abs() > threshold && last.is_none_or(|l| i - l > gap) {
            out.push(i as f64 / f64::from(RATE));
            last = Some(i);
        }
    }
    out
}

pub fn stdev(values: &[f64]) -> f64 {
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64).sqrt()
}

pub fn duration_s(path: &Path) -> f64 {
    etv_render::tools::duration_s("ffprobe", path).expect("ffprobe reads the render")
}

pub fn temp() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    (dir, path)
}

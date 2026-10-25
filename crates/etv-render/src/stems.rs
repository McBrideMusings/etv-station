//! Vocal and instrumental stems for one hook, cut from its song and cached.
//!
//! A hook's region runs from four bars before its landing to eighteen bars
//! after it, separated into `vocals.wav` and `no_vocals.wav` by Demucs. The
//! result is cached by entry and region, so a song that appears in two
//! chapters is separated once, and a cache that was filled by any means —
//! including a pair of files put there by hand — is used as it stands.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::deps;
use crate::error::RenderError;
use crate::plan::{Hook, REGION_BARS_AFTER, REGION_BARS_BEFORE};
use crate::settings::Settings;
use crate::tools;

pub const VOCALS: &str = "vocals.wav";
pub const NO_VOCALS: &str = "no_vocals.wav";

/// Where a hook's stems are kept: `<cache>/stems/<entry>/<start ms>-<length ms>`.
pub fn stem_dir(settings: &Settings, hook: &Hook) -> PathBuf {
    let (start, len) = region(hook);
    settings
        .cache_dir
        .join("stems")
        .join(sanitize(&hook.entry_id))
        .join(format!("{}-{}", ms(start), ms(len)))
}

/// The slice of the song that is separated: (start, length) in seconds.
pub fn region(hook: &Hook) -> (f64, f64) {
    let bar = 240.0 / hook.native_bpm;
    (
        hook.landing_s - REGION_BARS_BEFORE * bar,
        (REGION_BARS_BEFORE + f64::from(REGION_BARS_AFTER)) * bar,
    )
}

fn ms(seconds: f64) -> i64 {
    (seconds * 1000.0).round() as i64
}

/// A directory name for `entry_id`: its readable characters plus a short hash
/// of the exact id, so `plex:1/a` and `plex_1_a` get different directories.
fn sanitize(entry_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(entry_id.as_bytes());
    let tag: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    let readable: String = entry_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{readable}-{tag}")
}

/// Make sure `hook`'s stems exist, separating the song if they do not.
/// `layer` names the hook in any error.
pub fn ensure(settings: &Settings, hook: &Hook, layer: &str) -> Result<PathBuf, RenderError> {
    let dir = stem_dir(settings, hook);
    if dir.join(VOCALS).is_file() && dir.join(NO_VOCALS).is_file() {
        tracing::info!(event = "render.stems_cached", entry = %hook.entry_id, dir = %dir.display(), "stems already separated");
        return Ok(dir);
    }
    separate(settings, hook, &dir, layer)?;
    Ok(dir)
}

fn separate(settings: &Settings, hook: &Hook, dir: &Path, layer: &str) -> Result<(), RenderError> {
    let fail = |layer: &str, message: String| RenderError::new(layer, message);
    let (demucs, torch) = deps::python_modules(settings);
    if let Err(reason) = demucs {
        return Err(fail(
            "dependency demucs",
            format!("demucs is missing: {reason}"),
        ));
    }
    if let Err(reason) = torch {
        return Err(fail(
            "dependency pytorch",
            format!("pytorch is missing: {reason}"),
        ));
    }

    let parent = dir.parent().expect("stem_dir has a parent");
    fs::create_dir_all(parent)
        .map_err(|e| fail(layer, format!("cannot create {}: {e}", parent.display())))?;
    let scratch = parent.join(format!(".separating-{}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch)
        .map_err(|e| fail(layer, format!("cannot create {}: {e}", scratch.display())))?;
    let result = separate_into(settings, hook, &scratch, dir, layer);
    let _ = fs::remove_dir_all(&scratch);
    result
}

fn separate_into(
    settings: &Settings,
    hook: &Hook,
    scratch: &Path,
    dir: &Path,
    layer: &str,
) -> Result<(), RenderError> {
    let fail = |message: String| RenderError::new(layer, message);
    let (start, len) = region(hook);
    let region_wav = scratch.join("region.wav");
    tools::run(
        Command::new(&settings.ffmpeg)
            .args(["-y", "-v", "error", "-ss"])
            .arg(format!("{start:.4}"))
            .arg("-t")
            .arg(format!("{len:.4}"))
            .arg("-i")
            .arg(&hook.path)
            .args(["-vn", "-ac", "2", "-ar", "44100"])
            .arg(&region_wav),
    )
    .map_err(|e| {
        fail(format!(
            "cannot cut the region of {}: {e}",
            hook.path.display()
        ))
    })?;

    let out = scratch.join("demucs");
    let started = std::time::Instant::now();
    let mut cmd = Command::new(&settings.python);
    cmd.args(["-m", "demucs", "--two-stems=vocals", "-n", "htdemucs", "-o"])
        .arg(&out)
        .arg(&region_wav);
    // Model weights download once, into the cache, rather than into a home
    // directory the container's uid may not own.
    if std::env::var_os("TORCH_HOME").is_none() {
        cmd.env("TORCH_HOME", settings.cache_dir.join("torch"));
    }
    tools::run(&mut cmd)
        .map_err(|e| fail(format!("demucs failed on {}: {e}", hook.path.display())))?;
    tracing::info!(
        event = "render.stems_separated",
        entry = %hook.entry_id,
        seconds = started.elapsed().as_secs_f64(),
        "demucs separated a hook region"
    );

    let produced = out.join("htdemucs").join("region");
    for name in [VOCALS, NO_VOCALS] {
        if !produced.join(name).is_file() {
            return Err(fail(format!(
                "demucs finished but wrote no {name} under {}",
                produced.display()
            )));
        }
    }
    match fs::rename(&produced, dir) {
        Ok(()) => Ok(()),
        // A concurrent render separated the same region first; its copy is as good.
        Err(_) if dir.join(VOCALS).is_file() && dir.join(NO_VOCALS).is_file() => Ok(()),
        Err(e) => Err(fail(format!("cannot move stems to {}: {e}", dir.display()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook() -> Hook {
        Hook {
            entry_id: "plex:123/a b".into(),
            path: "/m/a.mp4".into(),
            landing_s: 20.0,
            native_bpm: 120.0,
            semitones: 0,
            vocal_over_bars: 0,
            full_bars: 8,
        }
    }

    #[test]
    fn the_region_runs_four_bars_before_to_eighteen_after_the_landing() {
        let (start, len) = region(&hook());
        assert_eq!(start, 20.0 - 8.0);
        assert_eq!(len, 22.0 * 2.0);
    }

    #[test]
    fn the_stem_dir_is_keyed_by_entry_and_region() {
        let s = Settings::from_lookup("/cache".into(), |_| None);
        let dir = stem_dir(&s, &hook()).display().to_string();
        assert!(dir.starts_with("/cache/stems/plex_123_a_b-"), "{dir}");
        assert!(dir.ends_with("/12000-44000"), "{dir}");
    }

    #[test]
    fn ids_that_differ_only_in_punctuation_get_different_directories() {
        assert_ne!(sanitize("plex:123/a"), sanitize("plex_123_a"));
    }
}

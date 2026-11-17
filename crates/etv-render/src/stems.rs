//! Separated stems for the stretch of a source a plan's stem layers read.
//!
//! For each source that any layer reads as `vocals` or `instrumental`, the
//! source's `separate` span, or else the region from the earliest such read to
//! the latest widened by a margin either side so separation has context at the
//! edges, is cut from the file and split by Demucs into `vocals.wav` and `no_vocals.wav`. The result is cached by
//! source and region, so a source read the same way by two plans is separated
//! once, and a cache filled by any means, a pair of files put there by hand
//! included, is used as it stands.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::deps;
use crate::error::RenderError;
use crate::plan::{Plan, Stem};
use crate::settings::Settings;
use crate::tools;

pub const VOCALS: &str = "vocals.wav";
pub const NO_VOCALS: &str = "no_vocals.wav";

/// Seconds of the source separated before the first read, so the separator
/// has context and the first read does not start at the separated edge.
pub const MARGIN_BEFORE_S: f64 = 8.0;
/// Seconds separated after the last read.
pub const MARGIN_AFTER_S: f64 = 1.0;

/// The stretch of one source that is separated.
#[derive(Debug, Clone, PartialEq)]
pub struct Region {
    pub source: String,
    pub path: PathBuf,
    pub start_s: f64,
    pub len_s: f64,
}

/// The file a stem lives in, inside a region's directory.
pub fn file_name(stem: Stem) -> Option<&'static str> {
    match stem {
        Stem::Full => None,
        Stem::Vocals => Some(VOCALS),
        Stem::Instrumental => Some(NO_VOCALS),
    }
}

/// One region per source that any layer reads as a separated stem, in source
/// id order.
pub fn regions(plan: &Plan) -> Vec<Region> {
    let mut span: BTreeMap<&str, (f64, f64)> = BTreeMap::new();
    for l in plan.layers.iter().filter(|l| l.stem != Stem::Full) {
        let (a, b) = (l.src_start_s, l.src_start_s + l.src_len_s());
        let e = span.entry(l.source.as_str()).or_insert((a, b));
        e.0 = e.0.min(a);
        e.1 = e.1.max(b);
    }
    span.into_iter()
        .map(|(id, (a, b))| {
            let source = plan
                .sources
                .iter()
                .find(|s| s.id == id)
                .expect("validated: every layer names a known source");
            let (start_s, len_s) = match source.separate {
                Some(sep) => (sep.start_s, sep.len_s),
                None => {
                    let start = (a - MARGIN_BEFORE_S).max(0.0);
                    (start, b + MARGIN_AFTER_S - start)
                }
            };
            Region {
                source: id.to_string(),
                path: source.path.clone(),
                start_s,
                len_s,
            }
        })
        .collect()
}

/// Where a region's stems are kept: `<cache>/stems/<source>/<start ms>-<length ms>`.
pub fn stem_dir(settings: &Settings, region: &Region) -> PathBuf {
    settings
        .cache_dir
        .join("stems")
        .join(sanitize(&region.source))
        .join(format!("{}-{}", ms(region.start_s), ms(region.len_s)))
}

fn ms(seconds: f64) -> i64 {
    (seconds * 1000.0).round() as i64
}

/// A directory name for `id`: its readable characters plus a short hash of the
/// exact id, so `plex:1/a` and `plex_1_a` get different directories.
fn sanitize(id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(id.as_bytes());
    let tag: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    let readable: String = id
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

/// Make sure `region`'s stems exist, separating it if they do not. `layer`
/// names the region in any error.
pub fn ensure(settings: &Settings, region: &Region, layer: &str) -> Result<PathBuf, RenderError> {
    let dir = stem_dir(settings, region);
    if dir.join(VOCALS).is_file() && dir.join(NO_VOCALS).is_file() {
        tracing::info!(event = "render.stems_cached", source = %region.source, dir = %dir.display(), "stems already separated");
        return Ok(dir);
    }
    separate(settings, region, &dir, layer)?;
    Ok(dir)
}

fn separate(
    settings: &Settings,
    region: &Region,
    dir: &Path,
    layer: &str,
) -> Result<(), RenderError> {
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
    let result = separate_into(settings, region, &scratch, dir, layer);
    let _ = fs::remove_dir_all(&scratch);
    result
}

fn separate_into(
    settings: &Settings,
    region: &Region,
    scratch: &Path,
    dir: &Path,
    layer: &str,
) -> Result<(), RenderError> {
    let fail = |message: String| RenderError::new(layer, message);
    let region_wav = scratch.join("region.wav");
    tools::run(
        Command::new(&settings.ffmpeg)
            .args(["-y", "-v", "error", "-ss"])
            .arg(format!("{:.4}", region.start_s))
            .arg("-t")
            .arg(format!("{:.4}", region.len_s))
            .arg("-i")
            .arg(&region.path)
            .args(["-vn", "-ac", "2", "-ar", "44100"])
            .arg(&region_wav),
    )
    .map_err(|e| {
        fail(format!(
            "cannot cut the region of {}: {e}",
            region.path.display()
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
        .map_err(|e| fail(format!("demucs failed on {}: {e}", region.path.display())))?;
    tracing::info!(
        event = "render.stems_separated",
        source = %region.source,
        seconds = started.elapsed().as_secs_f64(),
        "demucs separated a region"
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
    use crate::plan::tests::sample;

    #[test]
    fn a_region_spans_every_stem_read_of_its_source_plus_the_margins() {
        let mut p = sample();
        // A second vocals read of `a`, later in the source.
        let mut later = p.layers[0].clone();
        later.src_start_s = 50.0;
        later.out_start_s = 8.0;
        later.out_len_s = 5.0;
        later.ratio = 1.0;
        p.layers.push(later);
        let r = regions(&p);
        // Only `a` is read as a stem; `b` is read whole.
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].source, "a");
        // Reads run 30 s to 55 s: 8 s margin before, 1 s after.
        assert!((r[0].start_s - 22.0).abs() < 1e-9);
        assert!((r[0].len_s - 34.0).abs() < 1e-9);
    }

    #[test]
    fn a_region_near_the_top_of_the_file_starts_at_zero() {
        let mut p = sample();
        p.layers[0].src_start_s = 3.0;
        assert_eq!(regions(&p)[0].start_s, 0.0);
    }

    #[test]
    fn the_stem_dir_is_keyed_by_source_and_region() {
        let s = Settings::from_lookup("/cache".into(), |_| None);
        let region = Region {
            source: "plex:123/a b".into(),
            path: "/m/a.mp4".into(),
            start_s: 12.0,
            len_s: 44.0,
        };
        let dir = stem_dir(&s, &region).display().to_string();
        assert!(dir.starts_with("/cache/stems/plex_123_a_b-"), "{dir}");
        assert!(dir.ends_with("/12000-44000"), "{dir}");
    }

    #[test]
    fn ids_that_differ_only_in_punctuation_get_different_directories() {
        assert_ne!(sanitize("plex:123/a"), sanitize("plex_123_a"));
    }
}

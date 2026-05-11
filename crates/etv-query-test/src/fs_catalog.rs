use std::collections::HashMap;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::normalize::NormalizedItem;

#[derive(Debug, Error)]
pub enum FsError {
    #[error("io: {0}")]
    Io(String),
    #[error("ffprobe: {0}")]
    Ffprobe(String),
}

/// Returns configured FS roots from ETV_FS_ROOTS (colon-separated paths).
/// Falls back to the committed fixture directory when the env var is unset.
pub fn configured_roots() -> Vec<PathBuf> {
    if let Ok(val) = std::env::var("ETV_FS_ROOTS") {
        val.split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect()
    } else {
        vec![default_fixtures_dir()]
    }
}

pub fn default_fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// Scan all configured FS roots and return a path → NormalizedItem map.
/// The map key is the canonical absolute path string (used for dedup with Plex).
pub fn ingest_all_roots() -> Result<HashMap<String, NormalizedItem>, FsError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| FsError::Io(e.to_string()))?;
    let mut map = HashMap::new();
    for root in configured_roots() {
        for (k, v) in ingest_root_with_rt(&root, &rt)? {
            map.entry(k).or_insert(v);
        }
    }
    Ok(map)
}

fn ingest_root_with_rt(
    root: &Path,
    rt: &tokio::runtime::Runtime,
) -> Result<HashMap<String, NormalizedItem>, FsError> {
    let mut entries: Vec<PathBuf> = Vec::new();
    for ext in ["mp4", "mkv", "mov", "m4v", "webm"] {
        let pattern = format!("{}/**/*.{ext}", root.to_string_lossy());
        let matches = glob::glob(&pattern).map_err(|e| FsError::Io(e.to_string()))?;
        entries.extend(matches.filter_map(Result::ok));
    }
    entries.sort();
    entries.dedup();

    let durations = rt.block_on(probe_durations(&entries))?;
    let library = root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "fs".into());

    let mut map = HashMap::new();
    for (path, runtime_secs) in entries.iter().zip(durations) {
        let item = to_item(path, runtime_secs, &library);
        map.insert(item.path.clone(), item);
    }
    Ok(map)
}

/// Convenience: scan a single path, return a Vec (used by case files with explicit fs: source).
pub fn ingest(root: &Path) -> Result<Vec<NormalizedItem>, FsError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| FsError::Io(e.to_string()))?;
    Ok(ingest_root_with_rt(root, &rt)?.into_values().collect())
}

async fn probe_durations(paths: &[PathBuf]) -> Result<Vec<Option<f64>>, FsError> {
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let secs = tokio::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
                path.to_str().unwrap_or_default(),
            ])
            .output()
            .await
            .map_err(|e| FsError::Ffprobe(e.to_string()))?;
        if !secs.status.success() {
            out.push(None);
            continue;
        }
        let text = String::from_utf8_lossy(&secs.stdout);
        out.push(text.trim().parse::<f64>().ok());
    }
    Ok(out)
}

fn to_item(path: &Path, runtime_secs: Option<f64>, library: &str) -> NormalizedItem {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let media_type = type_from_path(path);
    NormalizedItem {
        sources: vec!["fs".into()],
        media_type,
        library: library.to_string(),
        title: stem,
        sub_title: None,
        season: None,
        episode: None,
        year: None,
        categories: vec![],
        collections: vec![],
        franchise: None,
        content_rating: None,
        runtime_secs,
        path: path.to_string_lossy().into_owned(),
        rating_key: None,
    }
}

/// Derive semantic type from the file's parent directory name.
/// /bumpers/foo.mp4       → "bumper"
/// /musicvideos/foo.mp4   → "music_video"
/// /concerts/foo.mp4      → "concert"
/// /power_hours/foo.mp4   → "power_hour"
/// /commercials/foo.mp4   → "commercial"
/// /idents/foo.mp4        → "ident"
/// /promos/foo.mp4        → "promo"
/// anything else          → "video"
pub fn type_from_path(path: &Path) -> String {
    let dir = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    match dir.as_str() {
        "bumpers" | "bumper" => "bumper",
        "musicvideos" | "music_videos" | "music-videos" => "music_video",
        "concerts" | "concert" => "concert",
        "power_hours" | "power-hours" | "powerhours" | "powerhouse" | "powerhouses" => "power_hour",
        "commercials" | "commercial" => "commercial",
        "idents" | "ident" => "ident",
        "promos" | "promo" => "promo",
        _ => "video",
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_from_bumpers_dir() {
        assert_eq!(
            type_from_path(Path::new("/media/bumpers/foo.mp4")),
            "bumper"
        );
    }

    #[test]
    fn type_from_musicvideos_dir() {
        assert_eq!(
            type_from_path(Path::new("/mnt/library/musicvideos/bar.mp4")),
            "music_video"
        );
    }

    #[test]
    fn type_from_concerts_dir() {
        assert_eq!(
            type_from_path(Path::new("/media/concerts/show.mkv")),
            "concert"
        );
    }

    #[test]
    fn type_from_unknown_dir() {
        assert_eq!(type_from_path(Path::new("/misc/clip.mp4")), "video");
    }

    #[test]
    fn to_item_fills_basic_fields() {
        let item = to_item(
            Path::new("/media/bumpers/station-bumper-01.mp4"),
            Some(12.5),
            "bumpers",
        );
        assert_eq!(item.title, "station-bumper-01");
        assert_eq!(item.runtime_secs, Some(12.5));
        assert_eq!(item.media_type, "bumper");
        assert_eq!(item.sources, vec!["fs".to_string()]);
    }
}

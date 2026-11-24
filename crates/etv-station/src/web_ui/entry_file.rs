//! `GET /api/entries/{id}/file` and `GET /api/entries/{id}/thumb`: what the item
//! inspector shows for a catalog entry Radarr and Sonarr have no record of. `file`
//! is the entry's media file as ffprobe reads it (the same map `ctx.probe` returns)
//! with its path and size; `thumb` is the entry's cached Plex artwork, else one
//! frame grabbed from 10% into the file.
//!
//! Core fact: an entry's media file (path, size, ffprobe format and streams) and an
//! image of it.

use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use serde_json::{Value, json};

use super::{AppState, Reply};
use crate::catalog::Entry;
use crate::tune_in_still::{MediaRemap, TuneInStill, Unavailable, media_path};

/// Where thumbnail frames are rendered and kept, apart from the review stills so a
/// Pools session never evicts a Review page's stills.
fn thumb_dir() -> PathBuf {
    std::env::temp_dir().join("etv-station-ui-thumbs")
}

/// The frame size a thumbnail is fitted into.
const THUMB_CANVAS: (u32, u32) = (640, 360);

/// One ffprobe at a time from the inspector, however fast rows are clicked: each
/// reads the media array the channels transcode from, for up to `PROBE_TIMEOUT`.
static PROBE: Mutex<()> = Mutex::new(());

/// `GET /api/entries/{id}/file`. A file that cannot be found answers 200 with
/// `unavailable` (and the path looked for); the probe itself runs off the
/// router's thread.
pub(super) fn file(
    entry: &Entry,
    sources: Result<Vec<crate::catalog::EntrySource>, String>,
) -> Reply {
    let id = entry.entry_id.clone();
    let media = match media_path(sources, &MediaRemap::from_env()) {
        Ok(media) => media,
        Err(Unavailable { why, path }) => {
            tracing::info!(event = "ui.entry.file", entry = %id, outcome = "unavailable", why = %why, path = ?path, "no file to probe");
            return Reply::Json(
                200,
                json!({ "entry_id": id, "unavailable": why, "path": path.map(|p| p.display().to_string()) }),
            );
        }
    };
    Reply::OffThread(Box::new(move || probe_reply(&id, &media)))
}

fn probe_reply(id: &str, media: &Path) -> (u16, Value) {
    let _one_at_a_time = PROBE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let began = Instant::now();
    let path = media.display().to_string();
    let size = std::fs::metadata(media).map(|m| m.len()).ok();
    let (outcome, body) = match crate::ffprobe::probe_media(media) {
        Ok(Some(probe)) => (
            "probed",
            json!({ "entry_id": id, "path": path, "size_bytes": size, "probe": probe }),
        ),
        Ok(None) => (
            "unreadable",
            json!({ "entry_id": id, "path": path, "size_bytes": size, "error": "ffprobe could not read the file" }),
        ),
        Err(e) => (
            "failed",
            json!({ "entry_id": id, "path": path, "size_bytes": size, "error": e }),
        ),
    };
    tracing::info!(
        event = "ui.entry.file",
        entry = id,
        path = %path,
        outcome,
        ms = began.elapsed().as_millis() as u64,
        "probed an entry's file"
    );
    (200, body)
}

/// `GET /api/entries/{id}/thumb`: the cached Plex artwork when the catalog has it
/// under `artwork_dir`, else a frame from 10% into the file (the start when the
/// catalog knows no runtime), rendered and cached like a review still.
pub(super) fn thumb(
    state: &AppState,
    entry: &Entry,
    sources: Result<Vec<crate::catalog::EntrySource>, String>,
) -> Reply {
    if let Some(path) = artwork_file(
        state.artwork_dir.as_deref(),
        entry.artwork_cache_path.as_deref(),
    ) {
        tracing::info!(event = "ui.entry.thumb", entry = %entry.entry_id, source = "artwork", path = %path.display(), "thumbnail");
        let content_type = image_type(&path);
        return Reply::RenderFile(path, content_type);
    }
    let media = match media_path(sources, &MediaRemap::from_env()) {
        Ok(media) => media,
        Err(Unavailable { why, path }) => {
            tracing::info!(event = "ui.entry.thumb", entry = %entry.entry_id, source = "none", why = %why, path = ?path, "no thumbnail");
            return Reply::Json(
                404,
                json!({ "error": why, "path": path.map(|p| p.display().to_string()) }),
            );
        }
    };
    let at_secs = entry
        .duration_ms
        .filter(|&ms| ms > 0)
        .map_or(0.0, |ms| ms as f64 / 1000.0 * 0.1);
    let still = TuneInStill {
        media,
        at_secs,
        canvas: THUMB_CANVAS,
        overlay: None,
        program: etv_overlay::ProgramContext::unknown(),
        fonts: Vec::new(),
    };
    let name: String = entry
        .entry_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let id = format!("{name}-{}.jpg", still.digest());
    tracing::info!(event = "ui.entry.thumb", entry = %entry.entry_id, source = "frame", path = %still.media.display(), at_secs, "thumbnail");
    Reply::Still(thumb_dir(), id, Box::new(still))
}

/// The cached artwork file for `name` under `dir`, when it exists. `name` comes
/// from the catalog and is joined onto a directory the server reads, so only a
/// single plain file name is accepted.
fn artwork_file(dir: Option<&Path>, name: Option<&str>) -> Option<PathBuf> {
    let (dir, name) = (dir?, name?);
    let mut parts = Path::new(name).components();
    if !matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(_)), None)
    ) {
        return None;
    }
    Some(dir.join(name)).filter(|p| p.is_file())
}

fn image_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        _ => "image/jpeg",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artwork_is_served_only_as_a_plain_file_name_that_exists() {
        let dir = std::env::temp_dir().join(format!("etv-artwork-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("abc.jpg"), b"jpeg").unwrap();
        assert_eq!(
            artwork_file(Some(&dir), Some("abc.jpg")),
            Some(dir.join("abc.jpg"))
        );
        assert_eq!(artwork_file(Some(&dir), Some("missing.jpg")), None);
        assert_eq!(artwork_file(Some(&dir), Some("../abc.jpg")), None);
        assert_eq!(artwork_file(Some(&dir), Some("/etc/passwd")), None);
        assert_eq!(artwork_file(Some(&dir), Some("sub/abc.jpg")), None);
        assert_eq!(artwork_file(None, Some("abc.jpg")), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

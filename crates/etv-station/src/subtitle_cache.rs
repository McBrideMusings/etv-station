//! Subtitle tracks extracted ahead of air.
//!
//! ETV-next turns a text subtitle into WebVTT by reading its source end to end
//! before the item's first segment. For a subtitle stream inside the media file
//! that is the whole film — 109 s for a 3.1 GB mkv — which is longer than Plex
//! waits for a tune to start. So [`crate::tracks::assign`] points an item's
//! embedded text subtitle at a file in [`dir`] instead, [`run`] extracts that
//! file shortly before the item airs, and deletes it once no unfinished item
//! names it. Reading a small WebVTT file costs ETV-next nothing at tune time.
//!
//! An item whose file is not there yet plays without subtitles: ETV-next skips
//! a subtitle source that is not on disk rather than fail the item.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ersatztv_playout::playout::{Playout, PlayoutItemSource};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tokio::sync::Notify;

use crate::reconcile::{ChunkScan, scan_chunks};

/// How often [`run`] looks at the schedule.
const TICK: Duration = Duration::from_secs(30);

/// How far ahead of an item's start its subtitle is extracted. Extraction is a
/// full read of the media file on the same disks live channels play from, so
/// only what airs soon is read, one file at a time.
const LEAD: time::Duration = time::Duration::hours(3);

/// How long a file whose extraction failed waits before it is tried again.
const RETRY_AFTER: Duration = Duration::from_secs(30 * 60);

/// The longest one extraction may run before it is killed.
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// The folder extracted subtitle files live in, beside the channel folders.
pub fn dir(output_base: &Path) -> PathBuf {
    output_base.join(".subtitles")
}

/// The file stream `index` of `media` is extracted to. The index rides in the
/// name, so the work list can be read back from the playout files alone and
/// survives a restart.
pub fn path_for(dir: &Path, media: &str, index: u32) -> PathBuf {
    let digest = Sha256::digest(media.as_bytes());
    let key: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{key}-s{index}.vtt"))
}

/// The stream index a [`path_for`] file name carries.
fn index_of(file_name: &str) -> Option<u32> {
    file_name
        .strip_suffix(".vtt")?
        .rsplit_once("-s")?
        .1
        .parse()
        .ok()
}

/// One item that names an extracted subtitle file.
#[derive(Debug, Clone, PartialEq)]
struct Wanted {
    file: PathBuf,
    media: String,
    index: u32,
    start: OffsetDateTime,
    finish: OffsetDateTime,
}

/// The items in `playout` whose subtitle source is a file in `dir`.
fn wanted_in(dir: &Path, playout: &Playout) -> Vec<Wanted> {
    playout
        .items
        .iter()
        .filter_map(|item| {
            let source = item.tracks.as_ref()?.subtitle.as_ref()?.source.as_ref()?;
            let PlayoutItemSource::Local { path, .. } = source else {
                return None;
            };
            let file = PathBuf::from(path);
            if file.parent()? != dir {
                return None;
            }
            let index = index_of(file.file_name()?.to_str()?)?;
            let Some(PlayoutItemSource::Local { path: media, .. }) = item.source.as_ref() else {
                return None;
            };
            Some(Wanted {
                file,
                media: media.clone(),
                index,
                start: item.start,
                finish: item.finish,
            })
        })
        .collect()
}

/// What one tick does: the files in the folder (`present`, by name) to delete,
/// and the files to extract, earliest airing first.
///
/// A file is kept while an item that has not finished names it. A `.part` file
/// is always deleted: extraction runs one file at a time inside the tick, so
/// one left over at the start of a tick is from an extraction that died.
fn plan(
    dir: &Path,
    wanted: &[Wanted],
    present: &[String],
    now: OffsetDateTime,
) -> (Vec<PathBuf>, Vec<Wanted>) {
    let live: Vec<&Wanted> = wanted.iter().filter(|w| w.finish > now).collect();
    let keep: HashSet<&Path> = live.iter().map(|w| w.file.as_path()).collect();
    let delete = present
        .iter()
        .map(|name| dir.join(name))
        .filter(|path| !keep.contains(path.as_path()))
        .collect();

    let mut earliest: HashMap<&Path, &Wanted> = HashMap::new();
    for w in live {
        if w.start >= now + LEAD || present.iter().any(|name| dir.join(name) == w.file) {
            continue;
        }
        earliest
            .entry(w.file.as_path())
            .and_modify(|e| {
                if w.start < e.start {
                    *e = w;
                }
            })
            .or_insert(w);
    }
    let mut extract: Vec<Wanted> = earliest.into_values().cloned().collect();
    extract.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.file.cmp(&b.file)));
    (delete, extract)
}

/// Keep [`dir`] holding the subtitle file of every item that airs within
/// [`LEAD`] or is airing now, until `stop` is notified.
pub async fn run(output_base: PathBuf, stop: Arc<Notify>) {
    let dir = dir(&output_base);
    let mut scan = ChunkScan::default();
    let mut failed: HashMap<PathBuf, Instant> = HashMap::new();
    loop {
        tokio::select! {
            _ = stop.notified() => return,
            _ = tokio::time::sleep(TICK) => {}
        }
        if !tick(&output_base, &dir, &mut scan, &mut failed, &stop).await {
            return;
        }
    }
}

/// One pass. `false` when `stop` arrived mid-extraction.
async fn tick(
    output_base: &Path,
    dir: &Path,
    scan: &mut ChunkScan<Wanted>,
    failed: &mut HashMap<PathBuf, Instant>,
    stop: &Notify,
) -> bool {
    let now = OffsetDateTime::now_utc();
    let wanted = match scan_chunks(output_base, now, now + time::Duration::days(3650), scan, |p| {
        wanted_in(dir, p)
    }) {
        Ok(wanted) => wanted,
        Err(error) => {
            // A partial schedule would read as "nothing names these files";
            // delete and extract nothing until it reads whole.
            tracing::warn!(event = "subtitle_cache.scan_failed", %error, "could not read the schedule");
            return true;
        }
    };
    let present: Vec<String> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            tracing::warn!(event = "subtitle_cache.scan_failed", dir = %dir.display(), %error, "could not list the subtitle folder");
            return true;
        }
    };

    let (delete, extract) = plan(dir, &wanted, &present, now);
    let mut pruned = 0usize;
    for path in &delete {
        match std::fs::remove_file(path) {
            Ok(()) => pruned += 1,
            Err(error) => tracing::warn!(event = "subtitle_cache.prune_failed", file = %path.display(), %error, "could not delete a subtitle file"),
        }
    }
    if pruned > 0 {
        tracing::info!(event = "subtitle_cache.pruned", count = pruned, "deleted subtitle files no unfinished item names");
    }
    failed.retain(|_, at| at.elapsed() < RETRY_AFTER);

    for w in extract {
        if failed.contains_key(&w.file) || w.finish <= OffsetDateTime::now_utc() {
            continue;
        }
        let began = Instant::now();
        let result = tokio::select! {
            _ = stop.notified() => return false,
            r = extract_one(dir, &w) => r,
        };
        match result {
            Ok(bytes) => tracing::info!(
                event = "subtitle_cache.extracted",
                media = %w.media,
                index = w.index,
                file = %w.file.display(),
                airs_at = %w.start,
                secs = began.elapsed().as_secs_f64(),
                bytes,
                "extracted a subtitle track ahead of air",
            ),
            Err(error) => {
                tracing::warn!(
                    event = "subtitle_cache.failed",
                    media = %w.media,
                    index = w.index,
                    airs_at = %w.start,
                    secs = began.elapsed().as_secs_f64(),
                    %error,
                    "could not extract a subtitle track; the item plays without subtitles",
                );
                failed.insert(w.file.clone(), Instant::now());
            }
        }
    }
    true
}

/// Extract `w`'s stream into its file, written whole or not at all. The bytes
/// written on success.
async fn extract_one(dir: &Path, w: &Wanted) -> Result<u64, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let name = w.file.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let part = dir.join(format!("{name}.part"));
    let child = tokio::process::Command::new("ffmpeg")
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(&w.media)
        .args(["-map", &format!("0:{}", w.index), "-c:s", "webvtt", "-f", "webvtt", "-y"])
        .arg(&part)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    let outcome = match tokio::time::timeout(EXTRACT_TIMEOUT, child).await {
        Err(_) => Err(format!("ffmpeg ran past {}s", EXTRACT_TIMEOUT.as_secs())),
        Ok(Err(e)) => Err(format!("cannot run ffmpeg: {e}")),
        Ok(Ok(out)) if !out.status.success() => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let tail: String = stderr.lines().rev().take(3).collect::<Vec<_>>().join(" | ");
            Err(format!("ffmpeg exited {}: {tail}", out.status))
        }
        Ok(Ok(_)) => std::fs::rename(&part, &w.file)
            .map_err(|e| format!("cannot move {} into place: {e}", part.display()))
            .and_then(|()| {
                std::fs::metadata(&w.file)
                    .map(|m| m.len())
                    .map_err(|e| format!("cannot read {}: {e}", w.file.display()))
            }),
    };
    if outcome.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn wanted(dir: &Path, media: &str, index: u32, start: OffsetDateTime, mins: i64) -> Wanted {
        Wanted {
            file: path_for(dir, media, index),
            media: media.into(),
            index,
            start,
            finish: start + time::Duration::minutes(mins),
        }
    }

    fn name(path: &Path) -> String {
        path.file_name().unwrap().to_str().unwrap().into()
    }

    #[test]
    fn the_stream_index_reads_back_from_the_file_name() {
        let file = path_for(Path::new("/c"), "/media/movies/A (2025)/A.mkv", 3);
        assert_eq!(file.parent(), Some(Path::new("/c")));
        assert_eq!(index_of(&name(&file)), Some(3));
        assert_eq!(index_of(&format!("{}.part", name(&file))), None);
        assert_eq!(index_of("notes.txt"), None);
        assert_ne!(file, path_for(Path::new("/c"), "/media/movies/B.mkv", 3));
        assert_ne!(file, path_for(Path::new("/c"), "/media/movies/A (2025)/A.mkv", 4));
    }

    #[test]
    fn only_items_whose_subtitle_is_in_the_folder_are_wanted() {
        let dir = Path::new("/data/playout/.subtitles");
        let cached = path_for(dir, "/m/a.mkv", 3);
        let json = serde_json::json!({
            "version": "https://ersatztv.org/playout/version/0.0.1",
            "items": [
                {"id": "a", "start": "2026-11-23T15:00:00Z", "finish": "2026-11-23T17:00:00Z",
                 "source": {"source_type": "local", "path": "/m/a.mkv"},
                 "tracks": {"subtitle": {"source": {"source_type": "local", "path": cached}, "stream_index": 0}}},
                {"id": "b", "start": "2026-11-23T17:00:00Z", "finish": "2026-11-23T18:00:00Z",
                 "source": {"source_type": "local", "path": "/m/b.mkv"},
                 "tracks": {"subtitle": {"source": {"source_type": "local", "path": "/m/b.en.srt"}, "stream_index": 0}}},
                {"id": "c", "start": "2026-11-23T18:00:00Z", "finish": "2026-11-23T19:00:00Z",
                 "source": {"source_type": "local", "path": "/m/c.mkv"},
                 "tracks": {"subtitle": {"stream_index": 2}}}
            ]
        });
        let playout: Playout = serde_json::from_value(json).unwrap();
        let found = wanted_in(dir, &playout);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].file, cached);
        assert_eq!(found[0].media, "/m/a.mkv");
        assert_eq!(found[0].index, 3);
    }

    #[test]
    fn a_tick_keeps_what_unfinished_items_name_and_extracts_what_airs_soon() {
        let dir = Path::new("/c");
        let now = datetime!(2026-11-23 15:00 UTC);
        let airing = wanted(dir, "/m/airing.mkv", 2, now - time::Duration::minutes(30), 120);
        let soon = wanted(dir, "/m/soon.mkv", 3, now + time::Duration::hours(1), 90);
        let later = wanted(dir, "/m/later.mkv", 3, now + time::Duration::hours(5), 90);
        let aired = wanted(dir, "/m/aired.mkv", 3, now - time::Duration::hours(3), 60);
        // The same film again tonight shares the file soon's airing extracts.
        let soon_again = wanted(dir, "/m/soon.mkv", 3, now + time::Duration::hours(2), 90);
        let all = [later.clone(), soon_again, soon.clone(), aired.clone(), airing.clone()];
        let present = vec![
            name(&airing.file),
            name(&aired.file),
            "stale-s1.vtt".to_string(),
            format!("{}.part", name(&soon.file)),
        ];

        let (delete, extract) = plan(dir, &all, &present, now);

        let mut delete: Vec<String> = delete.iter().map(|p| name(p)).collect();
        delete.sort();
        let mut expected = vec![
            name(&aired.file),
            "stale-s1.vtt".to_string(),
            format!("{}.part", name(&soon.file)),
        ];
        expected.sort();
        assert_eq!(delete, expected);
        assert_eq!(extract, vec![soon]);
    }

    #[test]
    fn a_missing_file_for_the_item_on_air_is_extracted_first() {
        let dir = Path::new("/c");
        let now = datetime!(2026-11-23 15:00 UTC);
        let airing = wanted(dir, "/m/airing.mkv", 2, now - time::Duration::minutes(30), 120);
        let soon = wanted(dir, "/m/soon.mkv", 3, now + time::Duration::hours(1), 90);
        let (delete, extract) = plan(dir, &[soon.clone(), airing.clone()], &[], now);
        assert!(delete.is_empty());
        assert_eq!(extract, vec![airing, soon]);
    }
}

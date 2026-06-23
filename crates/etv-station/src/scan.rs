use std::path::{Path, PathBuf};

use ersatztv_playout::playout::DATE_FORMAT;
use time::OffsetDateTime;

use crate::errors::StationError;

#[derive(Debug, Clone)]
pub struct DiscoveredFile {
    pub path: PathBuf,
    pub start: OffsetDateTime,
    pub finish: OffsetDateTime,
}

pub async fn scan_output_folder(folder: &Path) -> Result<Vec<DiscoveredFile>, StationError> {
    let io_err = |source: std::io::Error| StationError::Io {
        path: folder.to_path_buf(),
        source,
    };

    let mut out = Vec::new();
    let mut entries = match tokio::fs::read_dir(folder).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(source) => return Err(io_err(source)),
    };

    while let Some(entry) = entries.next_entry().await.map_err(io_err)? {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.ends_with(".json") {
            continue;
        }
        let stem = &name[..name.len() - ".json".len()];
        let Some((start_s, finish_s)) = split_start_finish(stem) else {
            continue;
        };
        let Ok(start) = OffsetDateTime::parse(start_s, &DATE_FORMAT) else {
            continue;
        };
        let Ok(finish) = OffsetDateTime::parse(finish_s, &DATE_FORMAT) else {
            continue;
        };
        out.push(DiscoveredFile {
            path,
            start,
            finish,
        });
    }
    out.sort_by_key(|f| f.start);
    Ok(out)
}

pub fn highest_finish(files: &[DiscoveredFile]) -> Option<OffsetDateTime> {
    files.iter().map(|f| f.finish).max()
}

/// Delete emitted playout files whose window has fully elapsed past the
/// retention horizon (`finish < now - retention_days`) and return how many were
/// removed.
///
/// Cheap and idempotent: it parses the `{start}_{finish}.json` filename rather
/// than the file contents, so re-running with the same inputs removes nothing.
/// `scan_output_folder` only matches the window naming, so the `.anchor`
/// sidecar and `.durations.json` cache are never candidates — station state is
/// never swept. Housekeeping-grade: a scan failure or an individual delete
/// failure is logged and skipped, never propagated, so a single un-removable
/// file can't abort the sweep or tear down the daemon.
pub async fn sweep_retention(folder: &Path, retention_days: u32, now: OffsetDateTime) -> usize {
    let cutoff = now - time::Duration::days(i64::from(retention_days));
    let files = match scan_output_folder(folder).await {
        Ok(files) => files,
        Err(e) => {
            tracing::warn!(
                event = "retention.scan_failed",
                folder = %folder.display(),
                error = %e,
                "retention sweep: scan failed; skipping",
            );
            return 0;
        }
    };

    let mut removed = 0;
    for f in files.iter().filter(|f| f.finish < cutoff) {
        match tokio::fs::remove_file(&f.path).await {
            Ok(()) => {
                tracing::info!(
                    event = "retention.delete",
                    file = %f.path.display(),
                    finish = %f.finish,
                    "pruned playout file past retention horizon",
                );
                removed += 1;
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                tracing::warn!(
                    event = "retention.delete_failed",
                    file = %f.path.display(),
                    error = %source,
                    "retention sweep: failed to remove file",
                );
            }
        }
    }
    removed
}

fn split_start_finish(stem: &str) -> Option<(&str, &str)> {
    // Filename is `{start}_{finish}` where each side may contain `-` or `+` from
    // the offset and `T`. Underscore appears once between them — our DATE_FORMAT
    // does not produce `_` itself.
    let idx = stem.find('_')?;
    Some((&stem[..idx], &stem[idx + 1..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use time::macros::datetime;

    async fn write_blank(path: &Path) {
        tokio::fs::write(path, b"{}").await.unwrap();
    }

    #[test]
    fn highest_finish_returns_none_for_empty() {
        assert!(highest_finish(&[]).is_none());
    }

    #[tokio::test]
    async fn ignores_non_json_and_garbage() {
        let dir = tempdir().unwrap();
        write_blank(&dir.path().join("garbage.txt")).await;
        write_blank(&dir.path().join("not_a_window.json")).await;
        let files = scan_output_folder(dir.path()).await.unwrap();
        assert!(files.is_empty());
    }

    fn window_name(start: OffsetDateTime, finish: OffsetDateTime) -> String {
        let s = start.format(&DATE_FORMAT).unwrap();
        let f = finish.format(&DATE_FORMAT).unwrap();
        format!("{s}_{f}.json")
    }

    #[tokio::test]
    async fn sweep_removes_only_past_retention_files() {
        let dir = tempdir().unwrap();
        let now = datetime!(2026-04-20 00:00 UTC);

        // Old: finished 10 days ago — outside a 7-day horizon.
        let old = window_name(
            datetime!(2026-04-09 00:00 UTC),
            datetime!(2026-04-10 00:00 UTC),
        );
        // Recent: finished 2 days ago — inside the horizon, kept.
        let recent = window_name(
            datetime!(2026-04-17 00:00 UTC),
            datetime!(2026-04-18 00:00 UTC),
        );
        // Future: not yet finished, kept.
        let future = window_name(
            datetime!(2026-04-20 00:00 UTC),
            datetime!(2026-04-21 00:00 UTC),
        );
        // State sidecars: never parsed as windows, so never swept.
        write_blank(&dir.path().join(&old)).await;
        write_blank(&dir.path().join(&recent)).await;
        write_blank(&dir.path().join(&future)).await;
        write_blank(&dir.path().join(".anchor")).await;
        write_blank(&dir.path().join(".durations.json")).await;

        let removed = sweep_retention(dir.path(), 7, now).await;
        assert_eq!(removed, 1);
        assert!(!dir.path().join(&old).exists(), "old file should be pruned");
        assert!(dir.path().join(&recent).exists(), "recent file kept");
        assert!(dir.path().join(&future).exists(), "future file kept");
        assert!(dir.path().join(".anchor").exists(), "anchor sidecar kept");
        assert!(
            dir.path().join(".durations.json").exists(),
            "duration cache kept",
        );

        // Idempotent: a second sweep removes nothing.
        assert_eq!(sweep_retention(dir.path(), 7, now).await, 0);
    }

    #[tokio::test]
    async fn parses_well_formed_filenames() {
        let dir = tempdir().unwrap();
        let start = datetime!(2026-04-13 00:00 UTC);
        let finish = datetime!(2026-04-14 00:00 UTC);
        let s = start.format(&DATE_FORMAT).unwrap();
        let f = finish.format(&DATE_FORMAT).unwrap();
        let name = format!("{s}_{f}.json");
        write_blank(&dir.path().join(&name)).await;
        let files = scan_output_folder(dir.path()).await.unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].start, start);
        assert_eq!(files[0].finish, finish);
        assert_eq!(highest_finish(&files), Some(finish));
    }
}

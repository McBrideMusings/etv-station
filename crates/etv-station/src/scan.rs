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

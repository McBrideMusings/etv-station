use std::path::{Path, PathBuf};

use ersatztv_playout::playout::{DATE_FORMAT, Playout};
use time::OffsetDateTime;
use time_tz::Tz;

use crate::atomic::atomic_write_json;
use crate::errors::StationError;
use crate::rule::Rule;
use crate::tz as tzmod;

pub async fn emit_window(
    output_folder: &Path,
    rule: &impl Rule,
    anchor_utc: OffsetDateTime,
    tz: &'static Tz,
    chunk_hours: u32,
    from: OffsetDateTime,
    to: OffsetDateTime,
) -> Result<Vec<PathBuf>, StationError> {
    tokio::fs::create_dir_all(output_folder)
        .await
        .map_err(|source| StationError::Io {
            path: output_folder.to_path_buf(),
            source,
        })?;

    let mut written = Vec::new();
    let mut chunk_start = from;
    while chunk_start < to {
        let chunk_finish = tzmod::add_chunk(chunk_start, chunk_hours, tz);
        let items = rule.items_covering(anchor_utc, chunk_start, chunk_finish);
        let playout = Playout::new(items);

        let name = chunk_filename(chunk_start, chunk_finish)?;
        let path = output_folder.join(&name);
        atomic_write_json(&path, &playout).await?;
        written.push(path);

        chunk_start = chunk_finish;
    }
    Ok(written)
}

fn chunk_filename(start: OffsetDateTime, finish: OffsetDateTime) -> Result<String, StationError> {
    let s = format_for_filename(start)?;
    let f = format_for_filename(finish)?;
    Ok(format!("{s}_{f}.json"))
}

fn format_for_filename(dt: OffsetDateTime) -> Result<String, StationError> {
    dt.format(&DATE_FORMAT)
        .map_err(|e| StationError::BadFilename {
            name: format!("{dt}"),
            reason: format!("format: {e}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SourceConfig;
    use crate::resolve::ResolvedItem;
    use crate::rule::LoopForever;
    use std::time::Duration;
    use tempfile::tempdir;
    use time::macros::datetime;

    fn item(id: &str, secs: u64) -> ResolvedItem {
        ResolvedItem {
            id: id.into(),
            source: SourceConfig::Lavfi {
                params: format!("src={id}"),
            },
            in_point: Some(Duration::ZERO),
            out_point: Some(Duration::from_secs(secs)),
            program: None,
        }
    }

    #[tokio::test]
    async fn emits_24h_chunks_from_loop() {
        let dir = tempdir().unwrap();
        let items = vec![item("a", 60), item("b", 60)];
        let durs = vec![Duration::from_secs(60), Duration::from_secs(60)];
        let rule = LoopForever::new(&items, &durs);
        let tz = tzmod::parse("UTC").unwrap();
        let anchor = datetime!(2026-04-13 00:00 UTC);
        let from = anchor;
        let to = datetime!(2026-04-15 00:00 UTC); // 2 days

        let files = emit_window(dir.path(), &rule, anchor, tz, 24, from, to)
            .await
            .unwrap();
        assert_eq!(files.len(), 2);

        // First file's contents
        let bytes = tokio::fs::read(&files[0]).await.unwrap();
        let playout: Playout = serde_json::from_slice(&bytes).unwrap();
        assert!(!playout.items.is_empty());
        assert_eq!(playout.items[0].id, "a");
    }

    #[tokio::test]
    async fn second_emission_byte_identical() {
        let dir1 = tempdir().unwrap();
        let dir2 = tempdir().unwrap();
        let items = vec![item("a", 30), item("b", 90)];
        let durs = vec![Duration::from_secs(30), Duration::from_secs(90)];
        let rule = LoopForever::new(&items, &durs);
        let tz = tzmod::parse("America/Chicago").unwrap();
        let anchor = datetime!(2026-04-13 05:00 UTC); // local midnight CDT
        let from = anchor;
        let to = datetime!(2026-04-14 05:00 UTC);

        let f1 = emit_window(dir1.path(), &rule, anchor, tz, 24, from, to)
            .await
            .unwrap();
        let f2 = emit_window(dir2.path(), &rule, anchor, tz, 24, from, to)
            .await
            .unwrap();
        assert_eq!(f1.len(), f2.len());
        for (a, b) in f1.iter().zip(f2.iter()) {
            let ba = tokio::fs::read(a).await.unwrap();
            let bb = tokio::fs::read(b).await.unwrap();
            assert_eq!(ba, bb, "files differ between identical emissions");
        }
    }
}

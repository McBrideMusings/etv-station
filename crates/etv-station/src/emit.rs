use std::path::{Path, PathBuf};

use ersatztv_playout::playout::{DATE_FORMAT, Playout, PlayoutItem};
use time::OffsetDateTime;
use time_tz::Tz;

use crate::atomic::atomic_write_json;
use crate::errors::StationError;
use crate::rule::Rule;
use crate::scan;
use crate::tz as tzmod;

/// Write a generation's items into chunk-aligned playout files.
///
/// The storage unit is the **chunk** — a fixed `chunk_hours` slice on the
/// local-time grid — not the generation. A generation (one playlist pass) is
/// usually far shorter than a chunk, so its items are *merged into* the file for
/// the chunk they fall in: the existing chunk file is read, this generation's
/// items are appended, and the file is rewritten. A chunk therefore grows into
/// one file across many generations and ticks, rather than spawning a file per
/// pass (see ADR 0003).
///
/// Naming keeps ErsatzTV-next's two-stage lookup honest — next picks a file
/// whose *name* span contains `now`, then an *item* whose span contains `now`,
/// and both must hold. A **full** chunk (content reached its boundary) is named
/// `[boundary, boundary]` so adjacent chunks tile exactly and a boundary-
/// straddling item, emitted whole into both neighbours, is found from either
/// side. A **still-filling** chunk is named `[boundary, last-item-finish]` so it
/// never claims the empty tail — the over-claim that made channels play black.
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
    let mut cursor = from;
    while cursor < to {
        // The chunk that contains `cursor`, and its end boundary.
        let chunk_start = tzmod::chunk_boundary_at_or_before(cursor, chunk_hours, tz);
        let chunk_end = tzmod::add_chunk(chunk_start, chunk_hours, tz);

        // This generation's items from `cursor` up to the chunk boundary. Slicing
        // to `chunk_end` (not `to`) lets an item straddling the boundary be
        // emitted whole here; it is re-emitted into the next chunk on the
        // following iteration, so either neighbour can play across the seam.
        let new_items = rule.items_covering(anchor_utc, cursor, chunk_end);

        // Grow the chunk in place: fold this generation's items into whatever the
        // chunk already holds from earlier generations or ticks. `cursor` equals
        // the previous content end, so the existing items (ending at `cursor`)
        // and the new items (starting at `cursor`) meet without overlap.
        let mut items = read_chunk_items(output_folder, chunk_start).await?;
        items.extend(new_items);

        if items.is_empty() {
            // The rule produced nothing for this chunk — the list is exhausted,
            // and being ordered, nothing later can qualify. No file to write.
            break;
        }

        let content_end = items.last().map(|i| i.finish).unwrap_or(chunk_start);
        let name_finish = if content_end >= chunk_end {
            chunk_end
        } else {
            content_end
        };

        let path = output_folder.join(chunk_filename(chunk_start, name_finish)?);
        atomic_write_json(&path, &Playout::new(items)).await?;
        // A grown chunk is renamed (its finish advanced), so drop the prior
        // shorter-named file for this same chunk. Housekeeping-grade: a failed
        // removal leaves a stale duplicate, which the next grow will clear.
        remove_other_chunk_files(output_folder, chunk_start, &path).await;
        written.push(path);

        cursor = chunk_end;
    }
    Ok(written)
}

/// Items already stored for the chunk beginning at `chunk_start`, or empty when
/// the chunk has no file yet. Matches on the filename's start instant, so a
/// chunk is one file regardless of how its finish has grown.
async fn read_chunk_items(
    folder: &Path,
    chunk_start: OffsetDateTime,
) -> Result<Vec<PlayoutItem>, StationError> {
    let files = scan::scan_output_folder(folder).await?;
    let Some(file) = files.into_iter().find(|f| f.start == chunk_start) else {
        return Ok(Vec::new());
    };
    let bytes = tokio::fs::read(&file.path)
        .await
        .map_err(|source| StationError::Io {
            path: file.path.clone(),
            source,
        })?;
    let playout: Playout =
        serde_json::from_slice(&bytes).map_err(|source| StationError::PlayoutCorrupt {
            path: file.path.clone(),
            source,
        })?;
    Ok(playout.items)
}

/// Remove any other file for the chunk beginning at `chunk_start` — the
/// predecessor left behind when a growing chunk's finish (and thus its name)
/// advances. Errors are swallowed: a leftover duplicate is cleared on the next
/// grow, and a removal failure must not abort a generation.
async fn remove_other_chunk_files(folder: &Path, chunk_start: OffsetDateTime, keep: &Path) {
    let Ok(files) = scan::scan_output_folder(folder).await else {
        return;
    };
    for f in files {
        if f.start == chunk_start && f.path != keep {
            let _ = tokio::fs::remove_file(&f.path).await;
        }
    }
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
    use crate::rule::Sequential;
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

    async fn read_items(path: &Path) -> Vec<PlayoutItem> {
        let bytes = tokio::fs::read(path).await.unwrap();
        serde_json::from_slice::<Playout>(&bytes).unwrap().items
    }

    /// Parse a chunk file's `[start, finish]` back out of its name.
    fn name_span(path: &Path) -> (OffsetDateTime, OffsetDateTime) {
        let stem = path.file_stem().unwrap().to_str().unwrap();
        let (s, f) = stem.split_once('_').unwrap();
        (
            OffsetDateTime::parse(s, &DATE_FORMAT).unwrap(),
            OffsetDateTime::parse(f, &DATE_FORMAT).unwrap(),
        )
    }

    // A generation shorter than one chunk writes ONE file for that chunk, named
    // for its true content end — never stretched to the far boundary.
    #[tokio::test]
    async fn short_generation_names_the_file_to_its_content_end() {
        let dir = tempdir().unwrap();
        let items = vec![item("a", 60), item("b", 60)]; // 2 min total
        let durs = vec![Duration::from_secs(60), Duration::from_secs(60)];
        let rule = Sequential::new(&items, &durs);
        let tz = tzmod::parse("UTC").unwrap();
        let from = datetime!(2026-04-13 00:00 UTC);
        let to = from + rule.total_duration(); // exactly the content, 2 min

        let files = emit_window(dir.path(), &rule, from, tz, 6, from, to)
            .await
            .unwrap();
        assert_eq!(files.len(), 1, "one chunk, one file");
        let (ns, nf) = name_span(&files[0]);
        assert_eq!(ns, from);
        assert_eq!(nf, datetime!(2026-04-13 00:02 UTC), "named to content end");
        assert_eq!(read_items(&files[0]).await.len(), 2);
    }

    // Chained short generations (the real loop) accumulate into ONE growing
    // chunk file, not one file per pass. This is the 120-files-per-chunk fix.
    #[tokio::test]
    async fn successive_generations_grow_one_chunk_file() {
        let dir = tempdir().unwrap();
        let items = vec![item("a", 60), item("b", 60)];
        let durs = vec![Duration::from_secs(60), Duration::from_secs(60)];
        let tz = tzmod::parse("UTC").unwrap();

        // Three 2-minute passes laid end to end, each its own emit_window call.
        let mut from = datetime!(2026-04-13 00:00 UTC);
        for _ in 0..3 {
            let rule = Sequential::new(&items, &durs);
            let to = from + rule.total_duration();
            emit_window(dir.path(), &rule, from, tz, 6, from, to)
                .await
                .unwrap();
            from = to;
        }

        let files = scan::scan_output_folder(dir.path()).await.unwrap();
        assert_eq!(files.len(), 1, "three passes, still one chunk file");
        assert_eq!(read_items(&files[0].path).await.len(), 6, "all six items");
        let (_, nf) = name_span(&files[0].path);
        assert_eq!(nf, datetime!(2026-04-13 00:06 UTC), "name grew with content");
    }

    // A generation spanning a chunk boundary seals the completed chunk to its
    // boundary (so chunks tile) and starts the next chunk's file.
    #[tokio::test]
    async fn crossing_a_boundary_seals_the_full_chunk_and_opens_the_next() {
        let dir = tempdir().unwrap();
        // One 3-hour item, then a 1-hour item: with 2h chunks the first straddles
        // the 02:00 boundary.
        let items = vec![item("long", 3 * 3600), item("tail", 3600)];
        let durs = vec![Duration::from_secs(3 * 3600), Duration::from_secs(3600)];
        let rule = Sequential::new(&items, &durs);
        let tz = tzmod::parse("UTC").unwrap();
        let from = datetime!(2026-04-13 00:00 UTC);
        let to = from + rule.total_duration(); // 04:00

        emit_window(dir.path(), &rule, from, tz, 2, from, to)
            .await
            .unwrap();
        let files = scan::scan_output_folder(dir.path()).await.unwrap();
        // Chunks [00:00,02:00), [02:00,04:00) — two files.
        assert_eq!(files.len(), 2);
        // First chunk sealed to its boundary (full), not to the straddling
        // item's real finish at 03:00.
        assert_eq!(files[0].start, datetime!(2026-04-13 00:00 UTC));
        assert_eq!(files[0].finish, datetime!(2026-04-13 02:00 UTC));
        // Second chunk holds the remainder, named to its content end (04:00).
        assert_eq!(files[1].start, datetime!(2026-04-13 02:00 UTC));
        assert_eq!(files[1].finish, datetime!(2026-04-13 04:00 UTC));
        // The straddling item appears in BOTH chunks so either side can play it.
        assert!(read_items(&files[0].path).await.iter().any(|i| i.id == "long"));
        assert!(read_items(&files[1].path).await.iter().any(|i| i.id == "long"));
    }

    // Two independent emissions of the same inputs are byte-identical.
    #[tokio::test]
    async fn second_emission_byte_identical() {
        let dir1 = tempdir().unwrap();
        let dir2 = tempdir().unwrap();
        let items = vec![item("a", 30), item("b", 90)];
        let durs = vec![Duration::from_secs(30), Duration::from_secs(90)];
        let tz = tzmod::parse("America/Chicago").unwrap();
        let anchor = datetime!(2026-04-13 05:00 UTC); // local midnight CDT
        let to = anchor + Sequential::new(&items, &durs).total_duration();

        let r1 = Sequential::new(&items, &durs);
        let r2 = Sequential::new(&items, &durs);
        let f1 = emit_window(dir1.path(), &r1, anchor, tz, 6, anchor, to)
            .await
            .unwrap();
        let f2 = emit_window(dir2.path(), &r2, anchor, tz, 6, anchor, to)
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

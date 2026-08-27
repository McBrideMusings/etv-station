//! One-shot repair of the play-history ledger from the playout files on disk.
//!
//! The ledger ([`crate::history`]) is written once, at generation time, by the
//! same pass that emits the chunk JSON. That makes the two stores agree by
//! construction — until something deletes from one and not the other. The
//! coverage-heal path did exactly that: it truncated the ledger from the
//! instant the wipe *started* while regeneration resumed at the surviving
//! frontier, so every heal dropped rows for a span of schedule that was still
//! on disk and still aired. That asymmetry is fixed at its source in
//! `daemon.rs`; this module repairs the ledgers it already damaged.
//!
//! **This reads the playout files as the record of what was scheduled, which
//! is what they are** — not as a second source of truth competing with the
//! ledger. A chunk file is the schedule; the ledger is an index over it that
//! exists so the resume cursor and the adjacency seam can ask cheap questions.
//! Rebuilding the index from the thing it indexes is recovery, not duplication.
//!
//! # What it cannot recover
//!
//! Only what is still on disk. `retention_days` prunes elapsed chunk files, so
//! a ledger hole older than that window is gone for good — run this before
//! retention catches up with the damage, not after.
//!
//! Two fields are also unrecoverable from a chunk file and are filled in
//! honestly rather than guessed:
//!
//! - `played_at` becomes the moment of the repair, not the moment the original
//!   generation ran. It is provenance only — nothing schedules by it.
//! - `error_card` becomes `false`, because a chunk file does not record whether
//!   a slot ended up showing an error card. A repaired row therefore counts as
//!   watched content for the recency query ([`crate::history::HistoryDb::tail`])
//!   where the original may not have. The series cursor is unaffected: it counts
//!   error cards either way.

use std::collections::HashMap;
use std::path::Path;

use ersatztv_playout::playout::Playout;
use time::OffsetDateTime;

use crate::catalog::Catalog;
use crate::config::Station;
use crate::errors::StationError;
use crate::history::{HistoryDb, PlayRecord};
use crate::scan;

/// What one channel's repair did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ChannelRepair {
    pub channel: String,
    /// Airings found across this channel's playout files.
    pub on_disk: usize,
    /// Rows written — those the ledger was missing.
    pub inserted: usize,
    /// The span the inserted rows cover, absent when nothing was missing.
    pub filled_from: Option<OffsetDateTime>,
    pub filled_to: Option<OffsetDateTime>,
}

/// What the whole pass did, in the order channels were visited.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BackfillReport {
    pub channels: Vec<ChannelRepair>,
}

impl BackfillReport {
    pub fn inserted(&self) -> usize {
        self.channels.iter().map(|c| c.inserted).sum()
    }

    pub fn on_disk(&self) -> usize {
        self.channels.iter().map(|c| c.on_disk).sum()
    }

    /// Only the channels that were actually missing rows — what a report should
    /// lead with, since a healthy station repairs nothing.
    pub fn repaired(&self) -> impl Iterator<Item = &ChannelRepair> {
        self.channels.iter().filter(|c| c.inserted > 0)
    }
}

/// Replay every channel's playout files into the ledger, inserting only the
/// airings it does not already hold.
///
/// Idempotent: a second run over an unchanged station inserts nothing, because
/// [`HistoryDb::record_missing`] keys on `(entry_id, start)`.
///
/// `dry_run` does every read and reports what it would write, without writing.
pub async fn run(
    station: &Station,
    history_db: &HistoryDb,
    catalog: Option<&Catalog>,
    dry_run: bool,
) -> Result<BackfillReport, StationError> {
    let now = OffsetDateTime::now_utc();
    let mut report = BackfillReport::default();

    for channel in &station.channels {
        let repair = repair_channel(
            &channel.name,
            &channel.output_folder,
            history_db,
            catalog,
            now,
            dry_run,
        )
        .await?;
        report.channels.push(repair);
    }

    Ok(report)
}

async fn repair_channel(
    name: &str,
    output: &Path,
    history_db: &HistoryDb,
    catalog: Option<&Catalog>,
    now: OffsetDateTime,
    dry_run: bool,
) -> Result<ChannelRepair, StationError> {
    let mut items: Vec<(String, OffsetDateTime)> = Vec::new();

    // Every chunk file, oldest first, so the records go in in schedule order —
    // the same order `record` writes them in during a generation.
    let mut files = scan::scan_output_folder(output).await?;
    files.sort_by_key(|f| f.start);
    for file in &files {
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
        for item in playout.items {
            items.push((item.id, item.start));
        }
    }

    if items.is_empty() {
        return Ok(ChannelRepair {
            channel: name.to_string(),
            ..Default::default()
        });
    }

    // The ledger keys the resume cursor on the show, so a row without its
    // `show_id` would leave that series looking like a movie — its own series of
    // one, never resuming. One catalog query for the whole channel, the same
    // call the generation pass makes.
    let ids: Vec<String> = items.iter().map(|(id, _)| id.clone()).collect();
    let show_ids: HashMap<String, String> = match catalog {
        Some(cat) => cat.show_ids_for(&ids)?,
        None => HashMap::new(),
    };

    let records: Vec<PlayRecord> = items
        .iter()
        .map(|(id, start)| PlayRecord {
            entry_id: id.clone(),
            show_id: show_ids.get(id).cloned(),
            start: *start,
            played_at: now,
            error_card: false,
        })
        .collect();

    let on_disk = records.len();
    if dry_run {
        // Count what a write would insert without performing one. Cheaper than
        // it looks: this is the same set difference `record_missing` does.
        let missing = missing_records(history_db, name, &records)?;
        return Ok(ChannelRepair {
            channel: name.to_string(),
            on_disk,
            inserted: missing.len(),
            filled_from: missing.first().map(|r| r.start),
            filled_to: missing.last().map(|r| r.start),
        });
    }

    let missing = missing_records(history_db, name, &records)?;
    let filled_from = missing.first().map(|r| r.start);
    let filled_to = missing.last().map(|r| r.start);
    let inserted = history_db.record_missing(name, &records)?;

    Ok(ChannelRepair {
        channel: name.to_string(),
        on_disk,
        inserted,
        filled_from,
        filled_to,
    })
}

/// Which of `records` the ledger does not already hold, in schedule order.
///
/// Reported separately from the insert so a dry run can say what it *would*
/// write, and so the written report can name the span that was filled — which
/// is the number a reader actually wants ("recovered 6 hours on channel 2"),
/// not the raw row count.
fn missing_records<'a>(
    history_db: &HistoryDb,
    channel: &str,
    records: &'a [PlayRecord],
) -> Result<Vec<&'a PlayRecord>, StationError> {
    let present = history_db.airing_keys(channel)?;
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for r in records {
        let key = (r.entry_id.clone(), r.start);
        if !present.contains(&key) && seen.insert(key) {
            out.push(r);
        }
    }
    out.sort_by_key(|r| r.start);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ersatztv_playout::playout::PlayoutItem;
    use time::Duration;

    /// Write one chunk file holding `n` half-hour airings laid end to end from
    /// `start`, named the way `emit` names a full chunk.
    async fn write_chunk(folder: &Path, start: OffsetDateTime, n: usize) -> Vec<PlayRecord> {
        let mut items = Vec::new();
        let mut records = Vec::new();
        let mut cursor = start;
        for i in 0..n {
            let finish = cursor + Duration::minutes(30);
            let id = format!("entry-{i}");
            items.push(PlayoutItem {
                id: id.clone(),
                start: cursor,
                finish,
                source: None,
                tracks: None,
                watermark: None,
                program: None,
                metadata: None,
            });
            records.push(PlayRecord {
                entry_id: id,
                show_id: None,
                start: cursor,
                played_at: cursor,
                error_card: false,
            });
            cursor = finish;
        }
        let name = crate::emit::chunk_filename(start, cursor).unwrap();
        tokio::fs::write(
            folder.join(name),
            serde_json::to_vec(&Playout::new(items)).unwrap(),
        )
        .await
        .unwrap();
        records
    }

    fn at(hour: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_787_000_000 + hour * 3600).unwrap()
    }

    /// The repair inserts exactly the airings the ledger is missing, and a
    /// second pass over an unchanged station writes nothing. Idempotence is the
    /// property that makes this safe to run from a task runner without anyone
    /// having to check first whether it is needed.
    #[tokio::test]
    async fn it_inserts_only_what_is_missing_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db = HistoryDb::open_in_memory().unwrap();
        let all = write_chunk(dir.path(), at(0), 6).await;

        // The ledger holds the first two airings; the heal ate the rest.
        db.record("ch", &all[..2]).unwrap();

        let now = OffsetDateTime::now_utc();
        let first = repair_channel("ch", dir.path(), &db, None, now, false)
            .await
            .unwrap();
        assert_eq!(first.on_disk, 6);
        assert_eq!(first.inserted, 4, "only the four missing airings go in");
        assert_eq!(first.filled_from, Some(at(0) + Duration::hours(1)));

        let again = repair_channel("ch", dir.path(), &db, None, now, false)
            .await
            .unwrap();
        assert_eq!(again.inserted, 0, "a second pass must write nothing");
        assert_eq!(db.count("ch").unwrap(), 6);
    }

    /// A dry run reports the same count it would have written, and writes none
    /// of it.
    #[tokio::test]
    async fn a_dry_run_reports_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let db = HistoryDb::open_in_memory().unwrap();
        write_chunk(dir.path(), at(0), 4).await;

        let now = OffsetDateTime::now_utc();
        let report = repair_channel("ch", dir.path(), &db, None, now, true)
            .await
            .unwrap();
        assert_eq!(report.inserted, 4);
        assert_eq!(db.count("ch").unwrap(), 0, "a dry run writes nothing");
    }

    /// A channel whose ledger is intact is left completely alone — the case
    /// that runs on every healthy channel of a station being repaired for one
    /// sick one.
    #[tokio::test]
    async fn a_healthy_channel_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let db = HistoryDb::open_in_memory().unwrap();
        let all = write_chunk(dir.path(), at(0), 5).await;
        db.record("ch", &all).unwrap();

        let report = repair_channel(
            "ch",
            dir.path(),
            &db,
            None,
            OffsetDateTime::now_utc(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(report.inserted, 0);
        assert_eq!(report.filled_from, None);
        assert_eq!(db.count("ch").unwrap(), 5);
    }

    /// An empty playout folder is not an error — a channel that has never
    /// generated has nothing to repair.
    #[tokio::test]
    async fn an_empty_channel_reports_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db = HistoryDb::open_in_memory().unwrap();
        let report = repair_channel(
            "ch",
            dir.path(),
            &db,
            None,
            OffsetDateTime::now_utc(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(report.on_disk, 0);
        assert_eq!(report.inserted, 0);
    }
}

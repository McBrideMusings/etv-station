//! The `.history` sidecar — the play-history ledger (#70).
//!
//! One line per scheduled airing, appended as each generation is emitted. A
//! **dumb record**: no taste logic, no TTL, no relevance. Its whole job is to
//! be the single place that remembers what this channel has aired.
//!
//! # One structure, two read shapes
//!
//! The per-series resume cursor is a *projection* of this ledger, not a
//! separate store: [`Ledger::series_cursor`] answers "what did each series play
//! last" by walking the records, and that is what a pool with
//! `advance = "resume"` continues from. A future taste scorer reads the same
//! records the other way — everything, with timestamps. Two read shapes over
//! one structure, so there is no second copy of "where are we" to drift.
//!
//! # Why a file and not a table
//!
//! The volume is small — a channel airing half-hour episodes writes on the
//! order of tens of thousands of lines a year — and every read the generator
//! needs is "walk it once". A sqlite table would buy indexed and cross-channel
//! queries that nothing asks for yet; when the scoring work (#74) defines those
//! queries, promoting an append-only four-field log into a table is mechanical.
//! Keeping it a file also leaves the catalog's "delete it and re-ingest"
//! property intact: play history is not rebuildable, and it does not live in
//! the rebuildable store.
//!
//! # Shape of a line
//!
//! Each line is self-sufficient — it carries the `show_id` it belonged to, so
//! deriving the cursor never joins back to the catalog and a record still means
//! something after its entry has left the library.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::errors::StationError;

const SIDECAR_NAME: &str = ".history";

/// One scheduled airing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayRecord {
    /// The catalog entry that aired.
    pub entry_id: String,

    /// The show it belonged to, when it belonged to one. A movie has none —
    /// see [`PlayRecord::series_key`], which is what the cursor is keyed on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_id: Option<String>,

    /// When this airing starts — the instant it occupies in the schedule. This
    /// is the field a rewind truncates on, and the one a recency query wants.
    #[serde(with = "time::serde::rfc3339")]
    pub start: OffsetDateTime,

    /// When the row was written, i.e. when the generation that scheduled this
    /// airing ran. Distinct from [`PlayRecord::start`], which is when it airs —
    /// the schedule is written ahead of time, so a row normally exists well
    /// before its airing. Provenance, not scheduling input.
    #[serde(with = "time::serde::rfc3339")]
    pub played_at: OffsetDateTime,
}

impl PlayRecord {
    /// The key this airing counts against for resume purposes: its `show_id`,
    /// or — for an item that belongs to no show — the entry itself.
    ///
    /// This mirrors the series-key rule in [`crate::pattern`], where an item
    /// without a `show_id` is its own series of one. The two must agree, or a
    /// movie pool's cursor would be filed under a key the pattern never looks
    /// up.
    pub fn series_key(&self) -> &str {
        self.show_id.as_deref().unwrap_or(&self.entry_id)
    }
}

/// A channel's ledger, held in memory for the length of a catch-up.
///
/// A catch-up chains many generations in one tick, and each needs the cursor as
/// of the previous one. Loading once and appending in memory keeps that to a
/// single read no matter how long the chain runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ledger {
    records: Vec<PlayRecord>,
    /// How many leading records are already durable on disk. Everything after
    /// this index still needs appending; a rewrite resets it to zero.
    flushed: usize,
}

impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn records(&self) -> &[PlayRecord] {
        &self.records
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Record airings, in schedule order.
    pub fn extend(&mut self, records: impl IntoIterator<Item = PlayRecord>) {
        self.records.extend(records);
    }

    /// The most recently aired entry ids, oldest first, at most `n` of them.
    ///
    /// This is the adjacency seam (#73): the last id here airs immediately
    /// before the first item of the next generation, so the constraint pass
    /// reads it to avoid repeating across the boundary.
    ///
    /// "Most recent" is decided by [`PlayRecord::start`], not by position in
    /// the file — a writer that appends out of schedule order gets the same
    /// answer as one that doesn't. Equal starts keep file order, so the record
    /// written later sorts later.
    pub fn tail(&self, n: usize) -> Vec<String> {
        let mut order: Vec<usize> = (0..self.records.len()).collect();
        order.sort_by_key(|&i| self.records[i].start);
        order[order.len().saturating_sub(n)..]
            .iter()
            .map(|&i| self.records[i].entry_id.clone())
            .collect()
    }

    /// What each series played last — the resume cursor, projected.
    ///
    /// "Last" is the greatest [`PlayRecord::start`] for the key, not the last
    /// record in file order: appending out of schedule order — backfilling a
    /// gap, merging two ledgers — must not silently rewind a series. Equal
    /// starts resolve to the later record in file order. Returns
    /// `series_key -> entry_id`.
    pub fn series_cursor(&self) -> BTreeMap<String, String> {
        let mut cursor: BTreeMap<String, (OffsetDateTime, String)> = BTreeMap::new();
        for r in &self.records {
            let latest = cursor
                .entry(r.series_key().to_string())
                .or_insert_with(|| (r.start, r.entry_id.clone()));
            if latest.0 <= r.start {
                *latest = (r.start, r.entry_id.clone());
            }
        }
        cursor
            .into_iter()
            .map(|(key, (_, entry_id))| (key, entry_id))
            .collect()
    }

    /// Drop every airing scheduled at or after `from`.
    ///
    /// The rewind deletes the emitted chunk files from that instant forward
    /// because they are about to be regenerated; those airings are no longer
    /// scheduled, so their records go too. Keeping them would leave the ledger
    /// describing a schedule that no longer exists — and, because the cursor is
    /// a projection of the ledger, would silently skip the content that the
    /// replaced airings had claimed.
    pub fn truncate_from(&mut self, from: OffsetDateTime) {
        let before = self.records.len();
        self.records.retain(|r| r.start < from);
        if self.records.len() != before {
            // The file no longer matches: what is left has to be rewritten
            // rather than appended to.
            self.flushed = 0;
        }
    }
}

pub fn sidecar_path(output_folder: &Path) -> PathBuf {
    output_folder.join(SIDECAR_NAME)
}

/// Read the ledger, or an empty one if there is none.
///
/// A line that won't parse is skipped rather than failing the channel: the file
/// is append-only, so a torn final line is the plausible corruption, and losing
/// one airing's record costs a resume position — not playout. The count of
/// skipped lines is returned so the caller can say so.
pub async fn load(output_folder: &Path) -> Result<(Ledger, usize), StationError> {
    let path = sidecar_path(output_folder);
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Ledger::new(), 0)),
        Err(source) => return Err(StationError::Io { path, source }),
    };

    let text = String::from_utf8_lossy(&bytes);
    let mut records = Vec::new();
    let mut skipped = 0;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<PlayRecord>(line) {
            Ok(r) => records.push(r),
            Err(_) => skipped += 1,
        }
    }
    // The reads no longer depend on file order, so an out-of-order file is not
    // a correctness problem — but it does mean some writer appended out of
    // schedule order, and that should be visible rather than absorbed. Warn;
    // never fail, because failing here takes the channel off the air over a
    // condition the reads already handle.
    if let Some(i) = first_out_of_order(&records) {
        tracing::warn!(
            event = "history.out_of_order",
            path = %path.display(),
            index = i,
            "play history is not in schedule order; reads are ordered by start, but some writer appended out of order",
        );
    }

    let flushed = records.len();
    Ok((Ledger { records, flushed }, skipped))
}

/// The index of the first record whose `start` precedes its predecessor's, if
/// any.
fn first_out_of_order(records: &[PlayRecord]) -> Option<usize> {
    records
        .windows(2)
        .position(|w| w[1].start < w[0].start)
        .map(|i| i + 1)
}

/// Persist anything not yet on disk.
///
/// Normally this is a true append — the ledger only grows — so a long-running
/// channel never rewrites its history. After a truncation the file is rewritten
/// once, atomically, because the tail has to go.
pub async fn save(output_folder: &Path, ledger: &mut Ledger) -> Result<(), StationError> {
    tokio::fs::create_dir_all(output_folder)
        .await
        .map_err(|source| StationError::Io {
            path: output_folder.to_path_buf(),
            source,
        })?;
    let path = sidecar_path(output_folder);

    // Appending assumes the file still holds the records we think are flushed.
    // If it has gone missing — deleted by hand, or a volume remounted — an
    // append would create a fresh file containing only the tail, silently
    // dropping the earlier history and with it every series' position. Fall
    // back to writing the whole thing.
    let on_disk = tokio::fs::try_exists(&path)
        .await
        .map_err(|source| StationError::Io {
            path: path.clone(),
            source,
        })?;

    if ledger.flushed == 0 || !on_disk {
        let body = encode(&ledger.records);
        crate::atomic::atomic_write_bytes(&path, body.as_bytes()).await?;
        ledger.flushed = ledger.records.len();
        return Ok(());
    }

    if ledger.flushed >= ledger.records.len() {
        return Ok(());
    }

    let body = encode(&ledger.records[ledger.flushed..]);
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
        .map_err(|source| StationError::Io {
            path: path.clone(),
            source,
        })?;
    use tokio::io::AsyncWriteExt;
    file.write_all(body.as_bytes())
        .await
        .map_err(|source| StationError::Io {
            path: path.clone(),
            source,
        })?;
    file.flush()
        .await
        .map_err(|source| StationError::Io { path, source })?;
    ledger.flushed = ledger.records.len();
    Ok(())
}

fn encode(records: &[PlayRecord]) -> String {
    let mut out = String::new();
    for r in records {
        // A record is a fixed set of owned scalars; serialization cannot fail.
        out.push_str(&serde_json::to_string(r).expect("PlayRecord serializes"));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use time::macros::datetime;

    fn at(hour: i64) -> OffsetDateTime {
        datetime!(2026-07-22 00:00 UTC) + time::Duration::hours(hour)
    }

    fn ep(entry_id: &str, show: &str, hour: i64) -> PlayRecord {
        PlayRecord {
            entry_id: entry_id.into(),
            show_id: Some(show.into()),
            start: at(hour),
            played_at: at(0),
        }
    }

    fn movie(entry_id: &str, hour: i64) -> PlayRecord {
        PlayRecord {
            entry_id: entry_id.into(),
            show_id: None,
            start: at(hour),
            played_at: at(0),
        }
    }

    #[test]
    fn series_key_falls_back_to_the_entry_for_a_movie() {
        assert_eq!(ep("got-e1", "show:got", 0).series_key(), "show:got");
        assert_eq!(movie("mov-dune", 0).series_key(), "mov-dune");
    }

    #[test]
    fn cursor_projects_the_last_airing_of_each_series() {
        let mut l = Ledger::new();
        l.extend([
            ep("got-e1", "show:got", 0),
            ep("got-e2", "show:got", 1),
            ep("inv-e1", "show:inv", 2),
            movie("mov-dune", 3),
        ]);
        let cursor = l.series_cursor();
        assert_eq!(cursor.get("show:got").unwrap(), "got-e2");
        assert_eq!(cursor.get("show:inv").unwrap(), "inv-e1");
        assert_eq!(cursor.get("mov-dune").unwrap(), "mov-dune");
        assert_eq!(cursor.len(), 3);
    }

    #[test]
    fn an_empty_ledger_projects_an_empty_cursor() {
        assert!(Ledger::new().series_cursor().is_empty());
    }

    /// The airings of one busy day, in schedule order. Two shows interleave and
    /// a movie sits in the middle, so a shuffle can plausibly move any of them.
    fn a_days_airings() -> Vec<PlayRecord> {
        vec![
            ep("got-e1", "show:got", 0),
            ep("inv-e1", "show:inv", 1),
            ep("got-e2", "show:got", 2),
            movie("mov-dune", 3),
            ep("inv-e2", "show:inv", 4),
            ep("got-e3", "show:got", 5),
        ]
    }

    /// The same airings, appended in an order no generation would produce — a
    /// backfilled gap, a merge of two ledgers, a manual insertion.
    fn shuffled(records: &[PlayRecord]) -> Vec<PlayRecord> {
        let order = [3usize, 0, 5, 2, 4, 1];
        order.iter().map(|&i| records[i].clone()).collect()
    }

    fn ledger_of(records: Vec<PlayRecord>) -> Ledger {
        let mut l = Ledger::new();
        l.extend(records);
        l
    }

    #[test]
    fn the_cursor_reads_the_same_whatever_order_the_records_were_appended_in() {
        let in_order = ledger_of(a_days_airings());
        let out_of_order = ledger_of(shuffled(&a_days_airings()));
        assert_eq!(out_of_order.series_cursor(), in_order.series_cursor());
        assert_eq!(in_order.series_cursor().get("show:got").unwrap(), "got-e3");
    }

    #[test]
    fn the_seam_tail_reads_the_same_whatever_order_the_records_were_appended_in() {
        let in_order = ledger_of(a_days_airings());
        let out_of_order = ledger_of(shuffled(&a_days_airings()));
        for n in 0..=7 {
            assert_eq!(out_of_order.tail(n), in_order.tail(n), "tail({n})");
        }
        assert_eq!(in_order.tail(2), vec!["inv-e2", "got-e3"]);
    }

    #[test]
    fn two_airings_at_one_instant_resolve_to_the_later_record() {
        // Nothing schedules two things at the same start, but a merge of two
        // ledgers can. File order is the only tiebreak left, and it is the one
        // that matches what an in-order ledger does today.
        let l = ledger_of(vec![
            ep("got-e1", "show:got", 0),
            ep("got-e2", "show:got", 0),
        ]);
        assert_eq!(l.series_cursor().get("show:got").unwrap(), "got-e2");
        assert_eq!(l.tail(1), vec!["got-e2"]);
    }

    #[test]
    fn out_of_order_detection_reports_the_first_offending_index() {
        assert_eq!(first_out_of_order(&a_days_airings()), None);
        assert_eq!(first_out_of_order(&shuffled(&a_days_airings())), Some(1));
        assert_eq!(first_out_of_order(&[]), None);
    }

    #[test]
    fn truncation_drops_airings_at_or_after_the_instant() {
        let mut l = Ledger::new();
        l.extend([
            ep("got-e1", "show:got", 0),
            ep("got-e2", "show:got", 6),
            ep("got-e3", "show:got", 12),
        ]);
        l.truncate_from(at(6));
        assert_eq!(l.len(), 1);
        // The cursor follows the truncation — this is what stops a regenerated
        // span from skipping the content the replaced airings had claimed.
        assert_eq!(l.series_cursor().get("show:got").unwrap(), "got-e1");
    }

    #[test]
    fn truncation_that_removes_nothing_leaves_the_file_appendable() {
        let mut l = Ledger::new();
        l.extend([ep("got-e1", "show:got", 0)]);
        l.flushed = 1;
        l.truncate_from(at(6));
        assert_eq!(l.flushed, 1, "nothing removed, so no rewrite is needed");
    }

    #[tokio::test]
    async fn missing_sidecar_reads_as_empty() {
        let dir = tempdir().unwrap();
        let (l, skipped) = load(dir.path()).await.unwrap();
        assert!(l.is_empty());
        assert_eq!(skipped, 0);
    }

    #[tokio::test]
    async fn round_trips_through_disk() {
        let dir = tempdir().unwrap();
        let mut l = Ledger::new();
        l.extend([ep("got-e1", "show:got", 0), movie("mov-dune", 1)]);
        save(dir.path(), &mut l).await.unwrap();

        let (loaded, skipped) = load(dir.path()).await.unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(loaded.records(), l.records());
    }

    #[tokio::test]
    async fn a_second_save_appends_rather_than_rewriting() {
        let dir = tempdir().unwrap();
        let mut l = Ledger::new();
        l.extend([ep("got-e1", "show:got", 0)]);
        save(dir.path(), &mut l).await.unwrap();
        let first = tokio::fs::read(sidecar_path(dir.path())).await.unwrap();

        l.extend([ep("got-e2", "show:got", 1)]);
        save(dir.path(), &mut l).await.unwrap();
        let second = tokio::fs::read(sidecar_path(dir.path())).await.unwrap();

        assert!(
            second.starts_with(&first),
            "an append must leave the existing bytes untouched"
        );
        let (loaded, _) = load(dir.path()).await.unwrap();
        assert_eq!(loaded.len(), 2);
    }

    #[tokio::test]
    async fn save_after_truncation_rewrites_the_file() {
        let dir = tempdir().unwrap();
        let mut l = Ledger::new();
        l.extend([
            ep("got-e1", "show:got", 0),
            ep("got-e2", "show:got", 6),
            ep("got-e3", "show:got", 12),
        ]);
        save(dir.path(), &mut l).await.unwrap();

        l.truncate_from(at(6));
        save(dir.path(), &mut l).await.unwrap();

        let (loaded, _) = load(dir.path()).await.unwrap();
        assert_eq!(
            loaded.len(),
            1,
            "the tail must be gone from disk, not just memory"
        );
        assert_eq!(loaded.records()[0].entry_id, "got-e1");
    }

    #[tokio::test]
    async fn a_torn_line_is_skipped_rather_than_failing_the_channel() {
        let dir = tempdir().unwrap();
        let mut l = Ledger::new();
        l.extend([ep("got-e1", "show:got", 0)]);
        save(dir.path(), &mut l).await.unwrap();
        // Simulate a crash mid-append.
        let path = sidecar_path(dir.path());
        let mut bytes = tokio::fs::read(&path).await.unwrap();
        bytes.extend_from_slice(br#"{"entry_id":"got-e2","sta"#);
        tokio::fs::write(&path, bytes).await.unwrap();

        let (loaded, skipped) = load(dir.path()).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(skipped, 1);
    }

    #[tokio::test]
    async fn a_vanished_file_is_rewritten_whole_rather_than_appended_to() {
        let dir = tempdir().unwrap();
        let mut l = Ledger::new();
        l.extend([ep("got-e1", "show:got", 0), ep("got-e2", "show:got", 1)]);
        save(dir.path(), &mut l).await.unwrap();

        // Someone deletes the sidecar between runs.
        tokio::fs::remove_file(sidecar_path(dir.path()))
            .await
            .unwrap();
        l.extend([ep("got-e3", "show:got", 2)]);
        save(dir.path(), &mut l).await.unwrap();

        let (loaded, _) = load(dir.path()).await.unwrap();
        assert_eq!(
            loaded.len(),
            3,
            "the whole ledger must be rewritten, not just the unflushed tail"
        );
        assert_eq!(loaded.series_cursor().get("show:got").unwrap(), "got-e3");
    }

    /// Collects the formatted log output of whatever runs under it.
    #[derive(Clone, Default)]
    struct CapturedLogs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl CapturedLogs {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl std::io::Write for CapturedLogs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl tracing_subscriber::fmt::MakeWriter<'_> for CapturedLogs {
        type Writer = Self;

        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn an_out_of_order_file_warns_and_still_loads() {
        let dir = tempdir().unwrap();
        let path = sidecar_path(dir.path());
        std::fs::write(&path, encode(&shuffled(&a_days_airings()))).unwrap();

        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .finish();
        // A current-thread runtime so the load runs on the thread the
        // subscriber is defaulted on.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (loaded, skipped) =
            tracing::subscriber::with_default(subscriber, || rt.block_on(load(dir.path())))
                .unwrap();

        assert_eq!(skipped, 0);
        assert_eq!(loaded.len(), 6, "an out-of-order file still loads");
        assert_eq!(loaded.series_cursor().get("show:got").unwrap(), "got-e3");

        let text = logs.text();
        assert_eq!(
            text.matches("history.out_of_order").count(),
            1,
            "exactly one warning, got: {text}"
        );
        assert!(text.contains(&path.display().to_string()), "got: {text}");
        assert!(text.contains("index=1"), "got: {text}");
    }

    #[test]
    fn an_in_order_file_loads_silently() {
        let dir = tempdir().unwrap();
        std::fs::write(sidecar_path(dir.path()), encode(&a_days_airings())).unwrap();

        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .finish();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tracing::subscriber::with_default(subscriber, || rt.block_on(load(dir.path()))).unwrap();

        assert!(!logs.text().contains("history.out_of_order"));
    }

    #[tokio::test]
    async fn saving_an_unchanged_ledger_is_a_no_op() {
        let dir = tempdir().unwrap();
        let mut l = Ledger::new();
        l.extend([ep("got-e1", "show:got", 0)]);
        save(dir.path(), &mut l).await.unwrap();
        let first = tokio::fs::read(sidecar_path(dir.path())).await.unwrap();
        save(dir.path(), &mut l).await.unwrap();
        let second = tokio::fs::read(sidecar_path(dir.path())).await.unwrap();
        assert_eq!(first, second);
    }
}

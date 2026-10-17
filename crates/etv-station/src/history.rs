//! The play-history ledger (#70), promoted to sqlite (#111).
//!
//! One row per scheduled airing, inserted as each generation is emitted. A
//! **dumb record**: no taste logic, no TTL, no relevance. Its whole job is to
//! be the single place that remembers what every channel has aired.
//!
//! # One structure, many read shapes
//!
//! The per-series resume cursor is a *projection* of this store, not a
//! separate one: [`HistoryDb::series_cursor`] answers "what did each series
//! play last, on this channel" by an indexed query, and that is what a pool
//! with `advance = "resume"` continues from. The adjacency seam
//! ([`HistoryDb::tail`]) and a scorer's recency input read the same rows a
//! different way. [`HistoryDb::cross_channel_show_cursor`] reads them a third
//! way — the last airing per `show_id` over *every* channel, which no
//! per-channel file could ever answer without reading all of them. Four read
//! shapes over one table, so there is no second copy of "where are we" to
//! drift.
//!
//! # Why sqlite, and why not the catalog
//!
//! This used to be one JSONL file per channel: cheap to append, but a walk of
//! the whole file was the only way to answer any question, and nothing could
//! ask a question that spanned channels. Sqlite answers all four read shapes
//! above as indexed queries instead of a full scan, and a `GROUP BY show_id`
//! over one table is what makes the cross-channel cursor possible at all.
//!
//! It is a database file of its own, not a table in the catalog
//! (`crate::catalog`). The catalog's "delete it and re-ingest" property
//! depends on everything in it being rebuildable from Plex/the filesystem;
//! play history is not rebuildable — deleting it loses real broadcast
//! provenance forever — so it does not live in the rebuildable store.
//!
//! # Migration
//!
//! A channel's old `.history` JSONL sidecar is read exactly once, at the
//! first startup after this shipped: [`HistoryDb::migrate_channel`] checks a
//! marker in `history_meta`, and if the channel has never been migrated,
//! reads the file, inserts every row it can parse, and records the marker —
//! all in one transaction. The file is left in place; nothing reads it again.
//!
//! # Shape of a row
//!
//! Each row is self-sufficient — it carries the `show_id` it belonged to, so
//! deriving a cursor never joins back to the catalog and a row still means
//! something after its entry has left the library. The `channel` column is
//! what the old file path carried implicitly; a shared table has to carry it
//! explicitly.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;

const SIDECAR_NAME: &str = ".history";

/// One scheduled airing. Also the row shape the old `.history` JSONL sidecar
/// used, which is why this still implements [`Deserialize`] — migration reads
/// old lines straight into this type.
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

    /// True when the slot aired an error card instead of the item itself,
    /// because the file could not be read.
    ///
    /// The row still exists, and deliberately so: the resume cursor is a
    /// projection of this store, and omitting the row would leave the series
    /// pointed at the broken item forever — every generation would schedule it
    /// again and the channel would show the same error card on repeat. The
    /// slot genuinely was broadcast, so the series genuinely advanced.
    ///
    /// What it must NOT do is count as *watched*: the taste scorer reads these
    /// rows to learn what has been seen recently, and nobody saw this film.
    /// [`HistoryDb::tail`] filters on this flag.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub error_card: bool,

    /// Whether this row is a programme (a catalog item the scheduler chose) or
    /// one segment of a bumper timeline filling a gap. Every read that stands
    /// for "what was watched" or "where a series is" — the resume cursor, the
    /// adjacency tail, the recency input — counts programmes only.
    #[serde(default, skip_serializing_if = "AiringKind::is_programme")]
    pub kind: AiringKind,

    /// The media the slot played: the file path for a clip or still, the
    /// profile for a render, the lavfi graph for a generated source. `None`
    /// for a programme, which `entry_id` already names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,

    /// For a clip segment, the slice of `source` played, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_ms: Option<u64>,

    /// The plugin script that built a bumper segment; `None` for a programme.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub produced_by: Option<String>,
}

/// A programme row at the Unix epoch with no entry: a base for struct-update
/// syntax, so a caller names only the fields that differ.
impl Default for PlayRecord {
    fn default() -> Self {
        Self {
            entry_id: String::new(),
            show_id: None,
            start: OffsetDateTime::UNIX_EPOCH,
            played_at: OffsetDateTime::UNIX_EPOCH,
            error_card: false,
            kind: AiringKind::Programme,
            source: None,
            in_ms: None,
            out_ms: None,
            produced_by: None,
        }
    }
}

/// What a ledger row is. See [`PlayRecord::kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiringKind {
    #[default]
    Programme,
    Bumper,
}

impl AiringKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AiringKind::Programme => "programme",
            AiringKind::Bumper => "bumper",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "programme" => Some(AiringKind::Programme),
            "bumper" => Some(AiringKind::Bumper),
            _ => None,
        }
    }

    fn is_programme(&self) -> bool {
        *self == AiringKind::Programme
    }
}

/// One ledger row with the channel it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerRow {
    pub channel: String,
    pub record: PlayRecord,
}

/// A query over the ledger: every set field must match. `from`/`to` bound
/// [`PlayRecord::start`] (inclusive, exclusive), so a filter reaches scheduled
/// rows in the future exactly as it reaches rows already aired.
#[derive(Debug, Clone)]
pub struct AiringFilter {
    pub channel: Option<String>,
    pub kind: Option<AiringKind>,
    pub entry_id: Option<String>,
    pub show_id: Option<String>,
    pub source: Option<String>,
    pub produced_by: Option<String>,
    pub from: Option<OffsetDateTime>,
    pub to: Option<OffsetDateTime>,
    /// Newest first when true, oldest first otherwise.
    pub newest_first: bool,
    pub limit: usize,
}

impl Default for AiringFilter {
    /// Matches every row, newest last, capped at [`AiringFilter::DEFAULT_LIMIT`].
    fn default() -> Self {
        Self {
            channel: None,
            kind: None,
            entry_id: None,
            show_id: None,
            source: None,
            produced_by: None,
            from: None,
            to: None,
            newest_first: false,
            limit: Self::DEFAULT_LIMIT,
        }
    }
}

impl AiringFilter {
    pub const DEFAULT_LIMIT: usize = 100;
}

impl PlayRecord {
    /// The key this airing counts against for resume purposes: its `show_id`,
    /// or — for an item that belongs to no show — the entry itself.
    ///
    /// This mirrors the series-key rule in [`crate::pattern`], where an item
    /// without a `show_id` is its own series of one. The two must agree, or a
    /// movie pool's cursor would be filed under a key the pattern never looks
    /// up. [`HistoryDb::series_cursor`]'s SQL computes the same thing with
    /// `COALESCE(show_id, entry_id)`.
    pub fn series_key(&self) -> &str {
        self.show_id.as_deref().unwrap_or(&self.entry_id)
    }
}

/// Errors talking to the history database.
#[derive(Debug, Error)]
pub enum HistoryError {
    #[error("failed to open history db at {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("history sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A `start_ns` that is not a representable instant. Only reachable through
    /// a row this module did not write, since [`to_ns`] round-trips everything
    /// it stores.
    #[error("history row has an unrepresentable start_ns: {ns}")]
    CorruptTimestamp { ns: i64 },

    /// A `kind` column holding a string this module never writes.
    #[error("history row has an unknown kind: {kind:?}")]
    CorruptKind { kind: String },
}

/// Ordered migrations for the history database. Same discipline as the
/// catalog's (`crate::catalog::schema`): append-only, 1-based version, applied
/// inside a transaction so a failure leaves the version unchanged.
const MIGRATIONS: &[&str] = &[
    // v1 — one row per scheduled airing, station-wide.
    //
    // Times are stored as signed nanoseconds since the Unix epoch rather than
    // text: an RFC3339 string sorts correctly only if every row is formatted
    // to the same width, and `time`'s formatter omits the fractional part
    // when it's zero, which would break exactly that. An integer column
    // sorts correctly always and is what makes `ORDER BY start_ns` an
    // index-backed operation rather than a string comparison.
    r#"
    CREATE TABLE airings (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        channel      TEXT NOT NULL,
        entry_id     TEXT NOT NULL,
        show_id      TEXT,
        start_ns     INTEGER NOT NULL,
        played_at_ns INTEGER NOT NULL,
        error_card   INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX idx_airings_channel_start ON airings(channel, start_ns, id);
    CREATE INDEX idx_airings_show ON airings(show_id, start_ns, id) WHERE show_id IS NOT NULL;

    CREATE TABLE history_meta (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    "#,
    // v2 — bumper segments share the ledger with programmes. `kind` separates
    // them; `source`, `in_ms`, `out_ms` and `produced_by` say what a segment
    // played and which plugin built it. Existing rows are all programmes.
    r#"
    ALTER TABLE airings ADD COLUMN kind TEXT NOT NULL DEFAULT 'programme';
    ALTER TABLE airings ADD COLUMN source TEXT;
    ALTER TABLE airings ADD COLUMN in_ms INTEGER;
    ALTER TABLE airings ADD COLUMN out_ms INTEGER;
    ALTER TABLE airings ADD COLUMN produced_by TEXT;
    "#,
];

fn apply_migrations(conn: &Connection) -> Result<(), HistoryError> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);")?;
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |r| r.get(0),
    )?;
    for (i, ddl) in MIGRATIONS.iter().enumerate() {
        let version = (i + 1) as i64;
        if version <= current {
            continue;
        }
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(ddl)?;
        tx.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            [version],
        )?;
        tx.commit()?;
    }
    Ok(())
}

/// A row inserted or read anywhere in this module carries its instant as
/// nanoseconds since the Unix epoch — see the comment on the v1 migration for
/// why. Every wall-clock date in this process is built from `now_utc()` or
/// parsed from RFC3339 `Z`-suffixed text, so the round trip through this
/// integer never has to represent a leap second or an ambiguous local time.
fn to_ns(dt: OffsetDateTime) -> i64 {
    dt.unix_timestamp_nanos() as i64
}

/// The inverse of [`to_ns`], for the one caller that hands instants back out
/// ([`HistoryDb::airing_keys`]). A value this rejects is not a timestamp this
/// module ever wrote, so it is a corrupt row rather than a rounding question.
fn from_ns(ns: i64) -> Result<OffsetDateTime, HistoryError> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ns))
        .map_err(|_| HistoryError::CorruptTimestamp { ns })
}

/// What one call to [`HistoryDb::migrate_channel`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MigrationStats {
    /// This channel's `.history` file was already migrated on an earlier
    /// startup; nothing was read or inserted this time.
    pub already_done: bool,
    /// Records read from the JSONL file and inserted.
    pub migrated: usize,
    /// Lines that would not parse — the same "torn final line" tolerance the
    /// old JSONL reader had.
    pub skipped: usize,
}

/// A handle to the station-wide history database — one file, shared by every
/// channel, distinguished by the `channel` column on `airings`.
///
/// A single `Connection` behind a plain [`Mutex`] rather than one connection
/// per channel: every operation here is a handful of indexed rows in and out
/// — nothing like the arbitrarily slow, user-authored scorer queries that
/// made a shared catalog connection a station-wide chokepoint (see the
/// `CatalogInfo` doc comment in `daemon.rs`). Serializing history's cheap,
/// bounded reads and writes behind one lock costs nothing next to that.
pub struct HistoryDb {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for HistoryDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistoryDb").finish_non_exhaustive()
    }
}

impl HistoryDb {
    /// Open (creating if absent) the history db at `path`, applying pending
    /// migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, HistoryError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|source| HistoryError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let conn = Connection::open(path).map_err(|source| HistoryError::Open {
            path: path.to_path_buf(),
            source,
        })?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    /// An in-memory history db — used by tests.
    pub fn open_in_memory() -> Result<Self, HistoryError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, HistoryError> {
        apply_migrations(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Read `output_folder`'s `.history` JSONL sidecar and insert its rows
    /// under `channel`, exactly once. A channel already marked migrated (in
    /// `history_meta`) is a no-op — the file is not re-read.
    ///
    /// A missing file (a fresh channel with nothing aired yet) is not an
    /// error: it is treated as zero records and the channel is still marked
    /// migrated, so a later run never goes looking for a file that was never
    /// going to be there.
    ///
    /// A line that won't parse is skipped rather than failing the channel:
    /// the file is append-only, so a torn final line is the plausible
    /// corruption, and losing one airing's record costs a resume position —
    /// not playout — matching the tolerance the old JSONL reader had.
    pub async fn migrate_channel(
        &self,
        channel: &str,
        output_folder: &Path,
    ) -> Result<MigrationStats, HistoryError> {
        {
            let conn = self.lock();
            if channel_migrated(&conn, channel)? {
                return Ok(MigrationStats {
                    already_done: true,
                    ..Default::default()
                });
            }
        }

        let path = sidecar_path(output_folder);
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(source) => return Err(HistoryError::Io { path, source }),
        };
        let text = String::from_utf8_lossy(&bytes);
        let mut records = Vec::new();
        let mut skipped = 0usize;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<PlayRecord>(line) {
                Ok(r) => records.push(r),
                Err(_) => skipped += 1,
            }
        }
        // Migration reads whatever order the file held. Reads are ordered by
        // `start`/insertion-id, not file order, but an out-of-order file
        // means some writer appended out of schedule order — worth a log
        // line, never a failure.
        if let Some(i) = first_out_of_order(&records) {
            tracing::warn!(
                event = "history.out_of_order",
                path = %path.display(),
                index = i,
                "migrated play history is not in schedule order; reads are ordered by start, but some writer appended out of order",
            );
        }

        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        for r in &records {
            insert_record(&tx, channel, r)?;
        }
        mark_migrated(&tx, channel)?;
        tx.commit()?;

        Ok(MigrationStats {
            already_done: false,
            migrated: records.len(),
            skipped,
        })
    }

    /// Insert airings for `channel`, in schedule order. A no-op on an empty
    /// slice.
    pub fn record(&self, channel: &str, records: &[PlayRecord]) -> Result<(), HistoryError> {
        if records.is_empty() {
            return Ok(());
        }
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        for r in records {
            insert_record(&tx, channel, r)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every `(entry_id, start)` pair on record for `channel`.
    ///
    /// The identity a repair pass compares on: an airing is "already known" if
    /// this channel has a row for that entry at that instant. Exposed as
    /// `OffsetDateTime` so the nanosecond storage key stays inside this module.
    pub fn airing_keys(
        &self,
        channel: &str,
    ) -> Result<HashSet<(String, OffsetDateTime)>, HistoryError> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT entry_id, start_ns FROM airings WHERE channel = ?1")?;
        let rows = stmt.query_map(params![channel], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut out = HashSet::new();
        for row in rows {
            let (entry_id, ns) = row?;
            out.insert((entry_id, from_ns(ns)?));
        }
        Ok(out)
    }

    /// Insert only the airings this channel does not already hold, keyed on
    /// `(entry_id, start)`. Returns how many rows were written.
    ///
    /// [`Self::record`] inserts unconditionally, which is right for a
    /// generation laying down a schedule it has just computed — nothing it
    /// emits can already be on record. A repair pass ([`crate::backfill`])
    /// replays airings straight off the playout files, most of which the
    /// ledger already holds, so it needs the other semantic. Doing the dedupe
    /// here rather than in the caller is what keeps the nanosecond key an
    /// implementation detail of this module.
    ///
    /// One read of the channel's keys, then one transaction: a channel's whole
    /// ledger is a few thousand rows, and the alternative is a round trip per
    /// candidate item.
    pub fn record_missing(
        &self,
        channel: &str,
        records: &[PlayRecord],
    ) -> Result<usize, HistoryError> {
        if records.is_empty() {
            return Ok(0);
        }
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let mut present: HashSet<(String, i64)> = HashSet::new();
        {
            let mut stmt =
                tx.prepare("SELECT entry_id, start_ns FROM airings WHERE channel = ?1")?;
            let rows = stmt.query_map(params![channel], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            for row in rows {
                present.insert(row?);
            }
        }
        let mut inserted = 0;
        for r in records {
            // `insert` returns false when the pair was already there, which
            // also de-duplicates within `records` itself — two playout files
            // naming the same airing cannot double-count.
            if present.insert((r.entry_id.clone(), to_ns(r.start))) {
                insert_record(&tx, channel, r)?;
                inserted += 1;
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// What each series on `channel` played last — the resume cursor,
    /// projected.
    ///
    /// "Last" is the greatest [`PlayRecord::start`] for the key, not the last
    /// row inserted: appending out of schedule order — backfilling a gap,
    /// merging two ledgers — must not silently rewind a series. Equal starts
    /// resolve to the later row in insertion order (`id`). Returns
    /// `series_key -> entry_id`.
    pub fn series_cursor(&self, channel: &str) -> Result<BTreeMap<String, String>, HistoryError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT skey, entry_id FROM (
                SELECT COALESCE(show_id, entry_id) AS skey, entry_id,
                       ROW_NUMBER() OVER (
                           PARTITION BY COALESCE(show_id, entry_id)
                           ORDER BY start_ns DESC, id DESC
                       ) AS rn
                FROM airings WHERE channel = ?1 AND kind = 'programme'
             ) WHERE rn = 1",
        )?;
        let rows = stmt.query_map(params![channel], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (key, entry_id) = row?;
            out.insert(key, entry_id);
        }
        Ok(out)
    }

    /// The most recently aired entry ids on `channel`, oldest first, at most
    /// `n` of them.
    ///
    /// This is the adjacency seam (#73): the last id here airs immediately
    /// before the first item of the next generation, so the constraint pass
    /// reads it to avoid repeating across the boundary. The same query, at a
    /// different depth, is the scorer's recency input (`ScoreInputs::recent`).
    ///
    /// "Most recent" is decided by [`PlayRecord::start`], not by insertion
    /// order — a writer that inserts out of schedule order gets the same
    /// answer as one that doesn't. Equal starts resolve to the later
    /// insertion.
    ///
    /// Airings that showed an error card are left out. Both callers ask this
    /// question about content the viewer actually saw — the adjacency seam
    /// ("don't repeat what just played") and the scorer's recency input — and
    /// a slot that showed a "file not found" card played none of the film it
    /// was meant to. [`HistoryDb::series_cursor`] deliberately does the
    /// opposite and counts them, because the series still advanced past that
    /// item.
    pub fn tail(&self, channel: &str, n: usize) -> Result<Vec<String>, HistoryError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT entry_id FROM airings
             WHERE channel = ?1 AND error_card = 0 AND kind = 'programme'
             ORDER BY start_ns DESC, id DESC
             LIMIT ?2",
        )?;
        let limit = i64::try_from(n).unwrap_or(i64::MAX);
        let mut newest_first: Vec<String> = stmt
            .query_map(params![channel, limit], |row| row.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        newest_first.reverse();
        Ok(newest_first)
    }

    /// The last airing per `show_id`, across every channel (#160's query).
    ///
    /// Same greatest-`start` / later-insertion tiebreak as
    /// [`HistoryDb::series_cursor`], but partitioned by `show_id` alone
    /// instead of `(channel, show_id)` — a pool declaring `cursor_scope:
    /// global` continues a series from wherever it last aired, on any
    /// channel. Movies (`show_id IS NULL`) have no cross-channel identity to
    /// share and are excluded. Returns `show_id -> entry_id`.
    pub fn cross_channel_show_cursor(&self) -> Result<BTreeMap<String, String>, HistoryError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT show_id, entry_id FROM (
                SELECT show_id, entry_id,
                       ROW_NUMBER() OVER (
                           PARTITION BY show_id ORDER BY start_ns DESC, id DESC
                       ) AS rn
                FROM airings WHERE show_id IS NOT NULL AND kind = 'programme'
             ) WHERE rn = 1",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (show_id, entry_id) = row?;
            out.insert(show_id, entry_id);
        }
        Ok(out)
    }

    /// Drop every airing on `channel` scheduled at or after `from`.
    ///
    /// The rewind deletes the emitted chunk files from that instant forward
    /// because they are about to be regenerated; those airings are no longer
    /// scheduled, so their rows go too. Keeping them would leave the store
    /// describing a schedule that no longer exists — and, because the cursor
    /// is a projection of it, would silently skip the content that the
    /// replaced airings had claimed.
    pub fn truncate_from(&self, channel: &str, from: OffsetDateTime) -> Result<(), HistoryError> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM airings WHERE channel = ?1 AND start_ns >= ?2",
            params![channel, to_ns(from)],
        )?;
        Ok(())
    }

    /// Rows matching `filter`, in start order, at most `filter.limit` of them
    /// (`limit == 0` returns none). Scheduled rows are included: the ledger
    /// holds every row until a rewind deletes it.
    pub fn airings(&self, filter: &AiringFilter) -> Result<Vec<LedgerRow>, HistoryError> {
        let mut clauses: Vec<&str> = Vec::new();
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        let mut text = |clause: &'static str, v: &Option<String>| {
            if let Some(v) = v {
                clauses.push(clause);
                args.push(v.clone().into());
            }
        };
        text("channel = ?", &filter.channel);
        text("kind = ?", &filter.kind.map(|k| k.as_str().to_string()));
        text("entry_id = ?", &filter.entry_id);
        text("show_id = ?", &filter.show_id);
        text("source = ?", &filter.source);
        text("produced_by = ?", &filter.produced_by);
        if let Some(from) = filter.from {
            clauses.push("start_ns >= ?");
            args.push(to_ns(from).into());
        }
        if let Some(to) = filter.to {
            clauses.push("start_ns < ?");
            args.push(to_ns(to).into());
        }
        let where_sql = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        let order = if filter.newest_first { "DESC" } else { "ASC" };
        args.push(i64::try_from(filter.limit).unwrap_or(i64::MAX).into());
        let sql = format!(
            "SELECT channel, entry_id, show_id, start_ns, played_at_ns, error_card,
                    kind, source, in_ms, out_ms, produced_by
             FROM airings {where_sql}
             ORDER BY start_ns {order}, id {order}
             LIMIT ?"
        );
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<i64>>(8)?,
                row.get::<_, Option<i64>>(9)?,
                row.get::<_, Option<String>>(10)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (
                channel,
                entry_id,
                show_id,
                start,
                played_at,
                err,
                kind,
                source,
                in_ms,
                out_ms,
                by,
            ) = row?;
            out.push(LedgerRow {
                channel,
                record: PlayRecord {
                    entry_id,
                    show_id,
                    start: from_ns(start)?,
                    played_at: from_ns(played_at)?,
                    error_card: err != 0,
                    kind: AiringKind::parse(&kind).ok_or(HistoryError::CorruptKind { kind })?,
                    source,
                    in_ms: in_ms.and_then(|v| u64::try_from(v).ok()),
                    out_ms: out_ms.and_then(|v| u64::try_from(v).ok()),
                    produced_by: by,
                },
            });
        }
        Ok(out)
    }

    /// How many airings are on record for `channel`.
    pub fn count(&self, channel: &str) -> Result<usize, HistoryError> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM airings WHERE channel = ?1",
            params![channel],
            |row| row.get(0),
        )?;
        Ok(n as usize)
    }
}

fn insert_record(conn: &Connection, channel: &str, r: &PlayRecord) -> Result<(), HistoryError> {
    conn.execute(
        "INSERT INTO airings (channel, entry_id, show_id, start_ns, played_at_ns, error_card,
                              kind, source, in_ms, out_ms, produced_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            channel,
            r.entry_id,
            r.show_id,
            to_ns(r.start),
            to_ns(r.played_at),
            r.error_card as i64,
            r.kind.as_str(),
            r.source,
            r.in_ms.map(|v| v as i64),
            r.out_ms.map(|v| v as i64),
            r.produced_by,
        ],
    )?;
    Ok(())
}

fn migrated_key(channel: &str) -> String {
    format!("migrated:{channel}")
}

fn channel_migrated(conn: &Connection, channel: &str) -> Result<bool, HistoryError> {
    let found: Option<String> = conn
        .query_row(
            "SELECT value FROM history_meta WHERE key = ?1",
            params![migrated_key(channel)],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

fn mark_migrated(conn: &Connection, channel: &str) -> Result<(), HistoryError> {
    conn.execute(
        "INSERT INTO history_meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![
            migrated_key(channel),
            OffsetDateTime::now_utc().unix_timestamp().to_string()
        ],
    )?;
    Ok(())
}

fn sidecar_path(output_folder: &Path) -> PathBuf {
    output_folder.join(SIDECAR_NAME)
}

/// The index of the first record whose `start` precedes its predecessor's, if
/// any. Used only to decide whether a migrated file is worth warning about.
fn first_out_of_order(records: &[PlayRecord]) -> Option<usize> {
    records
        .windows(2)
        .position(|w| w[1].start < w[0].start)
        .map(|i| i + 1)
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
            error_card: false,
            ..Default::default()
        }
    }

    fn movie(entry_id: &str, hour: i64) -> PlayRecord {
        PlayRecord {
            entry_id: entry_id.into(),
            show_id: None,
            start: at(hour),
            played_at: at(0),
            error_card: false,
            ..Default::default()
        }
    }

    #[test]
    fn series_key_falls_back_to_the_entry_for_a_movie() {
        assert_eq!(ep("got-e1", "show:got", 0).series_key(), "show:got");
        assert_eq!(movie("mov-dune", 0).series_key(), "mov-dune");
    }

    #[test]
    fn cursor_projects_the_last_airing_of_each_series() {
        let db = HistoryDb::open_in_memory().unwrap();
        db.record(
            "ch1",
            &[
                ep("got-e1", "show:got", 0),
                ep("got-e2", "show:got", 1),
                ep("inv-e1", "show:inv", 2),
                movie("mov-dune", 3),
            ],
        )
        .unwrap();
        let cursor = db.series_cursor("ch1").unwrap();
        assert_eq!(cursor.get("show:got").unwrap(), "got-e2");
        assert_eq!(cursor.get("show:inv").unwrap(), "inv-e1");
        assert_eq!(cursor.get("mov-dune").unwrap(), "mov-dune");
        assert_eq!(cursor.len(), 3);
    }

    #[test]
    fn an_empty_ledger_projects_an_empty_cursor() {
        let db = HistoryDb::open_in_memory().unwrap();
        assert!(db.series_cursor("ch1").unwrap().is_empty());
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

    /// The same airings, inserted in an order no generation would produce — a
    /// backfilled gap, a merge of two ledgers, a manual insertion.
    fn shuffled(records: &[PlayRecord]) -> Vec<PlayRecord> {
        let order = [3usize, 0, 5, 2, 4, 1];
        order.iter().map(|&i| records[i].clone()).collect()
    }

    #[test]
    fn the_cursor_reads_the_same_whatever_order_the_records_were_inserted_in() {
        let in_order = HistoryDb::open_in_memory().unwrap();
        in_order.record("ch1", &a_days_airings()).unwrap();
        let out_of_order = HistoryDb::open_in_memory().unwrap();
        out_of_order
            .record("ch1", &shuffled(&a_days_airings()))
            .unwrap();
        assert_eq!(
            out_of_order.series_cursor("ch1").unwrap(),
            in_order.series_cursor("ch1").unwrap()
        );
        assert_eq!(
            in_order
                .series_cursor("ch1")
                .unwrap()
                .get("show:got")
                .unwrap(),
            "got-e3"
        );
    }

    #[test]
    fn the_seam_tail_reads_the_same_whatever_order_the_records_were_inserted_in() {
        let in_order = HistoryDb::open_in_memory().unwrap();
        in_order.record("ch1", &a_days_airings()).unwrap();
        let out_of_order = HistoryDb::open_in_memory().unwrap();
        out_of_order
            .record("ch1", &shuffled(&a_days_airings()))
            .unwrap();
        for n in 0..=7 {
            assert_eq!(
                out_of_order.tail("ch1", n).unwrap(),
                in_order.tail("ch1", n).unwrap(),
                "tail({n})"
            );
        }
        assert_eq!(in_order.tail("ch1", 2).unwrap(), vec!["inv-e2", "got-e3"]);
    }

    #[test]
    fn two_airings_at_one_instant_resolve_to_the_later_record() {
        // Nothing schedules two things at the same start, but a merge of two
        // ledgers can. Insertion order is the only tiebreak left, and it is
        // the one that matches what an in-order ledger does today.
        let db = HistoryDb::open_in_memory().unwrap();
        db.record(
            "ch1",
            &[ep("got-e1", "show:got", 0), ep("got-e2", "show:got", 0)],
        )
        .unwrap();
        assert_eq!(
            db.series_cursor("ch1").unwrap().get("show:got").unwrap(),
            "got-e2"
        );
        assert_eq!(db.tail("ch1", 1).unwrap(), vec!["got-e2"]);
    }

    #[test]
    fn error_card_airings_count_for_the_cursor_but_not_the_tail() {
        let db = HistoryDb::open_in_memory().unwrap();
        db.record(
            "ch1",
            &[
                ep("got-e1", "show:got", 0),
                PlayRecord {
                    entry_id: "got-e2".into(),
                    show_id: Some("show:got".into()),
                    start: at(1),
                    played_at: at(0),
                    error_card: true,
                    ..Default::default()
                },
            ],
        )
        .unwrap();
        // The series still advanced past the broken slot.
        assert_eq!(
            db.series_cursor("ch1").unwrap().get("show:got").unwrap(),
            "got-e2"
        );
        // Nobody saw it, so it is not in what the scorer/seam consider "aired".
        assert_eq!(db.tail("ch1", 2).unwrap(), vec!["got-e1"]);
    }

    #[test]
    fn truncation_drops_airings_at_or_after_the_instant() {
        let db = HistoryDb::open_in_memory().unwrap();
        db.record(
            "ch1",
            &[
                ep("got-e1", "show:got", 0),
                ep("got-e2", "show:got", 6),
                ep("got-e3", "show:got", 12),
            ],
        )
        .unwrap();
        db.truncate_from("ch1", at(6)).unwrap();
        assert_eq!(db.count("ch1").unwrap(), 1);
        // The cursor follows the truncation — this is what stops a regenerated
        // span from skipping the content the replaced airings had claimed.
        assert_eq!(
            db.series_cursor("ch1").unwrap().get("show:got").unwrap(),
            "got-e1"
        );
    }

    #[test]
    fn truncation_is_scoped_to_its_own_channel() {
        let db = HistoryDb::open_in_memory().unwrap();
        db.record("ch1", &[ep("got-e1", "show:got", 0)]).unwrap();
        db.record("ch2", &[ep("got-e1", "show:got", 0)]).unwrap();
        db.truncate_from("ch1", at(0)).unwrap();
        assert_eq!(db.count("ch1").unwrap(), 0);
        assert_eq!(db.count("ch2").unwrap(), 1, "channel 2's row must survive");
    }

    #[test]
    fn the_resume_cursor_is_scoped_to_its_own_channel() {
        let db = HistoryDb::open_in_memory().unwrap();
        db.record("ch1", &[ep("got-e1", "show:got", 0)]).unwrap();
        db.record("ch2", &[ep("got-e5", "show:got", 5)]).unwrap();
        assert_eq!(
            db.series_cursor("ch1").unwrap().get("show:got").unwrap(),
            "got-e1"
        );
        assert_eq!(
            db.series_cursor("ch2").unwrap().get("show:got").unwrap(),
            "got-e5"
        );
    }

    /// #111 acceptance: the query #160 needs and no per-channel file could
    /// serve — the last airing per `show_id` over every channel, not just one.
    #[test]
    fn cross_channel_cursor_finds_the_latest_airing_of_a_show_wherever_it_aired() {
        let db = HistoryDb::open_in_memory().unwrap();
        db.record(
            "ch1",
            &[ep("got-e1", "show:got", 0), ep("got-e2", "show:got", 1)],
        )
        .unwrap();
        // The same show airs on a second channel, later.
        db.record("ch2", &[ep("got-e5", "show:got", 10)]).unwrap();
        db.record("ch1", &[movie("mov-dune", 20)]).unwrap();

        let cursor = db.cross_channel_show_cursor().unwrap();
        assert_eq!(cursor.get("show:got").unwrap(), "got-e5");
        // Movies have no show_id and no cross-channel identity to share.
        assert!(!cursor.contains_key("mov-dune"));
    }

    #[test]
    fn cross_channel_cursor_breaks_a_start_tie_by_insertion_order() {
        let db = HistoryDb::open_in_memory().unwrap();
        db.record("ch1", &[ep("got-e1", "show:got", 0)]).unwrap();
        db.record("ch2", &[ep("got-e2", "show:got", 0)]).unwrap();
        assert_eq!(
            db.cross_channel_show_cursor()
                .unwrap()
                .get("show:got")
                .unwrap(),
            "got-e2"
        );
    }

    #[tokio::test]
    async fn migrating_an_absent_sidecar_inserts_nothing_and_still_marks_done() {
        let dir = tempdir().unwrap();
        let db = HistoryDb::open_in_memory().unwrap();
        let stats = db.migrate_channel("ch1", dir.path()).await.unwrap();
        assert!(!stats.already_done);
        assert_eq!(stats.migrated, 0);
        assert_eq!(stats.skipped, 0);
        assert_eq!(db.count("ch1").unwrap(), 0);

        // A second call finds the marker and does not re-read the (still
        // absent) file.
        let second = db.migrate_channel("ch1", dir.path()).await.unwrap();
        assert!(second.already_done);
    }

    #[tokio::test]
    async fn migrating_a_jsonl_sidecar_inserts_its_rows_exactly_once() {
        let dir = tempdir().unwrap();
        let path = sidecar_path(dir.path());
        let mut body = String::new();
        for r in a_days_airings() {
            body.push_str(&serde_json::to_string(&r).unwrap());
            body.push('\n');
        }
        tokio::fs::write(&path, body).await.unwrap();

        let db = HistoryDb::open_in_memory().unwrap();
        let stats = db.migrate_channel("ch1", dir.path()).await.unwrap();
        assert!(!stats.already_done);
        assert_eq!(stats.migrated, 6);
        assert_eq!(stats.skipped, 0);
        assert_eq!(db.count("ch1").unwrap(), 6);
        assert_eq!(
            db.series_cursor("ch1").unwrap().get("show:got").unwrap(),
            "got-e3"
        );

        // The file is left in place, and a second startup does not re-insert
        // its rows.
        assert!(tokio::fs::try_exists(&path).await.unwrap());
        let second = db.migrate_channel("ch1", dir.path()).await.unwrap();
        assert!(second.already_done);
        assert_eq!(db.count("ch1").unwrap(), 6, "rows must not be doubled");
    }

    #[tokio::test]
    async fn migration_is_scoped_per_channel() {
        let ch1_dir = tempdir().unwrap();
        let ch2_dir = tempdir().unwrap();
        tokio::fs::write(
            sidecar_path(ch1_dir.path()),
            format!(
                "{}\n",
                serde_json::to_string(&ep("got-e1", "show:got", 0)).unwrap()
            ),
        )
        .await
        .unwrap();
        tokio::fs::write(
            sidecar_path(ch2_dir.path()),
            format!(
                "{}\n",
                serde_json::to_string(&ep("got-e9", "show:got", 9)).unwrap()
            ),
        )
        .await
        .unwrap();

        let db = HistoryDb::open_in_memory().unwrap();
        db.migrate_channel("ch1", ch1_dir.path()).await.unwrap();
        db.migrate_channel("ch2", ch2_dir.path()).await.unwrap();

        assert_eq!(db.count("ch1").unwrap(), 1);
        assert_eq!(db.count("ch2").unwrap(), 1);
        assert_eq!(
            db.series_cursor("ch1").unwrap().get("show:got").unwrap(),
            "got-e1"
        );
        assert_eq!(
            db.series_cursor("ch2").unwrap().get("show:got").unwrap(),
            "got-e9"
        );
    }

    #[tokio::test]
    async fn a_torn_line_is_skipped_rather_than_failing_migration() {
        let dir = tempdir().unwrap();
        let path = sidecar_path(dir.path());
        let mut body = serde_json::to_string(&ep("got-e1", "show:got", 0)).unwrap();
        body.push('\n');
        body.push_str(r#"{"entry_id":"got-e2","sta"#);
        tokio::fs::write(&path, body).await.unwrap();

        let db = HistoryDb::open_in_memory().unwrap();
        let stats = db.migrate_channel("ch1", dir.path()).await.unwrap();
        assert_eq!(stats.migrated, 1);
        assert_eq!(stats.skipped, 1);
    }

    #[test]
    fn out_of_order_detection_reports_the_first_offending_index() {
        assert_eq!(first_out_of_order(&a_days_airings()), None);
        assert_eq!(first_out_of_order(&shuffled(&a_days_airings())), Some(1));
        assert_eq!(first_out_of_order(&[]), None);
    }

    #[test]
    fn opening_a_history_db_on_disk_creates_its_parent_directory() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("history.db");
        let db = HistoryDb::open(&path).unwrap();
        db.record("ch1", &[ep("got-e1", "show:got", 0)]).unwrap();
        assert!(path.exists());
        drop(db);

        // Reopening the same file sees what was written.
        let reopened = HistoryDb::open(&path).unwrap();
        assert_eq!(reopened.count("ch1").unwrap(), 1);
    }

    fn bumper_row(name: &str, hour: i64) -> PlayRecord {
        PlayRecord {
            entry_id: name.into(),
            start: at(hour),
            played_at: at(0),
            kind: AiringKind::Bumper,
            source: Some(format!("/m/{name}.mkv")),
            in_ms: Some(500),
            out_ms: Some(2_500),
            produced_by: Some("tease.rhai".into()),
            ..Default::default()
        }
    }

    #[test]
    fn bumper_rows_never_move_a_cursor_or_the_tail() {
        let db = HistoryDb::open_in_memory().unwrap();
        db.record(
            "ch1",
            &[
                ep("s1e1", "show-a", 1),
                bumper_row("tease", 2),
                ep("s1e2", "show-a", 3),
            ],
        )
        .unwrap();
        // A bumper after the last programme must not become "last played".
        db.record("ch1", &[bumper_row("late", 9)]).unwrap();

        assert_eq!(db.tail("ch1", 5).unwrap(), vec!["s1e1", "s1e2"]);
        assert_eq!(db.series_cursor("ch1").unwrap().len(), 1);
        assert_eq!(db.series_cursor("ch1").unwrap()["show-a"], "s1e2");
        assert_eq!(db.cross_channel_show_cursor().unwrap().len(), 1);
    }

    #[test]
    fn airings_filters_and_round_trips_bumper_columns() {
        let db = HistoryDb::open_in_memory().unwrap();
        db.record("ch1", &[ep("s1e1", "show-a", 1), bumper_row("tease", 2)])
            .unwrap();
        db.record("ch2", &[bumper_row("other", 2)]).unwrap();

        let rows = db
            .airings(&AiringFilter {
                channel: Some("ch1".into()),
                kind: Some(AiringKind::Bumper),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].record, bumper_row("tease", 2));

        let everywhere = db
            .airings(&AiringFilter {
                produced_by: Some("tease.rhai".into()),
                newest_first: true,
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(everywhere.len(), 2);

        let windowed = db
            .airings(&AiringFilter {
                channel: Some("ch1".into()),
                from: Some(at(2)),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            windowed.len(),
            1,
            "from is inclusive, so the hour-1 row is out"
        );
    }

    #[test]
    fn a_rewind_deletes_future_bumper_rows_with_the_schedule() {
        let db = HistoryDb::open_in_memory().unwrap();
        db.record(
            "ch1",
            &[
                ep("s1e1", "show-a", 1),
                bumper_row("early", 2),
                bumper_row("future", 6),
            ],
        )
        .unwrap();
        db.truncate_from("ch1", at(4)).unwrap();
        let left: Vec<String> = db
            .airings(&AiringFilter {
                channel: Some("ch1".into()),
                limit: 10,
                ..Default::default()
            })
            .unwrap()
            .into_iter()
            .map(|r| r.record.entry_id)
            .collect();
        assert_eq!(left, vec!["s1e1", "early"]);
    }

    #[test]
    fn a_v1_database_upgrades_with_its_rows_as_programmes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("history.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_version (version INTEGER NOT NULL);
                 INSERT INTO schema_version VALUES (1);
                 INSERT INTO airings (channel, entry_id, show_id, start_ns, played_at_ns, error_card)
                 VALUES ('ch1', 'old', 'show-a', 1000, 500, 0);",
            )
            .unwrap();
        }
        let db = HistoryDb::open(&path).unwrap();
        let rows = db
            .airings(&AiringFilter {
                channel: Some("ch1".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].record.entry_id, "old");
        assert_eq!(rows[0].record.kind, AiringKind::Programme);
        assert_eq!(rows[0].record.source, None);
        assert_eq!(db.tail("ch1", 5).unwrap(), vec!["old"]);
    }
}

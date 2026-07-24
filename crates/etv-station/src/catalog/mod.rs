//! Unified sqlite-backed catalog (#47).
//!
//! This module owns the durable store — the schema, the typed model, and the
//! deterministic `entry_id` derivation — that every query-based channel
//! resolves against. It is the persistence layer only: the two ingesters that
//! *populate* it (Plex API, local filesystem) are separate units that call this
//! API, and the query/order engines (#68/#69) read through it.
//!
//! ```
//! use etv_station::catalog::{Catalog, Entry, Source};
//!
//! let cat = Catalog::open_in_memory().unwrap();
//! cat.upsert_entry(&Entry::new("imdb:tt0076759", "movie", "Star Wars", Source::Plex))
//!     .unwrap();
//! assert!(cat.entry("imdb:tt0076759").unwrap().is_some());
//! ```

pub mod error;
pub mod identity;
pub mod ingest;
pub mod model;
pub mod order;
pub mod query;
pub mod schema;

use std::path::Path;

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params};

use crate::config::Order;

pub use error::CatalogError;
pub use identity::{canonical_path, derive_entry_id};
pub use model::{Collection, Entry, EntrySource, ExternalNs, Source, TagNs};

/// `catalog_meta` key holding the unix-seconds timestamp of the last completed
/// Plex ingest.
const META_LAST_PLEX_INGEST: &str = "last_plex_ingest";

/// A handle to the catalog database.
pub struct Catalog {
    conn: Connection,
}

impl Catalog {
    /// Open (creating if absent) the catalog at `path`, applying pending
    /// migrations. Enables foreign-key enforcement and WAL mode.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CatalogError> {
        let path = path.as_ref();
        let conn = Connection::open(path).map_err(|source| CatalogError::Open {
            path: path.to_path_buf(),
            source,
        })?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    /// An in-memory catalog — used by tests and ephemeral tooling.
    pub fn open_in_memory() -> Result<Self, CatalogError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, CatalogError> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        query::register_regexp(&conn)?;
        schema::apply(&conn)?;
        Ok(Catalog { conn })
    }

    /// Order a resolved set of `entry_id`s per an [`Order`] spec (#69).
    ///
    /// - `Fields` sorts by the named scalar columns (nulls last, `entry_id`
    ///   tiebreaker) — a non-sortable field is a config error.
    /// - `Manual` returns the input (authored) order unchanged.
    /// - `Random` is a deterministic seeded shuffle (`seed` supplied by the
    ///   caller; a pinned seed reproduces the order).
    ///
    /// Every case here is a function of the ids themselves — that is the whole
    /// contract. Two orders that were not are deliberately absent: collection
    /// order depends on *which* collection the set came from, so it is read at
    /// the entry that names it ([`Self::collection_members`], #107); score
    /// order depended on a plugin nothing here can reach (#108).
    ///
    /// An empty input yields an empty output.
    pub fn resolve_order(
        &self,
        ids: &[String],
        order: &Order,
        seed: u64,
    ) -> Result<Vec<String>, CatalogError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        match order {
            Order::Manual => Ok(ids.to_vec()),
            Order::Random => {
                let mut shuffled = ids.to_vec();
                // Sort first so the result depends only on the set + seed.
                shuffled.sort();
                order::seeded_shuffle(&mut shuffled, seed);
                Ok(shuffled)
            }
            Order::Fields(terms) => {
                let clause = order::order_by_clause(terms)?;
                let placeholders = vec!["?"; ids.len()].join(", ");
                let sql = format!(
                    "SELECT entry_id FROM entries WHERE entry_id IN ({placeholders}) ORDER BY {clause}"
                );
                let params: Vec<Value> = ids.iter().map(|s| Value::Text(s.clone())).collect();
                self.ordered_ids(&sql, params)
            }
        }
    }

    /// Run an ordering query and collect the `entry_id` column.
    fn ordered_ids(&self, sql: &str, params: Vec<Value>) -> Result<Vec<String>, CatalogError> {
        let mut stmt = self.conn.prepare(sql)?;
        let ids = stmt
            .query_map(rusqlite::params_from_iter(params), |r| {
                r.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Resolve a CEL `query` expression to the matching `entry_id`s.
    ///
    /// The expression is translated to a SQL `WHERE` over the catalog (#68);
    /// results come back in `entry_id` order (a stable, deterministic set —
    /// user-facing ordering is #69's job). An expression that matches nothing
    /// yields an empty vec, never an error.
    pub fn resolve_query(&self, cel: &str) -> Result<Vec<String>, CatalogError> {
        let where_clause = query::translate(cel)?;
        let sql = format!(
            "SELECT entry_id FROM entries WHERE {} ORDER BY entry_id",
            where_clause.sql
        );
        self.ordered_ids(&sql, where_clause.params)
    }

    // ---- writes -----------------------------------------------------------

    /// Run `f` inside a single transaction: commit on `Ok`, roll back on `Err`
    /// (the `Transaction` rolls back when dropped without a commit). Ingesters
    /// wrap their write pass in this so a mid-pass failure leaves the catalog
    /// untouched rather than partially written — a truncated catalog would make
    /// query channels silently emit an incomplete set.
    pub(crate) fn in_transaction<T, E, F>(&self, f: F) -> Result<T, E>
    where
        F: FnOnce(&Self) -> Result<T, E>,
        E: From<CatalogError>,
    {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(CatalogError::from)?;
        let out = f(self)?;
        tx.commit().map_err(CatalogError::from)?;
        Ok(out)
    }

    /// Insert or replace a logical entry by `entry_id`.
    pub fn upsert_entry(&self, e: &Entry) -> Result<(), CatalogError> {
        self.conn.execute(
            "INSERT INTO entries (
                entry_id, type, title, title_sort, show, show_id, season, episode,
                absolute_episode, edition, studio, year, release_date, duration_ms,
                content_rating, primary_source, raw_metadata
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17
            )
            ON CONFLICT(entry_id) DO UPDATE SET
                type=excluded.type, title=excluded.title, title_sort=excluded.title_sort,
                show=excluded.show, show_id=excluded.show_id, season=excluded.season,
                episode=excluded.episode, absolute_episode=excluded.absolute_episode,
                edition=excluded.edition, studio=excluded.studio, year=excluded.year,
                release_date=excluded.release_date, duration_ms=excluded.duration_ms,
                content_rating=excluded.content_rating, primary_source=excluded.primary_source,
                raw_metadata=excluded.raw_metadata",
            params![
                e.entry_id,
                e.kind,
                e.title,
                e.title_sort,
                e.show,
                e.show_id,
                e.season,
                e.episode,
                e.absolute_episode,
                e.edition,
                e.studio,
                e.year,
                e.release_date,
                e.duration_ms,
                e.content_rating,
                e.primary_source.as_str(),
                e.raw_metadata,
            ],
        )?;
        Ok(())
    }

    /// Attach a provenance row. Two sources on one `entry_id` = a deduped item.
    pub fn add_source(&self, s: &EntrySource) -> Result<(), CatalogError> {
        self.conn.execute(
            "INSERT INTO entry_sources (source, source_id, entry_id, playback_path, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(source, source_id) DO UPDATE SET
                entry_id=excluded.entry_id, playback_path=excluded.playback_path,
                last_seen=excluded.last_seen",
            params![
                s.source.as_str(),
                s.source_id,
                s.entry_id,
                s.playback_path,
                s.last_seen
            ],
        )?;
        Ok(())
    }

    /// Record an external GUID for an entry (also the dedup match index).
    pub fn add_external_id(
        &self,
        namespace: ExternalNs,
        value: &str,
        entry_id: &str,
    ) -> Result<(), CatalogError> {
        self.conn.execute(
            "INSERT INTO entry_external_ids (namespace, value, entry_id) VALUES (?1, ?2, ?3)
             ON CONFLICT(namespace, value) DO UPDATE SET entry_id=excluded.entry_id",
            params![namespace.as_str(), value, entry_id],
        )?;
        Ok(())
    }

    /// Add a tag. Idempotent on `(entry_id, namespace, value)`.
    pub fn add_tag(
        &self,
        entry_id: &str,
        namespace: TagNs,
        value: &str,
    ) -> Result<(), CatalogError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO tags (entry_id, namespace, value) VALUES (?1, ?2, ?3)",
            params![entry_id, namespace.as_str(), value],
        )?;
        Ok(())
    }

    /// Insert or rename a collection.
    pub fn upsert_collection(&self, c: &Collection) -> Result<(), CatalogError> {
        self.conn.execute(
            "INSERT INTO collections (collection_id, name, source) VALUES (?1, ?2, ?3)
             ON CONFLICT(collection_id) DO UPDATE SET name=excluded.name, source=excluded.source",
            params![c.collection_id, c.name, c.source.as_str()],
        )?;
        Ok(())
    }

    /// Drop every membership row for one collection.
    ///
    /// Membership is authored in Plex and re-read wholesale, so it has to be
    /// cleared before a re-ingest writes it back: `add_collection_item` only
    /// inserts and updates, which means an entry the user removed from a
    /// collection would otherwise survive in the catalog forever and keep
    /// airing. Called per collection actually fetched, so a collection skipped
    /// as unchanged keeps the rows it already has.
    pub fn clear_collection_items(&self, collection_id: &str) -> Result<(), CatalogError> {
        self.conn.execute(
            "DELETE FROM collection_items WHERE collection_id = ?1",
            params![collection_id],
        )?;
        Ok(())
    }

    /// Every collection id currently stored, ascending. Used to reconcile a full
    /// re-ingest against what Plex still returns.
    pub fn all_collection_ids(&self) -> Result<Vec<String>, CatalogError> {
        self.query_strings("SELECT collection_id FROM collections ORDER BY collection_id", [])
    }

    /// Delete a collection and, by cascade, its membership rows. For a collection
    /// that no longer exists in Plex; safe only on a full pass, where absence
    /// from the fetch means deletion rather than "unchanged".
    pub fn delete_collection(&self, collection_id: &str) -> Result<(), CatalogError> {
        self.conn.execute(
            "DELETE FROM collections WHERE collection_id = ?1",
            params![collection_id],
        )?;
        Ok(())
    }

    /// Place an entry in a collection at an authored `position`.
    pub fn add_collection_item(
        &self,
        collection_id: &str,
        entry_id: &str,
        position: i64,
    ) -> Result<(), CatalogError> {
        self.conn.execute(
            "INSERT INTO collection_items (collection_id, entry_id, position) VALUES (?1, ?2, ?3)
             ON CONFLICT(collection_id, entry_id) DO UPDATE SET position=excluded.position",
            params![collection_id, entry_id, position],
        )?;
        Ok(())
    }

    // ---- reads ------------------------------------------------------------

    /// Fetch one entry by id.
    pub fn entry(&self, entry_id: &str) -> Result<Option<Entry>, CatalogError> {
        let row = self
            .conn
            .query_row(
                &format!("SELECT {ENTRY_COLS} FROM entries WHERE entry_id = ?1"),
                params![entry_id],
                row_to_entry,
            )
            .optional()?;
        Ok(row)
    }

    /// The `entry_id` an external GUID resolves to, if any — the GUID-first half
    /// of ingest identity: every source sharing a GUID collapses onto one entry.
    pub fn entry_id_for_external_id(
        &self,
        namespace: ExternalNs,
        value: &str,
    ) -> Result<Option<String>, CatalogError> {
        let row = self
            .conn
            .query_row(
                "SELECT entry_id FROM entry_external_ids WHERE namespace = ?1 AND value = ?2",
                params![namespace.as_str(), value],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(row)
    }

    /// The `entry_id` a `(source, source_id)` provenance row resolves to, if any.
    /// Used to map a Plex `ratingKey` back to its catalog entry — e.g. resolving
    /// a collection's members to entry ids.
    pub fn entry_id_for_source(
        &self,
        source: Source,
        source_id: &str,
    ) -> Result<Option<String>, CatalogError> {
        let row = self
            .conn
            .query_row(
                "SELECT entry_id FROM entry_sources WHERE source = ?1 AND source_id = ?2",
                params![source.as_str(), source_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(row)
    }

    /// The `show_id` of each requested entry, in one round trip.
    ///
    /// The pattern engine (#72) needs nothing from an entry but its `show_id`,
    /// for every item of every pool, on every generation — and a catch-up runs
    /// many generations in a row. Fetching whole rows one id at a time turns
    /// that into a query per item; this is the same answer in a single
    /// statement. Ids absent from the catalog, and entries with no `show_id`,
    /// are simply missing from the returned map.
    pub fn show_ids_for(
        &self,
        entry_ids: &[String],
    ) -> Result<std::collections::HashMap<String, String>, CatalogError> {
        let mut out = std::collections::HashMap::new();
        if entry_ids.is_empty() {
            return Ok(out);
        }
        // Chunked so a large pool can't exceed sqlite's variable limit (999 by
        // default). Well under it, and still one query per ~500 items instead
        // of one per item.
        for chunk in entry_ids.chunks(500) {
            let placeholders = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT entry_id, show_id FROM entries \
                 WHERE show_id IS NOT NULL AND entry_id IN ({placeholders})"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (id, show_id) = row?;
                out.insert(id, show_id);
            }
        }
        Ok(out)
    }

    /// Every entry id, ascending — a stable enumeration for callers that scan.
    pub fn all_entry_ids(&self) -> Result<Vec<String>, CatalogError> {
        self.query_strings("SELECT entry_id FROM entries ORDER BY entry_id ASC", [])
    }

    /// Provenance rows for an entry, ordered by source then id.
    pub fn sources_for(&self, entry_id: &str) -> Result<Vec<EntrySource>, CatalogError> {
        let mut stmt = self.conn.prepare(
            "SELECT source, source_id, entry_id, playback_path, last_seen
             FROM entry_sources WHERE entry_id = ?1 ORDER BY source, source_id",
        )?;
        collect_sources(&mut stmt, params![entry_id])
    }

    /// Every provenance row across all entries, ordered by source then id — the
    /// enumeration an ingester scans to build a canonical-path → `entry_id` index
    /// for path-match inherit.
    pub fn all_sources(&self) -> Result<Vec<EntrySource>, CatalogError> {
        let mut stmt = self.conn.prepare(
            "SELECT source, source_id, entry_id, playback_path, last_seen
             FROM entry_sources ORDER BY source, source_id",
        )?;
        collect_sources(&mut stmt, [])
    }

    /// Tag values for an entry within a namespace, ascending.
    pub fn tags_for(&self, entry_id: &str, namespace: TagNs) -> Result<Vec<String>, CatalogError> {
        self.query_strings(
            "SELECT value FROM tags WHERE entry_id = ?1 AND namespace = ?2 ORDER BY value",
            params![entry_id, namespace.as_str()],
        )
    }

    /// Members of a collection in authored `position` order, `entry_id`
    /// breaking ties for a total order.
    pub fn collection_members(&self, collection_id: &str) -> Result<Vec<String>, CatalogError> {
        self.query_strings(
            "SELECT entry_id FROM collection_items WHERE collection_id = ?1
             ORDER BY position, entry_id",
            params![collection_id],
        )
    }

    /// Collection ids carrying `name`, ascending. A name is not unique — two
    /// sources can each define a "Halloween Marathon" — so this returns every
    /// match and leaves "missing" and "ambiguous" for the caller to phrase.
    pub fn collection_ids_by_name(&self, name: &str) -> Result<Vec<String>, CatalogError> {
        self.query_strings(
            "SELECT collection_id FROM collections WHERE name = ?1 ORDER BY collection_id",
            params![name],
        )
    }

    /// Read one `catalog_meta` value, `None` when the key was never written.
    pub fn meta(&self, key: &str) -> Result<Option<String>, CatalogError> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM catalog_meta WHERE key = ?1")?;
        let mut rows = stmt.query(params![key])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    /// Write one `catalog_meta` value, replacing any previous one.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<(), CatalogError> {
        self.conn.execute(
            "INSERT INTO catalog_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Unix seconds of the last completed Plex ingest, `None` if none has ever
    /// finished. Written only after a pass commits, so an ingest that failed
    /// half-way leaves the previous timestamp in place and the next start
    /// re-fetches the same span rather than skipping over the gap.
    pub fn last_plex_ingest(&self) -> Result<Option<i64>, CatalogError> {
        Ok(self
            .meta(META_LAST_PLEX_INGEST)?
            .and_then(|s| s.parse::<i64>().ok()))
    }

    /// Record that a Plex ingest completed at `unix_secs`.
    pub fn set_last_plex_ingest(&self, unix_secs: i64) -> Result<(), CatalogError> {
        self.set_meta(META_LAST_PLEX_INGEST, &unix_secs.to_string())
    }

    /// Run a query whose rows are a single TEXT column, collecting them in order.
    fn query_strings(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Vec<String>, CatalogError> {
        let mut stmt = self.conn.prepare(sql)?;
        let out = stmt
            .query_map(params, |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(out)
    }
}

/// Run a prepared `entry_sources` query (columns in the canonical `source,
/// source_id, entry_id, playback_path, last_seen` order) and map the result set
/// into typed [`EntrySource`] rows. Rows are collected before parsing so the
/// `source` discriminator's parse error can surface as a [`CatalogError`].
fn collect_sources(
    stmt: &mut rusqlite::Statement<'_>,
    params: impl rusqlite::Params,
) -> Result<Vec<EntrySource>, CatalogError> {
    let rows = stmt
        .query_map(params, |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|(source, source_id, entry_id, playback_path, last_seen)| {
            Ok(EntrySource {
                source: source.parse().map_err(|m| CatalogError::BadRow {
                    field: "source",
                    message: m,
                })?,
                source_id,
                entry_id,
                playback_path,
                last_seen,
            })
        })
        .collect()
}

/// Column list for `entries`, in the order [`row_to_entry`] reads.
const ENTRY_COLS: &str = "entry_id, type, title, title_sort, show, show_id, season, episode, \
     absolute_episode, edition, studio, year, release_date, duration_ms, content_rating, \
     primary_source, raw_metadata";

fn row_to_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    Ok(Entry {
        entry_id: r.get(0)?,
        kind: r.get(1)?,
        title: r.get(2)?,
        title_sort: r.get(3)?,
        show: r.get(4)?,
        show_id: r.get(5)?,
        season: r.get(6)?,
        episode: r.get(7)?,
        absolute_episode: r.get(8)?,
        edition: r.get(9)?,
        studio: r.get(10)?,
        year: r.get(11)?,
        release_date: r.get(12)?,
        duration_ms: r.get(13)?,
        content_rating: r.get(14)?,
        primary_source: {
            let raw: String = r.get(15)?;
            raw.parse().map_err(|_| {
                rusqlite::Error::FromSqlConversionFailure(
                    15,
                    rusqlite::types::Type::Text,
                    format!("invalid primary_source {raw:?}").into(),
                )
            })?
        },
        raw_metadata: r.get(16)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cat() -> Catalog {
        Catalog::open_in_memory().expect("open in-memory catalog")
    }

    #[test]
    fn migrations_are_idempotent_across_reopen() {
        let c = cat();
        // Applying again on the same connection is a no-op (version already set).
        schema::apply(&c.conn).unwrap();
        let version: i64 = c
            .conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, schema::SCHEMA_VERSION);
    }

    #[test]
    fn reopening_an_on_disk_catalog_applies_nothing_and_keeps_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.db");
        {
            let c = Catalog::open(&path).unwrap();
            c.upsert_entry(&Entry::new("id1", "movie", "Kept", Source::Plex))
                .unwrap();
        } // drop closes the connection

        let c = Catalog::open(&path).unwrap();
        let version: i64 = c
            .conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, schema::SCHEMA_VERSION);
        // A second open must not re-run v1 (would error on CREATE TABLE); data survives.
        assert_eq!(c.entry("id1").unwrap().unwrap().title, "Kept");
    }

    #[test]
    fn entry_round_trips_all_columns() {
        let c = cat();
        let e = Entry {
            title_sort: Some("Star Wars".into()),
            show: None,
            season: Some(4),
            episode: Some(1),
            absolute_episode: Some(1),
            edition: Some(String::new()),
            studio: Some("Lucasfilm".into()),
            year: Some(1977),
            release_date: Some("1977-05-25".into()),
            duration_ms: Some(7_320_000),
            content_rating: Some("PG".into()),
            raw_metadata: Some(r#"{"tagline":"…"}"#.into()),
            ..Entry::new("imdb:tt0076759", "movie", "Star Wars", Source::Plex)
        };
        c.upsert_entry(&e).unwrap();
        let got = c.entry("imdb:tt0076759").unwrap().unwrap();
        assert_eq!(got, e);
        assert!(c.entry("missing").unwrap().is_none());
    }

    #[test]
    fn upsert_entry_updates_in_place() {
        let c = cat();
        c.upsert_entry(&Entry::new("id1", "movie", "Old", Source::Plex))
            .unwrap();
        c.upsert_entry(&Entry::new("id1", "movie", "New", Source::Plex))
            .unwrap();
        assert_eq!(c.entry("id1").unwrap().unwrap().title, "New");
        assert_eq!(c.all_entry_ids().unwrap(), vec!["id1"]);
    }

    #[test]
    fn two_sources_on_one_entry_is_a_deduped_item() {
        let c = cat();
        c.upsert_entry(&Entry::new("imdb:tt1", "movie", "Dune", Source::Plex))
            .unwrap();
        c.add_source(&EntrySource {
            source: Source::Plex,
            source_id: "plex-42".into(),
            entry_id: "imdb:tt1".into(),
            playback_path: "/plex/dune.mkv".into(),
            last_seen: Some("2026-07-10T00:00:00Z".into()),
        })
        .unwrap();
        c.add_source(&EntrySource {
            source: Source::LocalFs,
            source_id: "fs-1".into(),
            entry_id: "imdb:tt1".into(),
            playback_path: "/Volumes/media/dune.mkv".into(),
            last_seen: None,
        })
        .unwrap();
        let sources = c.sources_for("imdb:tt1").unwrap();
        assert_eq!(sources.len(), 2);
        // Ordered by source text: "local_fs" sorts before "plex".
        assert_eq!(sources[0].source, Source::LocalFs);
        assert_eq!(sources[1].source, Source::Plex);
    }

    #[test]
    fn tags_are_namespaced_and_deduped() {
        let c = cat();
        c.upsert_entry(&Entry::new("id1", "movie", "X", Source::Plex))
            .unwrap();
        c.add_tag("id1", TagNs::Genre, "Sci-Fi").unwrap();
        c.add_tag("id1", TagNs::Genre, "Sci-Fi").unwrap(); // idempotent
        c.add_tag("id1", TagNs::Genre, "Action").unwrap();
        c.add_tag("id1", TagNs::Director, "Villeneuve").unwrap();
        assert_eq!(
            c.tags_for("id1", TagNs::Genre).unwrap(),
            vec!["Action", "Sci-Fi"]
        );
        assert_eq!(
            c.tags_for("id1", TagNs::Director).unwrap(),
            vec!["Villeneuve"]
        );
        assert!(c.tags_for("id1", TagNs::Cast).unwrap().is_empty());
    }

    #[test]
    fn collection_members_read_in_position_order() {
        let c = cat();
        for id in ["b", "a", "c"] {
            c.upsert_entry(&Entry::new(id, "movie", id.to_uppercase(), Source::Plex))
                .unwrap();
        }
        c.upsert_collection(&Collection {
            collection_id: "coll1".into(),
            name: "Marathon".into(),
            source: Source::Plex,
        })
        .unwrap();
        c.add_collection_item("coll1", "a", 2).unwrap();
        c.add_collection_item("coll1", "b", 0).unwrap();
        c.add_collection_item("coll1", "c", 1).unwrap();
        assert_eq!(c.collection_members("coll1").unwrap(), vec!["b", "c", "a"]);
    }

    #[test]
    fn deleting_entry_cascades_to_sources_tags_and_membership() {
        let c = cat();
        c.upsert_entry(&Entry::new("id1", "movie", "X", Source::Plex))
            .unwrap();
        c.add_source(&EntrySource {
            source: Source::Plex,
            source_id: "p1".into(),
            entry_id: "id1".into(),
            playback_path: "/x".into(),
            last_seen: None,
        })
        .unwrap();
        c.add_tag("id1", TagNs::Genre, "Sci-Fi").unwrap();
        c.upsert_collection(&Collection {
            collection_id: "coll1".into(),
            name: "C".into(),
            source: Source::Plex,
        })
        .unwrap();
        c.add_collection_item("coll1", "id1", 0).unwrap();

        c.conn
            .execute("DELETE FROM entries WHERE entry_id = 'id1'", [])
            .unwrap();

        assert!(c.sources_for("id1").unwrap().is_empty());
        assert!(c.tags_for("id1", TagNs::Genre).unwrap().is_empty());
        assert!(c.collection_members("coll1").unwrap().is_empty());
    }

    #[test]
    fn external_id_resolves_to_entry() {
        let c = cat();
        c.upsert_entry(&Entry::new(
            "imdb:tt1375666",
            "movie",
            "Inception",
            Source::Plex,
        ))
        .unwrap();
        c.add_external_id(ExternalNs::Imdb, "tt1375666", "imdb:tt1375666")
            .unwrap();
        c.add_external_id(ExternalNs::Tmdb, "27205", "imdb:tt1375666")
            .unwrap();
        let entry_id: String = c
            .conn
            .query_row(
                "SELECT entry_id FROM entry_external_ids WHERE namespace='tmdb' AND value='27205'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(entry_id, "imdb:tt1375666");
    }

    #[test]
    fn show_ids_for_returns_only_entries_that_have_one() {
        let c = cat();
        let mut ep = Entry::new("ep1", "episode", "Winter Is Coming", Source::Plex);
        ep.show_id = Some("show:got".into());
        c.upsert_entry(&ep).unwrap();
        let mut ep2 = Entry::new("ep2", "episode", "The Kingsroad", Source::Plex);
        ep2.show_id = Some("show:got".into());
        c.upsert_entry(&ep2).unwrap();
        // A movie has no show_id, and "missing" isn't in the catalog at all.
        c.upsert_entry(&Entry::new("mov1", "movie", "Inception", Source::Plex))
            .unwrap();

        let ids = vec![
            "ep1".to_string(),
            "ep2".to_string(),
            "mov1".to_string(),
            "missing".to_string(),
        ];
        let map = c.show_ids_for(&ids).unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("ep1").unwrap(), "show:got");
        assert_eq!(map.get("ep2").unwrap(), "show:got");
        assert!(!map.contains_key("mov1"), "a movie has no show_id");
        assert!(!map.contains_key("missing"));
    }

    #[test]
    fn show_ids_for_handles_an_empty_request_and_large_batches() {
        let c = cat();
        assert!(c.show_ids_for(&[]).unwrap().is_empty());

        // Past the 500-id chunk boundary, so the chunking loop is exercised
        // rather than assumed.
        let mut ids = Vec::new();
        for n in 0..1200 {
            let id = format!("ep{n}");
            let mut e = Entry::new(&id, "episode", format!("Episode {n}"), Source::Plex);
            e.show_id = Some(format!("show:{}", n % 3));
            c.upsert_entry(&e).unwrap();
            ids.push(id);
        }
        let map = c.show_ids_for(&ids).unwrap();
        assert_eq!(map.len(), 1200);
        assert_eq!(map.get("ep1199").unwrap(), "show:2");
    }
}

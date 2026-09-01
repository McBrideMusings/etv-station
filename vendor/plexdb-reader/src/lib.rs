//! Read-only, typed access to a `plexdb.db` store ([ADR-0003][adr-0003]).
//!
//! One process — the `plexdb` Python package in this repository — writes
//! the store; everything else, including this crate, reads it.
//! [`Reader::open`] enforces that at two levels: the connection itself is
//! opened with SQLite's own read-only flag (not a convention this crate
//! could forget to honour), and the store's schema version is checked
//! against what this crate's accessors were written for, so a mismatch
//! fails loudly at open time rather than as a missing-column error deep
//! inside a query.
//!
//! Typed accessors, not a generic query function: a schema change that
//! drops a column a consumer reads fails that consumer's **build**, not a
//! runtime string-built query.
//!
//! **No ranking policy lives here, and there is no constant to tune.**
//! [`Reader::taste_vector_for`] computes the Layer 2 rollup — each watched
//! title weighed by `sqrt(seasons watched)`, split across its attributes —
//! and nothing more. plex-db-ex#13 asked who owns the recency half-life,
//! the exploration fraction and the negative-signal weight; measured against
//! 25,835 real plays, the first two turned out not to be knobs at all (decay
//! concentrates the vector rather than mixing it; abandonment is no signal,
//! not negative signal) and the third describes how a channel is assembled
//! rather than what a person likes, so it stays with the consumer. See
//! ADR-0011.
//!
//! [`Reader::collections_for`] answers the crowd-list question the issue that
//! commissioned this crate (plex-db-ex#11) asked for: which lists a title
//! appears on, and where in them. The tables it reads — `collection` and
//! `collection_membership` — landed in schema v6, filled by
//! `plexdb harvest-mdblist` (plex-db-ex#34); the accessor is plex-db-ex#29.
//!
//! It carries no weight, and that is deliberate rather than an omission:
//! the store records `rank`, `mentions`, and the collection's own `size` and
//! `likes` as the source gave them, and a consumer wanting one number
//! computes it from those (plex-db-ex#33, ADR-0012). All four are nullable at
//! the source, so all four are `Option<_>` here — collapsing a missing `rank`
//! to `0` would make "unordered" and "ranked first" indistinguishable.
//!
//! [adr-0003]: https://github.com/McBrideMusings/plex-db-ex/blob/main/docs/adr/0003-rust-reader-crate-behind-a-plugin-capability-grant.md

mod error;
mod model;
mod schema;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rusqlite::{Connection, OpenFlags, Row};

pub use error::ReaderError;
pub use model::{CollectionMembership, Edge, EnrichmentFact, TasteAttribute, TasteVector};
pub use schema::SUPPORTED_SCHEMA_VERSION;

/// A title watched less than half a season contributes nothing. Hu, Koren &
/// Volinsky (ICDM 2008) §6, applied to seasons rather than whole programmes:
/// "watching less than half of a program is not a strong indication that a
/// user likes the program". They zero it; they do not flip it negative, and
/// neither does this (ADR-0011).
const MIN_SEASONS_WATCHED: f64 = 0.5;

/// One attribute's identity: `(namespace, key, value)`. The rollup keys on
/// this rather than carrying whole [`TasteAttribute`]s, so summing is a map
/// entry rather than a search.
type AttributeKey = (String, String, String);

/// Every title's attributes, keyed by `item_id`.
type AttributesByItem = BTreeMap<String, Vec<AttributeKey>>;

/// The exact flags every connection in this crate is opened with: read-only,
/// no implicit create, no read-write fallback. A write attempted through a
/// connection opened this way fails at SQLite's own gate, not by this
/// crate's convention — see the unit test below, which opens a connection
/// with this exact constant and proves a write against it fails.
const OPEN_FLAGS: OpenFlags = OpenFlags::SQLITE_OPEN_READ_ONLY;

/// A read-only handle on a `plexdb.db` store.
#[derive(Debug)]
pub struct Reader {
    conn: Connection,
}

impl Reader {
    /// Open the store at `path` read-only.
    ///
    /// Fails loudly — never lazily, never as a missing-column panic later —
    /// if the file is absent, is not a plexdb store, or is a schema version
    /// this build does not understand.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ReaderError> {
        let path = path.as_ref();
        let conn =
            Connection::open_with_flags(path, OPEN_FLAGS).map_err(|source| ReaderError::Open {
                path: path.to_path_buf(),
                source,
            })?;
        schema::check(&conn, path)?;
        Ok(Self { conn })
    }

    /// Every enrichment fact recorded for `item_id` under `namespace`, in
    /// `(key, value)` order. Empty, not an error, when nothing is recorded.
    pub fn enrichment_for(
        &self,
        item_id: &str,
        namespace: &str,
    ) -> Result<Vec<EnrichmentFact>, ReaderError> {
        let mut stmt = self.conn.prepare(
            "SELECT namespace, key, value, fetched_at \
             FROM enrichment \
             WHERE item_id = ?1 AND namespace = ?2 \
             ORDER BY key, value",
        )?;
        let rows = stmt
            .query_map((item_id, namespace), Self::enrichment_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every enrichment fact recorded under `namespace` for any of
    /// `item_ids`, grouped by `item_id`. Within each id's group the facts are
    /// in `(key, value)` order — for any single id, that group is
    /// byte-identical to what [`Self::enrichment_for`] returns for that id.
    /// An id with no rows in the namespace is simply absent from the result
    /// (2,373 of this store's movies have no TMDB keywords, and that is
    /// normal) — never an error, and never an empty entry a caller has to
    /// tell apart from "absent". Empty input returns an empty map without
    /// touching the database.
    ///
    /// One `SELECT ... WHERE item_id IN (...) AND namespace = ?` per chunk
    /// of ids, not one query per id: a scorer ranking a whole candidate set
    /// needs an id-keyed map built *before* its loop, and a call that
    /// degrades to one round trip per id makes that impossible to do — see
    /// plex-db-ex#40. Chunked against this connection's own
    /// `SQLITE_LIMIT_VARIABLE_NUMBER`, not a guessed constant, because a
    /// candidate set the size of this store's movie pool (12,462 ids) can
    /// exceed whatever that connection's SQLite was built with — one chunk,
    /// one query, for anything under the limit; several queries, still not
    /// one per id, above it.
    pub fn enrichment_for_many<'a>(
        &self,
        item_ids: impl IntoIterator<Item = &'a str>,
        namespace: &str,
    ) -> Result<BTreeMap<String, Vec<EnrichmentFact>>, ReaderError> {
        // Deduplicated: an id repeated in the input must not land in two
        // different chunks' `IN` lists and have its facts counted twice.
        let item_ids: Vec<&str> = item_ids
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if item_ids.is_empty() {
            return Ok(BTreeMap::new());
        }

        // One bound parameter goes to `namespace`; the rest to the `IN`
        // list, so each chunk holds one fewer id than the connection's own
        // limit.
        let max_vars = self
            .conn
            .limit(rusqlite::limits::Limit::SQLITE_LIMIT_VARIABLE_NUMBER);
        let chunk_size = usize::try_from(max_vars)
            .unwrap_or(1)
            .saturating_sub(1)
            .max(1);

        let mut by_item: BTreeMap<String, Vec<EnrichmentFact>> = BTreeMap::new();
        for chunk in item_ids.chunks(chunk_size) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            // `item_id` comes last so the first four columns are exactly the
            // ones [`Self::enrichment_row`] reads.
            let sql = format!(
                "SELECT namespace, key, value, fetched_at, item_id \
                 FROM enrichment \
                 WHERE item_id IN ({placeholders}) AND namespace = ? \
                 ORDER BY item_id, key, value"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let params =
                rusqlite::params_from_iter(chunk.iter().copied().chain(std::iter::once(namespace)));
            let rows = stmt
                .query_map(params, |row| {
                    Ok((row.get::<_, String>(4)?, Self::enrichment_row(row)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            for (item_id, fact) in rows {
                by_item.entry(item_id).or_default().push(fact);
            }
        }

        Ok(by_item)
    }

    /// Edges of `edge_type` pointing *out of* `item_id`, ranked ascending.
    pub fn edges_from(&self, item_id: &str, edge_type: &str) -> Result<Vec<Edge>, ReaderError> {
        self.query_edges(
            "SELECT from_id, to_id, edge_type, rank, fetched_at \
             FROM edges \
             WHERE from_id = ?1 AND edge_type = ?2 \
             ORDER BY rank",
            item_id,
            edge_type,
        )
    }

    /// Edges of `edge_type` pointing *into* `item_id`, ranked ascending.
    pub fn edges_to(&self, item_id: &str, edge_type: &str) -> Result<Vec<Edge>, ReaderError> {
        self.query_edges(
            "SELECT from_id, to_id, edge_type, rank, fetched_at \
             FROM edges \
             WHERE to_id = ?1 AND edge_type = ?2 \
             ORDER BY rank",
            item_id,
            edge_type,
        )
    }

    /// Shared by [`Self::edges_from`] and [`Self::edges_to`], which differ
    /// only in which column they filter — `sql` carries that difference,
    /// this carries the prepare/bind/collect boilerplate.
    fn query_edges(
        &self,
        sql: &str,
        item_id: &str,
        edge_type: &str,
    ) -> Result<Vec<Edge>, ReaderError> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map((item_id, edge_type), Self::edge_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every crowd list `item_id` appears on, ordered by `collection_id` for
    /// a stable, deterministic result — `rank` orders a title's position
    /// *within* one list, not one list against another, so it cannot supply
    /// this ordering. Empty, not an error, for a title on no lists.
    ///
    /// Each row joins one `collection_membership` fact with the `collection`
    /// it belongs to, so a caller gets a list's own name/url/size/likes
    /// alongside this title's rank/mentions in it without a second query.
    pub fn collections_for(&self, item_id: &str) -> Result<Vec<CollectionMembership>, ReaderError> {
        let mut stmt = self.conn.prepare(
            "SELECT cm.collection_id, c.source, c.name, c.url, c.size, c.likes, \
                    cm.rank, cm.mentions, cm.observed_at \
             FROM collection_membership cm \
             JOIN collection c ON c.collection_id = cm.collection_id \
             WHERE cm.item_id = ?1 \
             ORDER BY cm.collection_id",
        )?;
        let rows = stmt
            .query_map((item_id,), Self::collection_membership_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The Layer 2 rollup for one Plex account, weighted per ADR-0011.
    ///
    /// Each title the account watched contributes `sqrt(r)`, where `r` is
    /// consumption measured in seasons: `plays / median_season_length` for a
    /// show, `plays` for a film, so one full season or one film watched once
    /// is `r = 1` and a rewatch pushes past it. A title under `r = 0.5`
    /// contributes nothing — abandonment is no signal, not negative signal.
    /// That contribution is split across the title's attributes, so a
    /// heavily-tagged title cannot outvote a sparsely-tagged one.
    ///
    /// The median season length, not the mean and not the episode count the
    /// library holds: a specials season of two must not set the scale, and
    /// dividing by episodes-held would make the denominator a property of
    /// what happens to sit on disk rather than of the show.
    ///
    /// Computed in Rust rather than SQL because SQLite has no median, and
    /// `sqrt` is an optional compile-time extension there — a query that
    /// depends on how the caller's SQLite was built is exactly the runtime
    /// surprise ADR-0003 exists to avoid.
    ///
    /// Deterministic: calling this twice against unchanged data returns an
    /// identical vector, in the same order. Empty, not an error, for an
    /// account with no plays.
    pub fn taste_vector_for(&self, plex_account_id: i64) -> Result<TasteVector, ReaderError> {
        // Plays per unit: a film is its own unit, an episode belongs to its
        // show, so a season binge rolls up to the show rather than counting
        // each episode as a separate title.
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(i.show_item_id, p.item_id) AS unit, COUNT(*) AS plays \
             FROM plays p \
             JOIN items i ON i.item_id = p.item_id \
             WHERE p.plex_account_id = ?1 \
             GROUP BY unit \
             ORDER BY unit",
        )?;
        let units = stmt
            .query_map([plex_account_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        self.rollup(units)
    }

    /// Every unit one Plex account has ever played, as ids — the lifetime
    /// answer to "has this been watched", which no fixed-length history tail
    /// can give (issue #62).
    ///
    /// A unit is what [`Self::taste_vector_for`] weighs: a film is its own
    /// unit, an episode rolls up to its show. So a caller ranking films gets
    /// film ids back, and one ranking series gets show ids, with no second
    /// join on either side.
    ///
    /// **One play is the bar.** This answers "has this been played", not "was
    /// it finished" — `seconds_watched` is deliberately not read, because a
    /// caller asking whether to recommend something has no use for a rule
    /// that calls a film unwatched after two hours of it. ADR-0011's `r = 0.5`
    /// abandonment floor belongs to the taste rollup, where a weak signal must
    /// not move a weight; it is the wrong rule for a membership test.
    ///
    /// Sorted, so the result is deterministic. Empty, not an error, for an
    /// account with no plays.
    pub fn watched_units_for(&self, plex_account_id: i64) -> Result<Vec<String>, ReaderError> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT COALESCE(i.show_item_id, p.item_id) AS unit \
             FROM plays p \
             JOIN items i ON i.item_id = p.item_id \
             WHERE p.plex_account_id = ?1 \
             ORDER BY unit",
        )?;
        let units = stmt
            .query_map([plex_account_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(units)
    }

    /// The same, pooled across every account — what the *house* has seen,
    /// rather than one person. Same unit rollup and the same one-play bar as
    /// [`Self::watched_units_for`].
    pub fn watched_units(&self) -> Result<Vec<String>, ReaderError> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT COALESCE(i.show_item_id, p.item_id) AS unit \
             FROM plays p \
             JOIN items i ON i.item_id = p.item_id \
             ORDER BY unit",
        )?;
        let units = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(units)
    }

    /// The Layer 2 rollup pooled across every account, weighted per
    /// ADR-0011 exactly as [`Self::taste_vector_for`] weighs one account.
    ///
    /// Pooling is summation, not an approximation of it: an account that
    /// watched more contributes more `plays` to a unit's count before `r` is
    /// computed, which is what "the house has been watching cooking shows"
    /// means. There is no per-account normalisation and no account list in
    /// the result — a consumer that needs the *house's* taste, not any one
    /// account's, is the reason this exists (plex-db-ex#39).
    ///
    /// For a store with exactly one account's plays, this returns the same
    /// vector as [`Self::taste_vector_for`] on that account, because the
    /// per-unit play counts are identical.
    pub fn pooled_taste_vector(&self) -> Result<TasteVector, ReaderError> {
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(i.show_item_id, p.item_id) AS unit, COUNT(*) AS plays \
             FROM plays p \
             JOIN items i ON i.item_id = p.item_id \
             GROUP BY unit \
             ORDER BY unit",
        )?;
        let units = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        self.rollup(units)
    }

    /// The rollup shared by [`Self::taste_vector_for`] and
    /// [`Self::pooled_taste_vector`], which differ only in which plays feed
    /// `units` — one account's, or every account's summed together. Every
    /// ADR-0011 rule (season unit, `sqrt` damping, the 0.5 floor, per-title
    /// attribute division) lives here exactly once, so one caller changing
    /// it and not the other is not possible.
    ///
    /// `units` is `(item_id, play_count)` pairs, one row per watched title.
    ///
    /// Deterministic: calling either public accessor twice against
    /// unchanged data returns an identical vector, in the same order. Empty,
    /// not an error, when `units` is empty.
    fn rollup(&self, units: Vec<(String, i64)>) -> Result<TasteVector, ReaderError> {
        let seasons = self.median_season_lengths()?;

        // Three queries for the whole rollup, not two per watched title. The
        // obvious shape — ask per unit inside the loop — costs a fresh
        // statement compile and round-trip per title, which is invisible at
        // the 99 titles one account has today and is thousands of them on a
        // library that has been running for years.
        let shows = self.show_ids()?;
        let attributes_by_item = self.attributes_by_item()?;

        let mut totals: BTreeMap<AttributeKey, f64> = BTreeMap::new();
        let mut shows_without_seasons: Vec<String> = Vec::new();

        for (unit, plays) in units {
            let r = match seasons.get(&unit) {
                Some(length) => plays as f64 / *length as f64,
                None => {
                    // Only a show can be missing a season length. A film has
                    // none by nature and is one whole thing, so `plays` is
                    // already its `r`; a show landing here is one Plex files
                    // with no season numbers, and that is worth naming.
                    if shows.contains(&unit) {
                        shows_without_seasons.push(unit.clone());
                    }
                    plays as f64
                }
            };
            if r < MIN_SEASONS_WATCHED {
                continue;
            }
            let Some(attributes) = attributes_by_item.get(&unit) else {
                continue;
            };
            let share = r.sqrt() / attributes.len() as f64;
            for key in attributes {
                *totals.entry(key.clone()).or_insert(0.0) += share;
            }
        }

        // BTreeMap already orders by (namespace, key, value), which is the
        // ordering the determinism guarantee above promises.
        let attributes = totals
            .into_iter()
            .map(|((namespace, key, value), weight)| TasteAttribute {
                namespace,
                key,
                value,
                weight,
            })
            .collect();
        shows_without_seasons.sort();
        Ok(TasteVector {
            attributes,
            shows_without_seasons,
        })
    }

    /// Median episodes per season, per show. Shows Plex files with no season
    /// number are absent — the caller decides what that means.
    fn median_season_lengths(&self) -> Result<BTreeMap<String, i64>, ReaderError> {
        let mut stmt = self.conn.prepare(
            "SELECT show_item_id, COUNT(*) AS episodes \
             FROM items \
             WHERE type = 'episode' AND show_item_id IS NOT NULL AND season IS NOT NULL \
             GROUP BY show_item_id, season \
             ORDER BY show_item_id, episodes",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut by_show: BTreeMap<String, Vec<i64>> = BTreeMap::new();
        for (show, episodes) in rows {
            by_show.entry(show).or_default().push(episodes);
        }
        Ok(by_show
            .into_iter()
            .map(|(show, lengths)| {
                // Already ascending from the ORDER BY above.
                let median = lengths[lengths.len() / 2];
                (show, median)
            })
            .collect())
    }

    /// Every `item_id` the store calls a show. Read once per rollup so the
    /// per-unit check is a set lookup rather than a query.
    fn show_ids(&self) -> Result<BTreeSet<String>, ReaderError> {
        let mut stmt = self
            .conn
            .prepare("SELECT item_id FROM items WHERE type = 'show'")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<BTreeSet<_>, _>>()?;
        Ok(rows)
    }

    /// Every title's attributes, keyed by `item_id`.
    ///
    /// No filter, deliberately. `enrichment` holds facts about titles and
    /// nothing else since schema v7 — writers record their own progress in
    /// `enrichment_cursor`, a table this rollup never reads (ADR-0013).
    ///
    /// This used to say `WHERE key NOT LIKE '\_%' ESCAPE '\'`, and that line
    /// was the only thing standing between the house's taste profile and a
    /// top attribute of the string `1`. A rule known in exactly one query is
    /// a rule the next accessor gets wrong; the filter is gone rather than
    /// kept alongside the fix, so a sentinel written back into `enrichment`
    /// shows up loudly instead of being silently swallowed here.
    ///
    /// One scan rather than a query per watched title. Enrichment is the
    /// biggest table a rollup touches, so this is the one place worth
    /// measuring if a store ever grows past what a scan can hold.
    fn attributes_by_item(&self) -> Result<AttributesByItem, ReaderError> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT item_id, namespace, key, value FROM enrichment \
             ORDER BY item_id, namespace, key, value",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (row.get(1)?, row.get(2)?, row.get(3)?),
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut by_item: AttributesByItem = BTreeMap::new();
        for (item_id, attribute) in rows {
            by_item.entry(item_id).or_default().push(attribute);
        }
        Ok(by_item)
    }

    fn enrichment_row(row: &Row) -> rusqlite::Result<EnrichmentFact> {
        Ok(EnrichmentFact {
            namespace: row.get(0)?,
            key: row.get(1)?,
            value: row.get(2)?,
            fetched_at: row.get(3)?,
        })
    }

    fn edge_row(row: &Row) -> rusqlite::Result<Edge> {
        Ok(Edge {
            from_id: row.get(0)?,
            to_id: row.get(1)?,
            edge_type: row.get(2)?,
            rank: row.get(3)?,
            fetched_at: row.get(4)?,
        })
    }

    fn collection_membership_row(row: &Row) -> rusqlite::Result<CollectionMembership> {
        Ok(CollectionMembership {
            collection_id: row.get(0)?,
            source: row.get(1)?,
            name: row.get(2)?,
            url: row.get(3)?,
            size: row.get(4)?,
            likes: row.get(5)?,
            rank: row.get(6)?,
            mentions: row.get(7)?,
            observed_at: row.get(8)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The acceptance test ADR-0003 and issue #11 ask for: a write attempted
    /// through a connection opened the way every `Reader` connection is
    /// opened fails at SQLite's own gate. This opens a plain file with the
    /// crate's real `OPEN_FLAGS` constant — not a hand-copied approximation
    /// of it — so drift between this test and `Reader::open` is impossible.
    #[test]
    fn the_flags_every_reader_connection_uses_reject_a_write() {
        let file = tempfile::NamedTempFile::new().expect("create a temp file");
        {
            // Set up with an ordinary read-write connection; OPEN_FLAGS
            // alone cannot create a file, by design.
            let setup = Connection::open(file.path()).expect("open for setup");
            setup
                .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY);")
                .expect("create a table to attempt writing into");
        }

        let conn = Connection::open_with_flags(file.path(), OPEN_FLAGS).expect("open read-only");
        let result = conn.execute("INSERT INTO t (id) VALUES (1)", []);

        assert!(
            result.is_err(),
            "a connection opened with OPEN_FLAGS must not accept a write"
        );
    }

    /// A naive `enrichment_for_many` — one `IN` clause holding every id plus
    /// `namespace` — would fail outright against a connection whose
    /// `SQLITE_LIMIT_VARIABLE_NUMBER` is smaller than the candidate set: 10
    /// ids plus the namespace parameter is 11 bound variables. This pins the
    /// limit to 3 (room for 2 ids per chunk) and asserts the call still
    /// succeeds and every id's facts still come back correct — proof the
    /// accessor chunks against the connection's real limit rather than
    /// assuming any set of ids fits in one statement.
    #[test]
    fn enrichment_for_many_chunks_against_a_small_variable_limit() {
        let file = tempfile::NamedTempFile::new().expect("create a temp file");
        {
            let setup = Connection::open(file.path()).expect("open for setup");
            setup
                .execute_batch(
                    "CREATE TABLE schema_version (version INTEGER NOT NULL);
                     INSERT INTO schema_version (version) VALUES (8);
                     CREATE TABLE items (item_id TEXT PRIMARY KEY);
                     CREATE TABLE enrichment (
                         item_id    TEXT NOT NULL,
                         namespace  TEXT NOT NULL,
                         key        TEXT NOT NULL,
                         value      TEXT NOT NULL,
                         fetched_at TEXT NOT NULL
                     );",
                )
                .expect("build a minimal enrichment-only schema");
            for i in 0..10 {
                setup
                    .execute(
                        "INSERT INTO enrichment (item_id, namespace, key, value, fetched_at) \
                         VALUES (?1, 'ns', 'k', ?2, 't')",
                        rusqlite::params![format!("id{i}"), format!("v{i}")],
                    )
                    .expect("seed a fact");
            }
        }

        let reader = Reader::open(file.path()).expect("open the minimal store");
        reader
            .conn
            .set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_VARIABLE_NUMBER, 3);

        let ids: Vec<String> = (0..10).map(|i| format!("id{i}")).collect();
        let result = reader
            .enrichment_for_many(ids.iter().map(String::as_str), "ns")
            .expect("a set larger than the variable limit must still succeed by chunking");

        assert_eq!(result.len(), 10, "every id must be present");
        for i in 0..10 {
            let facts = result
                .get(&format!("id{i}"))
                .unwrap_or_else(|| panic!("id{i} must be present"));
            assert_eq!(facts.len(), 1);
            assert_eq!(facts[0].value, format!("v{i}"));
        }
    }
}

//! Reconciling this catalog's `entry_id` against `plex-db-ex`'s `item_id`
//! (#269).
//!
//! Both derive identity by the same first-hit-wins rule, from two
//! independent implementations — Rust here
//! ([`crate::catalog::identity::derive_entry_id`]), Python in `plex-db-ex`
//! at `plexdb/identity.py` — kept in agreement only by a shared test
//! fixture (`tests/fixtures/entry_id.json`), pinned by a SHA-256 constant on
//! both sides. Nothing has ever compared the two derivations over a real
//! library, where the awkward inputs actually live. This module is that
//! comparison. It is a report, not a fix: whatever it finds becomes its own
//! issue.
//!
//! This check used to live in `plex-db-ex` as `plexdb reconcile-etv`,
//! opening this repository's `catalog.db` read-only from the other side.
//! That ran `plex-db-ex`'s ADR-0001 backwards — the one process that writes
//! `plexdb.db` opening a consumer's private database — and only worked when
//! both stores happened to sit on one host, reached through an
//! `ETV_CATALOG_PATH` environment variable pointing across a repository
//! boundary. This repository already reads `plex-db-ex`'s published
//! snapshot (its ADR-0007), so it already holds both halves; doing the
//! comparison here needs no new coupling.
//!
//! **The join key is the Plex `ratingKey`.** Both stores record every
//! physical Plex item's rating key as raw provenance, independent of
//! whatever identity each side derived for it: `entry_sources.source_id`
//! where `source = 'plex'` here, `plex_items.rating_key` in `plex-db-ex`.
//! Matching on it is what lets this module tell "the two stores derived
//! different ids for the same title" apart from "the two stores don't know
//! about the same titles" — and it only makes sense when both stores were
//! built by walking the *same* Plex server, which nothing in either schema
//! records, so this module cannot check it.
//!
//! **Read-only on both sides, always.** `plexdb_conn` is opened via
//! [`open_plexdb_readonly`], which uses SQLite's own read-only flag; the
//! `Catalog` this compares against is whatever the caller already opened
//! (normally [`super::Catalog::open_readonly`]). Nothing here executes a
//! write statement, so a write attempted through either connection fails at
//! the database rather than being prevented by this module's own
//! discipline.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use thiserror::Error;

use super::model::ExternalNs;
use super::{Catalog, CatalogError};

/// Recognised GUID namespaces, in derivation-priority order. Only these can
/// decide which id a title derives, so only these matter to a mismatch's
/// reason. `plex-db-ex` records every namespace Plex reports in
/// `external_ids`; this repository's Plex ingester keeps only these four
/// (`catalog::ingest::plex::parse_guid`). Restricting the comparison to
/// this set is what keeps that difference in *storage* from being
/// misreported as a difference in *derivation*.
const RECOGNISED_NAMESPACES: [ExternalNs; 4] = ExternalNs::PRIORITY;

/// How many "present in only one store" titles [`reconcile`]'s caller
/// should print per category by default. Every mismatch always belongs in
/// full — that's the finding this comparison exists to surface — but a
/// store that has simply never been reconciled can carry thousands of
/// one-sided titles, and dumping all of them is noise, not a finding.
pub const ONE_SIDED_SAMPLE_LIMIT: usize = 50;

/// One title where the two stores derived different identities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    pub rating_key: String,
    pub title: String,
    /// This catalog's derived id (`entries.entry_id`).
    pub entry_id: String,
    /// `plex-db-ex`'s derived id (`items.item_id`).
    pub item_id: String,
    pub reason: String,
}

/// What comparing every title both stores know about found.
///
/// `only_in_etv` and `only_in_plexdb` are `(rating_key, title)` pairs,
/// sorted by rating key — same ordering discipline as `mismatches`, so two
/// runs over unchanged data produce byte-identical output.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReconcileReport {
    pub compared: usize,
    pub agree: usize,
    pub mismatches: Vec<Mismatch>,
    pub only_in_etv: Vec<(String, String)>,
    pub only_in_plexdb: Vec<(String, String)>,
}

impl ReconcileReport {
    pub fn mismatched(&self) -> usize {
        self.mismatches.len()
    }
}

/// Opening a `plex-db-ex` snapshot for [`reconcile`].
#[derive(Debug, Error)]
pub enum ReconcileOpenError {
    #[error("failed to open plex-db-ex snapshot at {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("plex-db-ex snapshot at {path} has no schema_version row — not a plexdb store")]
    NotAStore { path: PathBuf },

    #[error(
        "plex-db-ex snapshot at {path} is schema version {found}, but this comparison is \
         written against version {supported} (the version the vendored plexdb-reader crate \
         also expects)"
    )]
    UnsupportedSchemaVersion {
        path: PathBuf,
        found: i64,
        supported: i64,
    },
}

/// Open a `plex-db-ex` snapshot read-only, refusing anything that isn't a
/// plexdb store at exactly the schema version this comparison (and the
/// vendored `plexdb-reader` crate) is written against — older or newer —
/// rather than let a renamed or missing column surface as a confusing query
/// failure deep inside [`reconcile`].
pub fn open_plexdb_readonly(path: impl AsRef<Path>) -> Result<Connection, ReconcileOpenError> {
    let path = path.as_ref();
    let conn =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|source| {
            ReconcileOpenError::Open {
                path: path.to_path_buf(),
                source,
            }
        })?;
    let version: Option<i64> = conn
        .query_row("SELECT version FROM schema_version LIMIT 1", [], |row| {
            row.get(0)
        })
        .optional()
        .map_err(|source| ReconcileOpenError::Open {
            path: path.to_path_buf(),
            source,
        })?;
    let version = version.ok_or_else(|| ReconcileOpenError::NotAStore {
        path: path.to_path_buf(),
    })?;
    if version != plexdb_reader::SUPPORTED_SCHEMA_VERSION {
        return Err(ReconcileOpenError::UnsupportedSchemaVersion {
            path: path.to_path_buf(),
            found: version,
            supported: plexdb_reader::SUPPORTED_SCHEMA_VERSION,
        });
    }
    Ok(conn)
}

/// Compare every title both stores know about, joined by Plex rating key.
/// `catalog` is this repository's already-open catalog; `plexdb_conn` is a
/// read-only connection onto a `plex-db-ex` snapshot, normally opened via
/// [`open_plexdb_readonly`].
pub fn reconcile(
    catalog: &Catalog,
    plexdb_conn: &Connection,
) -> Result<ReconcileReport, CatalogError> {
    let etv_conn = &catalog.conn;

    let etv_by_rk = fetch_pairs(
        etv_conn,
        "SELECT source_id, entry_id FROM entry_sources WHERE source = 'plex'",
    )?;
    let plexdb_by_rk = fetch_pairs(plexdb_conn, "SELECT rating_key, item_id FROM plex_items")?;
    let etv_titles = fetch_pairs(etv_conn, "SELECT entry_id, title FROM entries")?;
    let plexdb_titles = fetch_pairs(plexdb_conn, "SELECT item_id, title FROM items")?;
    let etv_guids = fetch_guids(
        etv_conn,
        "SELECT entry_id, namespace, value FROM entry_external_ids WHERE namespace IN (?1, ?2, ?3, ?4)",
    )?;
    let plexdb_guids = fetch_guids(
        plexdb_conn,
        "SELECT item_id, ns, value FROM external_ids WHERE ns IN (?1, ?2, ?3, ?4)",
    )?;

    // How many rating keys each of plex-db-ex's identities covers. An
    // identity covering more than one did not derive its id for every one
    // of them — the later rating keys inherited it, which `classify` must
    // not report as the two derivation rules having drifted.
    let mut rating_keys_per_item: HashMap<&str, usize> = HashMap::new();
    for item_id in plexdb_by_rk.values() {
        *rating_keys_per_item.entry(item_id.as_str()).or_insert(0) += 1;
    }

    let mut common: Vec<&String> = etv_by_rk
        .keys()
        .filter(|rk| plexdb_by_rk.contains_key(rk.as_str()))
        .collect();
    common.sort();

    let mut agree = 0usize;
    let mut mismatches = Vec::new();
    let empty: Vec<(ExternalNs, String)> = Vec::new();
    for rating_key in &common {
        let entry_id = &etv_by_rk[rating_key.as_str()];
        let item_id = &plexdb_by_rk[rating_key.as_str()];
        if entry_id == item_id {
            agree += 1;
            continue;
        }
        let title = plexdb_titles
            .get(item_id.as_str())
            .or_else(|| etv_titles.get(entry_id.as_str()))
            .cloned()
            .unwrap_or_default();
        let reason = classify(
            item_id,
            entry_id,
            plexdb_guids.get(item_id.as_str()).unwrap_or(&empty),
            etv_guids.get(entry_id.as_str()).unwrap_or(&empty),
            *rating_keys_per_item.get(item_id.as_str()).unwrap_or(&1),
        );
        mismatches.push(Mismatch {
            rating_key: (*rating_key).clone(),
            title,
            entry_id: entry_id.clone(),
            item_id: item_id.clone(),
            reason,
        });
    }

    let mut only_in_etv: Vec<(String, String)> = etv_by_rk
        .keys()
        .filter(|rk| !plexdb_by_rk.contains_key(rk.as_str()))
        .map(|rk| {
            let title = etv_titles
                .get(etv_by_rk[rk].as_str())
                .cloned()
                .unwrap_or_default();
            (rk.clone(), title)
        })
        .collect();
    only_in_etv.sort();

    let mut only_in_plexdb: Vec<(String, String)> = plexdb_by_rk
        .keys()
        .filter(|rk| !etv_by_rk.contains_key(rk.as_str()))
        .map(|rk| {
            let title = plexdb_titles
                .get(plexdb_by_rk[rk].as_str())
                .cloned()
                .unwrap_or_default();
            (rk.clone(), title)
        })
        .collect();
    only_in_plexdb.sort();

    Ok(ReconcileReport {
        compared: common.len(),
        agree,
        mismatches,
        only_in_etv,
        only_in_plexdb,
    })
}

/// `SELECT <key>, <value> FROM ...` into a map, for the four `(rating_key →
/// id)` / `(id → title)` lookups [`reconcile`] needs on each side.
fn fetch_pairs(conn: &Connection, sql: &str) -> Result<HashMap<String, String>, CatalogError> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (key, value) = row?;
        out.insert(key, value);
    }
    Ok(out)
}

/// `SELECT <id>, <namespace>, <value> FROM ... WHERE <namespace> IN (four
/// recognised namespaces)`, grouped by id.
fn fetch_guids(
    conn: &Connection,
    sql: &str,
) -> Result<HashMap<String, Vec<(ExternalNs, String)>>, CatalogError> {
    let mut stmt = conn.prepare(sql)?;
    let ns_params: Vec<&str> = RECOGNISED_NAMESPACES.iter().map(|ns| ns.as_str()).collect();
    let rows = stmt.query_map(rusqlite::params_from_iter(ns_params.iter()), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut out: HashMap<String, Vec<(ExternalNs, String)>> = HashMap::new();
    for row in rows {
        let (id, ns, value) = row?;
        // `WHERE namespace IN (...)` already restricted this to the four
        // recognised namespaces, so the parse cannot fail.
        let ns: ExternalNs = ns.parse().expect("filtered to recognised namespaces");
        out.entry(id).or_default().push((ns, value));
    }
    Ok(out)
}

/// Why `item_id` (plex-db-ex) and `entry_id` (this catalog) disagree for one
/// title.
///
/// Both sides run the same rule, so a mismatch traces back to the *inputs*
/// the two walks recorded differing — except the one case this function
/// names explicitly: identical recognised GUID sets on both sides that
/// still produced different winners, which would mean the two
/// implementations have actually drifted, not just the data they were fed.
///
/// `plexdb_rating_keys` is how many Plex rating keys `item_id` covers in
/// plex-db-ex. More than one and the derivation rule never ran on this
/// title in that store at all — it inherited an identity another rating
/// key had already claimed, so reporting drift here would be wrong.
fn classify(
    item_id: &str,
    entry_id: &str,
    plexdb_guids: &[(ExternalNs, String)],
    etv_guids: &[(ExternalNs, String)],
    plexdb_rating_keys: usize,
) -> String {
    let (ns_p, val_p) = item_id.split_once(':').unwrap_or((item_id, ""));
    let (ns_e, val_e) = entry_id.split_once(':').unwrap_or((entry_id, ""));

    if plexdb_rating_keys > 1 {
        return format!(
            "plex-db-ex's {item_id:?} covers {plexdb_rating_keys} Plex rating keys, so this \
             title inherited an identity another rating key already held rather than deriving \
             its own — the two derivation rules agree here; the stored identity is not a \
             derivation at all. A genuine duplicate of one title across two library sections is \
             expected to stay merged"
        );
    }

    if ns_p == "fs" && ns_e == "fs" {
        return "both fell back to a path hash, but the hashes differ — the canonical path \
                 disagreed between the two walks (different source roots, or one side used a \
                 fallback key the other didn't)"
            .to_string();
    }
    if ns_p == "fs" || ns_e == "fs" {
        let (fell_back, kept, winning_ns) = if ns_p == "fs" {
            ("plex-db-ex", "etv-station", ns_e)
        } else {
            ("etv-station", "plex-db-ex", ns_p)
        };
        return format!(
            "{kept} recorded a {winning_ns} GUID for this title that {fell_back} does not \
             have, so {fell_back} fell back to a path hash"
        );
    }
    if ns_p == ns_e {
        return format!(
            "both derived a {ns_p} id but the values disagree ({val_p:?} vs {val_e:?}) — the \
             two walks likely ran against different Plex data for this title"
        );
    }
    let etv_has_plexdbs_ns = etv_guids.iter().any(|(ns, _)| ns.as_str() == ns_p);
    let plexdb_has_etvs_ns = plexdb_guids.iter().any(|(ns, _)| ns.as_str() == ns_e);
    if !etv_has_plexdbs_ns {
        return format!(
            "a {ns_p} GUID is present in plex-db-ex but absent from etv-station for this \
             title, so etv-station fell through to its next-best GUID ({ns_e})"
        );
    }
    if !plexdb_has_etvs_ns {
        return format!(
            "a {ns_e} GUID is present in etv-station but absent from plex-db-ex for this \
             title, so plex-db-ex fell through to its next-best GUID ({ns_p})"
        );
    }
    format!(
        "both stores recorded a {ns_p} and a {ns_e} GUID for this title but picked different \
         ones — the derivation rule itself disagrees, not the data it was fed"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::model::{Entry, EntrySource, Source};

    /// A minimal plex-db-ex store: `schema_version`, `items`, `plex_items`,
    /// `external_ids` — the exact subset this module reads, pinned by
    /// `plexdb_reader::SUPPORTED_SCHEMA_VERSION` so a real drift in that
    /// crate's pin fails this test too rather than silently comparing
    /// against the wrong version.
    fn plexdb_store() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory plexdb");
        conn.execute_batch(&format!(
            "CREATE TABLE schema_version (version INTEGER NOT NULL);
             INSERT INTO schema_version (version) VALUES ({});
             CREATE TABLE items (item_id TEXT PRIMARY KEY, type TEXT NOT NULL, title TEXT NOT NULL);
             CREATE TABLE plex_items (
                 rating_key TEXT PRIMARY KEY,
                 item_id TEXT NOT NULL,
                 section_id TEXT NOT NULL,
                 last_seen TEXT
             );
             CREATE TABLE external_ids (
                 item_id TEXT NOT NULL,
                 ns TEXT NOT NULL,
                 value TEXT NOT NULL,
                 kind TEXT NOT NULL
             );",
            plexdb_reader::SUPPORTED_SCHEMA_VERSION
        ))
        .expect("build a minimal plexdb schema");
        conn
    }

    fn add_item(
        conn: &Connection,
        item_id: &str,
        title: &str,
        rating_key: &str,
        guids: &[(ExternalNs, &str)],
    ) {
        conn.execute(
            "INSERT INTO items (item_id, type, title) VALUES (?1, 'movie', ?2)",
            rusqlite::params![item_id, title],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO plex_items (rating_key, item_id, section_id, last_seen) \
             VALUES (?1, ?2, '1', '2024-01-01T00:00:00+00:00')",
            rusqlite::params![rating_key, item_id],
        )
        .unwrap();
        for (ns, value) in guids {
            conn.execute(
                "INSERT INTO external_ids (item_id, ns, value, kind) VALUES (?1, ?2, ?3, 'movie')",
                rusqlite::params![item_id, ns.as_str(), value],
            )
            .unwrap();
        }
    }

    fn add_entry(
        catalog: &Catalog,
        entry_id: &str,
        title: &str,
        rating_key: &str,
        guids: &[(ExternalNs, &str)],
    ) {
        catalog
            .upsert_entry(&Entry::new(entry_id, "movie", title, Source::Plex))
            .unwrap();
        catalog
            .add_source(&EntrySource {
                source: Source::Plex,
                source_id: rating_key.to_string(),
                entry_id: entry_id.to_string(),
                playback_path: String::new(),
                last_seen: None,
                missing_since: None,
            })
            .unwrap();
        for (ns, value) in guids {
            catalog
                .add_external_id(*ns, value, "movie", entry_id)
                .unwrap();
        }
    }

    #[test]
    fn matching_titles_agree() {
        let catalog = Catalog::open_in_memory().unwrap();
        let plexdb = plexdb_store();
        add_entry(
            &catalog,
            "imdb:tt1",
            "Die Hard",
            "100",
            &[(ExternalNs::Imdb, "tt1")],
        );
        add_item(
            &plexdb,
            "imdb:tt1",
            "Die Hard",
            "100",
            &[(ExternalNs::Imdb, "tt1")],
        );

        let report = reconcile(&catalog, &plexdb).unwrap();

        assert_eq!(report.compared, 1);
        assert_eq!(report.agree, 1);
        assert!(report.mismatches.is_empty());
        assert!(report.only_in_etv.is_empty());
        assert!(report.only_in_plexdb.is_empty());
    }

    #[test]
    fn a_guid_present_in_only_one_store_is_reported_with_that_reason() {
        let catalog = Catalog::open_in_memory().unwrap();
        let plexdb = plexdb_store();
        // etv-station only ever recorded the tmdb guid for this rating key.
        add_entry(
            &catalog,
            "tmdb:562",
            "Die Hard",
            "100",
            &[(ExternalNs::Tmdb, "562")],
        );
        add_item(
            &plexdb,
            "imdb:tt1",
            "Die Hard",
            "100",
            &[(ExternalNs::Imdb, "tt1"), (ExternalNs::Tmdb, "562")],
        );

        let report = reconcile(&catalog, &plexdb).unwrap();

        assert_eq!(report.compared, 1);
        assert_eq!(report.agree, 0);
        assert_eq!(report.mismatches.len(), 1);
        let mismatch = &report.mismatches[0];
        assert_eq!(mismatch.rating_key, "100");
        assert_eq!(mismatch.item_id, "imdb:tt1");
        assert_eq!(mismatch.entry_id, "tmdb:562");
        assert!(
            mismatch
                .reason
                .contains("imdb GUID is present in plex-db-ex but absent from etv-station")
        );
    }

    /// Issue #269 names this the most alarming message the check can emit,
    /// and the wrong one: when one `item_id` in plex-db-ex covers several
    /// rating keys, the later ones never ran the derivation rule at all —
    /// they inherited an identity. Reporting that as the two
    /// implementations having drifted would be wrong.
    #[test]
    fn an_identity_covering_two_rating_keys_is_reported_as_inherited_not_drifted() {
        let catalog = Catalog::open_in_memory().unwrap();
        let plexdb = plexdb_store();
        add_item(
            &plexdb,
            "imdb:tt0047034",
            "Godzilla",
            "5550",
            &[(ExternalNs::Imdb, "tt0047034")],
        );
        // A second rating key that inherited Godzilla's identity — the show
        // Plex reports as The Golden Girls.
        plexdb
            .execute(
                "INSERT INTO plex_items (rating_key, item_id, section_id, last_seen) \
                 VALUES ('141718', 'imdb:tt0047034', '2', '2024-01-01T00:00:00+00:00')",
                [],
            )
            .unwrap();
        add_entry(
            &catalog,
            "imdb:tt0047034",
            "Godzilla",
            "5550",
            &[(ExternalNs::Imdb, "tt0047034")],
        );
        add_entry(
            &catalog,
            "imdb:tt0088526",
            "The Golden Girls",
            "141718",
            &[(ExternalNs::Imdb, "tt0088526")],
        );

        let report = reconcile(&catalog, &plexdb).unwrap();

        assert_eq!(report.mismatches.len(), 1);
        let mismatch = &report.mismatches[0];
        assert_eq!(mismatch.rating_key, "141718");
        assert!(mismatch.reason.contains("covers 2 Plex rating keys"));
        assert!(mismatch.reason.contains("inherited an identity"));
        assert!(
            !mismatch
                .reason
                .contains("the derivation rule itself disagrees")
        );
    }

    #[test]
    fn both_falling_back_to_a_path_hash_is_reported_as_a_canonical_path_disagreement() {
        let catalog = Catalog::open_in_memory().unwrap();
        let plexdb = plexdb_store();
        add_item(&plexdb, "fs:aaaaaaaaaaaaaaaa", "Home Video", "200", &[]);
        add_entry(&catalog, "fs:bbbbbbbbbbbbbbbb", "Home Video", "200", &[]);

        let report = reconcile(&catalog, &plexdb).unwrap();

        assert_eq!(report.mismatches.len(), 1);
        assert!(
            report.mismatches[0]
                .reason
                .contains("canonical path disagreed")
        );
    }

    #[test]
    fn a_guid_win_against_a_path_hash_is_reported_as_a_missing_guid() {
        let catalog = Catalog::open_in_memory().unwrap();
        let plexdb = plexdb_store();
        add_item(
            &plexdb,
            "imdb:tt9",
            "Unmatched Elsewhere",
            "300",
            &[(ExternalNs::Imdb, "tt9")],
        );
        add_entry(
            &catalog,
            "fs:cccccccccccccccc",
            "Unmatched Elsewhere",
            "300",
            &[],
        );

        let report = reconcile(&catalog, &plexdb).unwrap();

        assert_eq!(report.mismatches.len(), 1);
        let reason = &report.mismatches[0].reason;
        assert!(reason.contains("plex-db-ex recorded a imdb GUID"));
        assert!(reason.contains("etv-station does not have"));
    }

    #[test]
    fn titles_present_in_only_one_store_are_counted_and_listed() {
        let catalog = Catalog::open_in_memory().unwrap();
        let plexdb = plexdb_store();
        add_item(
            &plexdb,
            "imdb:tt1",
            "Only In Plexdb",
            "100",
            &[(ExternalNs::Imdb, "tt1")],
        );
        add_entry(
            &catalog,
            "imdb:tt2",
            "Only In Etv",
            "200",
            &[(ExternalNs::Imdb, "tt2")],
        );

        let report = reconcile(&catalog, &plexdb).unwrap();

        assert_eq!(report.compared, 0);
        assert!(report.mismatches.is_empty());
        assert_eq!(
            report.only_in_etv,
            vec![("200".to_string(), "Only In Etv".to_string())]
        );
        assert_eq!(
            report.only_in_plexdb,
            vec![("100".to_string(), "Only In Plexdb".to_string())]
        );
    }

    #[test]
    fn running_it_twice_produces_the_same_report() {
        let catalog = Catalog::open_in_memory().unwrap();
        let plexdb = plexdb_store();
        add_item(
            &plexdb,
            "imdb:tt1",
            "A",
            "100",
            &[(ExternalNs::Imdb, "tt1")],
        );
        add_item(&plexdb, "tmdb:2", "B", "101", &[(ExternalNs::Tmdb, "2")]);
        add_entry(&catalog, "tvdb:9", "A", "100", &[(ExternalNs::Tvdb, "9")]);
        add_entry(&catalog, "tmdb:2", "B", "101", &[(ExternalNs::Tmdb, "2")]);

        let first = reconcile(&catalog, &plexdb).unwrap();
        let second = reconcile(&catalog, &plexdb).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.compared, 2);
        assert_eq!(first.agree, 1);
        assert_eq!(first.mismatches.len(), 1);
    }

    /// The acceptance criterion that matters: a write attempted through a
    /// `plex-db-ex` connection opened via [`open_plexdb_readonly`] must
    /// fail at SQLite's own gate.
    #[test]
    fn open_plexdb_readonly_rejects_a_write() {
        let file = tempfile::NamedTempFile::new().expect("create a temp file");
        {
            let setup = Connection::open(file.path()).expect("open for setup");
            setup
                .execute_batch(&format!(
                    "CREATE TABLE schema_version (version INTEGER NOT NULL);
                     INSERT INTO schema_version (version) VALUES ({});
                     CREATE TABLE t (id INTEGER PRIMARY KEY);",
                    plexdb_reader::SUPPORTED_SCHEMA_VERSION
                ))
                .expect("build a minimal plexdb store");
        }

        let conn = open_plexdb_readonly(file.path()).expect("open read-only");
        let result = conn.execute("INSERT INTO t (id) VALUES (1)", []);

        assert!(
            result.is_err(),
            "a read-only plexdb connection must not accept a write"
        );
    }

    #[test]
    fn open_plexdb_readonly_refuses_an_unsupported_schema_version() {
        let file = tempfile::NamedTempFile::new().expect("create a temp file");
        {
            let setup = Connection::open(file.path()).expect("open for setup");
            let wrong_version = plexdb_reader::SUPPORTED_SCHEMA_VERSION + 1;
            setup
                .execute_batch(&format!(
                    "CREATE TABLE schema_version (version INTEGER NOT NULL);
                     INSERT INTO schema_version (version) VALUES ({wrong_version});"
                ))
                .expect("build a store at the wrong version");
        }

        let err = open_plexdb_readonly(file.path())
            .expect_err("wrong schema version must refuse to open");
        assert!(matches!(
            err,
            ReconcileOpenError::UnsupportedSchemaVersion { .. }
        ));
    }
}

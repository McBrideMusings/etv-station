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
//! boundary. This repository already reads `plex-db-ex` through its explorer
//! API (ADR 0024), so it already holds both halves; doing the comparison here
//! needs no new coupling.
//!
//! **The join key is the Plex `ratingKey`.** Both stores record every
//! physical Plex item's rating key as raw provenance, independent of
//! whatever identity each side derived for it: `entry_sources.source_id`
//! where `source = 'plex'` here, each title's `plex[].rating_key` on the
//! explorer's `GET /items?include=plex`.
//! Matching on it is what lets this module tell "the two stores derived
//! different ids for the same title" apart from "the two stores don't know
//! about the same titles" — and it only makes sense when both stores were
//! built by walking the *same* Plex server, which nothing in either schema
//! records, so this module cannot check it.
//!
//! **Read-only on both sides, always.** The `plex-db-ex` half is
//! [`PlexdbRows`], read from the explorer's GET routes
//! ([`PlexdbRows::from_explorer`]); the `Catalog` this compares against is
//! whatever the caller already opened (normally
//! [`super::Catalog::open_readonly`]). Nothing here executes a write
//! statement.

use std::collections::HashMap;

use rusqlite::Connection;

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

/// The `plex-db-ex` half of [`reconcile`]: what its explorer reports for every
/// title, keyed the way the comparison joins.
#[derive(Debug, Clone, Default)]
pub struct PlexdbRows {
    /// Plex rating key → `item_id`.
    pub by_rk: HashMap<String, String>,
    /// `item_id` → title.
    pub titles: HashMap<String, String>,
    /// `item_id` → its external ids in the four recognised namespaces.
    pub external: HashMap<String, Vec<(ExternalNs, String)>>,
}

impl PlexdbRows {
    /// One walk of `GET /items?include=external_ids,plex`.
    pub fn from_explorer(explorer: &crate::plexdb_ex::Explorer) -> Result<Self, String> {
        let mut rows = Self::default();
        explorer.items("external_ids,plex", |item| {
            let Some(id) = item.get("item_id").and_then(|v| v.as_str()) else {
                return;
            };
            let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
            rows.titles.insert(id.to_string(), title.to_string());
            for p in item
                .get("plex")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                if let Some(rk) = p.get("rating_key").and_then(|v| v.as_str()) {
                    rows.by_rk.insert(rk.to_string(), id.to_string());
                }
            }
            for x in item
                .get("external_ids")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                let (Some(ns), Some(value)) = (
                    x.get("ns").and_then(|v| v.as_str()),
                    x.get("value").and_then(|v| v.as_str()),
                ) else {
                    continue;
                };
                if let Some(ns) = RECOGNISED_NAMESPACES.iter().find(|r| r.as_str() == ns) {
                    rows.external
                        .entry(id.to_string())
                        .or_default()
                        .push((*ns, value.to_string()));
                }
            }
        })?;
        Ok(rows)
    }
}

/// Compare every title both stores know about, joined by Plex rating key.
/// `catalog` is this repository's already-open catalog; `plexdb` is what the
/// explorer reported ([`PlexdbRows::from_explorer`]).
pub fn reconcile(catalog: &Catalog, plexdb: &PlexdbRows) -> Result<ReconcileReport, CatalogError> {
    let etv_conn = &catalog.conn;

    let etv_by_rk = fetch_pairs(
        etv_conn,
        "SELECT source_id, entry_id FROM entry_sources WHERE source = 'plex'",
    )?;
    let plexdb_by_rk = &plexdb.by_rk;
    let etv_titles = fetch_pairs(etv_conn, "SELECT entry_id, title FROM entries")?;
    let plexdb_titles = &plexdb.titles;
    let etv_guids = fetch_guids(
        etv_conn,
        "SELECT entry_id, namespace, value FROM entry_external_ids WHERE namespace IN (?1, ?2, ?3, ?4)",
    )?;
    let plexdb_guids = &plexdb.external;

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

/// `SELECT <key>, <value> FROM ...` into a map, for the `(rating_key → id)` /
/// `(id → title)` lookups [`reconcile`] needs on the catalog side.
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

    fn plexdb_store() -> PlexdbRows {
        PlexdbRows::default()
    }

    fn add_item(
        rows: &mut PlexdbRows,
        item_id: &str,
        title: &str,
        rating_key: &str,
        guids: &[(ExternalNs, &str)],
    ) {
        rows.titles.insert(item_id.into(), title.into());
        rows.by_rk.insert(rating_key.into(), item_id.into());
        for (ns, value) in guids {
            rows.external
                .entry(item_id.into())
                .or_default()
                .push((*ns, value.to_string()));
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
                unairable_reason: None,
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
        let mut plexdb = plexdb_store();
        add_entry(
            &catalog,
            "imdb:tt1",
            "Die Hard",
            "100",
            &[(ExternalNs::Imdb, "tt1")],
        );
        add_item(
            &mut plexdb,
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
        let mut plexdb = plexdb_store();
        // etv-station only ever recorded the tmdb guid for this rating key.
        add_entry(
            &catalog,
            "tmdb:562",
            "Die Hard",
            "100",
            &[(ExternalNs::Tmdb, "562")],
        );
        add_item(
            &mut plexdb,
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
        let mut plexdb = plexdb_store();
        add_item(
            &mut plexdb,
            "imdb:tt0047034",
            "Godzilla",
            "5550",
            &[(ExternalNs::Imdb, "tt0047034")],
        );
        // A second rating key that inherited Godzilla's identity — the show
        // Plex reports as The Golden Girls.
        plexdb
            .by_rk
            .insert("141718".into(), "imdb:tt0047034".into());
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
        let mut plexdb = plexdb_store();
        add_item(&mut plexdb, "fs:aaaaaaaaaaaaaaaa", "Home Video", "200", &[]);
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
        let mut plexdb = plexdb_store();
        add_item(
            &mut plexdb,
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
        let mut plexdb = plexdb_store();
        add_item(
            &mut plexdb,
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
        let mut plexdb = plexdb_store();
        add_item(
            &mut plexdb,
            "imdb:tt1",
            "A",
            "100",
            &[(ExternalNs::Imdb, "tt1")],
        );
        add_item(
            &mut plexdb,
            "tmdb:2",
            "B",
            "101",
            &[(ExternalNs::Tmdb, "2")],
        );
        add_entry(&catalog, "tvdb:9", "A", "100", &[(ExternalNs::Tvdb, "9")]);
        add_entry(&catalog, "tmdb:2", "B", "101", &[(ExternalNs::Tmdb, "2")]);

        let first = reconcile(&catalog, &plexdb).unwrap();
        let second = reconcile(&catalog, &plexdb).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.compared, 2);
        assert_eq!(first.agree, 1);
        assert_eq!(first.mismatches.len(), 1);
    }

    /// The explorer's rows land keyed the way the join reads them, and an
    /// external id outside the four recognised namespaces is dropped.
    #[test]
    fn rows_from_the_explorer_key_by_rating_key_and_keep_recognised_ids() {
        let mut store = crate::plexdb_ex::fake::Store::default();
        let item = store.item("imdb:tt1", "movie");
        item.title = Some("Die Hard".into());
        item.plex = vec![("100".into(), "1".into()), ("101".into(), "2".into())];
        item.external_ids = vec![
            ("imdb".into(), "tt1".into()),
            ("letterboxd".into(), "die-hard".into()),
        ];
        let server = crate::plexdb_ex::fake::serve(store);
        let rows =
            PlexdbRows::from_explorer(&crate::plexdb_ex::Explorer::new(&server.url)).unwrap();
        assert_eq!(rows.by_rk.get("100").map(String::as_str), Some("imdb:tt1"));
        assert_eq!(rows.by_rk.get("101").map(String::as_str), Some("imdb:tt1"));
        assert_eq!(rows.titles["imdb:tt1"], "Die Hard");
        assert_eq!(
            rows.external["imdb:tt1"],
            vec![(ExternalNs::Imdb, "tt1".to_string())]
        );
    }
}

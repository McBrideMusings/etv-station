//! Audio facts: read from the plex-db-ex snapshot.
//!
//! plex-db-ex's `harvest-audio` analyzes each media file once (tempo, beat grid,
//! key, energy) and stores the JSON in its snapshot, keyed by file hash. The
//! station reads it through the vendored `plexdb-reader`
//! ([`plexdb_reader::Reader::audio_facts_for_many`]) by the entry's id, which is
//! plex-db-ex's `item_id` (both derive it by the same rule, see
//! [`crate::catalog::reconcile_plexdb`]). The facts reach plugins as
//! `ctx.catalog(expr)[i].audio` and `ctx.probe(path).audio`, and the dashboard
//! through `/api/audio/{entry_id}` and `/api/health/audio`.
//!
//! The snapshot is the file `PLEXDB_SNAPSHOT_PATH` names, the same one a pool's
//! `datastores:` grant expands to. When it is unset, missing, or at another
//! reader shape, every entry has no facts and the caller logs one warning.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::catalog::Catalog;

/// Names the plex-db-ex snapshot file.
pub const SNAPSHOT_ENV: &str = "PLEXDB_SNAPSHOT_PATH";

/// The snapshot `PLEXDB_SNAPSHOT_PATH` names; `None` when it is unset or blank.
pub fn snapshot_path_from_env() -> Option<PathBuf> {
    std::env::var(SNAPSHOT_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
}

/// The facts the snapshot at `snapshot` holds for each of `entry_ids`; an entry
/// with none is absent from the map.
pub fn facts_for_entries(
    snapshot: &Path,
    entry_ids: &[String],
) -> Result<HashMap<String, Value>, String> {
    let reader = plexdb_reader::Reader::open(snapshot, None, None).map_err(|e| e.to_string())?;
    reader
        .audio_facts_for_many(entry_ids.iter().map(String::as_str))
        .map_err(|e| e.to_string())
}

/// The facts for the entry that plays from `playback_path`, read through the
/// catalog at `catalog` and the snapshot at `snapshot`; `None` when the file
/// has no entry, the entry has no facts, or either file cannot be read (logged).
/// `ctx.probe` returns it under `audio`.
pub fn facts_for_path(catalog: &Path, snapshot: &Path, playback_path: &str) -> Option<Value> {
    let found = Catalog::open_readonly(catalog)
        .and_then(|c| c.entry_id_for_playback_path(playback_path))
        .map_err(|e| e.to_string())
        .and_then(|id| match id {
            Some(id) => facts_for_entries(snapshot, std::slice::from_ref(&id))
                .map(|mut facts| facts.remove(&id)),
            None => Ok(None),
        });
    match found {
        Ok(facts) => facts,
        Err(e) => {
            tracing::warn!(
                event = "audio.snapshot_unreadable",
                path = playback_path,
                snapshot = %snapshot.display(),
                error = %e,
                "ctx.probe could not read audio facts from the plex-db-ex snapshot"
            );
            None
        }
    }
}

/// What `/api/health/audio` reports: how many live catalog entries the snapshot
/// knows (`in_snapshot`), how many of those have audio facts, and which snapshot
/// answered. `problem` says why the numbers are zero when the snapshot cannot be
/// read.
pub fn coverage(catalog: &Catalog, snapshot: Option<&Path>) -> Result<Value, String> {
    let ids = catalog.live_entry_ids().map_err(|e| e.to_string())?;
    let entries = ids.len();
    let shape = plexdb_reader::SUPPORTED_READER_SHAPE;
    let Some(path) = snapshot else {
        return Ok(json!({
            "entries": entries,
            "in_snapshot": 0,
            "with_facts": 0,
            "without_facts": 0,
            "snapshot": { "path": null, "env": SNAPSHOT_ENV, "reader_shape": shape },
            "problem": format!("{SNAPSHOT_ENV} is not set, so no entry has audio facts"),
        }));
    };
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| OffsetDateTime::from(t).format(&Rfc3339).ok());
    let snapshot_json = json!({
        "path": path.display().to_string(),
        "mtime": mtime,
        "env": SNAPSHOT_ENV,
        "reader_shape": shape,
    });
    let read = (|| -> Result<(usize, usize), String> {
        let conn = crate::catalog::reconcile_plexdb::open_plexdb_readonly(path)
            .map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("SELECT item_id FROM items")
            .map_err(|e| e.to_string())?;
        let known: HashSet<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        let in_snapshot: Vec<String> = ids.into_iter().filter(|id| known.contains(id)).collect();
        let with = facts_for_entries(path, &in_snapshot)?.len();
        Ok((in_snapshot.len(), with))
    })();
    Ok(match read {
        Ok((in_snapshot, with)) => json!({
            "entries": entries,
            "in_snapshot": in_snapshot,
            "with_facts": with,
            "without_facts": in_snapshot - with,
            "snapshot": snapshot_json,
            "problem": null,
        }),
        Err(e) => json!({
            "entries": entries,
            "in_snapshot": 0,
            "with_facts": 0,
            "without_facts": 0,
            "snapshot": snapshot_json,
            "problem": format!("cannot read the snapshot (this build reads reader shape {shape}): {e}"),
        }),
    })
}

#[cfg(test)]
#[path = "../tests/support/audio_snapshot.rs"]
pub(crate) mod fixture;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Entry, Source};
    use serde_json::json;

    fn catalog_with(ids: &[&str]) -> Catalog {
        let cat = Catalog::open_in_memory().unwrap();
        for id in ids {
            cat.upsert_entry(&Entry::new(*id, "movie", *id, Source::LocalFs))
                .unwrap();
        }
        cat
    }

    #[test]
    fn coverage_counts_snapshot_entries_with_and_without_facts() {
        let dir = tempfile::tempdir().unwrap();
        let snap = dir.path().join("plexdb.snapshot.db");
        fixture::write(
            &snap,
            &[
                ("a", Some(r#"{"bpm":120.0}"#)),
                ("b", None),
                ("c", Some(r#"{"bpm":90.0}"#)),
            ],
        );
        // `d` is in the catalog only; `c` is in the snapshot only.
        let cat = catalog_with(&["a", "b", "d"]);
        let body = coverage(&cat, Some(&snap)).unwrap();
        assert_eq!(body["entries"], json!(3));
        assert_eq!(body["in_snapshot"], json!(2));
        assert_eq!(body["with_facts"], json!(1));
        assert_eq!(body["without_facts"], json!(1));
        assert_eq!(body["problem"], Value::Null);
        assert_eq!(body["snapshot"]["path"], json!(snap.display().to_string()));
        assert!(body["snapshot"]["mtime"].as_str().is_some(), "{body}");
    }

    #[test]
    fn coverage_says_so_when_the_snapshot_is_unset_missing_or_another_shape() {
        let cat = catalog_with(&["a"]);
        let body = coverage(&cat, None).unwrap();
        assert_eq!(body["with_facts"], json!(0));
        assert!(body["problem"].as_str().unwrap().contains(SNAPSHOT_ENV));

        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.db");
        let body = coverage(&cat, Some(&missing)).unwrap();
        assert!(body["problem"].as_str().unwrap().contains("cannot read"));

        let wrong = dir.path().join("wrong.db");
        let conn = rusqlite::Connection::open(&wrong).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER NOT NULL);
             INSERT INTO schema_version VALUES (1);
             CREATE TABLE reader_shape (version INTEGER NOT NULL);
             INSERT INTO reader_shape VALUES (10);",
        )
        .unwrap();
        drop(conn);
        let body = coverage(&cat, Some(&wrong)).unwrap();
        let problem = body["problem"].as_str().unwrap();
        assert!(problem.contains("reader shape"), "{problem}");
        assert_eq!(body["snapshot"]["reader_shape"], json!(29));
    }
}

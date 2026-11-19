//! Audio facts: read from the plex-db-ex explorer (ADR 0024).
//!
//! plex-db-ex's `harvest-audio` analyzes each media file once (tempo, beat grid,
//! key, energy) and serves the JSON at `GET /items/{id}/audio-facts`, by the
//! entry's id, which is plex-db-ex's `item_id` (both derive it by the same rule,
//! see [`crate::catalog::reconcile_plexdb`]). The facts reach plugins as
//! `ctx.catalog(expr)[i].audio` and `ctx.probe(path).audio`, and the dashboard
//! through `/api/audio/{entry_id}` and `/api/health/audio`.
//!
//! The explorer is the one [`crate::plexdb_ex::URL_ENV`] names. When it is
//! unset or unreachable, every entry has no facts and the caller logs one
//! warning.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::{Value, json};

use crate::catalog::Catalog;
use crate::plexdb_ex::{Explorer, URL_ENV};

/// The facts for the entry that plays from `playback_path`, read through the
/// catalog at `catalog` and the explorer; `None` when the file has no live
/// entry, the entry has no facts, or either cannot be read (logged).
/// `ctx.probe` returns it under `audio`.
pub fn facts_for_path(catalog: &Path, explorer: &Explorer, playback_path: &str) -> Option<Value> {
    let found = Catalog::open_readonly(catalog)
        .and_then(|c| c.entry_id_for_playback_path(playback_path))
        .map_err(|e| e.to_string())
        .and_then(|id| match id {
            Some(id) => explorer
                .audio_facts_for(std::slice::from_ref(&id))
                .map(|mut facts| facts.remove(&id)),
            None => Ok(None),
        });
    match found {
        Ok(facts) => facts,
        Err(e) => {
            tracing::warn!(
                event = "audio.explorer_unreadable",
                path = playback_path,
                explorer = explorer.base(),
                error = %e,
                "ctx.probe could not read audio facts from the plex-db-ex explorer"
            );
            None
        }
    }
}

/// What `/api/health/audio` reports: how many live catalog entries the store
/// holds (`in_store`), how many of those have audio facts, and which explorer
/// answered. `problem` says why the numbers are zero when the explorer cannot
/// be read. `live` is the catalog's live entry ids. Walks every title with its
/// facts (`GET /items?include=audio_facts`, 93 pages and 66 s on prod), so the
/// dashboard runs it off the request thread and keeps the answer (`web_ui`).
pub fn coverage(live: Vec<String>, explorer: Option<&Explorer>) -> Value {
    let entries = live.len();
    let Some(explorer) = explorer else {
        return json!({
            "entries": entries,
            "in_store": 0,
            "with_facts": 0,
            "without_facts": 0,
            "explorer": { "url": null, "env": URL_ENV },
            "problem": format!("{URL_ENV} is not set, so no entry has audio facts"),
        });
    };
    let explorer_json = json!({ "url": explorer.base(), "env": URL_ENV });
    let live: HashSet<String> = live.into_iter().collect();
    let (mut in_store, mut with) = (0usize, 0usize);
    let read = explorer.items("audio_facts", |item| {
        let Some(id) = item.get("item_id").and_then(Value::as_str) else {
            return;
        };
        if live.contains(id) {
            in_store += 1;
            if !item.get("audio_facts").is_none_or(Value::is_null) {
                with += 1;
            }
        }
    });
    match read {
        Ok(()) => json!({
            "entries": entries,
            "in_store": in_store,
            "with_facts": with,
            "without_facts": in_store - with,
            "explorer": explorer_json,
            "problem": null,
        }),
        Err(e) => json!({
            "entries": entries,
            "in_store": 0,
            "with_facts": 0,
            "without_facts": 0,
            "explorer": explorer_json,
            "problem": format!("cannot read the plex-db-ex explorer: {e}"),
        }),
    }
}

/// The audio facts for `ids`, or why they cannot be read: the dashboard's
/// `/api/audio/{id}` and item-search hints.
pub fn facts_for_entries(
    explorer: Option<&Explorer>,
    ids: &[String],
) -> Result<HashMap<String, Value>, String> {
    let explorer =
        explorer.ok_or_else(|| format!("{URL_ENV} is not set, so no entry has audio facts"))?;
    explorer
        .audio_facts_for(ids)
        .map_err(|e| format!("cannot read the plex-db-ex explorer: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Entry, Source};
    use crate::plexdb_ex::fake;
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
    fn coverage_counts_store_entries_with_and_without_facts() {
        let mut store = fake::Store::default();
        store.audio("a", json!({"bpm": 120.0}));
        store.audio_null("b");
        store.audio("c", json!({"bpm": 90.0}));
        let server = fake::serve(store);
        // `d` is in the catalog only; `c` is in the store only.
        let cat = catalog_with(&["a", "b", "d"]);
        let ex = Explorer::new(&server.url);
        let body = coverage(cat.live_entry_ids().unwrap(), Some(&ex));
        assert_eq!(body["entries"], json!(3));
        assert_eq!(body["in_store"], json!(2));
        assert_eq!(body["with_facts"], json!(1));
        assert_eq!(body["without_facts"], json!(1));
        assert_eq!(body["problem"], Value::Null);
        assert_eq!(body["explorer"]["url"], json!(server.url));
    }

    #[test]
    fn coverage_says_so_when_the_explorer_is_unset_or_unreachable() {
        let cat = catalog_with(&["a"]);
        let body = coverage(cat.live_entry_ids().unwrap(), None);
        assert_eq!(body["with_facts"], json!(0));
        assert!(body["problem"].as_str().unwrap().contains(URL_ENV));

        let down = Explorer::new("http://127.0.0.1:9");
        let body = coverage(cat.live_entry_ids().unwrap(), Some(&down));
        let problem = body["problem"].as_str().unwrap();
        assert!(problem.contains("cannot read"), "{problem}");
        assert!(problem.contains("127.0.0.1:9"), "{problem}");
    }
}

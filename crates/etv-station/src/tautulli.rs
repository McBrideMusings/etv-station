//! Tautulli watch history (#74) — the station's one source of "what has been
//! watched on this server lately".
//!
//! Read once per generation, pooled across every user with no user dimension,
//! and handed to a scorer plugin as a signal. It is never a query filter field:
//! a channel cannot say "movies nobody has watched" in CEL, because watch
//! activity belongs to the algorithm's judgment, not to the catalog.
//!
//! Connection details come from the environment — `TAUTULLI_URL` and
//! `TAUTULLI_API_KEY` — and never from tracked config, so a deployment supplies
//! them as container environment variables or Docker secrets.
//!
//! # Failure is not fatal
//!
//! [`fetch`] returns an empty history rather than an error when Tautulli is
//! unset or unreachable. A plugin still has release dates, `last_seen`, tags,
//! and the channel's own recently-aired tail to rank on, so an outage degrades
//! the ranking instead of stopping a channel that is otherwise fine. The reason
//! is logged at each generation so the degradation is visible.

use std::time::Duration;

use crate::catalog::{Catalog, Source};
use crate::score::WatchEvent;

const URL_VAR: &str = "TAUTULLI_URL";
const KEY_VAR: &str = "TAUTULLI_API_KEY";

/// How long to wait on Tautulli before giving up and generating without it.
const TIMEOUT: Duration = Duration::from_secs(10);

/// How many history rows to ask for. Tautulli returns newest-first, so this is
/// a recency window expressed as a row count — the API has no "since" filter on
/// `get_history`.
const HISTORY_ROWS: usize = 1000;

/// Raw history rows straight off the API, before any catalog join.
///
/// Split from [`join`] because the two halves want different threads: this one
/// blocks on the network (`ureq`) and must run under `spawn_blocking`, while
/// the join needs the catalog mutex and no network at all. Never returns an
/// error — see the module docs.
pub fn fetch_rows_from_env() -> Vec<HistoryRow> {
    let (url, key) = match (std::env::var(URL_VAR), std::env::var(KEY_VAR)) {
        (Ok(u), Ok(k)) if !u.is_empty() && !k.is_empty() => (u, k),
        _ => {
            tracing::debug!(
                event = "tautulli.skip",
                "{URL_VAR}/{KEY_VAR} unset; generating with no watch history",
            );
            return Vec::new();
        }
    };

    match fetch_rows(&url, &key) {
        Ok(rows) => {
            tracing::info!(
                event = "tautulli.history",
                rows = rows.len(),
                "fetched watch history",
            );
            rows
        }
        Err(e) => {
            tracing::warn!(
                event = "tautulli.unavailable",
                error = %e,
                "watch history unavailable; generating without it",
            );
            Vec::new()
        }
    }
}

/// Join raw rows to catalog entries.
///
/// Rows that match nothing in the catalog are dropped: Tautulli remembers plays
/// of media that has since been removed, and an `entry_id` a plugin cannot look
/// up is noise.
pub fn join(catalog: &Catalog, rows: Vec<HistoryRow>) -> Vec<WatchEvent> {
    resolve(catalog, rows)
}

/// One `get_history` row, narrowed to the two fields that matter.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct HistoryRow {
    /// The Plex `ratingKey`, which is also the `source_id` of the entry's
    /// `plex` provenance row — the join back to the catalog.
    #[serde(default)]
    rating_key: Option<serde_json::Value>,
    /// Unix seconds when playback stopped. Rows for an in-flight stream have
    /// no stop time yet.
    #[serde(default)]
    stopped: Option<i64>,
}

fn fetch_rows(url: &str, key: &str) -> Result<Vec<HistoryRow>, String> {
    let endpoint = format!(
        "{}/api/v2?apikey={}&cmd=get_history&length={HISTORY_ROWS}",
        url.trim_end_matches('/'),
        key
    );

    let body: serde_json::Value = ureq::get(&endpoint)
        .timeout(TIMEOUT)
        .call()
        .map_err(|e| format!("request failed: {e}"))?
        .into_json()
        .map_err(|e| format!("decode response: {e}"))?;

    let data = body
        .get("response")
        .and_then(|r| r.get("data"))
        .and_then(|d| d.get("data"))
        .ok_or_else(|| "response has no response.data.data array".to_string())?;

    serde_json::from_value(data.clone()).map_err(|e| format!("decode history rows: {e}"))
}

/// Join history rows to catalog entries by Plex `ratingKey`, keeping the most
/// recent watch per entry.
fn resolve(catalog: &Catalog, rows: Vec<HistoryRow>) -> Vec<WatchEvent> {
    let mut out: Vec<WatchEvent> = Vec::new();
    for row in rows {
        // Tautulli types rating_key inconsistently across versions — a JSON
        // number in some, a string in others — so accept either rather than
        // silently matching nothing.
        let rating_key = match row.rating_key.as_ref() {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            _ => continue,
        };
        let Some(watched_at) = row.stopped else {
            continue;
        };
        match catalog.entry_id_for_source(Source::Plex, &rating_key) {
            Ok(Some(entry_id)) => out.push(WatchEvent {
                entry_id,
                watched_at,
            }),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    event = "tautulli.lookup_failed",
                    rating_key = %rating_key,
                    error = %e,
                    "skipping a history row",
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Entry, EntrySource};

    fn seeded() -> Catalog {
        let c = Catalog::open_in_memory().unwrap();
        c.upsert_entry(&Entry::new("m1", "movie", "Alpha", Source::Plex))
            .unwrap();
        c.add_source(&EntrySource {
            source: Source::Plex,
            source_id: "plex-1".into(),
            entry_id: "m1".into(),
            playback_path: "/media/alpha.mkv".into(),
            last_seen: None,
        })
        .unwrap();
        c
    }

    fn row(key: serde_json::Value, stopped: Option<i64>) -> HistoryRow {
        HistoryRow {
            rating_key: Some(key),
            stopped,
        }
    }

    #[test]
    fn joins_rows_to_entries_by_rating_key() {
        let got = resolve(&seeded(), vec![row("plex-1".into(), Some(100))]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].entry_id, "m1");
        assert_eq!(got[0].watched_at, 100);
    }

    #[test]
    fn accepts_a_numeric_rating_key() {
        let c = Catalog::open_in_memory().unwrap();
        c.upsert_entry(&Entry::new("m2", "movie", "Beta", Source::Plex))
            .unwrap();
        c.add_source(&EntrySource {
            source: Source::Plex,
            source_id: "4242".into(),
            entry_id: "m2".into(),
            playback_path: "/media/beta.mkv".into(),
            last_seen: None,
        })
        .unwrap();
        let got = resolve(&c, vec![row(serde_json::json!(4242), Some(7))]);
        assert_eq!(got.len(), 1, "a numeric ratingKey must still match");
        assert_eq!(got[0].entry_id, "m2");
    }

    #[test]
    fn drops_rows_that_match_nothing_in_the_catalog() {
        // Tautulli remembers plays of media that has since been removed.
        assert!(resolve(&seeded(), vec![row("gone".into(), Some(1))]).is_empty());
    }

    #[test]
    fn drops_rows_still_playing() {
        assert!(resolve(&seeded(), vec![row("plex-1".into(), None)]).is_empty());
    }
}

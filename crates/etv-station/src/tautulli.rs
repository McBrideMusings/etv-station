//! Tautulli watch history (#74) — the station's one source of "what has been
//! watched on this server lately".
//!
//! Read at most once per station tick and shared by every channel (#126) —
//! pooled across every user with no user dimension, and handed to a scorer
//! plugin as a signal. It is never a query filter field:
//! a channel cannot say "movies nobody has watched" in CEL, because watch
//! activity belongs to the algorithm's judgment, not to the catalog.
//!
//! Connection details come from the environment — `TAUTULLI_URL` and
//! `TAUTULLI_API_KEY` — and never from tracked config, so a deployment supplies
//! them as container environment variables or Docker secrets. The environment is
//! read in exactly one place, [`credentials_from_env`], and everything below it
//! takes the connection as an argument (#132).
//!
//! # Failure is not fatal
//!
//! [`fetch_rows`] returns an empty history rather than an error when Tautulli is
//! unreachable, and a station with no credentials never calls it at all. A
//! plugin still has release dates, `last_seen`, tags, and the channel's own
//! recently-aired tail to rank on, so an outage degrades the ranking instead of
//! stopping a channel that is otherwise fine. The reason is logged on each fetch
//! so the degradation is visible.

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

/// The `(url, api key)` pair to fetch against, or `None` when the deployment has
/// not configured Tautulli — which is normal, not an error.
///
/// This is the only place in the crate that reads `TAUTULLI_URL` /
/// `TAUTULLI_API_KEY`, and it is called once per generation, at startup. Keeping
/// the read here and passing the result down is what makes the fetch testable:
/// a test hands its `SharedHistory` a `None` instead of deleting the variables
/// out of the running process. `std::env::remove_var` is unsound in Rust 2024
/// the moment any other thread reads the environment, and `cargo test` runs a
/// module's tests concurrently, so the old approach was one added env-reading
/// test away from a real data race (#132).
pub fn credentials_from_env() -> Option<(String, String)> {
    match (std::env::var(URL_VAR), std::env::var(KEY_VAR)) {
        (Ok(u), Ok(k)) if !u.is_empty() && !k.is_empty() => Some((u, k)),
        _ => {
            tracing::debug!(
                event = "tautulli.skip",
                "{URL_VAR}/{KEY_VAR} unset; generating with no watch history",
            );
            None
        }
    }
}

/// Raw history rows straight off the API, before any catalog join.
///
/// Split from [`join`] because the two halves want different threads: this one
/// blocks on the network (`ureq`) and must run under `spawn_blocking`, while
/// the join needs the catalog mutex and no network at all. Never returns an
/// error — see the module docs.
pub fn fetch_rows(url: &str, key: &str) -> Vec<HistoryRow> {
    match request_rows(url, key) {
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

/// Strip the API key out of anything on its way to a log.
///
/// Tautulli authenticates by query parameter, so the key is part of the request
/// URL — and `ureq` puts the URL it was given into its error's `Display`. That
/// error is what [`fetch_rows`] logs on every failed poll, so without this the
/// key lands in the daemon log in plaintext, once per tick, for as long as the
/// outage lasts.
fn redact_key(e: impl std::fmt::Display, key: &str) -> String {
    let text = e.to_string();
    if key.is_empty() {
        // `str::replace` with an empty pattern splices the replacement between
        // every character; never let an empty key reach it.
        return text;
    }
    text.replace(key, "<redacted>")
}

fn request_rows(url: &str, key: &str) -> Result<Vec<HistoryRow>, String> {
    let endpoint = format!(
        "{}/api/v2?apikey={}&cmd=get_history&length={HISTORY_ROWS}",
        url.trim_end_matches('/'),
        key
    );

    let body: serde_json::Value = ureq::get(&endpoint)
        .timeout(TIMEOUT)
        .call()
        .map_err(|e| format!("request failed: {}", redact_key(e, key)))?
        .into_json()
        .map_err(|e| format!("decode response: {}", redact_key(e, key)))?;

    let data = body
        .get("response")
        .and_then(|r| r.get("data"))
        .and_then(|d| d.get("data"))
        .ok_or_else(|| "response has no response.data.data array".to_string())?;

    serde_json::from_value(data.clone()).map_err(|e| format!("decode history rows: {e}"))
}

/// Join history rows to catalog entries by Plex `ratingKey`, emitting one
/// [`WatchEvent`] per row that resolves.
///
/// A rewatch is deliberately two events — a plugin ranking on play counts needs
/// to see both — which is why the health numbers logged below are counted per
/// distinct `ratingKey` instead of per row.
fn resolve(catalog: &Catalog, rows: Vec<HistoryRow>) -> Vec<WatchEvent> {
    let row_count = rows.len();
    // Counted per distinct `ratingKey`, not per row, so `matched` lines up with
    // `SELECT count(*) FROM entry_sources WHERE source_id IN (…)` — which
    // dedupes the key list. Per-row counts would exceed that query by however
    // many rewatches the batch happens to contain, making an honest join
    // indistinguishable from a broken one.
    let mut looked_up: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut matched: std::collections::HashSet<String> = std::collections::HashSet::new();
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
        looked_up.insert(rating_key.clone());
        match catalog.entry_id_for_source(Source::Plex, &rating_key) {
            Ok(Some(entry_id)) => {
                matched.insert(rating_key);
                out.push(WatchEvent {
                    entry_id,
                    watched_at,
                });
            }
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

    // The join's health on one line. A per-row warn only fires on a lookup
    // error, so without this a total join failure — every ratingKey missing
    // from `entry_sources` — reads exactly like a healthy server nobody has
    // watched lately: rows in, nothing out, no warning.
    //
    // `keys - matched` is exactly the set of misses to explain.
    tracing::info!(
        event = "tautulli.join",
        rows = row_count,
        keys = looked_up.len(),
        matched = matched.len(),
        "joined watch history to the catalog",
    );

    out
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

    use super::*;
    use crate::catalog::{Entry, EntrySource};

    /// The counts on a `tautulli.join` event, pulled off a `tracing` event
    /// without a formatter in the way.
    #[derive(Default)]
    struct JoinFields {
        event: Option<String>,
        rows: Option<u64>,
        keys: Option<u64>,
        matched: Option<u64>,
    }

    impl Visit for JoinFields {
        fn record_u64(&mut self, field: &Field, value: u64) {
            match field.name() {
                "rows" => self.rows = Some(value),
                "keys" => self.keys = Some(value),
                "matched" => self.matched = Some(value),
                _ => {}
            }
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "event" {
                self.event = Some(value.to_string());
            }
        }

        fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
    }

    /// Collects every `tautulli.join` event's `(rows, keys, matched)` triple.
    struct CaptureJoins(Arc<Mutex<Vec<(u64, u64, u64)>>>);

    impl<S: tracing::Subscriber> Layer<S> for CaptureJoins {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let mut fields = JoinFields::default();
            event.record(&mut fields);
            if fields.event.as_deref() != Some("tautulli.join") {
                return;
            }
            let (Some(rows), Some(keys), Some(matched)) =
                (fields.rows, fields.keys, fields.matched)
            else {
                panic!("a tautulli.join event must carry rows, keys and matched");
            };
            self.0.lock().unwrap().push((rows, keys, matched));
        }
    }

    /// Run [`resolve`] under a subscriber that captures `tautulli.join`.
    ///
    /// **Every** test in this module goes through here, including the ones that
    /// only assert on the returned events. `tracing` caches callsite interest
    /// process-globally: the first time `tautulli.join` is hit with no
    /// subscriber installed, the callsite is registered against `NoSubscriber`
    /// and cached as `Interest::never()` — which then silently blinds a test
    /// running concurrently on another thread that *did* install one. Keeping
    /// every call under a subscriber means the callsite is never registered
    /// against the no-op, so the capture cannot race.
    fn resolve_capturing(
        catalog: &Catalog,
        rows: Vec<HistoryRow>,
    ) -> (Vec<WatchEvent>, Vec<(u64, u64, u64)>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(CaptureJoins(Arc::clone(&seen)));
        let events = tracing::subscriber::with_default(subscriber, || resolve(catalog, rows));
        let logged = seen.lock().unwrap().clone();
        (events, logged)
    }

    /// Just the watch events, for tests that do not assert on the log.
    fn resolved(catalog: &Catalog, rows: Vec<HistoryRow>) -> Vec<WatchEvent> {
        resolve_capturing(catalog, rows).0
    }

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
        let got = resolved(&seeded(), vec![row("plex-1".into(), Some(100))]);
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
        let got = resolved(&c, vec![row(serde_json::json!(4242), Some(7))]);
        assert_eq!(got.len(), 1, "a numeric ratingKey must still match");
        assert_eq!(got[0].entry_id, "m2");
    }

    #[test]
    fn drops_rows_that_match_nothing_in_the_catalog() {
        // Tautulli remembers plays of media that has since been removed.
        assert!(resolved(&seeded(), vec![row("gone".into(), Some(1))]).is_empty());
    }

    #[test]
    fn drops_rows_still_playing() {
        assert!(resolved(&seeded(), vec![row("plex-1".into(), None)]).is_empty());
    }

    #[test]
    fn keeps_the_api_key_out_of_a_failure_message() {
        // Shaped like a real `ureq` transport error: the whole request URL,
        // key and all, followed by the transport's own complaint.
        let key = "s3cr3t-tautulli-key";
        let raw = format!(
            "http://tautulli.invalid:8181/api/v2?apikey={key}&cmd=get_history&length=1000: \
             Connection Failed: Connect error: Connection refused (os error 61)"
        );
        let got = redact_key(raw, key);
        assert!(
            !got.contains(key),
            "the api key must not survive into a log"
        );
        assert!(
            got.contains("apikey=<redacted>"),
            "the rest of the message must survive so the error stays diagnosable: {got}"
        );
    }

    #[test]
    fn an_empty_key_is_left_alone() {
        // `str::replace` with an empty pattern would splice the replacement
        // between every character, turning the message into garbage.
        assert_eq!(redact_key("request failed", ""), "request failed");
    }

    #[test]
    fn logs_one_join_event_carrying_rows_keys_and_matched() {
        let (events, logged) = resolve_capturing(
            &seeded(),
            vec![
                row("plex-1".into(), Some(100)),
                // A rewatch: a second row for a ratingKey already counted.
                row("plex-1".into(), Some(200)),
                // Removed from the library since it was played.
                row("gone".into(), Some(1)),
                // Still playing, so it never reaches the catalog lookup and
                // never counts towards `keys`.
                row("plex-1".into(), None),
            ],
        );

        // 4 rows in; 2 distinct ratingKeys reached the lookup (plex-1, gone);
        // 1 of those resolved. `keys - matched == 1` is the single miss to
        // explain — the rewatch does not inflate any of the three.
        assert_eq!(logged, vec![(4, 2, 1)]);

        // The plugin-facing events are untouched by the per-key counting: two
        // plays of plex-1 are still two events, so a scorer can count plays.
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e.entry_id == "m1"));
    }

    #[test]
    fn a_rewatch_never_pushes_matched_past_the_distinct_key_count() {
        // The reconciliation in #114 compares `matched` against
        // `SELECT count(*) FROM entry_sources WHERE source_id IN (…)`, which
        // dedupes the key list. Ten plays of one film must still read as one.
        let rows = (0..10)
            .map(|i| row("plex-1".into(), Some(i)))
            .collect::<Vec<_>>();
        let (events, logged) = resolve_capturing(&seeded(), rows);
        assert_eq!(logged, vec![(10, 1, 1)]);
        assert_eq!(events.len(), 10, "every play is still its own event");
    }

    #[test]
    fn logs_the_join_event_even_when_nothing_matched() {
        // The case the event exists for: a broken join and an idle server are
        // indistinguishable without a line that says zero of many matched.
        let (_, logged) = resolve_capturing(&seeded(), vec![row("gone".into(), Some(1))]);
        assert_eq!(logged, vec![(1, 1, 0)]);
    }
}

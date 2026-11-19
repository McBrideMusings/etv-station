//! The plex-db-ex explorer's HTTP API, as the core reads it (ADR 0024).
//!
//! plex-db-ex publishes its store as resource-shaped REST on its explorer
//! (`plexdb-rs explore`, port 5194; its `docs/api.md`). The station opens no
//! plex-db-ex file: the facts the core exposes — audio facts on `ctx.catalog`
//! and `ctx.probe`, a profile's `keyword:` resolution, the reconcile report and
//! the dashboard's audio routes — come from these routes. Plugins reach the
//! same server through `ctx.http` (`{{PLEXDB_EX_URL}}`), never through here.
//!
//! Every list on the API is cursor-paged (`limit` at most 1000, `after`, reply
//! `has_more`/`next`), and the explorer refuses a ninth concurrent request with
//! a 429, so [`Explorer::audio_facts_for`] runs at most [`WORKERS`] at once.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use serde_json::Value;

/// The variable naming the explorer's base URL.
pub const URL_ENV: &str = "PLEXDB_EX_URL";

/// How long one request may take. The slowest page measured against prod
/// (1000 movies with every enrichment namespace, 3.4 MB) answered in 0.38 s.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Concurrent requests for per-title reads: half the explorer's cap of eight,
/// so a dashboard read and a daemon read at the same moment still fit.
pub const WORKERS: usize = 4;

/// The largest page the API serves.
const PAGE: usize = 1000;

/// A client for one explorer.
#[derive(Debug, Clone)]
pub struct Explorer {
    base: String,
    agent: ureq::Agent,
}

/// Why a read failed: the request, and the status or transport error.
pub type Error = String;

impl Explorer {
    /// A client for the explorer at `base` (`http://host:5194`).
    pub fn new(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            agent: ureq::AgentBuilder::new()
                .timeout(TIMEOUT)
                .max_idle_connections_per_host(WORKERS)
                .build(),
        }
    }

    /// The explorer [`URL_ENV`] names; `None` when it is unset or blank.
    pub fn from_env() -> Option<Self> {
        std::env::var(URL_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| Self::new(&v))
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// `GET {base}{path}` as JSON; `Ok(None)` on a 404 (a title, account or
    /// keyword the store does not hold).
    fn get(&self, path: &str) -> Result<Option<Value>, Error> {
        let started = Instant::now();
        let url = format!("{}{path}", self.base);
        let result = self.agent.get(&url).call();
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(resp) => {
                let text = resp
                    .into_string()
                    .map_err(|e| format!("GET {path}: reading the body: {e}"))?;
                tracing::debug!(
                    event = "plexdb_ex.get",
                    path,
                    status = 200,
                    bytes = text.len(),
                    elapsed_ms,
                    "read the plex-db-ex explorer"
                );
                serde_json::from_str(&text)
                    .map(Some)
                    .map_err(|e| format!("GET {path}: not JSON: {e}"))
            }
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                tracing::warn!(
                    event = "plexdb_ex.get",
                    path,
                    status = code,
                    elapsed_ms,
                    "plex-db-ex explorer answered an error"
                );
                Err(format!("GET {}{path} -> status {code}: {body}", self.base))
            }
            Err(ureq::Error::Transport(t)) => {
                tracing::warn!(
                    event = "plexdb_ex.get",
                    path,
                    error = %t,
                    elapsed_ms,
                    "plex-db-ex explorer unreachable"
                );
                Err(format!("GET {}{path}: {t}", self.base))
            }
        }
    }

    /// Every row of the paged list at `path` (which already carries its own
    /// query, without `limit` or `after`), handed to `row` one at a time.
    pub fn walk(&self, path: &str, mut row: impl FnMut(Value)) -> Result<(), Error> {
        let sep = if path.contains('?') { '&' } else { '?' };
        let mut after: Option<String> = None;
        let started = Instant::now();
        let mut pages = 0usize;
        loop {
            let page_path = match &after {
                Some(a) => format!("{path}{sep}limit={PAGE}&after={}", encode(a)),
                None => format!("{path}{sep}limit={PAGE}"),
            };
            let page = self
                .get(&page_path)?
                .ok_or_else(|| format!("GET {page_path}: 404"))?;
            pages += 1;
            let Value::Object(mut page) = page else {
                return Err(format!("GET {page_path}: not a page"));
            };
            if let Some(Value::Array(data)) = page.remove("data") {
                data.into_iter().for_each(&mut row);
            }
            let more = page.get("has_more").and_then(Value::as_bool) == Some(true);
            match page.remove("next") {
                Some(Value::String(next)) if more => after = Some(next),
                _ => break,
            }
        }
        tracing::info!(
            event = "plexdb_ex.walk",
            path,
            pages,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "walked a plex-db-ex list"
        );
        Ok(())
    }

    /// The audio facts the store holds for each of `ids`; a title with none,
    /// or one the store does not hold, is absent. One
    /// `GET /items/{id}/audio-facts` per id, [`WORKERS`] at a time: the API has
    /// no read by a set of ids, and walking every title with its facts is 93
    /// pages and 280 MB on prod.
    pub fn audio_facts_for(&self, ids: &[String]) -> Result<HashMap<String, Value>, Error> {
        let started = Instant::now();
        let next = std::sync::atomic::AtomicUsize::new(0);
        let results: Vec<Result<Vec<(String, Value)>, Error>> = std::thread::scope(|s| {
            let workers: Vec<_> = (0..WORKERS.min(ids.len()))
                .map(|_| {
                    s.spawn(|| {
                        let mut found = Vec::new();
                        loop {
                            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(id) = ids.get(i) else { break };
                            match self.get(&format!("/items/{}/audio-facts", encode(id)))? {
                                Some(Value::Null) | None => {}
                                Some(facts) => found.push((id.clone(), facts)),
                            }
                        }
                        Ok(found)
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|w| w.join().unwrap_or_else(|_| Err("worker panicked".into())))
                .collect()
        });
        let mut out = HashMap::new();
        for r in results {
            out.extend(r?);
        }
        tracing::info!(
            event = "plexdb_ex.audio_facts",
            asked = ids.len(),
            found = out.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "read audio facts from the plex-db-ex explorer"
        );
        Ok(out)
    }

    /// The stored keyword a raw spelling maps to (`GET /keywords?surface=`), or
    /// `None` for a spelling the store has never recorded.
    pub fn keyword_for_surface(&self, surface: &str) -> Result<Option<String>, Error> {
        let page = self
            .get(&format!("/keywords?surface={}", encode(surface)))?
            .unwrap_or(Value::Null);
        Ok(page
            .get("data")
            .and_then(Value::as_array)
            .and_then(|d| d.first())
            .and_then(|k| k.get("keyword"))
            .and_then(Value::as_str)
            .map(str::to_string))
    }

    /// Every stored keyword with its recorded spellings, `(keyword, surfaces)`,
    /// in keyword order — one walk of `GET /keywords`.
    pub fn keywords(&self) -> Result<Vec<(String, Vec<String>)>, Error> {
        let mut out = Vec::new();
        self.walk("/keywords", |k| {
            let keyword = k.get("keyword").and_then(Value::as_str).unwrap_or_default();
            let surfaces = k
                .get("surfaces")
                .and_then(Value::as_array)
                .map(|s| {
                    s.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            out.push((keyword.to_string(), surfaces));
        })?;
        Ok(out)
    }

    /// The `keywords`-namespace values filed under each of `units` (movie and
    /// show `item_id`s), from walking `GET /items?type=movie|show` with that
    /// namespace. A unit with none is absent.
    pub fn keywords_by_unit(
        &self,
        units: &HashSet<&str>,
    ) -> Result<HashMap<String, Vec<String>>, Error> {
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        for kind in ["movie", "show"] {
            self.walk(
                &format!("/items?type={kind}&include=enrichment&namespace=keywords"),
                |item| {
                    let Some(id) = item.get("item_id").and_then(Value::as_str) else {
                        return;
                    };
                    if !units.contains(id) {
                        return;
                    }
                    let values: Vec<String> = item
                        .get("enrichment")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|f| f.get("value").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect();
                    if !values.is_empty() {
                        out.insert(id.to_string(), values);
                    }
                },
            )?;
        }
        Ok(out)
    }

    /// Every title the store holds, each as the page row `include` asks for
    /// (`external_ids`, `plex`, …), one walk of `GET /items`.
    pub fn items(&self, include: &str, mut row: impl FnMut(Value)) -> Result<(), Error> {
        let path = if include.is_empty() {
            "/items".to_string()
        } else {
            format!("/items?include={include}")
        };
        self.walk(&path, &mut row)
    }
}

/// `s` percent-encoded as one path segment or query value.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
#[path = "../tests/support/plexdb_ex_fake.rs"]
pub(crate) mod fake;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn encode_keeps_an_item_id_and_escapes_a_slash_and_space() {
        assert_eq!(encode("imdb:tt0113277"), "imdb:tt0113277");
        assert_eq!(encode("9/11 attack"), "9%2F11%20attack");
    }

    #[test]
    fn walk_follows_next_across_pages_and_audio_facts_skip_null_and_404() {
        let mut store = fake::Store::default();
        for i in 0..2500 {
            store.item(&format!("fs:{i:05}"), "movie");
        }
        store.audio("fs:00001", json!({"bpm": 120.0}));
        store.audio_null("fs:00002");
        let server = fake::serve(store);
        let ex = Explorer::new(&server.url);

        let mut seen = 0;
        ex.walk("/items?type=movie", |_| seen += 1).unwrap();
        assert_eq!(seen, 2500);

        let ids: Vec<String> = ["fs:00001", "fs:00002", "fs:99999"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let facts = ex.audio_facts_for(&ids).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts["fs:00001"], json!({"bpm": 120.0}));
    }

    #[test]
    fn keyword_for_surface_answers_the_stored_form_or_none() {
        let mut store = fake::Store::default();
        store.keyword("bank heist", &["Bank Heist"]);
        let server = fake::serve(store);
        let ex = Explorer::new(&server.url);
        assert_eq!(
            ex.keyword_for_surface("bank heist").unwrap().as_deref(),
            Some("bank heist")
        );
        assert_eq!(ex.keyword_for_surface("nope").unwrap(), None);
    }

    #[test]
    fn an_unreachable_explorer_is_an_error_naming_the_url() {
        let ex = Explorer::new("http://127.0.0.1:9");
        let e = ex.audio_facts_for(&["fs:1".into()]).unwrap_err();
        assert!(e.contains("127.0.0.1:9"), "{e}");
    }
}

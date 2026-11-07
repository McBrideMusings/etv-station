//! Extras ingester (etv-station-esqb.1): released extras — music videos, bumpers
//! and the like, organized in a Plex DB-EX store — read over HTTP and written to
//! the [`Catalog`] as ordinary entries.
//!
//! One call, `GET {extras_url}/api/extras/items[?since=<cursor>]`, answers every
//! organized extra (or those changed after the cursor an earlier answer carried).
//! Nothing from plex-db-ex is compiled in (ADR 0014): the answer is plain JSON,
//! and an extra's `item_id` is its `entry_id`, so a store that keeps an id keeps
//! the airings ledger's join key.
//!
//! The pass follows the Plex ingest's contract. The fetch completes before the
//! write transaction opens, so an unreachable server leaves the catalog exactly
//! as it was; only a **full** pass — one that read every organized extra —
//! marks an extra it did not see `missing_since`, because on a delta "not
//! mentioned" means "unchanged".

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use time::OffsetDateTime;

use crate::cancel::Cancel;
use crate::catalog::model::{Entry, EntrySource, Source};
use crate::catalog::{Catalog, CatalogError};

const HTTP_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, thiserror::Error)]
pub enum ExtrasIngestError {
    #[error("http: {0}")]
    Http(String),
    #[error("parse: {0}")]
    Parse(String),
    #[error("catalog: {0}")]
    Catalog(#[from] CatalogError),
    /// The daemon asked for shutdown before the request went out.
    #[error("cancelled by shutdown")]
    Cancelled,
}

/// One organized extra as `/api/extras/items` lists it. Fields the station does
/// not store (`source_url`, `changed_at`) are not read.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ExtrasItem {
    pub item_id: String,
    pub library: String,
    /// The library's declared type — becomes `entries.type`.
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    pub year: Option<i64>,
    pub duration_ms: Option<i64>,
    /// The primary file's path, `None` when the extra has none.
    pub path: Option<String>,
    /// True when the primary file is missing or absent.
    #[serde(default)]
    pub missing: bool,
}

#[derive(Debug, Deserialize)]
struct ExtrasResponse {
    items: Vec<ExtrasItem>,
    cursor: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExtrasIngestStats {
    pub entries_written: usize,
    pub sources_written: usize,
    /// Listed extras with no live primary file, left unwritten.
    pub skipped_missing: usize,
    pub sources_marked_missing: usize,
    pub entries_marked_missing: usize,
}

/// Fetch `base_url`'s organized extras and write them. `since` is the stored
/// cursor, honoured only on a delta; `full` says the pass reads every extra and
/// may therefore mark absentees missing. A full pass never sends `since`.
pub fn ingest(
    catalog: &Catalog,
    base_url: &str,
    full: bool,
    cancel: &Arc<Cancel>,
) -> Result<ExtrasIngestStats, ExtrasIngestError> {
    // Taken before the fetch, as the Plex pass takes its cursor: an extra
    // changed while the request runs is re-read next time, never lost.
    let started = OffsetDateTime::now_utc().unix_timestamp();
    let since = if full { None } else { catalog.extras_cursor()? };
    let answer = fetch(base_url, since.as_deref(), cancel)?;
    tracing::info!(
        event = "catalog.ingest.extras_fetched",
        mode = if full { "full" } else { "delta" },
        items = answer.items.len(),
        "fetched released extras; writing to the catalog",
    );
    // The stamp every source this pass writes carries; the sweep below marks
    // whatever does not carry it.
    let stamp = OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|e| ExtrasIngestError::Parse(e.to_string()))?;
    catalog.in_transaction(|c| {
        let mut stats = write_items(c, &answer.items, &stamp)?;
        if full {
            stats.sources_marked_missing = c.mark_unseen_sources_missing(Source::Extras, &stamp)?;
            stats.entries_marked_missing = c.mark_entries_missing_without_live_sources(&stamp)?;
        }
        c.record_extras_pass(started, full, answer.cursor.as_deref())?;
        Ok::<ExtrasIngestStats, ExtrasIngestError>(stats)
    })
}

fn fetch(
    base_url: &str,
    since: Option<&str>,
    cancel: &Arc<Cancel>,
) -> Result<ExtrasResponse, ExtrasIngestError> {
    if cancel.is_cancelled() {
        return Err(ExtrasIngestError::Cancelled);
    }
    let url = format!("{}/api/extras/items", base_url.trim_end_matches('/'));
    let mut request = ureq::AgentBuilder::new()
        .timeout(HTTP_TIMEOUT)
        .build()
        .get(&url)
        .set("Accept", "application/json");
    if let Some(since) = since {
        request = request.query("since", since);
    }
    request
        .call()
        .map_err(|e| ExtrasIngestError::Http(e.to_string()))?
        .into_json()
        .map_err(|e| ExtrasIngestError::Parse(e.to_string()))
}

/// Write every extra with a live primary file: its entry (id = the store's
/// `item_id`) and one `extras` source carrying the path. An extra whose file is
/// missing or absent is left unwritten, so a full pass's sweep marks it missing.
fn write_items(
    catalog: &Catalog,
    items: &[ExtrasItem],
    stamp: &str,
) -> Result<ExtrasIngestStats, ExtrasIngestError> {
    let mut stats = ExtrasIngestStats::default();
    for item in items {
        let path = match (&item.path, item.missing) {
            (Some(path), false) if !path.is_empty() => path,
            _ => {
                stats.skipped_missing += 1;
                continue;
            }
        };
        let mut entry = Entry::new(&item.item_id, &item.kind, &item.title, Source::Extras);
        entry.year = item.year;
        entry.duration_ms = item.duration_ms;
        entry.library = Some(item.library.clone());
        catalog.upsert_entry(&entry)?;
        stats.entries_written += 1;
        catalog.add_source(&EntrySource {
            source: Source::Extras,
            source_id: item.item_id.clone(),
            entry_id: item.item_id.clone(),
            playback_path: path.clone(),
            last_seen: Some(stamp.to_string()),
            missing_since: None,
            unairable_reason: None,
        })?;
        stats.sources_written += 1;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn item(id: &str, path: Option<&str>, missing: bool) -> ExtrasItem {
        ExtrasItem {
            item_id: id.into(),
            library: "Music Videos".into(),
            kind: "music_video".into(),
            title: format!("Title {id}"),
            year: Some(1999),
            duration_ms: Some(215_000),
            path: path.map(str::to_string),
            missing,
        }
    }

    /// Serve `bodies` in order, one per connection, recording each request line.
    fn serve(bodies: Vec<String>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut lines = Vec::new();
            for body in bodies {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap();
                lines.push(
                    String::from_utf8_lossy(&buf[..n])
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .to_string(),
                );
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
            lines
        });
        (url, handle)
    }

    fn body(items: &[(&str, Option<&str>, bool)], cursor: &str) -> String {
        let items: Vec<String> = items
            .iter()
            .map(|(id, path, missing)| {
                format!(
                    r#"{{"item_id":"{id}","library":"Music Videos","type":"music_video","title":"T {id}","year":1999,"duration_ms":215000,"path":{},"missing":{missing},"source_url":null,"changed_at":"{cursor}"}}"#,
                    path.map_or("null".to_string(), |p| format!("\"{p}\"")),
                )
            })
            .collect();
        format!(r#"{{"items":[{}],"cursor":"{cursor}"}}"#, items.join(","))
    }

    #[test]
    fn writes_an_entry_and_source_for_each_live_extra() {
        let cat = Catalog::open_in_memory().unwrap();
        let items = [
            item("x1", Some("/media/mv/a.mkv"), false),
            item("x2", Some("/media/mv/b.mkv"), true),
            item("x3", None, true),
        ];
        let stats = write_items(&cat, &items, "2026-11-05T00:00:00Z").unwrap();
        assert_eq!(stats.entries_written, 1);
        assert_eq!(stats.skipped_missing, 2);
        let e = cat.entry("x1").unwrap().unwrap();
        assert_eq!(e.kind, "music_video");
        assert_eq!(e.library.as_deref(), Some("Music Videos"));
        assert_eq!(e.year, Some(1999));
        assert_eq!(e.duration_ms, Some(215_000));
        assert_eq!(e.primary_source, Source::Extras);
        let sources = cat.sources_for("x1").unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].source, Source::Extras);
        assert_eq!(sources[0].playback_path, "/media/mv/a.mkv");
        assert!(cat.entry("x2").unwrap().is_none());
    }

    #[test]
    fn a_query_on_the_library_name_resolves_released_extras() {
        let cat = Catalog::open_in_memory().unwrap();
        write_items(
            &cat,
            &[
                item("x1", Some("/m/a.mkv"), false),
                item("x2", Some("/m/b.mkv"), false),
            ],
            "2026-11-05T00:00:00Z",
        )
        .unwrap();
        assert_eq!(
            cat.resolve_query(r#"item.library == "Music Videos""#)
                .unwrap(),
            vec!["x1".to_string(), "x2".to_string()]
        );
    }

    #[test]
    fn a_full_pass_marks_the_absent_missing_and_a_delta_does_not() {
        let cat = Catalog::open_in_memory().unwrap();
        let cancel = Arc::new(Cancel::default());
        let (url, server) = serve(vec![
            body(
                &[
                    ("x1", Some("/m/a.mkv"), false),
                    ("x2", Some("/m/b.mkv"), false),
                ],
                "c1",
            ),
            body(&[("x1", Some("/m/a.mkv"), false)], "c2"),
            body(&[], "c2"),
        ]);
        ingest(&cat, &url, true, &cancel).unwrap();
        assert_eq!(cat.extras_cursor().unwrap().as_deref(), Some("c1"));
        // x2 left the listing on a full pass: marked missing.
        let full = ingest(&cat, &url, true, &cancel).unwrap();
        assert_eq!(full.entries_marked_missing, 1);
        assert!(cat.entry("x2").unwrap().unwrap().missing_since.is_some());
        assert!(cat.entry("x1").unwrap().unwrap().missing_since.is_none());
        // A delta that mentions nothing marks nothing, and sends the cursor.
        let delta = ingest(&cat, &url, false, &cancel).unwrap();
        assert_eq!(delta.entries_marked_missing, 0);
        let requests = server.join().unwrap();
        assert_eq!(requests[0], "GET /api/extras/items HTTP/1.1");
        assert_eq!(requests[1], "GET /api/extras/items HTTP/1.1");
        assert_eq!(requests[2], "GET /api/extras/items?since=c2 HTTP/1.1");
    }

    #[test]
    fn an_unreachable_server_leaves_the_catalog_alone() {
        let cat = Catalog::open_in_memory().unwrap();
        let cancel = Arc::new(Cancel::default());
        write_items(
            &cat,
            &[item("x1", Some("/m/a.mkv"), false)],
            "2026-11-05T00:00:00Z",
        )
        .unwrap();
        let dead = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", dead.local_addr().unwrap());
        drop(dead);
        let err = ingest(&cat, &url, true, &cancel).unwrap_err();
        assert!(matches!(err, ExtrasIngestError::Http(_)), "{err}");
        assert!(cat.entry("x1").unwrap().unwrap().missing_since.is_none());
        assert_eq!(cat.last_extras_full_ingest().unwrap(), None);
    }
}

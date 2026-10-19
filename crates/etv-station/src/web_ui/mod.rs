//! The station's web dashboard, served by `etv-station --serve-ui`: one page
//! at `/` with a tab bar (Channels, Guide, Health) and, on Channels, a rail of
//! every lineup channel. A channel's Schedule is its review — the upcoming
//! schedule simulated as the daemon would run it, its depth, overlap and
//! programming grades, and why each airing is on it (`channel`). The rail is
//! the lineup in `station.yaml` order (`lineup`). The Tune page at `/tune`
//! (`tune`) edits a pool's taste profile live.
//!
//! The page's routes and the API's use the same nouns in the same order: the
//! page's `#/channels/{ch}/schedule` is answered by
//! `GET /api/channels/{ch}/review`, so a URL read off the page names its
//! endpoint. `{ch}` is the channel's lineup key, percent-encoded when it holds
//! a `/`.
//!
//! Nothing here writes to the station's config or data: Tune's profile edits
//! live in this process's memory, stills and simulations cache under the OS
//! temp directory. The server listens on the network in the container with no
//! auth of its own, so every channel a request names is checked against the
//! lineup. Requests are served one at a time on one thread; simulations run on
//! `sim_cache`'s worker thread and stills on their own threads, and the
//! container runs the whole process at `nice 19` so the channels' transcodes
//! always come first. `GET /api/health` returns what this process was started
//! with, including the ETV-next URL the Guide reads from (`ETV_NEXT_URL`). `GET
//! /api/airings/{airing}/audit` returns the stored audit record of one real
//! airing, which is how the Guide answers "why is this on" (`airing`).

mod airing;
mod channel;
mod lineup;
mod sim_cache;
mod tune;

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::catalog::{Catalog, Entry as CatalogEntry, TagNs, like_escape};
use crate::config::{self, DatastoreGrant, Pool};
use crate::profile::ProfileEntry;
use crate::score::{GrantedCapabilities, PickedItem, ScoreCache, ScoreInputs};
use crate::tautulli::{self, HistoryScope};
use crate::tune_in_still::{MediaRemap, Neighbours, StillJpeg, TuneInStill, Unavailable};
use airing::api_airing_audit;
use channel::{ReviewState, api_channel_review, still_reply};
use lineup::api_lineup;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use tiny_http::Method;
use tune::{
    Session, api_channels, api_profile_add, api_profile_remove, api_profile_update,
    api_search_item, api_search_keyword, api_search_tag, api_select, api_tautulli_users,
    discover_channels,
};

/// How [`serve`] was started. `etv-station --serve-ui` builds this from its
/// flags and the station config.
pub struct Options {
    /// The station config whose lineup the dashboard steps through, in its order.
    pub station: PathBuf,
    /// The station's catalog sqlite database, opened read-only.
    pub catalog: PathBuf,
    /// In-development plugin scripts a pool's `plugin:` is looked up in by
    /// file name (see `tune`). `None` scores with the deployed scripts.
    pub plugins_dir: Option<PathBuf>,
    /// Address to listen on: loopback locally, `0.0.0.0` in the container.
    pub bind: IpAddr,
    pub port: u16,
}

struct AppState {
    catalog: Catalog,
    options: Options,
    cache: ScoreCache,
    session: Option<Session>,
    review: ReviewState,
}

/// Listen on `options.bind:options.port` and serve until the process ends.
/// Fails only when the catalog cannot be opened or the port cannot be bound.
pub fn serve(options: Options) -> Result<(), String> {
    if cfg!(debug_assertions) {
        tracing::warn!(
            event = "ui.debug_build",
            "running a debug build: score::pick() takes 15-22s per call against the real \
             catalog — use a release build (tools/station-ui.sh does)"
        );
    }

    let catalog = Catalog::open_readonly(&options.catalog)
        .map_err(|e| format!("open catalog {}: {e}", options.catalog.display()))?;
    let server = tiny_http::Server::http((options.bind, options.port))
        .map_err(|e| format!("bind {}:{}: {e}", options.bind, options.port))?;
    tracing::info!(
        event = "ui.listen",
        bind = %options.bind,
        port = options.port,
        station = %options.station.display(),
        catalog = %options.catalog.display(),
        plugins_dir = ?options.plugins_dir,
        "web UI listening"
    );

    let review = ReviewState::new(options.station.clone(), options.catalog.clone());
    review.sims.spawn(channel::DEFAULT_REVIEW_DAYS);
    let mut state = AppState {
        catalog,
        cache: ScoreCache::default(),
        session: None,
        review,
        options,
    };
    for request in server.incoming_requests() {
        handle(&mut state, request);
    }
    Ok(())
}

/// `GET /api/health`: what this process serves from, and whether the station
/// config still loads — the state query `admin status` and the gate read.
/// 503 when the station config does not load, so a status check that reads
/// only the code sees a broken config as down.
fn api_health(state: &AppState) -> (u16, Value) {
    let o = &state.options;
    let channels = discover_channels(&o.station);
    let status = if channels.is_ok() { 200 } else { 503 };
    let sims = state.review.sims.stats();
    let body = json!({
        "sim_cache": {
            "in_memory": sims.in_memory,
            "queued": sims.queued,
            "running": sims.running,
        },
        "ok": channels.is_ok(),
        "station": o.station.display().to_string(),
        "catalog": o.catalog.display().to_string(),
        "plugins_dir": o.plugins_dir.as_ref().map(|p| p.display().to_string()),
        "bind": o.bind.to_string(),
        "port": o.port,
        "tunable_channels": channels.as_ref().map(Vec::len).ok(),
        "error": channels.err(),
        "revision": std::env::var("ETV_STATION_REVISION").ok(),
        "etv_next_url": std::env::var("ETV_NEXT_URL").ok().filter(|u| !u.is_empty()),
    });
    (status, body)
}

/// What a request is answered with. [`dispatch`] decides it without touching
/// the socket, so a test drives the router directly.
enum Reply {
    Json(u16, Value),
    Html(&'static str),
    /// A tune-in still a review offered under this id, rendered on its own
    /// thread by [`channel::respond_still`].
    Still(String, Box<TuneInStill>),
}

fn handle(state: &mut AppState, mut request: tiny_http::Request) {
    let method = request.method().clone();
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url.clone(), String::new()),
    };

    let mut body = String::new();
    if matches!(method, Method::Post) {
        let _ = std::io::Read::read_to_string(request.as_reader(), &mut body);
    }

    let began = std::time::Instant::now();
    match dispatch(state, &method, &path, &query, &body) {
        Reply::Html(html) => respond_html(request, html),
        Reply::Still(id, still) => {
            channel::respond_still(state.review.still_dir.clone(), id, *still, request)
        }
        Reply::Json(status, body) => {
            let error = body.get("error").and_then(|e| e.as_str());
            tracing::info!(
                event = "ui.request",
                method = %method,
                path = %path,
                status,
                error,
                ms = began.elapsed().as_millis() as u64,
                "request served"
            );
            respond_json(request, status, &body);
        }
    }
}

/// The router. A handler's `Err` is a 400 naming why.
fn dispatch(state: &mut AppState, method: &Method, path: &str, query: &str, body: &str) -> Reply {
    if *method == Method::Get {
        if let Some(id) = path.strip_prefix("/api/stills/") {
            return still_reply(state, &percent_decode(id));
        }
        if let Some(airing) = path
            .strip_prefix("/api/airings/")
            .and_then(|rest| rest.strip_suffix("/audit"))
        {
            return json_reply(api_airing_audit(state, &percent_decode(airing)));
        }
        if let Some(key) = path
            .strip_prefix("/api/channels/")
            .and_then(|rest| rest.strip_suffix("/review"))
        {
            return json_reply(api_channel_review(state, &percent_decode(key), query));
        }
    }
    let result = match (method, path) {
        (Method::Get, "/") => return Reply::Html(SHELL_HTML),
        (Method::Get, "/tune") => return Reply::Html(TUNE_HTML),
        (Method::Get, "/api/health") => Ok(api_health(state)),
        (Method::Get, "/api/lineup") => api_lineup(state),
        (Method::Get, "/api/channels") => api_channels(state),
        (Method::Post, "/api/select") => api_select(state, body),
        (Method::Post, "/api/profile/add") => api_profile_add(state, body),
        (Method::Post, "/api/profile/update") => api_profile_update(state, body),
        (Method::Post, "/api/profile/remove") => api_profile_remove(state, body),
        (Method::Get, "/api/search/keyword") => api_search_keyword(state, query),
        (Method::Get, "/api/search/tag") => api_search_tag(state, query),
        (Method::Get, "/api/search/item") => api_search_item(state, query),
        (Method::Get, "/api/tautulli/users") => api_tautulli_users(),
        _ => Ok((
            404,
            json!({"error": format!("no route: {} {}", method, path)}),
        )),
    };
    json_reply(result)
}

fn json_reply(result: Result<(u16, Value), String>) -> Reply {
    match result {
        Ok((status, body)) => Reply::Json(status, body),
        Err(e) => Reply::Json(400, json!({"error": e})),
    }
}

fn respond_json(request: tiny_http::Request, status: u16, body: &Value) {
    let text = body.to_string();
    let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
        .expect("static header");
    let response = tiny_http::Response::from_string(text)
        .with_status_code(status)
        .with_header(header);
    let _ = request.respond(response);
}

fn respond_jpeg(request: tiny_http::Request, body: Vec<u8>) {
    let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"image/jpeg"[..])
        .expect("static header");
    let response = tiny_http::Response::from_data(body).with_header(header);
    let _ = request.respond(response);
}

fn respond_html(request: tiny_http::Request, body: &str) {
    let header =
        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
            .expect("static header");
    let response = tiny_http::Response::from_string(body).with_header(header);
    let _ = request.respond(response);
}

/// `key=value&key2=value2`, percent-decoded, `+` read as a literal `+` (this
/// tool only ever sends plain text through these, never a form submission).
fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(percent_decode(k), percent_decode(v));
    }
    out
}

fn percent_decode(s: &str) -> String {
    // Works on raw bytes throughout — `s[i+1..i+3]` would panic on a `%`
    // immediately followed by a multi-byte UTF-8 character (not itself
    // percent-encoded), since that byte range can land mid-codepoint.
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

const SHELL_HTML: &str = include_str!("shell.html");
const TUNE_HTML: &str = include_str!("tune.html");

#[cfg(test)]
mod tests {
    use super::*;

    /// The router over the repo's example station and an empty catalog: no
    /// socket, no simulation worker.
    fn example_state() -> AppState {
        let station = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/station.yaml");
        AppState {
            catalog: Catalog::open_in_memory().expect("in-memory catalog"),
            cache: ScoreCache::default(),
            session: None,
            review: ReviewState::new(station.clone(), PathBuf::from("/nonexistent/catalog.db")),
            options: Options {
                station,
                catalog: PathBuf::from("/nonexistent/catalog.db"),
                plugins_dir: None,
                bind: IpAddr::from([127, 0, 0, 1]),
                port: 0,
            },
        }
    }

    fn get_json(state: &mut AppState, path: &str) -> (u16, Value) {
        match dispatch(state, &Method::Get, path, "", "") {
            Reply::Json(status, body) => (status, body),
            _ => panic!("{path} did not answer JSON"),
        }
    }

    #[test]
    fn lineup_lists_every_channel_in_station_order() {
        let mut state = example_state();
        let station =
            config::load_for_inspection(&state.options.station).expect("example station loads");
        let base = state.options.station.parent().unwrap().to_path_buf();
        let expected = lineup::lineup_keys(&station.lineup, &base);
        assert!(
            expected.len() > 1,
            "the example lineup should have several channels"
        );

        let (status, body) = get_json(&mut state, "/api/lineup");
        assert_eq!(status, 200);
        let keys: Vec<&str> = body["channels"]
            .as_array()
            .expect("channels array")
            .iter()
            .map(|c| c["channel"].as_str().expect("channel key"))
            .collect();
        assert_eq!(keys, expected);
    }

    #[test]
    fn a_percent_encoded_key_reaches_its_channel_and_answers_pending() {
        let mut state = example_state();
        // No worker runs here, so a channel the lineup holds stays queued.
        let (status, body) = get_json(&mut state, "/api/channels/channels%2Fa24.yaml/review");
        assert_eq!(status, 202, "{body}");
        assert_eq!(body["pending"], true);
        assert_eq!(body["channel"], "channels/a24.yaml");
    }

    #[test]
    fn a_channel_outside_the_lineup_is_refused() {
        let mut state = example_state();
        let (status, body) = get_json(&mut state, "/api/channels/not-a-channel/review");
        assert_eq!(status, 404);
        assert!(
            body["error"].as_str().unwrap().contains("not-a-channel"),
            "{body}"
        );
    }
}

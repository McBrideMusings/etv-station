//! The station's web dashboard, served by `etv-station --serve-ui`: one page
//! at `/` with a tab bar (Channels, Guide, Health) and, on Channels, a rail of
//! every lineup channel. A channel's Schedule is its review — the airings
//! the daemon has published and aired, a projection past them on request, their
//! depth, overlap and programming grades, and why each airing is on it
//! (`channel`); its Pools lists every pool with its sources, config, taste
//! profile and ranking, and lets a person try profile edits in a draft
//! (`pools`). The rail is the lineup in `station.yaml` order (`lineup`).
//!
//! The page's routes and the API's use the same nouns in the same order: the
//! page's `#/channels/{ch}/schedule` is answered by
//! `GET /api/channels/{ch}/review`, so a URL read off the page names its
//! endpoint. `{ch}` is the channel's lineup key, percent-encoded when it holds
//! a `/`.
//!
//! Nothing here writes to the station's config or data: a draft's profile
//! edits live in this process's memory, stills cache under the OS temp
//! directory and projections under the data volume beside the catalog. The server listens on the network in the container with no
//! auth of its own, so every channel a request names is checked against the
//! lineup. Requests are served one at a time on one thread; simulations run on
//! `projection`'s worker thread and stills on their own threads, and the
//! container runs the whole process at `nice 19` so the channels' transcodes
//! always come first. A channel's Graphics page renders its overlay and bumper
//! scripts the same way (`graphics`): `GET /api/channels/{ch}/graphics` lists
//! them, `POST /api/channels/{ch}/graphics/render` queues a render on its own
//! worker, and `GET /api/renders/{render}` answers 202 until it lands. `GET /api/health` returns what this process was started
//! with, including the ETV-next URL the Guide reads from (`ETV_NEXT_URL`). `GET
//! /api/airings/{airing}/audit` returns the stored audit record of one real
//! airing, which is how the Guide answers "why is this on" (`airing`).
//! `GET /assets/hls.min.js` serves the hls.js copy embedded in the binary, which
//! the Guide's player loads for browsers that cannot play HLS natively.
//! `GET /api/health/channels` is the Health tab: each channel's encoder verdict,
//! ETV-next's failure state and its last stall (`health`).

mod airing;
mod channel;
mod graphics;
mod health;
mod lineup;
mod pools;
mod projection;
mod real;

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
use graphics::{
    api_channel_graphics, api_graphics_render, api_render, render_file, respond_render_file,
};
pub use health::HealthSources;
use health::api_health_channels;
use lineup::api_lineup;
use pools::{
    PoolState, api_draft_discard, api_draft_edit, api_draft_open, api_pools, api_search_item,
    api_search_keyword, api_search_tag, api_tautulli_users, draft_yaml, scored_channel_count,
};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use tiny_http::Method;

/// How [`serve`] was started. `etv-station --serve-ui` builds this from its
/// flags and the station config.
pub struct Options {
    /// The station config whose lineup the dashboard steps through, in its order.
    pub station: PathBuf,
    /// The station's catalog sqlite database, opened read-only.
    pub catalog: PathBuf,
    /// In-development plugin scripts a pool's `plugin:` is looked up in by
    /// file name (see `pools`). `None` scores with the deployed scripts.
    pub plugins_dir: Option<PathBuf>,
    /// Address to listen on: loopback locally, `0.0.0.0` in the container.
    pub bind: IpAddr,
    pub port: u16,
    /// Where `GET /api/health/channels` reads from (`health`).
    pub health: HealthSources,
}

struct AppState {
    catalog: Catalog,
    options: Options,
    pools: PoolState,
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
    review.sims.spawn();
    let mut state = AppState {
        catalog,
        pools: PoolState::default(),
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
    let channels = scored_channel_count(&o.station);
    let status = if channels.is_ok() { 200 } else { 503 };
    let sims = state.review.sims.stats();
    let body = json!({
        "projections": {
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
        "tunable_channels": channels.as_ref().ok(),
        "drafts": state.pools.draft_count(),
        "error": channels.err(),
        "revision": std::env::var("ETV_STATION_REVISION").ok(),
        "build_time": std::env::var("ETV_STATION_CREATED").ok().filter(|t| !t.is_empty()),
        "etv_next_url": std::env::var("ETV_NEXT_URL").ok().filter(|u| !u.is_empty()),
    });
    (status, body)
}

/// What a request is answered with. [`dispatch`] decides it without touching
/// the socket, so a test drives the router directly.
enum Reply {
    Json(u16, Value),
    Html(&'static str),
    /// A pool's profile as YAML, pasted into a `channel.yaml` by hand.
    Yaml(String),
    /// A script the page loads from this server, so playback needs no CDN.
    Script(&'static str),
    /// A tune-in still a review offered under this id, rendered on its own
    /// thread by [`channel::respond_still`].
    Still(String, Box<TuneInStill>),
    /// A finished Graphics render's PNG or mp4.
    RenderFile(PathBuf, &'static str),
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

    let range = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Range"))
        .map(|h| h.value.to_string());
    let began = std::time::Instant::now();
    match dispatch(state, &method, &path, &query, &body) {
        Reply::Html(html) => respond_html(request, html),
        Reply::Yaml(text) => respond_yaml(request, text),
        Reply::Script(js) => respond_script(request, js),
        Reply::Still(id, still) => {
            channel::respond_still(state.review.still_dir.clone(), id, *still, request)
        }
        Reply::RenderFile(path, content_type) => {
            respond_render_file(request, &path, content_type, range.as_deref())
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
    if *method == Method::Get
        && let Some(id) = path.strip_prefix("/api/stills/")
    {
        return still_reply(state, &percent_decode(id));
    }
    if *method == Method::Get
        && let Some(airing) = path
            .strip_prefix("/api/airings/")
            .and_then(|rest| rest.strip_suffix("/audit"))
    {
        return json_reply(api_airing_audit(state, &percent_decode(airing)));
    }
    if let Some(rest) = path.strip_prefix("/api/channels/") {
        let segments: Vec<String> = rest.split('/').map(percent_decode).collect();
        let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
        match (method, segments.as_slice()) {
            (Method::Get, [key, "review"]) => {
                return json_reply(api_channel_review(state, key, query));
            }
            (Method::Get, [key, "pools"]) => return json_reply(api_pools(state, key, query)),
            (Method::Post, [key, "drafts"]) => {
                return json_reply(api_draft_open(state, key, body));
            }
            (Method::Post, [key, "drafts", id, "edits"]) => {
                return json_reply(api_draft_edit(state, key, id, body));
            }
            (Method::Get, [key, "drafts", id, "yaml"]) => {
                return match draft_yaml(state, key, id, query) {
                    Ok(text) => Reply::Yaml(text),
                    Err((status, error)) => Reply::Json(status, json!({ "error": error })),
                };
            }
            (Method::Delete, [key, "drafts", id]) => {
                return json_reply(api_draft_discard(state, key, id));
            }
            (Method::Get, [key, "graphics"]) => {
                return json_reply(api_channel_graphics(state, key));
            }
            (Method::Post, [key, "graphics", "render"]) => {
                return json_reply(api_graphics_render(state, key, body));
            }
            _ => {}
        }
    }
    if *method == Method::Get
        && let Some(rest) = path.strip_prefix("/api/renders/")
    {
        if let Some(id) = rest.strip_suffix("/file") {
            return match render_file(state, &percent_decode(id)) {
                Some((file, content_type)) => Reply::RenderFile(file, content_type),
                None => Reply::Json(404, json!({"error": format!("no finished render {id:?}")})),
            };
        }
        let (status, body) = api_render(state, &percent_decode(rest));
        return Reply::Json(status, body);
    }
    let result = match (method, path) {
        (Method::Get, "/") => return Reply::Html(SHELL_HTML),
        (Method::Get, "/assets/hls.min.js") => return Reply::Script(HLS_JS),
        (Method::Get, "/api/health") => Ok(api_health(state)),
        (Method::Get, "/api/health/channels") => api_health_channels(state),
        (Method::Get, "/api/lineup") => api_lineup(state),
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

fn respond_yaml(request: tiny_http::Request, body: String) {
    let header =
        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/yaml; charset=utf-8"[..])
            .expect("static header");
    let response = tiny_http::Response::from_string(body).with_header(header);
    let _ = request.respond(response);
}

fn respond_html(request: tiny_http::Request, body: &str) {
    let header =
        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
            .expect("static header");
    let response = tiny_http::Response::from_string(body).with_header(header);
    let _ = request.respond(response);
}

fn respond_script(request: tiny_http::Request, body: &str) {
    let content_type =
        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/javascript; charset=utf-8"[..])
            .expect("static header");
    let response = tiny_http::Response::from_string(body).with_header(content_type);
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
/// hls.js 1.5.17 (Apache-2.0, https://github.com/video-dev/hls.js), unmodified, for browsers without native HLS.
const HLS_JS: &str = include_str!("hls.min.js");

#[cfg(test)]
mod tests {
    use super::*;

    /// The router over the repo's example station and an empty catalog: no
    /// socket, no simulation worker.
    fn example_state() -> AppState {
        let station = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/station.yaml");
        AppState {
            catalog: Catalog::open_in_memory().expect("in-memory catalog"),
            pools: PoolState::default(),
            review: ReviewState::new(station.clone(), PathBuf::from("/nonexistent/catalog.db")),
            options: Options {
                station,
                catalog: PathBuf::from("/nonexistent/catalog.db"),
                plugins_dir: None,
                bind: IpAddr::from([127, 0, 0, 1]),
                port: 0,
                health: HealthSources::default(),
            },
        }
    }

    fn get_json(state: &mut AppState, path: &str) -> (u16, Value) {
        match dispatch(state, &Method::Get, path, "", "") {
            Reply::Json(status, body) => (status, body),
            _ => panic!("{path} did not answer JSON"),
        }
    }

    /// The Health endpoint over a diag folder: a software argv fails, a
    /// passing block four days old is stale, and an unreachable ETV-next is
    /// named under `upstream` without losing the encoder verdicts.
    #[test]
    fn health_judges_each_channels_newest_argv_block() {
        let mut state = example_state();
        let station =
            config::load_for_inspection(&state.options.station).expect("example station loads");
        let numbers: Vec<i64> = station.channels.iter().map(|c| c.config.number).collect();
        assert!(
            numbers.len() >= 3,
            "the example lineup should have several channels"
        );
        let diag = tempfile::tempdir().unwrap();
        let now = time::OffsetDateTime::now_utc();
        let stamp = |ago: time::Duration| {
            (now - ago)
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap()
        };
        let write = |n: i64, at: String, encoder: &str| {
            std::fs::write(
                diag.path().join(format!("ffmpeg-argv-ch{n}.log")),
                format!(
                    "=== {at} pid=1 channel={n}\nargv_begin\n-c:v\n{encoder}\n-vaapi_device\n/dev/dri/renderD128\nargv_end\n"
                ),
            )
            .unwrap();
        };
        write(numbers[0], stamp(time::Duration::minutes(5)), "libx264");
        write(numbers[1], stamp(time::Duration::days(4)), "h264_vaapi");
        write(numbers[2], stamp(time::Duration::minutes(5)), "h264_vaapi");
        state.options.health = HealthSources {
            diag_dir: Some(diag.path().to_path_buf()),
            etv_next_url: "http://127.0.0.1:1".into(),
            accel: Some("vaapi".into()),
            vaapi_device: Some("/dev/dri/renderD128".into()),
        };
        let (status, body) = get_json(&mut state, "/api/health/channels");
        assert_eq!(status, 200, "{body}");
        let verdict = |n: i64| {
            body["channels"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["channel"] == n)
                .map(|c| c["encoder"]["verdict"].as_str().unwrap().to_string())
                .unwrap()
        };
        assert_eq!(verdict(numbers[0]), "FAIL");
        assert_eq!(verdict(numbers[1]), "STALE");
        assert_eq!(verdict(numbers[2]), "PASS");
        assert_eq!(body["upstream"]["url"], "http://127.0.0.1:1");
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
    fn a_percent_encoded_key_reaches_its_channel_and_answers_its_real_schedule() {
        let mut state = example_state();
        let station =
            config::load_for_inspection(&state.options.station).expect("example station loads");
        let base = state.options.station.parent().unwrap().to_path_buf();
        let key = lineup::lineup_keys(&station.lineup, &base)
            .into_iter()
            .find(|k| k.contains('/'))
            .expect("the example lineup repeats folder names, so keys are paths");
        let encoded = key.replace('/', "%2F");
        let (status, body) = get_json(&mut state, &format!("/api/channels/{encoded}/review"));
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["channel"], key.as_str());
        assert_eq!(body["span"]["label"], "real only");
    }

    #[test]
    fn the_embedded_hls_script_is_served_from_the_dashboard() {
        let mut state = example_state();
        match dispatch(&mut state, &Method::Get, "/assets/hls.min.js", "", "") {
            Reply::Script(js) => assert!(js.contains("Hls"), "not hls.js"),
            _ => panic!("/assets/hls.min.js did not answer a script"),
        }
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

    /// A one-channel station on disk: 20 half-hour movies in the catalog, and
    /// one published chunk from local midnight to 01:00 UTC, so the frontier
    /// is a fixed instant.
    struct Fixture {
        dir: tempfile::TempDir,
        state: AppState,
    }

    const FIXTURE_CHANNEL: &str = "number: 1\nname: a\nwindow_days: 1\nroll_interval: \"1h\"\n\
        retention_days: 1\nchunk_hours: 6\nseed: 7\nrule:\n  blocks:\n    - mode: all\n      \
        pools:\n        - name: movies\n          expr: 'item.type == \"movie\"'\n          \
        order: random\n          advance: resume\n      pattern:\n        - pool: movies\n          take: 4\n";

    fn fixture() -> Fixture {
        use crate::catalog::{Entry, EntrySource, Source};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("channels/a")).unwrap();
        std::fs::write(root.join("channels/a/channel.yaml"), FIXTURE_CHANNEL).unwrap();
        std::fs::write(
            root.join("station.yaml"),
            format!(
                "tz: UTC\noutput_base: {out}\nchannels:\n  - channels/a/channel.yaml\ncatalog_path: {cat}\n\
                 normalization:\n  audio:\n    format: aac\n    bitrate_kbps: 192\n    buffer_kbps: 384\n    \
                 channels: 2\n    sample_rate_hz: 48000\n    normalize_loudness: false\n  video:\n    \
                 format: h264\n    bit_depth: 8\n    width: 1280\n    height: 720\n    bitrate_kbps: 2000\n    \
                 buffer_kbps: 4000\n  subtitle:\n    mode: convert\n",
                out = root.join("out").display(),
                cat = root.join("catalog.db").display(),
            ),
        )
        .unwrap();
        let writable = Catalog::open(root.join("catalog.db")).unwrap();
        for n in 0..20 {
            let id = format!("m{n:02}");
            let mut e = Entry::new(&id, "movie", &id, crate::catalog::Source::Plex);
            e.duration_ms = Some(30 * 60 * 1000);
            writable.upsert_entry(&e).unwrap();
            writable
                .add_source(&EntrySource {
                    source: Source::LocalFs,
                    source_id: format!("fs-{id}"),
                    entry_id: id.clone(),
                    playback_path: format!("/media/{id}.mkv"),
                    last_seen: None,
                    missing_since: None,
                    unairable_reason: None,
                })
                .unwrap();
        }
        drop(writable);

        let midnight = crate::tz::local_midnight_at_or_before(
            time::OffsetDateTime::now_utc(),
            crate::tz::parse("UTC").unwrap(),
        );
        let (start, finish) = (midnight, midnight + time::Duration::hours(1));
        let name =
            |t: time::OffsetDateTime| t.format(&ersatztv_playout::playout::DATE_FORMAT).unwrap();
        let out = root.join("out/a");
        std::fs::create_dir_all(&out).unwrap();
        let rfc = |t: time::OffsetDateTime| {
            t.format(&time::format_description::well_known::Rfc3339)
                .unwrap()
        };
        std::fs::write(
            out.join(format!("{}_{}.json", name(start), name(finish))),
            format!(
                r#"{{"version":"https://ersatztv.org/playout/version/0.0.3","items":[
                {{"id":"m00","start":"{}","finish":"{}"}},
                {{"id":"m01","start":"{}","finish":"{}"}}]}}"#,
                rfc(start),
                rfc(start + time::Duration::minutes(30)),
                rfc(start + time::Duration::minutes(30)),
                rfc(finish),
            ),
        )
        .unwrap();

        let station = root.join("station.yaml");
        let catalog_path = root.join("catalog.db");
        let state = AppState {
            catalog: Catalog::open_readonly(&catalog_path).unwrap(),
            pools: PoolState::default(),
            review: ReviewState::new(station.clone(), catalog_path.clone()),
            options: Options {
                station,
                catalog: catalog_path,
                plugins_dir: None,
                bind: IpAddr::from([127, 0, 0, 1]),
                port: 0,
                health: HealthSources::default(),
            },
        };
        state.review.sims.spawn();
        Fixture { dir, state }
    }

    fn review(state: &mut AppState, query: &str) -> (u16, Value) {
        match dispatch(state, &Method::Get, "/api/channels/a/review", query, "") {
            Reply::Json(status, body) => (status, body),
            _ => panic!("review did not answer JSON"),
        }
    }

    /// Asks again until the worker has finished the projection.
    fn projected(state: &mut AppState, query: &str) -> Value {
        for _ in 0..600 {
            let (status, body) = review(state, query);
            if status == 200 {
                return body;
            }
            assert_eq!(status, 202, "{body}");
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("the projection never finished");
    }

    #[test]
    fn the_schedule_opens_on_the_real_playout_with_no_simulation() {
        let mut f = fixture();
        let (status, body) = review(&mut f.state, "");
        assert_eq!(status, 200, "{body}");
        let airings = body["airings"].as_array().unwrap();
        let ids: Vec<&str> = airings.iter().map(|a| a["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["m00", "m01"]);
        assert!(airings.iter().all(|a| a["projected"] == false));
        assert_eq!(body["span"]["label"], "real only");
        assert_eq!(body["projection"], Value::Null);
        let frontier = body["frontier"].as_i64().unwrap();
        assert_eq!(body["span"]["real_until"], frontier);
        assert_eq!(body["span"]["projected_until"], Value::Null);
    }

    #[test]
    fn a_projection_continues_past_the_frontier_and_is_stable_across_catalog_touches() {
        let mut f = fixture();
        let first = projected(&mut f.state, "project=3d");
        let frontier = first["frontier"].as_i64().unwrap();
        let airings = first["airings"].as_array().unwrap();
        let (real, ahead): (Vec<&Value>, Vec<&Value>) =
            airings.iter().partition(|a| a["projected"] == false);
        assert_eq!(real.len(), 2);
        assert!(!ahead.is_empty());
        assert!(
            ahead
                .iter()
                .all(|a| a["start"].as_i64().unwrap() >= frontier)
        );
        assert_eq!(first["span"]["projected_until"], frontier + 3 * 86_400);
        assert_eq!(first["span"]["label"], "real + 3 days projected");
        let key = first["projection"]["key"].as_str().unwrap().to_string();

        // Touching only the catalog file changes no input, so the same
        // projection comes back at once.
        let catalog = std::fs::File::options()
            .write(true)
            .open(f.dir.path().join("catalog.db"))
            .unwrap();
        catalog
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3600))
            .unwrap();
        let (status, again) = review(&mut f.state, "project=3d");
        assert_eq!(status, 200, "{again}");
        assert_eq!(again["projection"]["key"], key);
        assert_eq!(again["airings"], first["airings"]);

        // Editing the channel file changes the key, and the page is told which input.
        let channel = f.dir.path().join("channels/a/channel.yaml");
        let edited = std::fs::read_to_string(&channel)
            .unwrap()
            .replace("take: 4", "take: 3");
        std::fs::write(&channel, edited).unwrap();
        let (status, pending) = review(&mut f.state, "project=3d");
        assert_eq!(status, 202, "{pending}");
        assert_ne!(pending["projection"]["key"], key.as_str());
        let after = projected(&mut f.state, "project=3d");
        assert_eq!(
            after["projection"]["changed"],
            serde_json::json!(["config"])
        );
    }

    /// The fixture channel with an inline overlay naming one script and a
    /// block that fills gaps with one bumper script, both written beside the
    /// channel file.
    fn graphics_fixture() -> Fixture {
        let f = fixture();
        let dir = f.dir.path().join("channels/a");
        let card = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../etv-overlay/fixtures/scripts/text_card.rhai");
        std::fs::copy(card, dir.join("card.rhai")).unwrap();
        std::fs::write(
            dir.join("bumper.rhai"),
            "fn hooks() { [\"bumper\"] }\n\
             fn build(ctx) { let t = timeline(); \
             t.push(segment().black().name(\"x\").flex(3.0, 15.0, 1).looped()); t }\n",
        )
        .unwrap();
        let channel = FIXTURE_CHANNEL
            .replace(
                "rule:\n",
                "overlay:\n  width: 640\n  height: 360\n  framerate: 30\n  graphics:\n    - script: card.rhai\nrule:\n",
            )
            .replace(
                "- mode: all\n",
                "- mode: all\n      align: 30m\n      fill:\n        - script: bumper.rhai\n",
            );
        std::fs::write(dir.join("channel.yaml"), channel).unwrap();
        f
    }

    fn post_render(state: &mut AppState, body: Value) -> (u16, Value) {
        match dispatch(
            state,
            &Method::Post,
            "/api/channels/a/graphics/render",
            "",
            &body.to_string(),
        ) {
            Reply::Json(status, body) => (status, body),
            _ => panic!("render did not answer JSON"),
        }
    }

    #[test]
    fn graphics_lists_the_overlay_and_bumper_scripts_the_channel_names() {
        let mut f = graphics_fixture();
        let (status, body) = get_json(&mut f.state, "/api/channels/a/graphics");
        assert_eq!(status, 200, "{body}");
        let dir = f.dir.path().join("channels/a");
        let name = |list: &str| body[list][0]["name"].as_str().unwrap().to_string();
        assert_eq!(name("overlays"), "card.rhai");
        assert_eq!(name("bumpers"), "bumper.rhai");
        assert!(
            body["overlays"][0]["script"]
                .as_str()
                .unwrap()
                .ends_with("channels/a/card.rhai"),
            "{body}"
        );
        assert!(Path::new(body["bumpers"][0]["script"].as_str().unwrap()).starts_with(&dir));
        assert_eq!(body["canvas"], json!({"width": 640, "height": 360}));
    }

    #[test]
    fn a_channel_naming_no_scripts_lists_none() {
        let mut f = fixture();
        let (status, body) = get_json(&mut f.state, "/api/channels/a/graphics");
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["overlays"], json!([]));
        assert_eq!(body["bumpers"], json!([]));
    }

    #[test]
    fn a_render_of_a_script_the_channel_does_not_name_is_refused() {
        let mut f = graphics_fixture();
        let (status, body) = post_render(
            &mut f.state,
            json!({"kind": "still", "script": "/etc/passwd"}),
        );
        assert_eq!(status, 400, "{body}");
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("not an overlay script")
        );
        let bumper = f.dir.path().join("channels/a/card.rhai");
        let (status, body) = post_render(
            &mut f.state,
            json!({"kind": "bumper", "script": bumper.display().to_string()}),
        );
        assert_eq!(status, 400, "{body}");
        let (status, _) = post_render(&mut f.state, json!({"kind": "gif", "script": "x"}));
        assert_eq!(status, 400);
        let script = f.dir.path().join("channels/a/bumper.rhai");
        let (status, body) = post_render(
            &mut f.state,
            json!({"kind": "bumper", "script": script.display().to_string(), "gap": "soon"}),
        );
        assert_eq!(status, 400, "{body}");
    }

    #[test]
    fn saving_a_script_asks_for_a_new_render_and_an_unchanged_one_is_served_from_disk() {
        let mut f = graphics_fixture();
        let script = f.dir.path().join("channels/a/card.rhai");
        let ask =
            serde_json::json!({"kind": "still", "script": script.display().to_string(), "at": 2.0});

        let (status, first) = post_render(&mut f.state, ask.clone());
        assert_eq!(status, 202, "{first}");
        let id = first["render"].as_str().unwrap().to_string();

        // The renderer is not built here, so the render fails; wait for that
        // so no worker is still running when the cache entry is written.
        for _ in 0..600 {
            let (_, body) = get_json(&mut f.state, &format!("/api/renders/{id}"));
            if body["state"] == "failed" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        // A finished render on disk is a cache hit with no new work.
        let dir = f.state.review.renders.dir.clone();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{id}.png")), b"png").unwrap();
        std::fs::write(
            dir.join(format!("{id}.json")),
            json!({"kind": "still", "script": "x", "file": format!("{id}.png"),
                   "width": 640, "height": 360, "duration": null, "log": ""})
            .to_string(),
        )
        .unwrap();
        let (status, hit) = post_render(&mut f.state, ask.clone());
        assert_eq!(status, 200, "{hit}");
        assert_eq!(hit["cached"], true);
        assert_eq!(hit["render"], id.as_str());
        let (status, done) = match dispatch(
            &mut f.state,
            &Method::Get,
            &format!("/api/renders/{id}"),
            "",
            "",
        ) {
            Reply::Json(status, body) => (status, body),
            _ => panic!("render did not answer JSON"),
        };
        assert_eq!(status, 200, "{done}");
        assert_eq!(done["state"], "done");
        assert_eq!(done["width"], 640);
        assert_eq!(done["url"], format!("/api/renders/{id}/file"));

        // Saving the script changes a file the render reads: a different render.
        let file = std::fs::File::options().write(true).open(&script).unwrap();
        file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3600))
            .unwrap();
        let (status, fresh) = post_render(&mut f.state, ask);
        assert_eq!(status, 202, "{fresh}");
        assert_ne!(fresh["render"], id.as_str());
    }

    #[test]
    fn an_unknown_or_path_shaped_render_id_is_a_404() {
        let mut f = graphics_fixture();
        for id in ["still-0000", "..%2Fx"] {
            let (status, body) = get_json(&mut f.state, &format!("/api/renders/{id}"));
            assert_eq!(status, 404, "{body}");
        }
    }

    #[test]
    fn a_project_length_outside_the_choices_is_refused() {
        let mut f = fixture();
        let (status, body) = review(&mut f.state, "project=5d");
        assert_eq!(status, 400, "{body}");
    }
}

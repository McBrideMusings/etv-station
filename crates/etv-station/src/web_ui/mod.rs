//! The station's web UI, served by `etv-station --serve-ui`: a Review page
//! per channel (its upcoming schedule, turnover and depth, scores) and a tuning
//! page that iterates a pool's taste-profile weights against real data
//! (etv-station-sctf.8). It runs over `score::{ScoreCache, pick}`, the real
//! scoring path (ADR-0020): a channel picker, live keyword/genre/item search,
//! and one unified schedule (etv-station-sctf.9) that re-scores in place as a
//! profile entry is added, reweighted, or removed — no file write, no process
//! restart. Nothing it does writes to the station's config or data; profile
//! edits live in this process's memory and stills render under the OS temp
//! directory. Scoring runs the pool's real plugin with its granted
//! capabilities, so a tuning request can make the `exec`/`http` calls that
//! plugin is granted, exactly as a generation would.
//!
//! The schedule is the daemon's, replayed ([`simulate_schedule`], built on
//! `crate::simulate`): the same generation loop, the same
//! `ctx.target_count` per generation, and each generation's airings fed
//! forward as the next one's `ctx.recent`, so a scorer's repeat suppression
//! shows up in the schedule as it does on air. A pool's own ranked list (the
//! candidate table) is a different view: one full `pick()` with empty
//! `ctx.recent` and no exploration slots, i.e. what the profile alone ranks
//! first.
//!
//! A run of consecutive airings of one show from one pool (a season the
//! pattern took whole) renders as ONE schedule entry (a "visit"), not one row
//! per episode; the grouping is read off the catalog's `show_id`.
//!
//! Two properties shape the server:
//!
//! - **Perf.** Against the real ~11,634-movie catalog, a debug build's
//!   `pick()` takes 15-22s, so run a release build (`tools/station-ui.sh`
//!   does), and [`score_pool`] below runs `pick()` exactly once per pool per
//!   edit, only for the pool an edit actually touched — every other pool's
//!   cached ranking is untouched. Requests are served one at a time on one
//!   thread, and the replay is single-threaded, so the UI uses at most one
//!   core however many browsers are open (the Review view's schedules are the
//!   exception: a worker thread simulates them, see `sim_cache`); the container also runs it at
//!   `nice 19` so the channels' transcodes always come first.
//! - **Fidelity.** The pool's own real `sources:` (or, absent one, its
//!   script's own default `sources()`) is always what gets scored against —
//!   never `sources: None`'s silent whole-library substitute. Achieved by
//!   cloning the pool straight out of the real `channel.yaml` (see
//!   [`clone_pool`]) and mutating only its `profile` field for live edits;
//!   `plugin`/`sources`/`capabilities`/`datastores`/`config` all stay
//!   exactly what the channel author wrote.
//!
//! The channels it offers are the station config's lineup, and `POST
//! /api/select` refuses any other path: the server listens on the network in
//! the container, with no auth of its own.
//!
//! The plugin script scored against is the pool's own deployed one, unless
//! [`Options::plugins_dir`] names a directory of in-development scripts —
//! `tools/station-ui.sh` points it at this checkout's `examples/plugins/` —
//! in which case the pool's script is looked up there by file name, and `POST
//! /api/select`'s `use_deployed_plugin: true` reaches the deployed one.
//!
//! Each pool's plexdb path comes from its `datastores:` grant, not a flag —
//! set `PLEXDB_SNAPSHOT_PATH` (or whatever env var the grant references).
//! `GET /api/health` returns what this process was started with.

mod sim_cache;

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
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};

/// How [`serve`] was started. `etv-station --serve-ui` builds this from its
/// flags and the station config.
pub struct Options {
    /// The station config whose lineup both pages step through, in its order.
    pub station: PathBuf,
    /// The station's catalog sqlite database, opened read-only.
    pub catalog: PathBuf,
    /// In-development plugin scripts a pool's `plugin:` is looked up in by
    /// file name (see module docs). `None` scores with the deployed scripts.
    pub plugins_dir: Option<PathBuf>,
    /// Address to listen on: loopback locally, `0.0.0.0` in the container.
    pub bind: IpAddr,
    pub port: u16,
}

/// A working copy of a real pool's config — everything but `profile` stays
/// exactly what the channel author wrote. `Pool` derives `Deserialize` +
/// `Serialize` but not `Clone` (it is not on `config::Pool`'s own type, and
/// this tool has no reason to add it there), so a JSON round trip stands in.
fn clone_pool(pool: &Pool) -> Result<Pool, String> {
    let value = serde_json::to_value(pool).map_err(|e| format!("clone pool: {e}"))?;
    serde_json::from_value(value).map_err(|e| format!("clone pool: {e}"))
}

/// One `pattern:` step of the selected channel — which pool it draws from and
/// whether that pool is one this tool can score (has a `plugin:`). Only the
/// plugin-backed pools get a working copy the person can edit; the simulated
/// schedule ([`simulate_schedule`]) runs every pool the channel has, so a
/// static pool's items still appear where the pattern puts them.
struct PlanStep {
    block_index: usize,
    pool_name: String,
    has_plugin: bool,
}

/// One lineup channel with at least one plugin-backed pattern pool — the
/// channels the tuning page offers.
struct DiscoveredChannel {
    channel_path: PathBuf,
    channel_name: String,
    display_name: Option<String>,
}

/// Every `pattern:` step across `channel`'s blocks, in block-then-pattern order.
fn pattern_plan(channel: &config::ChannelConfig) -> Vec<PlanStep> {
    let mut out = Vec::new();
    for (block_index, block) in channel.rule.all_blocks().enumerate() {
        if !block.is_pattern() {
            continue;
        }
        for step in &block.pattern {
            let has_plugin = block
                .pools
                .iter()
                .find(|p| p.name == step.pool)
                .is_some_and(|p| p.plugin.is_some());
            out.push(PlanStep {
                block_index,
                pool_name: step.pool.clone(),
                has_plugin,
            });
        }
    }
    out
}

/// Every channel in the station's lineup, in lineup order, that loads and has
/// a plugin-backed pattern pool. Re-read on every call, so a channel edit
/// shows without a restart.
fn discover_channels(station_path: &Path) -> Result<Vec<DiscoveredChannel>, String> {
    let station = config::load_for_inspection(station_path)
        .map_err(|e| format!("load {}: {e}", station_path.display()))?;
    let mut out = Vec::new();
    for channel_path in &station.lineup {
        let Ok(channel) = config::read_channel(channel_path) else {
            continue;
        };
        if !pattern_plan(&channel).iter().any(|s| s.has_plugin) {
            continue;
        }
        out.push(DiscoveredChannel {
            channel_path: channel_path.clone(),
            channel_name: channel_key(channel_path),
            display_name: channel.display_name.clone(),
        });
    }
    Ok(out)
}

/// One plugin-backed pool referenced by the selected channel's pattern —
/// a live-edited working copy of its profile, plus the last full ranked
/// list `score_pool` computed for it. `full` is `None` until the first
/// (re)score, and stays populated across an edit to a *different* pool: only
/// [`score_pool`] on this pool's own name recomputes it, which is what makes
/// a weight tweak on one pool cheap instead of re-scoring the whole channel.
///
/// `sources_effective` and `candidates_json` cache work `build_payload`
/// would otherwise redo for every pool on every request, not only the one an
/// edit touched — `sources_effective` never changes for the pool's lifetime
/// in this session (it depends only on the plugin script and the pool's own
/// `sources:`, neither of which this tool ever mutates), so it's computed
/// once in [`build_pool_working`]. `candidates_json` depends on `full`, so
/// it's recomputed exactly when `full` is, inside [`score_pool`], instead of
/// on every response.
struct PoolWorking {
    pool: Pool,
    plugin_path: PathBuf,
    plexdb_path: PathBuf,
    datastore_name: String,
    full: Option<Vec<PickedItem>>,
    sources_effective: Option<Vec<(String, String)>>,
    candidates_json: Vec<Value>,
}

/// The currently selected channel and the live-edited working copy of every
/// plugin-backed pool its pattern draws from. Rebuilt wholesale by `POST
/// /api/select`; every profile-editing endpoint mutates one [`PoolWorking`]
/// in place and re-scores only that pool.
struct Session {
    channel_path: PathBuf,
    channel_dir: PathBuf,
    channel_name: String,
    account_id: Option<i64>,
    /// How many days of roll ticks the simulated schedule covers.
    days: u32,
    /// The channel exactly as its `channel.yaml` reads, as JSON: `Pool` and
    /// `ChannelConfig` are not `Clone`, so [`simulate_schedule`] rebuilds a
    /// fresh config from this each run and swaps in the working pools.
    config_json: Value,
    pools: HashMap<String, PoolWorking>,
    /// The simulated schedule, cached: it is recomputed by
    /// [`resimulate`] after a score, not on every request.
    schedule: Vec<Value>,
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
    review.sims.spawn(DEFAULT_REVIEW_DAYS);
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
    });
    (status, body)
}

fn handle(state: &mut AppState, mut request: tiny_http::Request) {
    let method = request.method().clone();
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url.clone(), String::new()),
    };

    let mut body = String::new();
    if matches!(method, tiny_http::Method::Post) {
        let _ = std::io::Read::read_to_string(request.as_reader(), &mut body);
    }

    let began = std::time::Instant::now();
    let result: Result<(u16, Value), String> = match (&method, path.as_str()) {
        (tiny_http::Method::Get, "/") => {
            respond_html(request, INDEX_HTML);
            return;
        }
        (tiny_http::Method::Get, "/review") => {
            respond_html(request, REVIEW_HTML);
            return;
        }
        (tiny_http::Method::Get, "/api/health") => Ok(api_health(state)),
        (tiny_http::Method::Get, "/api/review") => api_review(state, &query),
        (tiny_http::Method::Get, "/api/review/still") => {
            serve_review_still(state, request, &query);
            return;
        }
        (tiny_http::Method::Get, "/api/channels") => api_channels(state),
        (tiny_http::Method::Post, "/api/select") => api_select(state, &body),
        (tiny_http::Method::Post, "/api/profile/add") => api_profile_add(state, &body),
        (tiny_http::Method::Post, "/api/profile/update") => api_profile_update(state, &body),
        (tiny_http::Method::Post, "/api/profile/remove") => api_profile_remove(state, &body),
        (tiny_http::Method::Get, "/api/search/keyword") => api_search_keyword(state, &query),
        (tiny_http::Method::Get, "/api/search/tag") => api_search_tag(state, &query),
        (tiny_http::Method::Get, "/api/search/item") => api_search_item(state, &query),
        (tiny_http::Method::Get, "/api/tautulli/users") => api_tautulli_users(),
        _ => Ok((
            404,
            json!({"error": format!("no route: {} {}", method, path)}),
        )),
    };

    let (status, body) = match result {
        Ok((status, body)) => (status, body),
        Err(e) => (400, json!({"error": e})),
    };
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

fn api_channels(state: &AppState) -> Result<(u16, Value), String> {
    let channels = discover_channels(&state.options.station)?;
    let list: Vec<Value> = channels
        .iter()
        .map(|c| {
            json!({
                "channel_path": c.channel_path.display().to_string(),
                "channel_name": c.channel_name,
                "display_name": c.display_name,
            })
        })
        .collect();
    Ok((200, json!({ "channels": list })))
}

#[derive(serde::Deserialize)]
struct SelectRequest {
    channel_path: String,
    #[serde(default)]
    account_id: Option<i64>,
    #[serde(default)]
    use_deployed_plugin: bool,
    #[serde(default)]
    days: Option<u32>,
}

/// Build the working copy of one plugin-backed pool named by a pattern step —
/// resolves which plugin script to score against (deployed, or its namesake
/// in [`Options::plugins_dir`], see module docs), expands its datastore
/// grant's env references, and clones the pool so profile edits never touch
/// the real `channel.yaml`.
fn build_pool_working(
    state: &AppState,
    channel_dir: &Path,
    pool: &Pool,
    use_deployed_plugin: bool,
) -> Result<PoolWorking, String> {
    let plugin = pool
        .plugin
        .as_ref()
        .ok_or_else(|| format!("pool {:?} has no `plugin:`", pool.name))?;
    let deployed_path = if plugin.is_absolute() {
        plugin.clone()
    } else {
        channel_dir.join(plugin)
    };

    let plugin_path = match &state.options.plugins_dir {
        Some(dir) if !use_deployed_plugin => {
            let basename = deployed_path.file_name().ok_or_else(|| {
                format!("plugin path {} has no file name", deployed_path.display())
            })?;
            dir.join(basename)
        }
        _ => deployed_path.clone(),
    };
    if !plugin_path.is_file() {
        return Err(format!(
            "plugin script not found at {} (deployed copy is {})",
            plugin_path.display(),
            deployed_path.display()
        ));
    }

    if pool.datastores.is_empty() {
        return Err(format!("pool {:?} declares no datastores", pool.name));
    }

    let mut pool_owned = clone_pool(pool)?;
    // `ScoreCache::prepare_profile` opens `pool.datastores.first()` itself,
    // reading its raw `path` — unlike `score::pick`, which this tool hands
    // an explicitly-expanded grant. Expanding every grant here once, on the
    // working copy, is what makes a `keyword:` profile entry resolve instead
    // of failing to open a literal `${PLEXDB_SNAPSHOT_PATH}`.
    for ds in &mut pool_owned.datastores {
        ds.path = config::expand_process_env(&ds.path)?;
    }
    let first_grant = pool_owned
        .datastores
        .first()
        .expect("checked non-empty above");
    let plexdb_path = PathBuf::from(&first_grant.path);
    let datastore_name = first_grant.name.clone();

    // Computed once here rather than per-request (see `PoolWorking`'s doc):
    // depends only on the plugin script and the pool's own `sources:`,
    // neither of which changes for the rest of this session.
    let sources_effective =
        crate::score::effective_sources(&plugin_path, pool_owned.sources.as_ref()).ok();

    Ok(PoolWorking {
        pool: pool_owned,
        plugin_path,
        plexdb_path,
        datastore_name,
        full: None,
        sources_effective,
        candidates_json: Vec::new(),
    })
}

fn api_select(state: &mut AppState, body: &str) -> Result<(u16, Value), String> {
    let req: SelectRequest =
        serde_json::from_str(body).map_err(|e| format!("bad request body: {e}"))?;

    let channel_path = PathBuf::from(&req.channel_path);
    // Only a lineup channel: the path comes from the browser, and the server
    // is reachable over the network with no auth of its own.
    if !discover_channels(&state.options.station)?
        .iter()
        .any(|c| c.channel_path == channel_path)
    {
        return Err(format!(
            "{:?} is not a tunable channel in this station's lineup",
            req.channel_path
        ));
    }
    let channel = config::read_channel(&channel_path).map_err(|e| e.to_string())?;
    let channel_dir = channel_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let channel_name = channel_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let plan = pattern_plan(&channel);
    if !plan.iter().any(|s| s.has_plugin) {
        return Err(format!(
            "channel {:?} has no plugin-backed pattern pool",
            req.channel_path
        ));
    }

    let account_id = match req.account_id {
        Some(id) => Some(id),
        None => channel_account_id(&channel)?,
    };

    // A fresh cache, not a reused one: `ScoreCache::prepare_profile` only
    // ever runs once per generation in the daemon, so it treats "this pool's
    // profile is empty" as "nothing to resolve" and leaves whatever it
    // resolved last time sitting in its cache — correct there, wrong here,
    // where the same channel can be re-selected many times with different
    // (or emptied-back-out) profiles each time.
    state.cache = ScoreCache::default();

    let mut pools = HashMap::new();
    for step in plan.iter().filter(|s| s.has_plugin) {
        if pools.contains_key(&step.pool_name) {
            continue;
        }
        let pool = channel
            .rule
            .all_blocks()
            .nth(step.block_index)
            .and_then(|b| b.pools.iter().find(|p| p.name == step.pool_name))
            .ok_or_else(|| format!("pool {:?} vanished from its own block", step.pool_name))?;
        let working = build_pool_working(state, &channel_dir, pool, req.use_deployed_plugin)?;
        pools.insert(step.pool_name.clone(), working);
    }

    let pool_names: Vec<String> = pools.keys().cloned().collect();
    state.session = Some(Session {
        channel_path,
        channel_dir,
        channel_name,
        account_id,
        days: req.days.unwrap_or(DEFAULT_DAYS).clamp(1, MAX_DAYS),
        config_json: serde_json::to_value(&channel).map_err(|e| format!("channel config: {e}"))?,
        pools,
        schedule: Vec::new(),
    });

    let mut errors = Vec::new();
    for name in &pool_names {
        if let Err(e) = score_pool(state, name) {
            errors.push(format!("{name}: {e}"));
        }
    }
    let all_pools_failed = errors.len() == pool_names.len();
    if !all_pools_failed && let Err(e) = resimulate(state) {
        errors.push(format!("schedule: {e}"));
    }

    // `plan` has at least one plugin step (checked above), so `pool_names`
    // is never empty and "every pool failed" is distinct from "none did".
    let response = if errors.is_empty() {
        (200, build_payload(state))
    } else if all_pools_failed {
        (400, payload_with_error(state, errors.join("; ")))
    } else {
        let message = format!("some pools failed: {}", errors.join("; "));
        (200, payload_with_error(state, message))
    };
    Ok(response)
}

/// The Plex account a channel's scorer reads history for: none for
/// `taste_scope: all_users`, else the one Tautulli resolves its single user to.
fn channel_account_id(channel: &config::ChannelConfig) -> Result<Option<i64>, String> {
    match channel.history_scope() {
        HistoryScope::AllUsers => Ok(None),
        scope @ HistoryScope::User(_) => {
            let (url, key) = tautulli::credentials_from_env().ok_or_else(|| {
                "scoring.taste_scope is single_user, but TAUTULLI_URL/TAUTULLI_API_KEY \
                 aren't set (source .env)"
                    .to_string()
            })?;
            let rows = tautulli::fetch_rows(&url, &key, &scope);
            Ok(Some(
                tautulli::resolve_account_id(&scope, &rows)?
                    .ok_or_else(|| "single_user scope resolved to no account".to_string())?,
            ))
        }
    }
}

/// [`build_payload`] with an `"error"` field added — a failed score still
/// returns every pool's real config and the schedule, not a blank page.
fn payload_with_error(state: &AppState, error: String) -> Value {
    let mut payload = build_payload(state);
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("error".to_string(), json!(error));
    }
    payload
}

/// Re-score one pool in place, replay the schedule, and rebuild the whole
/// payload — the shape every profile-editing endpoint returns. Only the
/// touched pool re-runs its own full ranking (the candidate list); the
/// simulated schedule runs the channel's whole generation loop again, because
/// an edit to one pool changes what it draws in every later generation.
fn rescore_and_respond(state: &mut AppState, pool_name: &str) -> (u16, Value) {
    if let Err(e) = score_pool(state, pool_name) {
        // The edit is already on the working copy, so a schedule replayed
        // before it no longer describes the profile on screen.
        if let Some(session) = state.session.as_mut() {
            session.schedule.clear();
        }
        return (400, payload_with_error(state, e));
    }
    match resimulate(state) {
        Ok(()) => (200, build_payload(state)),
        Err(e) => (200, payload_with_error(state, format!("schedule: {e}"))),
    }
}

fn session_pool_mut<'a>(
    session: &'a mut Session,
    pool_name: &str,
) -> Result<&'a mut PoolWorking, String> {
    session
        .pools
        .get_mut(pool_name)
        .ok_or_else(|| format!("no pool named {pool_name:?} in the selected channel"))
}

#[derive(serde::Deserialize)]
struct ProfileAddRequest {
    pool_name: String,
    #[serde(flatten)]
    entry: ProfileEntry,
}

fn api_profile_add(state: &mut AppState, body: &str) -> Result<(u16, Value), String> {
    let req: ProfileAddRequest =
        serde_json::from_str(body).map_err(|e| format!("bad request body: {e}"))?;
    // Validate the shape before it ever reaches the session's profile — the
    // same check `profile::load` runs at real config load, so a bad entry
    // fails here with the same message a channel author would see, not a
    // stranger one out of `pick()`.
    crate::profile::check(&req.entry).map_err(|e| format!("profile entry {e}"))?;

    let session = state
        .session
        .as_mut()
        .ok_or_else(|| "no channel selected".to_string())?;
    let pool = session_pool_mut(session, &req.pool_name)?;
    pool.pool.profile.push(req.entry);
    Ok(rescore_and_respond(state, &req.pool_name))
}

#[derive(serde::Deserialize)]
struct ProfileUpdateRequest {
    pool_name: String,
    index: usize,
    weight: f64,
}

fn api_profile_update(state: &mut AppState, body: &str) -> Result<(u16, Value), String> {
    let req: ProfileUpdateRequest =
        serde_json::from_str(body).map_err(|e| format!("bad request body: {e}"))?;
    if !req.weight.is_finite() || req.weight == 0.0 {
        return Err("weight must be finite and nonzero".to_string());
    }
    let session = state
        .session
        .as_mut()
        .ok_or_else(|| "no channel selected".to_string())?;
    let pool = session_pool_mut(session, &req.pool_name)?;
    let entry = pool
        .pool
        .profile
        .get_mut(req.index)
        .ok_or_else(|| format!("no profile entry at index {}", req.index))?;
    entry.weight = Some(req.weight);
    Ok(rescore_and_respond(state, &req.pool_name))
}

#[derive(serde::Deserialize)]
struct ProfileRemoveRequest {
    pool_name: String,
    index: usize,
}

fn api_profile_remove(state: &mut AppState, body: &str) -> Result<(u16, Value), String> {
    let req: ProfileRemoveRequest =
        serde_json::from_str(body).map_err(|e| format!("bad request body: {e}"))?;
    let session = state
        .session
        .as_mut()
        .ok_or_else(|| "no channel selected".to_string())?;
    let pool = session_pool_mut(session, &req.pool_name)?;
    if req.index >= pool.pool.profile.len() {
        return Err(format!("no profile entry at index {}", req.index));
    }
    pool.pool.profile.remove(req.index);
    Ok(rescore_and_respond(state, &req.pool_name))
}

/// One pool's real config, independent of whether scoring it succeeds —
/// `sources:`/`config:`/which plugin script, all read straight off the pool
/// with no catalog access and nothing that can fail. Used to answer "what am
/// I actually pointed at" even when `pick()` errors out (a schema mismatch
/// between the picked plugin script and this pool's `config:`, most often),
/// so a failed select still shows the real pool instead of leaving the page
/// blank.
fn describe_pool(name: &str, pw: &PoolWorking) -> Value {
    let sources_authored = pw.pool.sources.is_some();
    // What the pool will actually score against, whether or not it wrote its
    // own `sources:` — cached on `pw` at select time (see `PoolWorking`'s
    // doc), not recomputed per request. `None` here means `build_pool_working`
    // failed to compile the script, which already surfaces as the pool's own
    // `pick()` error elsewhere, so it's shown as unknown rather than twice.
    let sources_effective = pw
        .sources_effective
        .clone()
        .map(|pairs| Value::Object(pairs.into_iter().map(|(k, v)| (k, json!(v))).collect()));
    json!({
        "pool_name": name,
        "plugin_path": pw.plugin_path.display().to_string(),
        "plexdb_path": pw.plexdb_path.display().to_string(),
        "sources_authored": sources_authored,
        "sources_effective": sources_effective,
        "config": pw.pool.config.clone().unwrap_or_else(|| json!({})),
        "config_authored": pw.pool.config.is_some(),
        "profile_yaml": profile_yaml(&pw.pool),
        "candidate_count": pw.full.as_ref().map(|f| f.len()).unwrap_or(0),
    })
}

/// The session's current `profile:` entries, rendered exactly as they'd read
/// pasted into the pool's real `channel.yaml` — copy-to-clipboard is the
/// only write-back this tool offers. These files carry extensive
/// hand-written prose comments (see `deploy/appdata/channels/002-for-pierce/
/// channel.yaml`'s tuning notes) that a round-tripped YAML rewrite would
/// silently destroy, so the tool never edits the file itself; the person
/// pastes this in by hand, wherever it belongs. Empty string when the
/// profile has nothing in it (the real file just omits the key entirely —
/// `Pool::profile`'s own `skip_serializing_if`).
fn profile_yaml(pool: &Pool) -> String {
    if pool.profile.is_empty() {
        return String::new();
    }
    let list = serde_norway::to_string(&pool.profile).unwrap_or_default();
    let indented = list
        .lines()
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else {
                format!("  {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("profile:\n{indented}\n")
}

/// Run one full-pool `pick()` for `pool_name` against its real `sources:`,
/// real (working-copy) profile, real capabilities/datastores — see the
/// module docs for why one call per pool, not two — and store the ranked
/// list on its [`PoolWorking::full`] for the candidate table.
/// Every other pool's `full` is untouched, which is what makes a weight
/// tweak on one pool cheap.
fn score_pool(state: &mut AppState, pool_name: &str) -> Result<(), String> {
    const EXTENDED_TARGET_COUNT: usize = 300;

    let session = state
        .session
        .as_ref()
        .ok_or_else(|| "no channel selected".to_string())?;
    let account_id = session.account_id;
    let channel_dir = session.channel_dir.clone();
    let pw = session
        .pools
        .get(pool_name)
        .ok_or_else(|| format!("no pool named {pool_name:?} in the selected channel"))?;
    let plugin_path = pw.plugin_path.clone();
    let plexdb_path = pw.plexdb_path.display().to_string();
    let datastore_name = pw.datastore_name.clone();

    state
        .cache
        .prepare_pool(&state.catalog, &pw.pool, &plugin_path, &channel_dir)
        .map_err(|e| format!("prepare: {e}"))?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let mut full_config = pw.pool.config.clone().unwrap_or_else(|| json!({}));
    if let Some(obj) = full_config.as_object_mut() {
        obj.insert("exploration_fraction".to_string(), json!(0.0));
    }
    let inputs = ScoreInputs {
        target_count: EXTENDED_TARGET_COUNT,
        now,
        account_id,
        ..Default::default()
    };
    let grant = GrantedCapabilities::from_names(&pw.pool.capabilities).with_datastores(&[
        DatastoreGrant {
            name: datastore_name,
            path: plexdb_path,
        },
    ])?;

    let full = crate::score::pick(
        &state.cache,
        &plugin_path,
        pw.pool.sources.as_ref(),
        &inputs,
        crate::variety::Dice::fixed(0),
        pool_name,
        Some(&full_config),
        grant,
    )
    .map_err(|e| format!("pick: {e}"))?;

    // Computed here, alongside `full`, rather than by `build_payload` on
    // every request — this is the one place `full` actually changes, so
    // it's the one place its derived `candidates` view needs to change too.
    let candidates_json: Vec<Value> = full
        .iter()
        .take(CANDIDATES_SHOWN)
        .map(|item| {
            let entry = state.catalog.entry(&item.id).ok().flatten();
            picked_item_json(
                &state.catalog,
                &item.id,
                item.metadata.as_ref(),
                entry.as_ref(),
            )
        })
        .collect();

    let session = state.session.as_mut().expect("checked above");
    let pw = session_pool_mut(session, pool_name).expect("checked above");
    pw.full = Some(full);
    pw.candidates_json = candidates_json;
    Ok(())
}

/// How many of a pool's ranked candidates the payload carries — the full
/// list can be thousands of rows (a movies pool ranks the whole library),
/// and every one already costs a catalog lookup (see [`picked_item_json`]),
/// so this caps it. `candidate_count` in [`describe_pool`] still reports
/// the true total.
const CANDIDATES_SHOWN: usize = 300;

/// Every pool's description — including its own ranked-candidate list, up to
/// [`CANDIDATES_SHOWN`], each carrying the same real audit detail the
/// schedule's picks do (`score::pick`'s `audit()` call covers every
/// candidate it returned, not only the ones a pattern step drew, so this is
/// the same data, not a second query) — plus the channel's unified,
/// pattern-interleaved schedule (etv-station-sctf.9). The whole response
/// body for `/api/select` and every profile-editing endpoint.
fn build_payload(state: &AppState) -> Value {
    let session = state.session.as_ref().expect("caller checked");
    let mut pools_json = serde_json::Map::new();
    for (name, pw) in &session.pools {
        let mut desc = describe_pool(name, pw);
        if let Some(obj) = desc.as_object_mut() {
            obj.insert("profile".to_string(), profile_json(&state.cache, name));
            // Cached on `pw` by `score_pool`, the one place `full` (and so
            // this derived view) actually changes — a pool an edit didn't
            // touch pays nothing here, not even the catalog lookups a fresh
            // `picked_item_json` per candidate would cost.
            obj.insert("candidates".to_string(), json!(pw.candidates_json));
        }
        pools_json.insert(name.clone(), desc);
    }
    json!({
        "channel_path": session.channel_path.display().to_string(),
        "channel_name": session.channel_name,
        "account_id": session.account_id,
        "days": session.days,
        "pools": pools_json,
        "schedule": session.schedule,
    })
}

/// Days of roll ticks the simulated schedule covers when a request names none,
/// and the most it may ask for. Each generation runs every pool's full ranking
/// again (about 10s on the real catalog), and a day holds anywhere from a few
/// generations (a pattern cycle that runs ten hours) to one per hour, so this
/// bounds how long an edit takes to show.
const DEFAULT_DAYS: u32 = 3;
const MAX_DAYS: u32 = 14;

/// Replay the selected channel's schedule and cache it on the session.
fn resimulate(state: &mut AppState) -> Result<(), String> {
    let session = state
        .session
        .as_ref()
        .ok_or_else(|| "no channel selected".to_string())?;
    // The station's zone, so `rule.schedules` place slots on the local clock
    // here as they do on air.
    let tz = config::load_for_inspection(&state.review.station_path)
        .map_err(|e| format!("load {}: {e}", state.review.station_path.display()))
        .and_then(|st| crate::tz::parse(&st.station.tz).map_err(|e| e.to_string()))?;
    let result = simulate_schedule(&state.catalog, session, tz);
    let session = state.session.as_mut().expect("checked above");
    match result {
        Ok(schedule) => {
            session.schedule = schedule;
            Ok(())
        }
        Err(e) => {
            session.schedule.clear();
            Err(e)
        }
    }
}

/// Run the daemon's generation loop ([`crate::simulate::simulate`]) over
/// the channel as `channel.yaml` writes it, with each plugin pool replaced by
/// its live-edited working copy — so a profile weight changed in the UI
/// changes what every later generation draws, and the pool's repeat
/// suppression (`ctx.recent`) sees what the earlier generations aired.
///
/// One schedule entry per run of consecutive airings from the same pool and
/// the same catalog `show_id` — a season the pattern took whole is one "visit"
/// block, a film is a "single". Pool and block come from the airing's own
/// `select` audit stage and `block` index, not from a second reading of the
/// pattern.
fn simulate_schedule(
    catalog: &Catalog,
    session: &Session,
    tz: &'static time_tz::Tz,
) -> Result<Vec<Value>, String> {
    let mut config: config::ChannelConfig = serde_json::from_value(session.config_json.clone())
        .map_err(|e| format!("channel config: {e}"))?;
    for block in config.rule.all_blocks_mut() {
        for pool in &mut block.pools {
            let Some(pw) = session.pools.get(&pool.name) else {
                continue;
            };
            let mut working = clone_pool(&pw.pool)?;
            // The script this session scores against (the checkout's own by
            // default, see the module docs), absolute so it does not resolve
            // against the channel directory.
            working.plugin = Some(
                std::path::absolute(&pw.plugin_path)
                    .map_err(|e| format!("plugin path {}: {e}", pw.plugin_path.display()))?,
            );
            *pool = working;
        }
    }
    let block_names: Vec<String> = config
        .rule
        .all_blocks()
        .enumerate()
        .map(|(i, b)| {
            b.program()
                .and_then(|p| p.title.clone())
                .or_else(|| config.rule.slot_name(i).map(str::to_string))
                .unwrap_or_else(|| format!("block {i}"))
        })
        .collect();

    let start = time::OffsetDateTime::now_utc();
    let sim = crate::simulate::simulate(
        &config,
        &session.channel_path,
        catalog,
        session.account_id,
        session.days,
        start,
        Some(tz),
    )?;

    let mut out = Vec::new();
    for generation in &sim.generations {
        let looked_up: Vec<Option<CatalogEntry>> = generation
            .airings
            .iter()
            .map(|a| catalog.entry(&a.id).ok().flatten())
            .collect();
        let pools: Vec<String> = generation
            .airings
            .iter()
            .map(|a| {
                drawn_from_pool(a.metadata.as_ref()).unwrap_or_else(|| "(no pool)".to_string())
            })
            .collect();

        let mut i = 0;
        while i < generation.airings.len() {
            let mut j = i + 1;
            if let Some(show_id) = generation.airings[i].show_id.as_deref() {
                while j < generation.airings.len()
                    && generation.airings[j].show_id.as_deref() == Some(show_id)
                    && pools[j] == pools[i]
                {
                    j += 1;
                }
            }
            let first = &generation.airings[i];
            let entries: Vec<Value> = (i..j)
                .map(|k| {
                    let a = &generation.airings[k];
                    picked_item_json(catalog, &a.id, a.metadata.as_ref(), looked_up[k].as_ref())
                })
                .collect();
            out.push(json!({
                "generation": generation.index,
                "target_count": generation.target_count,
                "offset_secs": (first.start - sim.start).whole_seconds(),
                "block_index": first.block,
                "block_name": block_names.get(first.block).cloned().unwrap_or_default(),
                "pool_name": pools[i],
                "kind": if j - i > 1 { "visit" } else { "single" },
                "entries": entries,
            }));
            i = j;
        }
    }
    Ok(out)
}

/// Where a plugin pool's pick stood in the list it walked: its `rank` (the
/// walk position, after any temperature), its `score_rank` (by score alone),
/// the `temperature` that separated the two, and whether the slot was an
/// exploration `draw` — read off the plugin's own `pool` audit stage, `null`
/// for an airing no plugin ranked. Under a rotating seed this is what shows
/// the walk: ranks climb 1, 2, 3 through one period's list and start over at
/// a rollover.
///
/// The `select` stage (the core's own record of the draw) adds the seed and
/// temperature the visit drew with, the period index each was in, and where
/// in that period's list the pick sat — `null` fields on a pool nothing
/// rotates.
fn ranking_of(metadata: Option<&Value>) -> Value {
    let stage = |name: &str| {
        metadata
            .and_then(|m| m.get("audit"))
            .and_then(Value::as_array)
            .and_then(|stages| {
                stages
                    .iter()
                    .find(|stage| stage.get("stage").and_then(Value::as_str) == Some(name))
            })
            .and_then(|stage| stage.get("detail"))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let (pool, select) = (stage("pool"), stage("select"));
    if pool.is_null() && select.is_null() {
        return Value::Null;
    }
    json!({
        "rank": pool.get("rank"),
        "score_rank": pool.get("score_rank"),
        "draw": pool.get("draw"),
        "seed": select.get("seed"),
        "temperature": select.get("temperature"),
        "seed_period": select.get("seed_period"),
        "temperature_period": select.get("temperature_period"),
        "list_position": select.get("list_position"),
        "list_length": select.get("list_length"),
    })
}

/// The pool an airing was drawn from, read off the pattern engine's `select`
/// audit stage (`detail.pool`).
fn drawn_from_pool(metadata: Option<&Value>) -> Option<String> {
    metadata?
        .get("audit")?
        .as_array()?
        .iter()
        .find(|stage| stage.get("stage").and_then(Value::as_str) == Some("select"))?
        .get("detail")?
        .get("pool")?
        .as_str()
        .map(str::to_string)
}

/// One picked item, ready for the schedule — title/show/season/episode read
/// from `entry` (the caller's own catalog lookup, reused rather than
/// repeated here — see [`simulate_schedule`]'s grouping loop), plus its own
/// `audit` record (the plugin's `audit()` output `score::pick` already
/// merged into `metadata.audit`, #389/#392/#393) with every near-miss's id
/// resolved to a title too, so the inspector never has to look anything up
/// itself.
fn picked_item_json(
    catalog: &Catalog,
    id: &str,
    metadata: Option<&Value>,
    entry: Option<&CatalogEntry>,
) -> Value {
    let (title, year, show, season, episode) = match entry {
        Some(e) => (e.title.clone(), e.year, e.show.clone(), e.season, e.episode),
        None => ("<unknown>".to_string(), None, None, None, None),
    };
    json!({
        "id": id,
        "title": title,
        "year": year,
        "show": show,
        "season": season,
        "episode": episode,
        "metadata": metadata,
        "audit": enrich_audit(catalog, metadata),
    })
}

/// The first `audit()` record `score::pick` attached to this item's
/// `metadata.audit` (module docs: `pick()` runs `audit()` itself and merges
/// its stage records in, keyed to the picked item), with every
/// `detail.near_misses[].id` resolved to a title/year — the near-miss list
/// is entry ids and a score/reason on its own (see `taste-cosine.rhai`'s
/// `audit()`), useless to a reader without knowing what they name.
fn enrich_audit(catalog: &Catalog, metadata: Option<&Value>) -> Option<Value> {
    let mut audit = metadata?
        .get("audit")?
        .as_array()?
        .iter()
        .find(|stage| stage.get("stage").and_then(Value::as_str) != Some("select"))?
        .clone();
    let near_misses = audit
        .get_mut("detail")
        .and_then(|d| d.get_mut("near_misses"))
        .and_then(|nm| nm.as_array_mut());
    if let Some(near_misses) = near_misses {
        for row in near_misses.iter_mut() {
            let id = row.get("id").and_then(|v| v.as_str()).map(str::to_string);
            let Some(id) = id else { continue };
            if let Ok(Some(e)) = catalog.entry(&id)
                && let Some(obj) = row.as_object_mut()
            {
                obj.insert("title".to_string(), json!(e.title));
                obj.insert("year".to_string(), json!(e.year));
            }
        }
    }
    Some(audit)
}

/// The `pool.profile` index an entry of `ctx.profile` edits: its 1-based
/// `position` within the inline list, less one. `ctx.profile` omits
/// `exclude`/`require` entries and lists `profile_files` entries first, so its
/// own order is not `pool.profile`'s. A file entry has no `pool.profile` slot
/// (the lab edits the working copy's inline list only), so its index is null.
fn inline_index(entry: &serde_json::Map<String, Value>) -> Value {
    let inline = entry.get("origin").and_then(Value::as_str) == Some("inline");
    match entry.get("position").and_then(Value::as_u64) {
        Some(pos) if inline && pos >= 1 => json!(pos - 1),
        _ => Value::Null,
    }
}

fn profile_json(cache: &ScoreCache, pool_name: &str) -> Value {
    let Some(resolved) = cache.profile(pool_name) else {
        return json!({ "entries": [], "ignore_keywords": [], "exclusions": [] });
    };
    let entries: Vec<Value> = resolved
        .entries
        .iter()
        .map(|dyn_entry| {
            let mut v = rhai::serde::from_dynamic::<Value>(dyn_entry).unwrap_or_else(|_| json!({}));
            if let Some(obj) = v.as_object_mut() {
                let index = inline_index(obj);
                obj.insert("index".to_string(), index);
            }
            v
        })
        .collect();
    let ignore_keywords: Vec<Value> = resolved
        .ignore_keywords
        .iter()
        .map(|d| rhai::serde::from_dynamic::<Value>(d).unwrap_or(Value::Null))
        .collect();
    let exclusions: Vec<Value> = resolved
        .exclusions
        .iter()
        .map(|x| {
            json!({
                "reference": x.reference,
                "origin": x.origin,
                "entry_ids": x.entry_ids,
            })
        })
        .collect();
    json!({
        "entries": entries,
        "ignore_keywords": ignore_keywords,
        "exclusions": exclusions,
    })
}

fn open_plexdb_readonly(path: &Path) -> Result<Connection, String> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("open {}: {e}", path.display()))
}

/// The plexdb path to search against: the `pool_name` query param if one was
/// given, else the session's first pool (by name — a channel's plugin-backed
/// pools nearly always share one `datastores:` grant, per module docs, so
/// any of them resolves the same plexdb).
fn resolve_search_plexdb(
    session: &Session,
    params: &HashMap<String, String>,
) -> Result<PathBuf, String> {
    let pw = match params.get("pool_name") {
        Some(name) => session
            .pools
            .get(name)
            .ok_or_else(|| format!("no pool named {name:?} in the selected channel"))?,
        None => session
            .pools
            .iter()
            .min_by_key(|(name, _)| name.as_str())
            .map(|(_, pw)| pw)
            .ok_or_else(|| "selected channel has no plugin-backed pool".to_string())?,
    };
    Ok(pw.plexdb_path.clone())
}

fn api_search_keyword(state: &AppState, query: &str) -> Result<(u16, Value), String> {
    let params = parse_query(query);
    let q = params.get("q").cloned().unwrap_or_default();
    let session = state
        .session
        .as_ref()
        .ok_or_else(|| "no channel selected".to_string())?;
    let plexdb_path = resolve_search_plexdb(session, &params)?;
    let conn = open_plexdb_readonly(&plexdb_path)?;
    let pattern = format!("%{}%", like_escape(&q));
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT surface, keyword FROM keyword_forms \
             WHERE surface LIKE ?1 COLLATE NOCASE OR keyword LIKE ?1 COLLATE NOCASE \
             ORDER BY surface LIMIT 30",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([&pattern], |r| {
            Ok(json!({
                "surface": r.get::<_, String>(0)?,
                "keyword": r.get::<_, String>(1)?,
            }))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok((200, json!({ "results": rows })))
}

fn api_search_tag(state: &AppState, query: &str) -> Result<(u16, Value), String> {
    let params = parse_query(query);
    let namespace = params
        .get("namespace")
        .ok_or_else(|| "missing `namespace`".to_string())?;
    let ns: TagNs = namespace
        .parse()
        .map_err(|e: String| format!("bad namespace {namespace:?}: {e}"))?;
    let q = params.get("q").cloned().unwrap_or_default();
    let values = state
        .catalog
        .search_tag_values(ns, &q, 30)
        .map_err(|e| e.to_string())?;
    Ok((200, json!({ "results": values })))
}

fn api_search_item(state: &AppState, query: &str) -> Result<(u16, Value), String> {
    let params = parse_query(query);
    let q = params.get("q").cloned().unwrap_or_default();
    let rows = state
        .catalog
        .search_titles(&q, 30)
        .map_err(|e| e.to_string())?;
    let results: Vec<Value> = rows
        .into_iter()
        .map(|(id, title, year)| json!({ "id": id, "title": title, "year": year }))
        .collect();
    Ok((200, json!({ "results": results })))
}

fn api_tautulli_users() -> Result<(u16, Value), String> {
    let (url, key) = tautulli::credentials_from_env()
        .ok_or_else(|| "TAUTULLI_URL/TAUTULLI_API_KEY aren't set".to_string())?;
    let users = tautulli::fetch_users(&url, &key)?;
    let results: Vec<Value> = users
        .iter()
        .map(|u| json!({ "user_id": u.user_id, "display_name": u.display_name() }))
        .collect();
    Ok((200, json!({ "results": results })))
}

// ---------------------------------------------------------------------------
// Review view (`/review`, `/api/review`): one channel's simulated schedule as
// a report — depth before repeats, film/series share, overlap with the rest of
// the lineup, seeded tune-in samples and a day-by-day guide. It reads the
// channel exactly as `channel.yaml` writes it (its own plugin scripts, no
// working-copy profile), reloaded on every request. The simulation itself is
// cached by `sim_cache` and filled by a worker thread, keyed on the mtimes of
// the channel file, its scripts and the catalog, so an edit to any of them
// shows on the next reload; a request that misses the cache gets 202
// `{"pending": true}` and the page asks again. The numbers are
// `crate::review`'s.
// ---------------------------------------------------------------------------

/// Days the Review view simulates when a request names none, and the most it
/// may ask for.
const DEFAULT_REVIEW_DAYS: u32 = 14;
const MAX_REVIEW_DAYS: u32 = 60;

/// How many seeded tune-in moments the view samples.
const TUNE_IN_SAMPLES: usize = 5;

/// What the Review view keeps between requests.
struct ReviewState {
    station_path: PathBuf,
    /// Each channel's candidate entry-id set, keyed by config path and
    /// recomputed when the file's mtime changes. A failure is not cached, so
    /// the next review retries it.
    fingerprints: HashMap<PathBuf, (std::time::SystemTime, Arc<HashSet<String>>)>,
    /// Every movie id and every episode id in the catalog, for telling a
    /// channel that draws from the whole library apart from one that doesn't.
    library: Option<(HashSet<String>, HashSet<String>)>,
    /// The last review's comparable numbers per channel, for "Since last run".
    previous: HashMap<String, Vec<(String, Value)>>,
    /// Every tune-in still a review has offered, keyed by the file name its
    /// URL carries, so `/api/review/still` renders it on first request.
    stills: HashMap<String, TuneInStill>,
    /// Where rendered stills are kept. A still's file name is a digest of
    /// everything that decides it and the directory names this binary's
    /// build, so a file here is never stale.
    still_dir: PathBuf,
    /// Simulated schedules, filled by a worker thread so no request waits on one.
    sims: Arc<sim_cache::SimCache>,
}

impl ReviewState {
    fn new(station_path: PathBuf, catalog_path: PathBuf) -> Self {
        // One directory per build of this binary, so a rebuilt renderer or
        // simulator never serves what the previous one produced.
        let build = std::env::current_exe()
            .and_then(|p| p.metadata())
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs().to_string())
            .unwrap_or_default();
        Self {
            sims: sim_cache::SimCache::new(
                station_path.clone(),
                catalog_path,
                std::env::temp_dir()
                    .join("etv-station-ui-sims")
                    .join(&build),
            ),
            station_path,
            fingerprints: HashMap::new(),
            library: None,
            previous: HashMap::new(),
            stills: HashMap::new(),
            still_dir: std::env::temp_dir()
                .join("etv-station-ui-stills")
                .join(&build),
        }
    }
}

/// A channel's folder name (`034-syfy`).
fn channel_key(config_path: &Path) -> String {
    config_path
        .parent()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Each lineup entry's key in the Review view's URLs: its folder name when
/// every folder name in the lineup is distinct, else every entry's path as
/// written relative to the station file, so two keys never name one channel.
fn lineup_keys(lineup: &[PathBuf], base: &Path) -> Vec<String> {
    let folders: Vec<String> = lineup.iter().map(|p| channel_key(p)).collect();
    if folders.iter().collect::<HashSet<_>>().len() == folders.len() {
        return folders;
    }
    lineup
        .iter()
        .map(|p| p.strip_prefix(base).unwrap_or(p).display().to_string())
        .collect()
}

/// The picker's options: one per lineup entry, in lineup order, each with the
/// key the Review URL takes. A channel that loaded carries its number and
/// display name; one that was dropped has only its key.
fn lineup_options(station: &config::Station, keys: &[String]) -> Vec<Value> {
    station
        .lineup
        .iter()
        .zip(keys)
        .map(|(path, key)| {
            let loaded = station.channels.iter().find(|c| c.config_path == *path);
            json!({
                "key": key,
                "number": loaded.map(|c| c.config.number),
                "display_name": loaded.and_then(|c| c.config.display_name.clone()),
            })
        })
        .collect()
}

fn file_mtime(path: &Path) -> Result<std::time::SystemTime, String> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// How many upcoming placements per rotation the Review JSON previews.
const ROTATION_PREVIEW_PLACEMENTS: usize = 14;

fn api_review(state: &mut AppState, query: &str) -> Result<(u16, Value), String> {
    let began = std::time::Instant::now();
    let params = parse_query(query);
    let station = config::load_for_inspection(&state.review.station_path)
        .map_err(|e| format!("load {}: {e}", state.review.station_path.display()))?;

    // Every channel file the station's `channels:` patterns found, loaded or
    // not, in station.yaml order.
    let base = state
        .review
        .station_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let keys = lineup_keys(&station.lineup, &base);
    let key = match params.get("channel") {
        Some(k) => k.clone(),
        None => keys.first().cloned().ok_or("the station has no channels")?,
    };
    let position = keys.iter().position(|k| *k == key).ok_or_else(|| {
        format!(
            "no channel {key:?} in {}",
            state.review.station_path.display()
        )
    })?;
    let lineup_json = json!({
        "position": position + 1,
        "total": keys.len(),
        "prev": position.checked_sub(1).map(|i| keys[i].clone()),
        "next": keys.get(position + 1).cloned(),
        "channels": lineup_options(&station, &keys),
    });
    let days = params
        .get("days")
        .and_then(|d| d.parse::<u32>().ok())
        .unwrap_or(DEFAULT_REVIEW_DAYS)
        .clamp(1, MAX_REVIEW_DAYS);

    let config_path = &station.lineup[position];
    if let Some(dropped) = station
        .dropped
        .iter()
        .find(|d| d.config_path == *config_path)
    {
        tracing::warn!(event = "ui.review.load_failed", channel = %key, reason = %dropped.reason, "review: channel failed to load");
        return Ok((
            200,
            json!({
                "channel": key,
                "config_path": dropped.config_path.display().to_string(),
                "days": days,
                "lineup": lineup_json,
                "load_error": dropped.reason,
            }),
        ));
    }
    let lc = station
        .channels
        .iter()
        .find(|c| c.config_path == *config_path)
        .ok_or_else(|| format!("{key} is in the lineup but neither loaded nor dropped"))?;

    let tz = crate::tz::parse(&station.station.tz).map_err(|e| e.to_string())?;
    let start = crate::tz::local_midnight_at_or_before(time::OffsetDateTime::now_utc(), tz);
    let mut day_starts = vec![start];
    for _ in 0..days {
        let last = *day_starts.last().expect("non-empty");
        day_starts.push(crate::tz::add_chunk(last, 24, tz));
    }
    let end = *day_starts.last().expect("non-empty");

    let sim_key = sim_cache::SimKey {
        config_path: lc.config_path.clone(),
        days,
        start: start.unix_timestamp(),
    };
    let fp = sim_cache::fingerprint(
        &state.options.station,
        &lc.config_path,
        &lc.config,
        &state.options.catalog,
    );
    let (sim, sim_source) = match state.review.sims.lookup(&sim_key, &fp) {
        sim_cache::Lookup::Ready(sim) => (sim, "cache"),
        sim_cache::Lookup::Failed(e) => return Err(e),
        sim_cache::Lookup::Pending => {
            tracing::info!(event = "ui.review.pending", channel = %key, days, "review: simulation queued");
            return Ok((
                202,
                json!({ "pending": true, "channel": key, "days": days }),
            ));
        }
    };

    let block_names: Vec<String> = lc
        .config
        .rule
        .all_blocks()
        .enumerate()
        .map(|(i, b)| {
            b.program()
                .and_then(|p| p.title.clone())
                .or_else(|| lc.config.rule.slot_name(i).map(str::to_string))
                .unwrap_or_else(|| format!("block {i}"))
        })
        .collect();
    let mut entries: HashMap<String, Option<CatalogEntry>> = HashMap::new();
    let mut airings: Vec<crate::review::ReviewAiring> = Vec::new();
    let mut simulated: Vec<&crate::simulate::SimulatedAiring> = Vec::new();
    for a in sim.generations.iter().flat_map(|g| &g.airings) {
        // Every airing, past the span's end too, so the last one in the span
        // still has a successor for its tune-in still's NEXT card. Airings
        // run in time order, so `simulated[i]` is `airings[i]` for each `i`
        // the span keeps.
        simulated.push(a);
        if a.start >= end {
            continue;
        }
        let entry = entries
            .entry(a.id.clone())
            .or_insert_with(|| state.catalog.entry(&a.id).ok().flatten());
        airings.push(crate::review::ReviewAiring {
            id: a.id.clone(),
            title: entry
                .as_ref()
                .map(airing_label)
                .unwrap_or_else(|| a.id.clone()),
            kind: entry
                .as_ref()
                .map(|e| e.kind.clone())
                .unwrap_or_else(|| "unknown".to_string()),
            show: entry.as_ref().and_then(|e| e.show.clone()),
            show_id: a.show_id.clone(),
            season: entry.as_ref().and_then(|e| e.season),
            episode: entry.as_ref().and_then(|e| e.episode),
            pool: drawn_from_pool(a.metadata.as_ref())
                .unwrap_or_else(|| block_names.get(a.block).cloned().unwrap_or_default()),
            start: a.start,
            duration: a.duration,
        });
    }

    let review = build_review(state, &station, lc, &airings, &simulated, start, end)?;
    let summary = comparable_numbers(&review);
    let since_last = state.review.previous.insert(key.clone(), summary.clone());
    let since_last_json = since_last.map(|old| {
        let old: HashMap<String, Value> = old.into_iter().collect();
        summary
            .iter()
            .map(|(label, new)| {
                let old = old.get(label).cloned().unwrap_or(Value::Null);
                json!({ "label": label, "old": old, "new": new, "changed": old != *new })
            })
            .collect::<Vec<_>>()
    });

    let nominal_secs = lc
        .config
        .scoring
        .as_ref()
        .map(|s| s.nominal_item_secs)
        .unwrap_or_else(|| config::ScoringConfig::default().nominal_item_secs);
    let mut notices = Vec::new();
    let unprobed = airings
        .iter()
        .filter(|a| {
            entries
                .get(&a.id)
                .and_then(|e| e.as_ref())
                .and_then(|e| e.duration_ms)
                .is_none()
        })
        .count();
    if unprobed > 0 {
        notices.push(format!(
            "{unprobed} of {} airings have no catalog length and run at nominal_item_secs ({nominal_secs}s); \
             the daemon would probe the file instead.",
            airings.len()
        ));
    }
    if lc.config.anchor.is_some() {
        notices.push(
            "This channel sets an anchor; the anchor join on its first generation is not modelled."
                .to_string(),
        );
    }

    // What the airing inspector shows beside each airing's audit trail: the
    // catalog fields a pool reads, and a label for every item a trail names
    // as passed over that never airs in the span (an aired one is labelled
    // in `airings`), so the page never prints a bare entry id.
    let ids: Vec<String> = airings.iter().map(|a| a.id.clone()).collect();
    let genres = state
        .catalog
        .tags_for_many(&ids, TagNs::Genre)
        .map_err(|e| e.to_string())?;
    let directors = state
        .catalog
        .tags_for_many(&ids, TagNs::Director)
        .map_err(|e| e.to_string())?;
    let collections = state
        .catalog
        .collections_for_many(&ids)
        .map_err(|e| e.to_string())?;
    let audits: Vec<Value> = (0..airings.len())
        .map(|i| crate::audit_report::split_audit(simulated[i].metadata.as_ref()).0)
        .collect();
    let passed_over: Vec<String> = audits
        .iter()
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|stage| stage["detail"]["near_misses"].as_array())
        .flatten()
        .filter_map(|m| m["id"].as_str().map(str::to_string))
        .filter(|id| !entries.contains_key(id))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let titles: HashMap<String, String> = state
        .catalog
        .entries_for(&passed_over)
        .map_err(|e| e.to_string())?
        .iter()
        .map(|(id, e)| (id.clone(), airing_label(e)))
        .collect();

    // Fields that belong to the item, not the airing: one copy per entry id,
    // however many times it airs. The page merges them back onto each airing.
    let mut items: HashMap<&str, Value> = HashMap::new();
    for a in &airings {
        items.entry(a.id.as_str()).or_insert_with(|| {
            let e = entries.get(&a.id).and_then(|e| e.as_ref());
            json!({
                "title": e.map(|e| e.title.clone()),
                "season": e.and_then(|e| e.season),
                "episode": e.and_then(|e| e.episode),
                "year": e.and_then(|e| e.year),
                "genres": genres.get(&a.id),
                "collections": collections.get(&a.id),
                "directors": directors.get(&a.id),
                "content_rating": e.and_then(|e| e.content_rating.clone()),
                "summary": e.and_then(|e| e.summary.clone()),
            })
        });
    }

    let airings_json: Vec<Value> = airings
        .iter()
        .enumerate()
        .map(|(i, a)| {
            json!({
                "index": i,
                "id": a.id,
                "label": a.title,
                "show": a.show,
                "kind": a.kind,
                "pool": a.pool,
                "start": a.start.unix_timestamp(),
                "offset_secs": (a.start - start).whole_seconds(),
                "duration_secs": a.duration.whole_seconds(),
                "ranking": ranking_of(simulated[i].metadata.as_ref()),
                "audit": audits[i],
            })
        })
        .collect();

    tracing::info!(
        event = "ui.review",
        channel = %key,
        days,
        sim_source,
        generations = sim.generations.len(),
        airings = airings.len(),
        overlap_channels = review["overlap"]["channels"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0),
        ms = began.elapsed().as_millis() as u64,
        "review: schedule replayed"
    );

    let mut out = review;
    let obj = out.as_object_mut().expect("build_review returns an object");
    obj.insert("channel".into(), json!(key));
    obj.insert("lineup".into(), lineup_json);
    obj.insert("since_last".into(), json!(since_last_json));
    obj.insert("notices".into(), json!(notices));
    obj.insert(
        "rotations".into(),
        crate::rotation::rows_json(&crate::rotation::preview_channel(
            &lc.config,
            tz,
            &crate::rotation::read_rotations(&lc.output_folder),
            time::OffsetDateTime::now_utc(),
            ROTATION_PREVIEW_PLACEMENTS,
        )),
    );
    obj.insert("airings".into(), json!(airings_json));
    obj.insert("items".into(), json!(items));
    obj.insert("titles".into(), json!(titles));
    obj.insert(
        "day_starts".into(),
        json!(
            day_starts
                .iter()
                .map(|d| d.unix_timestamp())
                .collect::<Vec<_>>()
        ),
    );
    Ok((200, out))
}

/// A movie as `Title (Year)`, an episode as `Show S01E02 · Title`.
fn airing_label(e: &CatalogEntry) -> String {
    match (&e.show, e.season, e.episode) {
        (Some(show), Some(s), Some(ep)) => format!("{show} S{s:02}E{ep:02} · {}", e.title),
        (Some(show), _, _) => format!("{show} · {}", e.title),
        _ => match e.year {
            Some(y) => format!("{} ({y})", e.title),
            None => e.title.clone(),
        },
    }
}

/// The channel's `rule.blocks` as the Review page lays them out: per block,
/// its title and whichever of a daypart sequencer, pattern steps, an
/// hour-switched schedule of patterns, a plain pool list or a fixed entry
/// count drives it. A step's `take` is `[min, max]`, `null` for `take: all`.
fn blocks_json(channel: &config::ChannelConfig) -> Value {
    let blocks: Vec<Value> = channel
        .rule
        .all_blocks()
        .enumerate()
        .map(|(i, b)| {
            let name = b
                .program()
                .and_then(|p| p.title.clone())
                .or_else(|| channel.rule.slot_name(i).map(str::to_string))
                .unwrap_or_else(|| format!("block {i}"));
            json!({
                "name": name,
                "sequencer": b.sequencer.as_ref()
                    .and_then(|s| s.file_name())
                    .map(|n| n.to_string_lossy().into_owned()),
                "steps": steps_json(&b.pattern),
                "pools": b.pools.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
                "entries": b.entries().len(),
                "placed": placements(&channel.rule, i),
            })
        })
        .collect();
    json!(blocks)
}

/// Where `rule.schedules` place block `idx`, one line per range: the
/// schedule's name, its days, and the clock range. Empty for an any-time block.
fn placements(rule: &config::RuleConfig, idx: usize) -> Vec<String> {
    let Some(slot) = rule.slot_name(idx) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for schedule in &rule.schedules {
        let mut days: Vec<String> = schedule.dates.iter().map(|d| String::from(*d)).collect();
        days.extend(
            schedule
                .weekdays
                .iter()
                .map(|w| format!("{w:?}").to_lowercase()),
        );
        let days = if days.is_empty() {
            "every day".to_string()
        } else {
            days.join(", ")
        };
        for (range, placed) in &schedule.hours {
            if placed == slot {
                out.push(format!("{}: {days}, {range}", schedule.name));
            }
        }
    }
    out
}

/// One pattern's steps: pool, `take` as `[min, max]` (`null` for `all`), and
/// the step's `chance`.
fn steps_json(steps: &[config::PatternStep]) -> Vec<Value> {
    steps
        .iter()
        .map(|s| json!({ "pool": s.pool, "take": s.take.bounds(), "chance": s.chance }))
        .collect()
}

/// One channel's candidate entry ids, resolved from its config with no
/// scorer run, cached until the file changes.
fn channel_fingerprint(
    review: &mut ReviewState,
    catalog: &Catalog,
    lc: &config::LoadedChannel,
) -> Result<Arc<HashSet<String>>, String> {
    let mtime = file_mtime(&lc.config_path)?;
    if let Some((cached_mtime, ids)) = review.fingerprints.get(&lc.config_path)
        && *cached_mtime == mtime
    {
        return Ok(ids.clone());
    }
    let ids = crate::resolve::resolve_channel_fingerprint_ids(
        &lc.config,
        &lc.config_path,
        &[],
        None,
        Some(catalog),
    )
    .map(|ids| Arc::new(ids.into_iter().collect::<HashSet<_>>()))
    .map_err(|e| e.to_string())?;
    review
        .fingerprints
        .insert(lc.config_path.clone(), (mtime, ids.clone()));
    Ok(ids)
}

/// Whether a channel's candidates hold every movie or every episode in the
/// catalog — a channel that can air anything shares everything, so listing
/// its overlap says nothing.
fn is_whole_library(library: &(HashSet<String>, HashSet<String>), ids: &HashSet<String>) -> bool {
    let (movies, episodes) = library;
    (!movies.is_empty() && movies.is_subset(ids))
        || (!episodes.is_empty() && episodes.is_subset(ids))
}

/// Every other channel sharing candidates with `lc`, largest first, each with
/// the shared titles (episodes rolled up to their show).
fn overlap_json(
    state: &mut AppState,
    station: &config::Station,
    lc: &config::LoadedChannel,
) -> Result<Value, String> {
    if state.review.library.is_none() {
        let movies = state
            .catalog
            .resolve_query(r#"item.type == "movie""#)
            .map_err(|e| e.to_string())?;
        let episodes = state
            .catalog
            .resolve_query(r#"item.type == "episode""#)
            .map_err(|e| e.to_string())?;
        state.review.library = Some((movies.into_iter().collect(), episodes.into_iter().collect()));
    }
    let own = channel_fingerprint(&mut state.review, &state.catalog, lc)?;
    let library = state.review.library.as_ref().expect("filled above");
    let own_whole = is_whole_library(library, &own);

    let mut whole_library = Vec::new();
    let mut errors = Vec::new();
    let mut shared: Vec<(&config::LoadedChannel, Vec<String>)> = Vec::new();
    let mut others = Vec::new();
    for other in station
        .channels
        .iter()
        .filter(|c| c.config_path != lc.config_path)
    {
        match channel_fingerprint(&mut state.review, &state.catalog, other) {
            Ok(ids) => others.push((other, ids)),
            Err(e) => {
                errors.push(json!({ "channel": channel_key(&other.config_path), "error": e }))
            }
        }
    }
    let library = state.review.library.as_ref().expect("filled above");
    for (other, ids) in others {
        if is_whole_library(library, &ids) {
            whole_library.push(channel_key(&other.config_path));
            continue;
        }
        if own_whole {
            continue;
        }
        let common: Vec<String> = own.intersection(&ids).cloned().collect();
        if !common.is_empty() {
            shared.push((other, common));
        }
    }
    shared.sort_by(|a, b| {
        b.1.len()
            .cmp(&a.1.len())
            .then(a.0.config.number.cmp(&b.0.config.number))
    });

    let channels: Vec<Value> = shared
        .iter()
        .map(|(other, ids)| {
            json!({
                "channel": channel_key(&other.config_path),
                "number": other.config.number,
                "display_name": other.config.display_name,
                "shared": ids.len(),
                "titles": shared_titles(&state.catalog, ids),
            })
        })
        .collect();
    Ok(json!({
        "this_channel_whole_library": own_whole,
        "candidates": own.len(),
        "channels": channels,
        "whole_library": whole_library,
        "errors": errors,
    }))
}

/// Shared ids as titles: a film as `Title (Year)`, a show's episodes as one
/// `Show (n episodes)` line. Most shared first.
fn shared_titles(catalog: &Catalog, ids: &[String]) -> Vec<String> {
    let mut shows: HashMap<String, usize> = HashMap::new();
    let mut films: Vec<String> = Vec::new();
    for id in ids {
        match catalog.entry(id).ok().flatten() {
            Some(e) => match &e.show {
                Some(show) => *shows.entry(show.clone()).or_insert(0) += 1,
                None => films.push(airing_label(&e)),
            },
            None => films.push(id.clone()),
        }
    }
    let mut shows: Vec<(String, usize)> = shows.into_iter().collect();
    shows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    films.sort();
    shows
        .into_iter()
        .map(|(show, n)| {
            if n == 1 {
                format!("{show} (1 episode)")
            } else {
                format!("{show} ({n} episodes)")
            }
        })
        .chain(films)
        .collect()
}

/// The report itself: header, tiles, depth rows, overlap and tune-in samples.
fn build_review(
    state: &mut AppState,
    station: &config::Station,
    lc: &config::LoadedChannel,
    airings: &[crate::review::ReviewAiring],
    simulated: &[&crate::simulate::SimulatedAiring],
    start: time::OffsetDateTime,
    end: time::OffsetDateTime,
) -> Result<Value, String> {
    use crate::review;

    let days = (end - start).as_seconds_f64() / 86_400.0;
    let first_repeat = review::first_repeat(airings, start, |_| true)
        .map(|r| json!({ "days": r.days, "index": r.index, "label": airings[r.index].title }));

    // The last airing can run past the span's end; only its share inside the
    // span counts toward airtime.
    let clipped: Vec<review::ReviewAiring> = airings
        .iter()
        .map(|a| review::ReviewAiring {
            duration: a.duration.min(end - a.start),
            ..a.clone()
        })
        .collect();
    let by_kind = review::airtime_by_kind(&clipped);
    let total: f64 = by_kind.values().sum();
    let share = |kind: &str| {
        if total > 0.0 {
            by_kind.get(kind).copied().unwrap_or(0.0) / total
        } else {
            0.0
        }
    };

    let depth: Vec<Value> = review::depth_rows(airings, start)
        .into_iter()
        .map(|row| {
            json!({
                "kind": row.kind,
                "name": row.name,
                "pool": row.pool,
                "aired": row.aired,
                "distinct": row.distinct,
                "first_repeat_days": row.first_repeat.as_ref().map(|r| r.days),
                "first_repeat_label": row.first_repeat.as_ref().map(|r| airings[r.index].title.clone()),
            })
        })
        .collect();

    let seed = review_seed(station, lc, start);
    let mut samples: Vec<Value> = Vec::new();
    for (n, t) in review::tune_in_samples(airings, start, end, seed, TUNE_IN_SAMPLES)
        .into_iter()
        .enumerate()
    {
        let next: Vec<usize> = (t.index + 1..airings.len()).take(2).collect();
        let still = offer_still(
            state,
            station,
            lc,
            Neighbours {
                prev: t.index.checked_sub(1).map(|i| simulated[i]),
                airing: simulated[t.index],
                next: simulated.get(t.index + 1).copied(),
            },
            t.into,
            n + 1,
        );
        samples.push(json!({
            "n": n + 1,
            "at": t.at.unix_timestamp(),
            "index": t.index,
            "minutes_in": t.into.whole_minutes(),
            "minutes_total": airings[t.index].duration.whole_minutes(),
            "next": next,
            "still": still,
        }));
    }

    let distinct = airings
        .iter()
        .map(|a| a.id.as_str())
        .collect::<HashSet<_>>()
        .len();
    let overlap = overlap_json(state, station, lc)?;

    let random_order_pools: HashSet<String> = lc
        .config
        .rule
        .blocks
        .iter()
        .flat_map(|b| &b.pools)
        .filter(|p| matches!(p.order, Some(config::Order::Random)))
        .map(|p| p.name.clone())
        .collect();
    let show_ids: Vec<String> = airings
        .iter()
        .filter_map(|a| a.show_id.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let episode_counts = state
        .catalog
        .episode_counts_for(&show_ids)
        .map_err(|e| e.to_string())?;
    let principles: Vec<Value> = review::grade_principles(&review::GradeInputs {
        airings,
        start,
        end,
        tz: crate::tz::parse(&station.station.tz).map_err(|e| e.to_string())?,
        random_order_pools: &random_order_pools,
        episode_counts: &episode_counts,
    })
    .into_iter()
    .map(|p| {
        json!({
            "name": p.name,
            "grade": p.grade,
            "value": p.value,
            "measure": p.measure,
            "not_scored": p.not_scored,
        })
    })
    .collect();

    Ok(json!({
        "channel": channel_key(&lc.config_path),
        "config_path": lc.config_path.display().to_string(),
        "number": lc.config.number,
        "name": lc.name,
        "display_name": lc.config.display_name,
        "seed": lc.config.seed,
        "tz": station.station.tz,
        "days": days.round() as u32,
        "start": start.unix_timestamp(),
        "end": end.unix_timestamp(),
        "blocks": blocks_json(&lc.config),
        "first_repeat": first_repeat,
        "airtime": {
            "total_secs": total,
            "by_kind": by_kind,
            "film_share": share("movie"),
            "series_share": share("episode"),
        },
        "items_aired": airings.len(),
        "distinct_items": distinct,
        "depth": depth,
        "principles": principles,
        "overlap": overlap,
        "samples": samples,
    }))
}

/// The channel's seed at `at` — what the review's seeded tune-in samples and
/// still names key on, so a rerun of the same span picks the same moments.
fn review_seed(
    station: &config::Station,
    lc: &config::LoadedChannel,
    at: time::OffsetDateTime,
) -> u64 {
    let tz = crate::tz::parse(&station.station.tz).unwrap_or(time_tz::timezones::db::UTC);
    crate::variety::Variety::new(&lc.config, tz)
        .dice(at, None)
        .seed
}

/// One tune-in sample's still, as the page reads it: `{url}` when there is a
/// media file to grab a frame from, else `{unavailable, path?}` naming why
/// not. Registers the still so its URL renders it on first request.
fn offer_still(
    state: &mut AppState,
    station: &config::Station,
    lc: &config::LoadedChannel,
    at: Neighbours<'_>,
    into: time::Duration,
    n: usize,
) -> Value {
    let still = match TuneInStill::offer(
        state.catalog.sources_for(&at.airing.id),
        &MediaRemap::from_env(),
        &lc.overlays,
        &station.station.normalization.video,
        &station.station.fonts,
        at,
        into,
    ) {
        Ok(still) => still,
        Err(Unavailable { why, path: None }) => return json!({ "unavailable": why }),
        Err(Unavailable {
            why,
            path: Some(path),
        }) => return json!({ "unavailable": why, "path": path.display().to_string() }),
    };
    let channel: String = channel_key(&lc.config_path)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let id = format!(
        "{channel}-s{}-n{n}-{}.jpg",
        review_seed(station, lc, at.airing.start),
        still.digest()
    );
    state.review.stills.insert(id.clone(), still);
    json!({ "url": format!("/api/review/still?id={id}") })
}

/// `GET /api/review/still?id=…`: the still a review offered under `id`,
/// rendered on first request and served from the still directory after. The
/// render runs on its own thread, which answers the request, so a slow media
/// read holds up that one image and not the rest of the tool. An id no review
/// offered is a 404; a render that fails is a 500 naming why.
fn serve_review_still(state: &AppState, request: tiny_http::Request, query: &str) {
    let params = parse_query(query);
    let Some((id, still)) = params
        .get("id")
        .and_then(|id| state.review.stills.get(id).map(|s| (id.clone(), s.clone())))
    else {
        respond_json(
            request,
            404,
            &json!({ "error": "no such still; reload the review" }),
        );
        return;
    };
    let dir = state.review.still_dir.clone();
    std::thread::spawn(
        move || match render_review_still_serially(&dir, &id, &still) {
            Ok(jpeg) => respond_jpeg(request, jpeg),
            Err(e) => respond_json(request, 500, &json!({ "error": e })),
        },
    );
}

/// One still renders at a time, whatever the number of request threads
/// waiting: a Review page asks for every sample's still at once, and each
/// render is an ffmpeg reading the media array the channels transcode from.
fn render_review_still_serially(
    dir: &Path,
    id: &str,
    still: &TuneInStill,
) -> Result<Vec<u8>, String> {
    static RENDER: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one_at_a_time = RENDER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let jpeg = render_review_still(dir, id, still)?;
    prune_still_dir(dir, STILL_DIR_MAX_FILES);
    Ok(jpeg)
}

/// The most rendered stills the still directory keeps; the oldest go first.
const STILL_DIR_MAX_FILES: usize = 200;

/// Deletes the oldest files in `dir` until at most `max` remain, so the
/// directory (the container's writable layer) stays bounded however many
/// distinct stills reviews offer. A file that cannot be listed or removed is
/// left; the next render tries again.
fn prune_still_dir(dir: &Path, max: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some((meta.modified().ok()?, e.path()))
        })
        .collect();
    if files.len() <= max {
        return;
    }
    files.sort();
    for (_, path) in &files[..files.len() - max] {
        if std::fs::remove_file(path).is_ok() {
            tracing::debug!(event = "ui.still.pruned", path = %path.display(), "still: pruned");
        }
    }
}

fn render_review_still(dir: &Path, id: &str, still: &TuneInStill) -> Result<Vec<u8>, String> {
    let began = std::time::Instant::now();
    let served = still
        .render_cached(Path::new("ffmpeg"), dir, id)
        .inspect_err(|e| {
            tracing::warn!(
                event = "ui.still.failed",
                id,
                media = %still.media.display(),
                at_secs = still.at_secs,
                error = %e,
                "still: render failed"
            )
        })?;
    let (jpeg, rendered) = match served {
        StillJpeg::Cached(jpeg) => {
            tracing::debug!(event = "ui.still.cached", id, "still: served from cache");
            return Ok(jpeg);
        }
        StillJpeg::Rendered(jpeg, rendered) => (jpeg, rendered),
    };
    tracing::info!(
        event = "ui.still.rendered",
        id,
        media = %still.media.display(),
        at_secs = still.at_secs,
        title = ?still.program.title,
        next_title = ?still.program.next_title,
        overlay_time = ?rendered.overlay_time,
        shows_next = rendered.shows_next,
        ms = began.elapsed().as_millis() as u64,
        "still: rendered"
    );
    Ok(jpeg)
}

/// The numbers "Since last run" compares, in display order.
fn comparable_numbers(review: &Value) -> Vec<(String, Value)> {
    let pct = |v: &Value| v.as_f64().map(|f| json!((f * 1000.0).round() / 10.0));
    let mut out = vec![
        ("Days shown".to_string(), review["days"].clone()),
        (
            "Days until first repeat".to_string(),
            review["first_repeat"]["days"]
                .as_f64()
                .map(|d| json!((d * 10.0).round() / 10.0))
                .unwrap_or(Value::Null),
        ),
        (
            "Film share of airtime (%)".to_string(),
            pct(&review["airtime"]["film_share"]).unwrap_or(Value::Null),
        ),
        (
            "Series share of airtime (%)".to_string(),
            pct(&review["airtime"]["series_share"]).unwrap_or(Value::Null),
        ),
        (
            "Channels sharing content".to_string(),
            json!(
                review["overlap"]["channels"]
                    .as_array()
                    .map(Vec::len)
                    .unwrap_or(0)
            ),
        ),
        ("Items aired".to_string(), review["items_aired"].clone()),
        (
            "Distinct items".to_string(),
            review["distinct_items"].clone(),
        ),
    ];
    for row in review["depth"].as_array().into_iter().flatten() {
        if row["kind"] != "pool" {
            continue;
        }
        out.push((
            format!(
                "Pool {} — days until first repeat",
                row["name"].as_str().unwrap_or("")
            ),
            row["first_repeat_days"]
                .as_f64()
                .map(|d| json!((d * 10.0).round() / 10.0))
                .unwrap_or(Value::Null),
        ));
    }
    out
}

const INDEX_HTML: &str = include_str!("index.html");
const REVIEW_HTML: &str = include_str!("review.html");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_still_dir_keeps_the_newest_max_files() {
        let dir = std::env::temp_dir().join(format!("etv-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let base = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        for i in 0..10u64 {
            let f = std::fs::File::create(dir.join(format!("{i}.jpg"))).unwrap();
            f.set_modified(base + std::time::Duration::from_secs(i))
                .unwrap();
        }
        prune_still_dir(&dir, 4);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["6.jpg", "7.jpg", "8.jpg", "9.jpg"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn inline_index_follows_position_not_ctx_profile_order() {
        let entry = |v: Value| v.as_object().unwrap().clone();
        // Inline entry 3 in the file, though `exclude:` entries 1-2 and a
        // file entry came before it in the list: slot 2 of `pool.profile`.
        assert_eq!(
            inline_index(&entry(json!({"origin": "inline", "position": 3}))),
            json!(2)
        );
        assert_eq!(
            inline_index(&entry(json!({"origin": "profile.yaml", "position": 1}))),
            Value::Null
        );
    }

    #[test]
    fn lineup_keys_are_folder_names_when_distinct() {
        let base = Path::new("/st");
        let lineup = [
            PathBuf::from("/st/channels/034-syfy/channel.yaml"),
            PathBuf::from("/st/channels/035-prestige/channel.yaml"),
        ];
        assert_eq!(lineup_keys(&lineup, base), ["034-syfy", "035-prestige"]);
    }

    #[test]
    fn lineup_keys_fall_back_to_paths_when_folders_collide() {
        let base = Path::new("/st");
        let lineup = [
            PathBuf::from("/st/channels/a.yaml"),
            PathBuf::from("/st/channels/b.yaml"),
        ];
        assert_eq!(
            lineup_keys(&lineup, base),
            ["channels/a.yaml", "channels/b.yaml"]
        );
    }
}

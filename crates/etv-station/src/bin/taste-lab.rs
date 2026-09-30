//! taste-lab: iteratively tune a pool's taste-profile weights against real
//! data (etv-station-sctf.8) — a dev tool, not part of the daemon, same
//! category as `taste-debug`. Serves a small local web UI over
//! `score::{ScoreCache, pick}`, the real scoring path (ADR-0020): a channel
//! picker, live keyword/genre/item search, and one unified schedule
//! (etv-station-sctf.9) that re-scores in place as a profile entry is added,
//! reweighted, or removed — no file write, no process restart.
//!
//! The schedule is the daemon's, replayed ([`simulate_schedule`], built on
//! `etv_station::simulate`): the same generation loop, the same
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
//! A throwaway spike (etv-station-sctf.1's follow-up, deleted per the spike
//! lifecycle) proved the scoring shape works end to end against real
//! production data, and surfaced two real bugs that are this tool's actual
//! scope:
//!
//! - **Perf.** Against the real ~11,634-movie catalog, a debug build's
//!   `pick()` took 15-22s, called twice per edit (~40s per weight tweak).
//!   `ScoreCache::prepare`'s `sources:` resolution already caches correctly
//!   across edits within one process; two `pick()` calls per edit did not,
//!   and cost the most. Fixed two ways: this bin must be run `--release`
//!   (`tools/taste-lab.sh` does), and [`score_pool`] below runs `pick()`
//!   exactly once per pool per edit, only for the pool an edit actually
//!   touched — every other pool's cached ranking is untouched.
//! - **Fidelity.** The pool's own real `sources:` (or, absent one, its
//!   script's own default `sources()`) is always what gets scored against —
//!   never `sources: None`'s silent whole-library substitute. Achieved by
//!   cloning the pool straight out of the real `channel.yaml` (see
//!   [`clone_pool`]) and mutating only its `profile` field for live edits;
//!   `plugin`/`sources`/`capabilities`/`datastores`/`config` all stay
//!   exactly what the channel author wrote.
//!
//! The plugin script scored against defaults to this checkout's
//! `examples/plugins/<name>.rhai` — the in-development script — rather than
//! the pool's own (gitignored, possibly stale) deployed copy; `POST
//! /api/select`'s `use_deployed_plugin: true` reaches the deployed one
//! explicitly.
//!
//! Usage:
//!   cargo run --release --bin taste-lab -- --catalog <path>
//! then open http://127.0.0.1:<port>/ (default port 4747). Each pool's own
//! plexdb path comes from its `datastores:` grant, not a CLI flag — set
//! `PLEXDB_SNAPSHOT_PATH` (or whatever env var the grant references) before
//! running. `tools/taste-lab.sh` wraps this with channel discovery and the
//! local catalog/plexdb cache
//! `tools/taste-debug.sh` already uses.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use clap::Parser;
use etv_station::catalog::{Catalog, Entry as CatalogEntry, TagNs, like_escape};
use etv_station::config::{self, DatastoreGrant, Pool};
use etv_station::profile::ProfileEntry;
use etv_station::score::{GrantedCapabilities, PickedItem, ScoreCache, ScoreInputs};
use etv_station::tautulli::{self, HistoryScope};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};

#[derive(Parser, Debug)]
#[command(about = "Local web UI for iteratively tuning a plugin pool's taste profile")]
struct Cli {
    /// The station's catalog sqlite database.
    #[arg(long)]
    catalog: PathBuf,

    /// Real channels live here — `<dir>/*/channel.yaml`.
    #[arg(long, default_value = "deploy/appdata/channels")]
    channels_dir: PathBuf,

    /// The in-development plugin scripts a channel's own `plugin:` path is
    /// mapped onto by default (see module docs).
    #[arg(long, default_value = "examples/plugins")]
    examples_plugins_dir: PathBuf,

    /// Where to listen.
    #[arg(long, default_value_t = 4747)]
    port: u16,
}

fn expand_env(raw: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| format!("unterminated ${{ in {raw:?}"))?;
        let var = &after[..end];
        let val = std::env::var(var)
            .map_err(|_| format!("env var `{var}` referenced by {raw:?} is not set"))?;
        out.push_str(&val);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
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

/// One channel this tool found under `--channels-dir` with at least one
/// plugin-backed pattern pool.
struct DiscoveredChannel {
    channel_path: PathBuf,
    channel_name: String,
    display_name: Option<String>,
}

/// Every `pattern:` step across `channel`'s blocks, in block-then-pattern order.
fn pattern_plan(channel: &config::ChannelConfig) -> Vec<PlanStep> {
    let mut out = Vec::new();
    for (block_index, block) in channel.rule.blocks.iter().enumerate() {
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

fn discover_channels(channels_dir: &Path) -> Vec<DiscoveredChannel> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(channels_dir) else {
        return out;
    };
    let mut dirs: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    dirs.sort();
    for dir in dirs {
        let channel_path = dir.join("channel.yaml");
        if !channel_path.is_file() {
            continue;
        }
        let Ok(channel) = config::read_channel(&channel_path) else {
            continue;
        };
        if !pattern_plan(&channel).iter().any(|s| s.has_plugin) {
            continue;
        }
        let channel_name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        out.push(DiscoveredChannel {
            channel_path,
            channel_name,
            display_name: channel.display_name.clone(),
        });
    }
    out
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
    channels_dir: PathBuf,
    examples_plugins_dir: PathBuf,
    cache: ScoreCache,
    session: Option<Session>,
}

fn main() -> Result<(), String> {
    let cli = Cli::parse();
    if !cfg!(debug_assertions) {
        // fine — release build, the whole point of this tool.
    } else {
        eprintln!(
            "taste-lab: running a debug build. score::pick() takes 15-22s per call against \
             the real catalog in debug — run `cargo run --release --bin taste-lab` instead \
             (tools/taste-lab.sh does this for you)."
        );
    }

    let catalog = Catalog::open_readonly(&cli.catalog).map_err(|e| e.to_string())?;
    let mut state = AppState {
        catalog,
        channels_dir: cli.channels_dir,
        examples_plugins_dir: cli.examples_plugins_dir,
        cache: ScoreCache::default(),
        session: None,
    };

    let server = tiny_http::Server::http(("127.0.0.1", cli.port))
        .map_err(|e| format!("bind 127.0.0.1:{}: {e}", cli.port))?;
    println!("taste-lab: http://127.0.0.1:{}/", cli.port);

    for request in server.incoming_requests() {
        handle(&mut state, request);
    }
    Ok(())
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

    let result: Result<(u16, Value), String> = match (&method, path.as_str()) {
        (tiny_http::Method::Get, "/") => {
            respond_html(request, INDEX_HTML);
            return;
        }
        (tiny_http::Method::Get, "/api/channels") => Ok((200, api_channels(state))),
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

fn api_channels(state: &AppState) -> Value {
    let channels = discover_channels(&state.channels_dir);
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
    json!({ "channels": list })
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
/// resolves which plugin script to score against (deployed vs. this
/// checkout's `examples/plugins/`, see module docs), expands its datastore
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

    let plugin_path = if use_deployed_plugin {
        deployed_path.clone()
    } else {
        let basename = deployed_path
            .file_name()
            .ok_or_else(|| format!("plugin path {} has no file name", deployed_path.display()))?;
        state.examples_plugins_dir.join(basename)
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
        ds.path = expand_env(&ds.path)?;
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
        etv_station::score::effective_sources(&plugin_path, pool_owned.sources.as_ref()).ok();

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
        None => match channel.history_scope() {
            HistoryScope::AllUsers => None,
            scope @ HistoryScope::User(_) => {
                let (url, key) = tautulli::credentials_from_env().ok_or_else(|| {
                    "scoring.taste_scope is single_user, but TAUTULLI_URL/TAUTULLI_API_KEY \
                     aren't set (source .env), and no account_id was given"
                        .to_string()
                })?;
                let rows = tautulli::fetch_rows(&url, &key, &scope);
                Some(
                    tautulli::resolve_account_id(&scope, &rows)?
                        .ok_or_else(|| "single_user scope resolved to no account".to_string())?,
                )
            }
        },
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
            .blocks
            .get(step.block_index)
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
    etv_station::profile::check(&req.entry).map_err(|e| format!("profile entry {e}"))?;

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
        .prepare(&state.catalog, &plugin_path, pw.pool.sources.as_ref())
        .map_err(|e| format!("prepare: {e}"))?;
    state
        .cache
        .prepare_profile(&state.catalog, &pw.pool, &channel_dir)
        .map_err(|e| format!("profile: {e}"))?;

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

    let full = etv_station::score::pick(
        &state.cache,
        &plugin_path,
        pw.pool.sources.as_ref(),
        &inputs,
        0,
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
    let result = simulate_schedule(&state.catalog, session);
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

/// Run the daemon's generation loop ([`etv_station::simulate::simulate`]) over
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
fn simulate_schedule(catalog: &Catalog, session: &Session) -> Result<Vec<Value>, String> {
    let mut config: config::ChannelConfig = serde_json::from_value(session.config_json.clone())
        .map_err(|e| format!("channel config: {e}"))?;
    for block in &mut config.rule.blocks {
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
        .blocks
        .iter()
        .enumerate()
        .map(|(i, b)| {
            b.program()
                .and_then(|p| p.title.clone())
                .unwrap_or_else(|| format!("block {i}"))
        })
        .collect();

    let start = time::OffsetDateTime::now_utc();
    let sim = etv_station::simulate::simulate(
        &config,
        &session.channel_path,
        catalog,
        session.account_id,
        session.days,
        start,
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

fn profile_json(cache: &ScoreCache, pool_name: &str) -> Value {
    let Some(resolved) = cache.profile(pool_name) else {
        return json!({ "entries": [], "exclude_keywords": [] });
    };
    let entries: Vec<Value> = resolved
        .entries
        .iter()
        .enumerate()
        .map(|(i, dyn_entry)| {
            let mut v = rhai::serde::from_dynamic::<Value>(dyn_entry).unwrap_or_else(|_| json!({}));
            if let Some(obj) = v.as_object_mut() {
                obj.insert("index".to_string(), json!(i));
            }
            v
        })
        .collect();
    let exclude_keywords: Vec<Value> = resolved
        .exclude_keywords
        .iter()
        .map(|d| rhai::serde::from_dynamic::<Value>(d).unwrap_or(Value::Null))
        .collect();
    json!({ "entries": entries, "exclude_keywords": exclude_keywords })
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

const INDEX_HTML: &str = include_str!("taste_lab_ui.html");

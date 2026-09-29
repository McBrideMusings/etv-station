//! taste-lab: iteratively tune a pool's taste-profile weights against real
//! data (etv-station-sctf.8) — a dev tool, not part of the daemon, same
//! category as `taste-debug`. Serves a small local web UI over
//! `score::{ScoreCache, pick}`, the real scoring path (ADR-0020): a picker
//! over real `deploy/appdata/channels/*/channel.yaml` pools, live keyword/
//! genre/item search, and two live tables (every ranked pool candidate, and
//! what would actually air) that re-score in place as a profile entry is
//! added, reweighted, or removed — no file write, no process restart.
//!
//! A throwaway spike (etv-station-sctf.1's follow-up, deleted per the spike
//! lifecycle) proved this shape works end to end against real production
//! data, and surfaced two real bugs that are this tool's actual scope:
//!
//! - **Perf.** Against the real ~11,634-movie catalog, a debug build's
//!   `pick()` took 15-22s, called twice per edit (~40s per weight tweak).
//!   `ScoreCache::prepare`'s `sources:` resolution already caches correctly
//!   across edits within one process; the two `pick()` calls did not, and
//!   cost the most. Fixed two ways: this bin must be run `--release`
//!   (`tools/taste-lab.sh` does), and [`score_session`] below derives
//!   "selected" as a prefix of one full-pool `pick()` run instead of running
//!   it twice.
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

use std::path::{Path, PathBuf};

use clap::Parser;
use etv_station::catalog::{Catalog, TagNs, like_escape};
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

/// One real plugin-backed pool this tool found under `--channels-dir`.
struct DiscoveredPool {
    channel_path: PathBuf,
    channel_name: String,
    pool_name: String,
}

fn discover_pools(channels_dir: &Path) -> Vec<DiscoveredPool> {
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
        let channel_name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        for block in &channel.rule.blocks {
            for pool in &block.pools {
                if pool.plugin.is_some() {
                    out.push(DiscoveredPool {
                        channel_path: channel_path.clone(),
                        channel_name: channel_name.clone(),
                        pool_name: pool.name.clone(),
                    });
                }
            }
        }
    }
    out
}

/// The currently selected channel/pool and the live-edited working copy of
/// its profile. Rebuilt wholesale by `POST /api/select`; every other
/// endpoint mutates `pool.profile` in place and re-scores.
struct Session {
    channel_dir: PathBuf,
    pool_name: String,
    pool: Pool,
    plugin_path: PathBuf,
    plexdb_path: PathBuf,
    datastore_name: String,
    account_id: Option<i64>,
    target_count: usize,
    extended_target_count: usize,
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
    let pools = discover_pools(&state.channels_dir);
    let list: Vec<Value> = pools
        .iter()
        .map(|p| {
            json!({
                "channel_path": p.channel_path.display().to_string(),
                "channel_name": p.channel_name,
                "pool_name": p.pool_name,
            })
        })
        .collect();
    json!({ "pools": list })
}

#[derive(serde::Deserialize)]
struct SelectRequest {
    channel_path: String,
    pool_name: String,
    #[serde(default)]
    account_id: Option<i64>,
    #[serde(default)]
    use_deployed_plugin: bool,
    #[serde(default)]
    target_count: Option<usize>,
    #[serde(default)]
    extended_target_count: Option<usize>,
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

    let pool = channel
        .rule
        .blocks
        .iter()
        .flat_map(|b| b.pools.iter())
        .find(|p| p.name == req.pool_name)
        .ok_or_else(|| format!("no pool named {:?} in {}", req.pool_name, req.channel_path))?;
    let plugin = pool
        .plugin
        .as_ref()
        .ok_or_else(|| format!("pool {:?} has no `plugin:`", req.pool_name))?;
    let deployed_path = if plugin.is_absolute() {
        plugin.clone()
    } else {
        channel_dir.join(plugin)
    };

    let plugin_path = if req.use_deployed_plugin {
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
        return Err(format!("pool {:?} declares no datastores", req.pool_name));
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

    // A fresh cache, not a reused one: `ScoreCache::prepare_profile` only
    // ever runs once per generation in the daemon, so it treats "this pool's
    // profile is empty" as "nothing to resolve" and leaves whatever it
    // resolved last time sitting in its cache — correct there, wrong here,
    // where the same pool name can be re-selected many times with a
    // different (or emptied-back-out) profile each time. Without this, a
    // fresh `/api/select` for a pool silently kept scoring against a
    // *previous session's* edited profile instead of the real channel.yaml's
    // own (unresolved sources: still get re-cached for free on the next
    // score_session, since `prepare` keys on script path + sources).
    state.cache = ScoreCache::default();

    state.session = Some(Session {
        channel_dir,
        pool_name: req.pool_name,
        pool: pool_owned,
        plugin_path,
        plexdb_path,
        datastore_name,
        account_id,
        target_count: req.target_count.unwrap_or(20),
        extended_target_count: req.extended_target_count.unwrap_or(300),
    });

    Ok(score_or_describe_error(state))
}

#[derive(serde::Deserialize)]
struct ProfileAddRequest {
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
        .ok_or_else(|| "no pool selected".to_string())?;
    session.pool.profile.push(req.entry);
    Ok(score_or_describe_error(state))
}

#[derive(serde::Deserialize)]
struct ProfileUpdateRequest {
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
        .ok_or_else(|| "no pool selected".to_string())?;
    let entry = session
        .pool
        .profile
        .get_mut(req.index)
        .ok_or_else(|| format!("no profile entry at index {}", req.index))?;
    entry.weight = Some(req.weight);
    Ok(score_or_describe_error(state))
}

#[derive(serde::Deserialize)]
struct ProfileRemoveRequest {
    index: usize,
}

fn api_profile_remove(state: &mut AppState, body: &str) -> Result<(u16, Value), String> {
    let req: ProfileRemoveRequest =
        serde_json::from_str(body).map_err(|e| format!("bad request body: {e}"))?;
    let session = state
        .session
        .as_mut()
        .ok_or_else(|| "no pool selected".to_string())?;
    if req.index >= session.pool.profile.len() {
        return Err(format!("no profile entry at index {}", req.index));
    }
    session.pool.profile.remove(req.index);
    Ok(score_or_describe_error(state))
}

/// The session's real config, independent of whether scoring it succeeds —
/// `sources:`/`config:`/which plugin script/which account, all read straight
/// off the pool with no catalog access and nothing that can fail. Used to
/// answer "what am I actually pointed at" even when `pick()` errors out (a
/// schema mismatch between the picked plugin script and this pool's
/// `config:`, most often), so a failed select still shows the real pool
/// instead of leaving the page blank.
fn describe_session(session: &Session) -> Value {
    let sources = match &session.pool.sources {
        Some(map) => json!(map),
        None => json!(null),
    };
    json!({
        "pool_name": session.pool_name,
        "plugin_path": session.plugin_path.display().to_string(),
        "plexdb_path": session.plexdb_path.display().to_string(),
        "account_id": session.account_id,
        "target_count": session.target_count,
        "extended_target_count": session.extended_target_count,
        "sources": sources,
        "config": session.pool.config.clone().unwrap_or(Value::Null),
        "profile_yaml": profile_yaml(&session.pool),
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

/// [`score_session`], but a failure still returns the session's real
/// `sources:`/`config:`/plugin path (via [`describe_session`]) alongside the
/// error instead of losing them — see that function's doc for why.
fn score_or_describe_error(state: &mut AppState) -> (u16, Value) {
    match score_session(state) {
        Ok(v) => (200, v),
        Err(e) => {
            let mut v = state
                .session
                .as_ref()
                .map(describe_session)
                .unwrap_or_else(|| json!({}));
            if let Some(obj) = v.as_object_mut() {
                obj.insert("error".to_string(), json!(e));
            }
            (400, v)
        }
    }
}

/// Run one full-pool `pick()` against the session's real `sources:`, real
/// profile, real capabilities/datastores — see the module docs for why one
/// call, not two. `selected` is a prefix of `full`, not a second run.
fn score_session(state: &mut AppState) -> Result<Value, String> {
    let session = state
        .session
        .as_ref()
        .ok_or_else(|| "no pool selected".to_string())?;

    state
        .cache
        .prepare(
            &state.catalog,
            &session.plugin_path,
            session.pool.sources.as_ref(),
        )
        .map_err(|e| format!("prepare: {e}"))?;
    state
        .cache
        .prepare_profile(&state.catalog, &session.pool, &session.channel_dir)
        .map_err(|e| format!("profile: {e}"))?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let mut full_config = session.pool.config.clone().unwrap_or_else(|| json!({}));
    if let Some(obj) = full_config.as_object_mut() {
        obj.insert("exploration_fraction".to_string(), json!(0.0));
    }
    let inputs = ScoreInputs {
        target_count: session.extended_target_count,
        now,
        account_id: session.account_id,
        ..Default::default()
    };
    let grant = GrantedCapabilities::from_names(&session.pool.capabilities).with_datastores(&[
        DatastoreGrant {
            name: session.datastore_name.clone(),
            path: session.plexdb_path.display().to_string(),
        },
    ])?;

    let full = etv_station::score::pick(
        &state.cache,
        &session.plugin_path,
        session.pool.sources.as_ref(),
        &inputs,
        0,
        &session.pool_name,
        Some(&full_config),
        grant,
    )
    .map_err(|e| format!("pick: {e}"))?;

    let selected_len = session.target_count.min(full.len());
    let selected = &full[..selected_len];

    // The full ranked list can be thousands of rows; the payload shows the
    // top slice plus the true count rather than every row.
    const IN_POOL_SHOWN: usize = 300;
    let in_pool_rows: Vec<Value> = full
        .iter()
        .take(IN_POOL_SHOWN)
        .map(|item| picked_item_json(&state.catalog, item))
        .collect();
    let selected_rows: Vec<Value> = selected
        .iter()
        .map(|item| picked_item_json(&state.catalog, item))
        .collect();

    let profile = profile_json(&state.cache, &session.pool_name);
    let mut payload = describe_session(session);
    let obj = payload
        .as_object_mut()
        .expect("describe_session returns an object");
    obj.insert("profile".to_string(), profile);
    obj.insert(
        "in_pool".to_string(),
        json!({ "total": full.len(), "shown": in_pool_rows.len(), "rows": in_pool_rows }),
    );
    obj.insert(
        "selected".to_string(),
        json!({ "total": selected.len(), "rows": selected_rows }),
    );
    Ok(payload)
}

fn picked_item_json(catalog: &Catalog, item: &PickedItem) -> Value {
    let (title, year) = match catalog.entry(&item.id) {
        Ok(Some(e)) => (e.title, e.year),
        _ => ("<unknown>".to_string(), None),
    };
    json!({
        "id": item.id,
        "title": title,
        "year": year,
        "metadata": item.metadata,
    })
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

fn api_search_keyword(state: &AppState, query: &str) -> Result<(u16, Value), String> {
    let params = parse_query(query);
    let q = params.get("q").cloned().unwrap_or_default();
    let session = state
        .session
        .as_ref()
        .ok_or_else(|| "no pool selected".to_string())?;
    let conn = open_plexdb_readonly(&session.plexdb_path)?;
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

//! Channels › Pools: every pool of a channel with its sources, config, taste
//! profile and profile-only ranking, and the **drafts** that let a person
//! try profile edits on a scorer pool without touching `channel.yaml`
//! (etv-station-67jo.2).
//!
//! A pool's ranking is one full `score::pick()` with empty `ctx.recent` and no
//! exploration slots — what the profile alone ranks first. It runs over
//! `score::{ScoreCache, pick}`, the real scoring path (ADR-0020), with the
//! pool's own deployed plugin (or its namesake in [`Options::plugins_dir`]),
//! its real `sources:`, `config:`, capabilities and datastores. Scoring a pool
//! runs its plugin with the `exec`/`http` calls it is granted, exactly as a
//! generation would.
//!
//! A draft is a set of profile edits for one channel, held in this process's
//! memory under an id the page keeps in its URL (`?draft=`). Every request
//! names its draft; nothing is global, so two drafts on one channel never see
//! each other's edits. A draft carries the Plex account it scores for. The
//! server keeps at most [`MAX_DRAFTS`] and drops one idle for [`DRAFT_IDLE`];
//! when full, the least recently used goes. A draft that expired answers 404.
//!
//! Each pool's ranking with no draft is its baseline, cached per channel,
//! pool and account until the channel file or the plugin changes. A draft's
//! ranking reports each item's movement against that baseline (`was`,
//! `movement`: places moved up, negative when it fell, null when the
//! baseline's ranking did not hold it).
//!
//! Scoring is synchronous: a request that ranks a pool holds the server's one
//! thread for the length of one `pick()` (10-22s on the real catalog; run a
//! release build). The page shows its own pending state meanwhile.
//!
//! Nothing here writes a file. "Copy profile YAML" returns the edited profile
//! as it would read in `channel.yaml`; the person pastes it in.
//!
//! Each pool's plexdb path comes from its `datastores:` grant, not a flag —
//! set `PLEXDB_SNAPSHOT_PATH` (or whatever env var the grant references).

use super::lineup::{has_scorer, lineup_keys};
use super::*;
use std::time::{Duration, Instant, SystemTime};

/// Drafts the server keeps at once.
const MAX_DRAFTS: usize = 8;
/// Cached baselines kept before the cache is emptied.
const MAX_BASELINES: usize = 64;
/// How long a draft may sit untouched before it is dropped.
const DRAFT_IDLE: Duration = Duration::from_secs(60 * 60);
/// How many of a pool's ranked items the response carries — a movies pool
/// ranks the whole library, and each item costs a catalog lookup. The total is
/// `ranking_count`.
const RANKING_SHOWN: usize = 300;
/// The `target_count` of the one full `pick()` behind a ranking.
const RANKED_ITEMS: usize = 300;

type Answer = Result<(u16, Value), String>;

fn not_found(message: String) -> Answer {
    Ok((404, json!({ "error": message })))
}

/// A set of profile edits for one channel.
struct Draft {
    channel: String,
    account_id: Option<i64>,
    /// Each edited pool's inline `profile:` as the draft has it, keyed by pool
    /// name. A pool the draft has not touched is absent and ranks as deployed.
    profiles: HashMap<String, Vec<ProfileEntry>>,
    edits: usize,
    touched: Instant,
}

#[derive(Default)]
struct Drafts {
    live: HashMap<String, Draft>,
    serial: u64,
}

impl Drafts {
    fn expire(&mut self) {
        let now = Instant::now();
        self.live
            .retain(|_, d| now.duration_since(d.touched) < DRAFT_IDLE);
    }

    fn open(&mut self, channel: &str, account_id: Option<i64>) -> String {
        self.expire();
        while self.live.len() >= MAX_DRAFTS {
            let Some(oldest) = self
                .live
                .iter()
                .min_by_key(|(_, d)| d.touched)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            tracing::info!(event = "ui.draft.evict", draft = %oldest, "draft evicted: the server holds {MAX_DRAFTS}");
            self.live.remove(&oldest);
        }
        self.serial += 1;
        let secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let id = format!("d{secs:x}-{}", self.serial);
        self.live.insert(
            id.clone(),
            Draft {
                channel: channel.to_string(),
                account_id,
                profiles: HashMap::new(),
                edits: 0,
                touched: Instant::now(),
            },
        );
        id
    }

    /// The draft `id` on `channel`, marked as used. A draft that expired, was
    /// discarded, or belongs to another channel is not found.
    fn touch(&mut self, channel: &str, id: &str) -> Result<&mut Draft, String> {
        self.expire();
        match self.live.get_mut(id) {
            Some(d) if d.channel == channel => {
                d.touched = Instant::now();
                Ok(d)
            }
            _ => Err(format!(
                "no draft {id:?} on channel {channel:?} (it expired, was discarded, or the server restarted)"
            )),
        }
    }
}

/// A pool's ranking with no draft: cached until the channel file or the
/// plugin script changes.
struct Baseline {
    stamp: (SystemTime, SystemTime),
    /// Each ranked item's 1-based place, for every item of the full ranking.
    places: HashMap<String, usize>,
    ranking: Vec<Value>,
    count: usize,
    profile: Value,
}

type BaselineKey = (String, String, Option<i64>);

/// What the Pools endpoints keep between requests.
#[derive(Default)]
pub(super) struct PoolState {
    drafts: Drafts,
    baselines: HashMap<BaselineKey, Arc<Baseline>>,
    /// A channel's default Plex account, once resolved (Tautulli is asked once).
    accounts: HashMap<String, (SystemTime, Option<i64>)>,
}

impl PoolState {
    pub(super) fn draft_count(&self) -> usize {
        self.drafts.live.len()
    }
}

/// A working copy of a real pool's config — everything but `profile` stays
/// exactly what the channel author wrote. `Pool` derives `Deserialize` +
/// `Serialize` but not `Clone`, so a JSON round trip stands in.
fn clone_pool(pool: &Pool) -> Result<Pool, String> {
    let value = serde_json::to_value(pool).map_err(|e| format!("clone pool: {e}"))?;
    serde_json::from_value(value).map_err(|e| format!("clone pool: {e}"))
}

/// How many lineup channels have a plugin-backed pattern pool (what a draft
/// can edit): `/api/health`'s `tunable_channels`.
pub(super) fn scored_channel_count(station_path: &Path) -> Result<usize, String> {
    let station = config::load_for_inspection(station_path)
        .map_err(|e| format!("load {}: {e}", station_path.display()))?;
    Ok(station
        .channels
        .iter()
        .filter(|c| has_scorer(&c.config))
        .count())
}

/// A lineup channel, loaded.
struct Located {
    config_path: PathBuf,
    dir: PathBuf,
    channel: config::ChannelConfig,
}

enum Lookup {
    Ready(Box<Located>),
    /// In the lineup, but its config fails to load.
    Broken {
        config_path: PathBuf,
        reason: String,
    },
    /// Not in the lineup.
    Missing(String),
}

/// The channel the lineup key `key` names. Only a lineup channel is ever
/// read: the key comes from the browser and the server has no auth of its own.
fn locate(state: &AppState, key: &str) -> Result<Lookup, String> {
    let path = &state.options.station;
    let station =
        config::load_for_inspection(path).map_err(|e| format!("load {}: {e}", path.display()))?;
    let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let keys = lineup_keys(&station.lineup, &base);
    let Some(position) = keys.iter().position(|k| k == key) else {
        return Ok(Lookup::Missing(format!(
            "no channel {key:?} in {}",
            path.display()
        )));
    };
    let config_path = station.lineup[position].clone();
    if let Some(dropped) = station
        .dropped
        .iter()
        .find(|d| d.config_path == config_path)
    {
        return Ok(Lookup::Broken {
            config_path,
            reason: dropped.reason.to_string(),
        });
    }
    match config::read_channel(&config_path) {
        Ok(channel) => Ok(Lookup::Ready(Box::new(Located {
            dir: config_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from(".")),
            config_path,
            channel,
        }))),
        Err(e) => Ok(Lookup::Broken {
            config_path,
            reason: e.to_string(),
        }),
    }
}

/// `locate` for the endpoints that need a loaded channel: a missing one is
/// the 404 to return, a broken one an error.
fn require(state: &AppState, key: &str) -> Result<Result<Located, (u16, String)>, String> {
    Ok(match locate(state, key)? {
        Lookup::Ready(l) => Ok(*l),
        Lookup::Missing(m) => Err((404, m)),
        Lookup::Broken { reason, .. } => {
            Err((400, format!("channel {key:?} fails to load: {reason}")))
        }
    })
}

/// Every pool of the channel, in block order.
fn pools_of(channel: &config::ChannelConfig) -> Vec<&Pool> {
    let mut seen = HashSet::new();
    channel
        .rule
        .all_blocks()
        .flat_map(|b| b.pools.iter())
        .filter(|p| seen.insert(p.name.as_str()))
        .collect()
}

/// The Plex account a channel's scorer reads history for: none for
/// `taste_scope: all_users`, else the one Tautulli resolves its single user to.
pub(super) fn channel_account_id(channel: &config::ChannelConfig) -> Result<Option<i64>, String> {
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

fn default_account(state: &mut AppState, key: &str, loc: &Located) -> Result<Option<i64>, String> {
    let stamp = mtime(&loc.config_path);
    if let Some((at, id)) = state.pools.accounts.get(key)
        && *at == stamp
    {
        return Ok(*id);
    }
    let id = channel_account_id(&loc.channel)?;
    state.pools.accounts.insert(key.to_string(), (stamp, id));
    Ok(id)
}

/// A scorer pool, ready to rank: a working copy with its datastore paths
/// expanded, and the plugin script it scores against.
struct Scorer {
    pool: Pool,
    plugin_path: PathBuf,
    plexdb_path: PathBuf,
    datastore_name: String,
    sources_effective: Option<Vec<(String, String)>>,
}

/// Resolve which plugin script to score against (the pool's deployed one, or
/// its namesake in [`Options::plugins_dir`]), expand the datastore grant's env
/// references, and clone the pool so profile edits never touch the real
/// `channel.yaml`.
fn build_scorer(state: &AppState, channel_dir: &Path, pool: &Pool) -> Result<Scorer, String> {
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
        Some(dir) => {
            let basename = deployed_path.file_name().ok_or_else(|| {
                format!("plugin path {} has no file name", deployed_path.display())
            })?;
            dir.join(basename)
        }
        None => deployed_path.clone(),
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

    let mut owned = clone_pool(pool)?;
    // `ScoreCache::prepare_profile` opens `pool.datastores.first()` itself,
    // reading its raw `path`, unlike `score::pick`, which is handed an
    // explicitly-expanded grant. Expanding here is what makes a `keyword:`
    // profile entry resolve instead of failing to open a literal
    // `${PLEXDB_SNAPSHOT_PATH}`.
    for ds in &mut owned.datastores {
        ds.path = config::expand_process_env(&ds.path)?;
    }
    let first = owned.datastores.first().expect("checked non-empty above");
    let plexdb_path = PathBuf::from(&first.path);
    let datastore_name = first.name.clone();
    let sources_effective =
        crate::score::effective_sources(&plugin_path, owned.sources.as_ref()).ok();
    Ok(Scorer {
        pool: owned,
        plugin_path,
        plexdb_path,
        datastore_name,
        sources_effective,
    })
}

struct Ranked {
    full: Vec<PickedItem>,
    profile: Value,
}

/// One full-pool `pick()` against the pool's real `sources:`, the profile the
/// scorer's working copy holds, and its real capabilities and datastores.
/// A fresh [`ScoreCache`] each time: it keys a resolved profile by pool name
/// alone, so one shared across channels would answer one channel's "movies"
/// with another's.
fn rank(
    catalog: &Catalog,
    scorer: &Scorer,
    channel_dir: &Path,
    account_id: Option<i64>,
) -> Result<Ranked, String> {
    let mut cache = ScoreCache::default();
    cache
        .prepare_pool(catalog, &scorer.pool, &scorer.plugin_path, channel_dir)
        .map_err(|e| format!("prepare: {e}"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut config = scorer.pool.config.clone().unwrap_or_else(|| json!({}));
    if let Some(obj) = config.as_object_mut() {
        obj.insert("exploration_fraction".to_string(), json!(0.0));
    }
    let inputs = ScoreInputs {
        target_count: RANKED_ITEMS,
        now,
        account_id,
        ..Default::default()
    };
    let grant = GrantedCapabilities::from_names(&scorer.pool.capabilities).with_datastores(&[
        DatastoreGrant {
            name: scorer.datastore_name.clone(),
            path: scorer.plexdb_path.display().to_string(),
        },
    ])?;
    let full = crate::score::pick(
        &cache,
        &scorer.plugin_path,
        scorer.pool.sources.as_ref(),
        &inputs,
        crate::variety::Dice::fixed(0),
        &scorer.pool.name,
        Some(&config),
        grant,
    )
    .map_err(|e| format!("pick: {e}"))?;
    Ok(Ranked {
        full,
        profile: profile_json(&cache, &scorer.pool.name),
    })
}

/// The first [`RANKING_SHOWN`] items of a ranking, each with the audit record
/// `score::pick` attached and its place. Against a baseline each also carries
/// `was` (its baseline place, null when the baseline lacked it) and
/// `movement` (places moved up; negative when it fell).
fn ranking_json(
    catalog: &Catalog,
    full: &[PickedItem],
    against: Option<&HashMap<String, usize>>,
) -> Vec<Value> {
    full.iter()
        .take(RANKING_SHOWN)
        .enumerate()
        .map(|(i, item)| {
            let entry = catalog.entry(&item.id).ok().flatten();
            let mut v = picked_item_json(catalog, &item.id, item.metadata.as_ref(), entry.as_ref());
            if let Some(obj) = v.as_object_mut() {
                let place = i + 1;
                obj.insert("rank".to_string(), json!(place));
                if let Some(places) = against {
                    let was = places.get(&item.id).copied();
                    obj.insert("was".to_string(), json!(was));
                    obj.insert(
                        "movement".to_string(),
                        json!(was.map(|w| w as i64 - place as i64)),
                    );
                }
            }
            v
        })
        .collect()
}

fn mtime(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// The pool's ranking with its deployed profile, from the cache when the
/// channel file and plugin are unchanged since it was ranked.
fn baseline(
    state: &mut AppState,
    key: &str,
    loc: &Located,
    scorer: &Scorer,
    account_id: Option<i64>,
) -> Result<Arc<Baseline>, String> {
    let stamp = (
        mtime(&loc.config_path),
        mtime(&scorer.plugin_path).max(mtime(&scorer.plexdb_path)),
    );
    let cache_key = (key.to_string(), scorer.pool.name.clone(), account_id);
    if let Some(b) = state.pools.baselines.get(&cache_key)
        && b.stamp == stamp
    {
        return Ok(Arc::clone(b));
    }
    let began = Instant::now();
    let ranked = rank(&state.catalog, scorer, &loc.dir, account_id)?;
    let places = ranked
        .full
        .iter()
        .enumerate()
        .map(|(i, item)| (item.id.clone(), i + 1))
        .collect();
    let built = Arc::new(Baseline {
        stamp,
        places,
        ranking: ranking_json(&state.catalog, &ranked.full, None),
        count: ranked.full.len(),
        profile: ranked.profile,
    });
    tracing::info!(
        event = "ui.pools.baseline",
        channel = key,
        pool = %scorer.pool.name,
        account_id,
        ranked = built.count,
        ms = began.elapsed().as_millis() as u64,
        "pool ranked with its deployed profile"
    );
    // The account comes from the client, so the cache is bounded.
    if state.pools.baselines.len() >= MAX_BASELINES {
        state.pools.baselines.clear();
    }
    state.pools.baselines.insert(cache_key, Arc::clone(&built));
    Ok(built)
}

/// The inline `profile:` entries as they would read pasted into the pool's
/// `channel.yaml`. These files carry hand-written prose comments that a
/// round-tripped rewrite would destroy, so the tool never edits the file;
/// the person pastes this in by hand. Empty when the profile has no entries
/// (the real file omits the key).
fn profile_yaml(entries: &[ProfileEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let list = serde_norway::to_string(entries).unwrap_or_default();
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

/// One pool as the Pools page shows it: its sources, config, profile and
/// ranking. `edited` is the draft's profile for this pool, when it has one.
/// A pool that cannot be scored still carries its real sources and config
/// with an `error`, so one failing pool never blanks the page.
fn pool_body(
    state: &mut AppState,
    key: &str,
    loc: &Located,
    pool: &Pool,
    account: &Result<Option<i64>, String>,
    edited: Option<&Vec<ProfileEntry>>,
) -> Value {
    let mut body = json!({
        "name": pool.name,
        "scored": pool.plugin.is_some(),
        "config": pool.config.clone().unwrap_or_else(|| json!({})),
        "config_authored": pool.config.is_some(),
        "profile": { "entries": [], "ignore_keywords": [], "exclusions": [] },
        "profile_yaml": "",
        "edited": false,
        "ranking": [],
        "ranking_count": 0,
    });
    let obj = body.as_object_mut().expect("json object");

    if pool.plugin.is_none() {
        let sources = match (&pool.expr, pool.groups.is_empty()) {
            (Some(expr), _) => json!({ "expr": expr }),
            (None, false) => json!({ "groups": pool.groups }),
            (None, true) => Value::Null,
        };
        obj.insert("sources".to_string(), sources);
        return body;
    }

    let outcome = (|| -> Result<(), String> {
        let account_id = account.clone()?;
        let mut scorer = build_scorer(state, &loc.dir, pool)?;
        obj.insert(
            "plugin".to_string(),
            json!(scorer.plugin_path.display().to_string()),
        );
        obj.insert(
            "sources_authored".to_string(),
            json!(pool.sources.is_some()),
        );
        obj.insert(
            "sources".to_string(),
            scorer
                .sources_effective
                .clone()
                .map(|pairs| Value::Object(pairs.into_iter().map(|(k, v)| (k, json!(v))).collect()))
                .unwrap_or(Value::Null),
        );
        let base = baseline(state, key, loc, &scorer, account_id)?;
        match edited {
            None => {
                obj.insert("profile".to_string(), base.profile.clone());
                obj.insert(
                    "profile_yaml".to_string(),
                    json!(profile_yaml(&pool.profile)),
                );
                obj.insert("ranking".to_string(), json!(base.ranking));
                obj.insert("ranking_count".to_string(), json!(base.count));
            }
            Some(entries) => {
                scorer.pool.profile = entries.clone();
                obj.insert("edited".to_string(), json!(true));
                obj.insert("profile_yaml".to_string(), json!(profile_yaml(entries)));
                let began = Instant::now();
                let ranked = rank(&state.catalog, &scorer, &loc.dir, account_id)?;
                obj.insert("profile".to_string(), ranked.profile);
                obj.insert(
                    "ranking".to_string(),
                    json!(ranking_json(
                        &state.catalog,
                        &ranked.full,
                        Some(&base.places)
                    )),
                );
                obj.insert("ranking_count".to_string(), json!(ranked.full.len()));
                tracing::info!(
                    event = "ui.pools.draft_ranked",
                    channel = key,
                    pool = %pool.name,
                    ms = began.elapsed().as_millis() as u64,
                    "pool ranked with a draft's profile"
                );
            }
        }
        Ok(())
    })();
    if let Err(e) = outcome {
        tracing::warn!(event = "ui.pools.error", channel = key, pool = %pool.name, error = %e, "pool could not be ranked");
        // `obj` still borrows `body`, so the error goes in through a fresh borrow.
        if let Some(obj) = body.as_object_mut() {
            obj.insert("error".to_string(), json!(e));
        }
    }
    body
}

/// Every pool of a channel, with `draft`'s edits applied when one is named.
fn pools_response(state: &mut AppState, key: &str, loc: &Located, draft: Option<&str>) -> Answer {
    let (account, edits, profiles, account_id) = match draft {
        Some(id) => match state.pools.drafts.touch(key, id) {
            Ok(d) => (Ok(d.account_id), d.edits, d.profiles.clone(), d.account_id),
            Err(e) => return not_found(e),
        },
        None => {
            let account = default_account(state, key, loc);
            let id = account.clone().ok().flatten();
            (account, 0, HashMap::new(), id)
        }
    };
    let mut pools = serde_json::Map::new();
    let mut order = Vec::new();
    for pool in pools_of(&loc.channel) {
        let body = pool_body(state, key, loc, pool, &account, profiles.get(&pool.name));
        order.push(pool.name.clone());
        pools.insert(pool.name.clone(), body);
    }
    Ok((
        200,
        json!({
            "channel": key,
            "config_path": loc.config_path.display().to_string(),
            "account_id": account_id,
            "draft": draft.map(|id| json!({ "id": id, "edits": edits })),
            "order": order,
            "pools": pools,
        }),
    ))
}

/// The draft `id`'s edits on `key`, for a projection. Marks the draft used.
pub(super) fn draft_edits(
    state: &mut AppState,
    key: &str,
    id: &str,
) -> Result<super::projection::DraftEdits, String> {
    let d = state.pools.drafts.touch(key, id)?;
    Ok(super::projection::DraftEdits {
        profiles: d
            .profiles
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        account_id: d.account_id,
    })
}

/// `GET /api/channels/{ch}/pools?draft=`
pub(super) fn api_pools(state: &mut AppState, key: &str, query: &str) -> Answer {
    let params = parse_query(query);
    let draft = params.get("draft").filter(|d| !d.is_empty()).cloned();
    let loc = match locate(state, key)? {
        Lookup::Ready(l) => *l,
        Lookup::Missing(m) => return not_found(m),
        Lookup::Broken {
            config_path,
            reason,
        } => {
            return Ok((
                200,
                json!({
                    "channel": key,
                    "config_path": config_path.display().to_string(),
                    "load_error": reason,
                }),
            ));
        }
    };
    pools_response(state, key, &loc, draft.as_deref())
}

/// `POST /api/channels/{ch}/drafts` `{account?}` → `{draft}`
pub(super) fn api_draft_open(state: &mut AppState, key: &str, body: &str) -> Answer {
    #[derive(serde::Deserialize, Default)]
    struct Request {
        #[serde(default)]
        account: Option<i64>,
    }
    let req: Request = if body.trim().is_empty() {
        Request::default()
    } else {
        serde_json::from_str(body).map_err(|e| format!("bad request body: {e}"))?
    };
    let loc = match require(state, key)? {
        Ok(l) => l,
        Err((404, m)) => return not_found(m),
        Err((_, m)) => return Err(m),
    };
    if !pools_of(&loc.channel).iter().any(|p| p.plugin.is_some()) {
        return Err(format!("channel {key:?} has no plugin-scored pool to edit"));
    }
    let account_id = match req.account {
        Some(id) => Some(id),
        None => default_account(state, key, &loc)?,
    };
    let id = state.pools.drafts.open(key, account_id);
    tracing::info!(event = "ui.draft.open", draft = %id, channel = key, account_id, "draft opened");
    Ok((
        200,
        json!({ "draft": id, "channel": key, "account_id": account_id }),
    ))
}

/// `POST /api/channels/{ch}/drafts/{draft}/edits` `{op, pool, entry}`
///
/// `entry` is the profile entry to `add` (the same keys a `profile:` entry
/// has in `channel.yaml`: one reference key and a `weight`), or `{index,
/// weight}` for `update`, or `{index}` for `remove`, where `index` is the
/// entry's place in the pool's inline `profile:` list.
pub(super) fn api_draft_edit(state: &mut AppState, key: &str, id: &str, body: &str) -> Answer {
    #[derive(serde::Deserialize)]
    struct Request {
        op: String,
        pool: String,
        #[serde(default)]
        entry: Value,
    }
    let req: Request = serde_json::from_str(body).map_err(|e| format!("bad request body: {e}"))?;
    let loc = match require(state, key)? {
        Ok(l) => l,
        Err((404, m)) => return not_found(m),
        Err((_, m)) => return Err(m),
    };
    let Some(pool) = pools_of(&loc.channel)
        .into_iter()
        .find(|p| p.name == req.pool)
    else {
        return Err(format!("no pool named {:?} in channel {key:?}", req.pool));
    };
    if pool.plugin.is_none() {
        return Err(format!(
            "pool {:?} is not scored by a plugin, so it has no profile to edit",
            req.pool
        ));
    }
    let mut profile = match state.pools.drafts.touch(key, id) {
        Ok(d) => d
            .profiles
            .get(&req.pool)
            .cloned()
            .unwrap_or_else(|| pool.profile.clone()),
        Err(e) => return not_found(e),
    };
    apply_edit(&mut profile, &req.op, &req.entry)?;
    let draft = state
        .pools
        .drafts
        .touch(key, id)
        .expect("touched just above");
    draft.profiles.insert(req.pool.clone(), profile);
    draft.edits += 1;
    let edits = draft.edits;

    let response = pools_response(state, key, &loc, Some(id))?;
    let top: Vec<String> = response.1["pools"][&req.pool]["ranking"]
        .as_array()
        .map(|r| {
            r.iter()
                .take(5)
                .map(|item| {
                    format!(
                        "{}. {}",
                        item["rank"],
                        item["title"].as_str().unwrap_or("?")
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    tracing::info!(
        event = "ui.draft.edit",
        draft = id,
        channel = key,
        pool = %req.pool,
        op = %req.op,
        entry = %req.entry,
        edits,
        top_ranks = ?top,
        "draft edited"
    );
    Ok(response)
}

fn apply_edit(profile: &mut Vec<ProfileEntry>, op: &str, entry: &Value) -> Result<(), String> {
    let index = || -> Result<usize, String> {
        let i = entry
            .get("index")
            .and_then(Value::as_u64)
            .ok_or_else(|| "entry needs an `index`".to_string())? as usize;
        if i >= profile.len() {
            return Err(format!("no profile entry at index {i}"));
        }
        Ok(i)
    };
    match op {
        "add" => {
            let added: ProfileEntry = serde_json::from_value(entry.clone())
                .map_err(|e| format!("bad profile entry: {e}"))?;
            // The check `profile::load` runs at config load, so a bad entry
            // fails here with the message a channel author would see.
            crate::profile::check(&added).map_err(|e| format!("profile entry {e}"))?;
            profile.push(added);
        }
        "update" => {
            let i = index()?;
            let weight = entry
                .get("weight")
                .and_then(Value::as_f64)
                .ok_or_else(|| "entry needs a numeric `weight`".to_string())?;
            if !weight.is_finite() || weight == 0.0 {
                return Err("weight must be finite and nonzero".to_string());
            }
            profile[i].weight = Some(weight);
        }
        "remove" => {
            let i = index()?;
            profile.remove(i);
        }
        other => {
            return Err(format!(
                "unknown op {other:?}; expected add, update or remove"
            ));
        }
    }
    Ok(())
}

/// `GET /api/channels/{ch}/drafts/{draft}/yaml?pool=`: the pool's profile
/// with the draft's edits, as it would read in `channel.yaml`.
pub(super) fn draft_yaml(
    state: &mut AppState,
    key: &str,
    id: &str,
    query: &str,
) -> Result<String, (u16, String)> {
    let params = parse_query(query);
    let pool_name = params
        .get("pool")
        .ok_or_else(|| (400, "missing `pool`".to_string()))?;
    let loc = require(state, key).map_err(|e| (400, e))??;
    let pool = pools_of(&loc.channel)
        .into_iter()
        .find(|p| p.name == *pool_name)
        .ok_or_else(|| {
            (
                404,
                format!("no pool named {pool_name:?} in channel {key:?}"),
            )
        })?;
    let draft = state.pools.drafts.touch(key, id).map_err(|e| (404, e))?;
    Ok(match draft.profiles.get(pool_name) {
        Some(edited) => profile_yaml(edited),
        None => profile_yaml(&pool.profile),
    })
}

/// `DELETE /api/channels/{ch}/drafts/{draft}`
pub(super) fn api_draft_discard(state: &mut AppState, key: &str, id: &str) -> Answer {
    if let Err(e) = state.pools.drafts.touch(key, id) {
        return not_found(e);
    }
    state.pools.drafts.live.remove(id);
    tracing::info!(
        event = "ui.draft.discard",
        draft = id,
        channel = key,
        "draft discarded"
    );
    Ok((200, json!({ "discarded": id })))
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

/// The plexdb a keyword search reads: the datastore of the pool `pool_name`
/// names on channel `channel`, or of the channel's first scorer pool when no
/// pool is named. A channel's scorer pools nearly always share one grant.
fn resolve_search_plexdb(
    state: &AppState,
    params: &HashMap<String, String>,
) -> Result<PathBuf, String> {
    let key = params
        .get("channel")
        .ok_or_else(|| "missing `channel`".to_string())?;
    let loc = match require(state, key)? {
        Ok(l) => l,
        Err((_, m)) => return Err(m),
    };
    let pools = pools_of(&loc.channel);
    let pool = match params.get("pool_name") {
        Some(name) => pools
            .into_iter()
            .find(|p| p.name == *name)
            .ok_or_else(|| format!("no pool named {name:?} in channel {key:?}"))?,
        None => pools
            .into_iter()
            .find(|p| p.plugin.is_some())
            .ok_or_else(|| format!("channel {key:?} has no plugin-scored pool"))?,
    };
    let grant = pool
        .datastores
        .first()
        .ok_or_else(|| format!("pool {:?} declares no datastores", pool.name))?;
    Ok(PathBuf::from(config::expand_process_env(&grant.path)?))
}

pub(super) fn api_search_keyword(state: &AppState, query: &str) -> Result<(u16, Value), String> {
    let params = parse_query(query);
    let q = params.get("q").cloned().unwrap_or_default();
    let plexdb_path = resolve_search_plexdb(state, &params)?;
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

pub(super) fn api_search_tag(state: &AppState, query: &str) -> Result<(u16, Value), String> {
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

pub(super) fn api_search_item(state: &AppState, query: &str) -> Result<(u16, Value), String> {
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

pub(super) fn api_tautulli_users() -> Result<(u16, Value), String> {
    let (url, key) = tautulli::credentials_from_env()
        .ok_or_else(|| "TAUTULLI_URL/TAUTULLI_API_KEY aren't set".to_string())?;
    let users = tautulli::fetch_users(&url, &key)?;
    let results: Vec<Value> = users
        .iter()
        .map(|u| json!({ "user_id": u.user_id, "display_name": u.display_name() }))
        .collect();
    Ok((200, json!({ "results": results })))
}

#[cfg(test)]
mod tests {
    use super::super::{Reply, dispatch};
    use super::*;
    use crate::catalog::{EntrySource, Source};

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

    /// A station of two channels over a four-movie catalog and a hand-built
    /// plexdb: `foryou` ranks movies with the committed taste-cosine script,
    /// `plain` has one CEL pool. Account 42 watched `mov-a` (keywords contact
    /// and time), so `mov-c` (contact) and `mov-d` (time, desert) rank on
    /// profile alone; a `desert` entry lifts `mov-d`.
    struct Fixture {
        _dir: tempfile::TempDir,
        state: AppState,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let db = root.join("plexdb.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE schema_version (version INTEGER NOT NULL);
             INSERT INTO schema_version (version) VALUES ({v});
             CREATE TABLE reader_shape (version INTEGER NOT NULL);
             INSERT INTO reader_shape (version) VALUES ({v});
             CREATE TABLE items (item_id TEXT PRIMARY KEY, type TEXT NOT NULL,
                 show_item_id TEXT, season INTEGER, episode INTEGER);
             CREATE TABLE enrichment (item_id TEXT NOT NULL, namespace TEXT NOT NULL,
                 key TEXT NOT NULL, value TEXT NOT NULL, fetched_at TEXT NOT NULL);
             CREATE TABLE plays (history_key TEXT PRIMARY KEY, item_id TEXT NOT NULL,
                 plex_account_id INTEGER NOT NULL, viewed_at INTEGER NOT NULL);
             CREATE TABLE keyword_forms (surface TEXT PRIMARY KEY, keyword TEXT NOT NULL);
             INSERT INTO items (item_id, type) VALUES
                 ('mov-a','movie'),('mov-b','movie'),('mov-c','movie'),('mov-d','movie');
             INSERT INTO enrichment (item_id, namespace, key, value, fetched_at) VALUES
                 ('mov-a','keywords','keyword','contact','2026-01-01T00:00:00+00:00'),
                 ('mov-a','keywords','keyword','time','2026-01-01T00:00:00+00:00'),
                 ('mov-c','keywords','keyword','contact','2026-01-01T00:00:00+00:00'),
                 ('mov-d','keywords','keyword','time','2026-01-01T00:00:00+00:00'),
                 ('mov-d','keywords','keyword','desert','2026-01-01T00:00:00+00:00');
             INSERT INTO plays (history_key, item_id, plex_account_id, viewed_at)
                 VALUES ('h1','mov-a',42,1700000000);
             INSERT INTO keyword_forms (surface, keyword) VALUES
                 ('desert','desert'),('contact','contact');",
            v = plexdb_reader::SUPPORTED_READER_SHAPE
        ))
        .unwrap();

        let plugin = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/plugins/taste-cosine.rhai")
            .canonicalize()
            .unwrap();
        let channels = root.join("channels");
        for name in ["foryou", "plain"] {
            std::fs::create_dir_all(channels.join(name)).unwrap();
        }
        std::fs::write(
            channels.join("foryou/channel.yaml"),
            format!(
                "number: 2\nname: foryou\ndisplay_name: \"For You\"\nwindow_days: 1\n\
                 roll_interval: \"60s\"\nretention_days: 1\n\
                 scoring:\n  nominal_item_secs: 5400\n\
                 rule:\n  blocks:\n    - mode: all\n      program:\n        title: \"For You\"\n      pools:\n\
                 \x20       - name: movies\n          plugin: \"{plugin}\"\n\
                 \x20         capabilities: [catalog_read, watch_history]\n\
                 \x20         datastores:\n            - name: taste\n              path: \"{db}\"\n\
                 \x20         config:\n            exploration_fraction: 0.0\n\
                 \x20         select: round_robin\n          advance: restart\n\
                 \x20     pattern:\n        - pool: movies\n          take: 2\n",
                plugin = plugin.display(),
                db = db.display()
            ),
        )
        .unwrap();
        std::fs::write(
            channels.join("plain/channel.yaml"),
            "number: 3\nname: plain\nwindow_days: 1\nroll_interval: \"60s\"\nretention_days: 1\n\
             scoring:\n  nominal_item_secs: 5400\n\
             rule:\n  blocks:\n    - mode: all\n      program:\n        title: \"Plain\"\n      pools:\n\
             \x20       - name: films\n          expr: 'item.type == \"movie\"'\n\
             \x20     pattern:\n        - pool: films\n          take: 1\n",
        )
        .unwrap();
        let station = root.join("station.yaml");
        std::fs::write(
            &station,
            "tz: \"America/Chicago\"\noutput_base: out\nchannels:\n  - channels/*/channel.yaml\n\
             catalog_path: catalog.db\ncatalog_refresh_secs: 86400\n\
             normalization:\n  audio:\n    format: aac\n    bitrate_kbps: 192\n    buffer_kbps: 384\n    channels: 2\n    sample_rate_hz: 48000\n    normalize_loudness: false\n  video:\n    format: h264\n    bit_depth: 8\n    width: 1280\n    height: 720\n    bitrate_kbps: 2000\n    buffer_kbps: 4000\n  subtitle:\n    mode: convert\n",
        )
        .unwrap();

        let catalog_path = root.join("catalog.db");
        let catalog = Catalog::open(&catalog_path).unwrap();
        for id in ["mov-a", "mov-b", "mov-c", "mov-d"] {
            catalog
                .upsert_entry(&CatalogEntry::new(id, "movie", id, Source::Plex))
                .unwrap();
            catalog
                .add_source(&EntrySource {
                    source: Source::LocalFs,
                    source_id: format!("fs-{id}"),
                    entry_id: id.to_string(),
                    playback_path: format!("/media/{id}.mkv"),
                    last_seen: None,
                    missing_since: None,
                    unairable_reason: None,
                })
                .unwrap();
        }
        drop(catalog);
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
        Fixture { _dir: dir, state }
    }

    fn call(state: &mut AppState, method: Method, path: &str, body: &str) -> (u16, Value) {
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        match dispatch(state, &method, path, query, body) {
            Reply::Json(status, body) => (status, body),
            _ => panic!("{path} did not answer JSON"),
        }
    }

    fn open_draft(state: &mut AppState, channel: &str) -> String {
        let (status, body) = call(
            state,
            Method::Post,
            &format!("/api/channels/{channel}/drafts"),
            "{\"account\": 42}",
        );
        assert_eq!(status, 200, "{body}");
        body["draft"].as_str().unwrap().to_string()
    }

    fn add_desert(state: &mut AppState, draft: &str) -> (u16, Value) {
        call(
            state,
            Method::Post,
            &format!("/api/channels/foryou/drafts/{draft}/edits"),
            r#"{"op":"add","pool":"movies","entry":{"keyword":"desert","weight":9.0}}"#,
        )
    }

    fn order(body: &Value) -> Vec<String> {
        body["pools"]["movies"]["ranking"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn pools_lists_a_scored_and_a_plain_channel() {
        let mut f = fixture();
        let (status, scored) = call(&mut f.state, Method::Get, "/api/channels/foryou/pools", "");
        assert_eq!(status, 200, "{scored}");
        assert_eq!(scored["order"], json!(["movies"]));
        assert_eq!(scored["pools"]["movies"]["scored"], true);
        assert_eq!(scored["pools"]["movies"]["error"], Value::Null);
        assert!(!order(&scored).is_empty(), "{scored}");

        let (status, plain) = call(&mut f.state, Method::Get, "/api/channels/plain/pools", "");
        assert_eq!(status, 200, "{plain}");
        assert_eq!(plain["pools"]["films"]["scored"], false, "{plain}");
        assert_eq!(
            plain["pools"]["films"]["sources"],
            json!({"expr": "item.type == \"movie\""})
        );
    }

    #[test]
    fn an_add_edit_changes_the_pools_ranking() {
        let mut f = fixture();
        let draft = open_draft(&mut f.state, "foryou");
        let (_, before) = call(&mut f.state, Method::Get, "/api/channels/foryou/pools", "");
        let (status, after) = add_desert(&mut f.state, &draft);
        assert_eq!(status, 200, "{after}");
        assert_eq!(after["draft"]["edits"], 1);
        assert_eq!(after["pools"]["movies"]["edited"], true);
        let (deployed, edited) = (order(&before), order(&after));
        assert_ne!(deployed, edited, "the edit left the ranking alone: {after}");
        let moved = after["pools"]["movies"]["ranking"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == "mov-d")
            .unwrap();
        let place = |order: &[String]| order.iter().position(|id| id == "mov-d").unwrap() as i64;
        assert_eq!(
            moved["movement"].as_i64().unwrap(),
            place(&deployed) - place(&edited)
        );
        assert!(moved["movement"].as_i64().unwrap() > 0, "{moved}");
    }

    #[test]
    fn two_drafts_on_one_channel_do_not_see_each_others_edits() {
        let mut f = fixture();
        let (a, b) = (
            open_draft(&mut f.state, "foryou"),
            open_draft(&mut f.state, "foryou"),
        );
        assert_ne!(a, b);
        let (_, edited) = add_desert(&mut f.state, &a);
        let (status, other) = call(
            &mut f.state,
            Method::Get,
            &format!("/api/channels/foryou/pools?draft={b}"),
            "",
        );
        assert_eq!(status, 200, "{other}");
        assert_eq!(other["draft"]["edits"], 0);
        assert_eq!(other["pools"]["movies"]["edited"], false);
        let (_, deployed) = call(&mut f.state, Method::Get, "/api/channels/foryou/pools", "");
        assert_eq!(order(&other), order(&deployed));
        assert_ne!(order(&other), order(&edited));
    }

    #[test]
    fn the_yaml_is_the_edited_profile_and_a_discarded_draft_is_gone() {
        let mut f = fixture();
        let draft = open_draft(&mut f.state, "foryou");
        add_desert(&mut f.state, &draft);
        let yaml = draft_yaml(&mut f.state, "foryou", &draft, "pool=movies").unwrap();
        assert!(yaml.starts_with("profile:\n"), "{yaml}");
        assert!(
            yaml.contains("keyword: desert") && yaml.contains("weight: 9"),
            "{yaml}"
        );

        let (status, _) = call(
            &mut f.state,
            Method::Delete,
            &format!("/api/channels/foryou/drafts/{draft}"),
            "",
        );
        assert_eq!(status, 200);
        let (status, body) = call(
            &mut f.state,
            Method::Get,
            &format!("/api/channels/foryou/pools?draft={draft}"),
            "",
        );
        assert_eq!(status, 404, "{body}");
        assert_eq!(
            draft_yaml(&mut f.state, "foryou", &draft, "pool=movies")
                .unwrap_err()
                .0,
            404
        );
    }

    #[test]
    fn a_draft_belongs_to_its_channel_and_the_server_keeps_a_bounded_number() {
        let mut f = fixture();
        let draft = open_draft(&mut f.state, "foryou");
        let (status, _) = call(
            &mut f.state,
            Method::Get,
            &format!("/api/channels/plain/pools?draft={draft}"),
            "",
        );
        assert_eq!(status, 404);
        for _ in 0..MAX_DRAFTS + 3 {
            open_draft(&mut f.state, "foryou");
        }
        assert_eq!(f.state.pools.draft_count(), MAX_DRAFTS);
    }

    #[test]
    fn a_bad_edit_is_refused_and_leaves_the_draft_unchanged() {
        let mut f = fixture();
        let draft = open_draft(&mut f.state, "foryou");
        for body in [
            r#"{"op":"add","pool":"movies","entry":{"keyword":"x","genre":"y","weight":1}}"#,
            r#"{"op":"update","pool":"movies","entry":{"index":7,"weight":1}}"#,
            r#"{"op":"add","pool":"nope","entry":{"keyword":"x","weight":1}}"#,
        ] {
            let (status, resp) = call(
                &mut f.state,
                Method::Post,
                &format!("/api/channels/foryou/drafts/{draft}/edits"),
                body,
            );
            assert_eq!(status, 400, "{body}: {resp}");
        }
        let (_, pools) = call(
            &mut f.state,
            Method::Get,
            &format!("/api/channels/foryou/pools?draft={draft}"),
            "",
        );
        assert_eq!(pools["draft"]["edits"], 0);
    }

    /// Asks again until both simulations have finished.
    fn graded(state: &mut AppState, query: &str) -> Value {
        for _ in 0..1200 {
            let (status, body) = call(state, Method::Get, query, "");
            if status == 200 {
                return body;
            }
            assert_eq!(status, 202, "{body}");
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("the simulations never finished");
    }

    #[test]
    fn deployed_and_draft_simulations_are_cached_under_different_keys() {
        let mut f = fixture();
        let draft = open_draft(&mut f.state, "foryou");
        add_desert(&mut f.state, &draft);
        let query = format!("/api/channels/foryou/review?draft={draft}&project=1d");
        let body = graded(&mut f.state, &query);
        let (dep, dra) = (
            body["deployed"]["projection"]["key"]
                .as_str()
                .unwrap()
                .to_string(),
            body["draft"]["projection"]["key"]
                .as_str()
                .unwrap()
                .to_string(),
        );
        assert_ne!(dep, dra);
        assert_eq!(
            body["draft"]["projection"]["inputs"]["draft"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
        assert_ne!(
            body["deployed"]["projection"]["inputs"]["draft"],
            body["draft"]["projection"]["inputs"]["draft"]
        );
        assert_eq!(
            body["draft"]["projection"]["changed"],
            json!([]),
            "the first draft projection has no earlier draft to differ from"
        );

        // A second draft with the identical edit set is the same projection:
        // it answers at once, with no 202.
        let again = open_draft(&mut f.state, "foryou");
        add_desert(&mut f.state, &again);
        let (status, hit) = call(
            &mut f.state,
            Method::Get,
            &format!("/api/channels/foryou/review?draft={again}&project=1d"),
            "",
        );
        assert_eq!(status, 200, "{hit}");
        assert_eq!(hit["draft"]["projection"]["key"], dra.as_str());
        assert_eq!(hit["deployed"]["projection"]["key"], dep.as_str());
    }

    #[test]
    fn a_draft_that_changes_what_airs_names_the_airings_and_the_grade_that_moved() {
        let mut f = fixture();
        let draft = open_draft(&mut f.state, "foryou");
        add_desert(&mut f.state, &draft);
        let body = graded(
            &mut f.state,
            &format!("/api/channels/foryou/review?draft={draft}&project=3d"),
        );
        let changed = body["diff"]["changed_airings"].as_array().unwrap();
        assert!(!changed.is_empty(), "{}", body["diff"]);
        let first = &changed[0];
        assert!(
            first["id"].is_string() && first["start"].is_i64(),
            "{first}"
        );
        assert_ne!(first["id"], first["was"], "{first}");
        let grades = body["diff"]["grades"].as_array().unwrap();
        assert!(!grades.is_empty());
        // Four movies cannot move a grade's 0-3 score, but the draft does
        // change what a graded principle measured.
        assert!(
            grades
                .iter()
                .any(|g| g["deployed_measure"] != g["draft_measure"]),
            "{grades:?}"
        );
        for g in grades {
            let dir = g["direction"].as_str().unwrap();
            let (was, now) = (g["deployed"].as_i64(), g["draft"].as_i64());
            let expected = match (was, now) {
                (Some(w), Some(n)) if n > w => "up",
                (Some(w), Some(n)) if n < w => "down",
                _ => "same",
            };
            assert_eq!(dir, expected, "{g}");
        }
        assert_eq!(
            body["deployed"]["span"]["label"], "real + 3 days projected",
            "the draft window is the deployed window"
        );

        // A draft with no edit is the deployed schedule: nothing changed.
        let idle = open_draft(&mut f.state, "foryou");
        let same = graded(
            &mut f.state,
            &format!("/api/channels/foryou/review?draft={idle}&project=3d"),
        );
        assert_eq!(same["diff"]["changed_airings"], json!([]));
    }

    #[test]
    fn a_missing_draft_is_a_404_on_the_review() {
        let mut f = fixture();
        let (status, body) = call(
            &mut f.state,
            Method::Get,
            "/api/channels/foryou/review?draft=nope",
            "",
        );
        assert_eq!(status, 404, "{body}");
    }
}

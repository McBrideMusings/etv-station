//! Scorer plugins (#74) — a pool whose items come from a Rhai script instead of
//! a CEL expression.
//!
//! The station computes no taste score of its own. It supplies inputs — the
//! catalog, recent watch history, what this channel already aired, and how many
//! items the generation needs — and takes back an ordered list of `entry_id`s.
//! Every judgment between those two points (what to surface, how to weight it,
//! how long to suppress a repeat) lives inside the script, so swapping one
//! script for another changes nothing here. See ADR 0002 for why this replaces
//! a pool's `expr` rather than its `order`.
//!
//! # The contract
//!
//! A plugin declares which hooks it implements (#159) and, for each one it
//! claims, the functions that hook requires. `pool_provider` — everything
//! below except `hooks()` itself — is what a `plugin:` pool has always meant.
//! `sequencer` is the second hook (#169, [`crate::sequence`]): a block names
//! one instead of `pattern`, and it draws its own timeline from this
//! module's item-map marshalling ([`load_items`]) rather than the pattern
//! walk.
//!
//! ```rhai
//! // Which hooks this script implements. Read at config load time, without
//! // running sources() or pick() — so a script that declares the wrong hook,
//! // or none, fails to load rather than failing mid-generation.
//! fn hooks() { ["pool_provider"] }
//!
//! // Which host capabilities this script needs (#167). Also read at config
//! // load time, alongside hooks(). Omitting capabilities() declares none —
//! // a script that only reads ctx.recent/ctx.config/ctx.target_count/ctx.now
//! // needs nothing here. `ctx.sets` and `ctx.history` below are the two
//! // gated inputs: reaching for either without declaring (and being
//! // granted) the matching capability fails the pick() call that reaches
//! // for it, naming the capability. A named external datastore is declared
//! // as `#{ datastore: "name" }` instead of a bare string; see
//! // `config::Pool::datastores`.
//! fn capabilities() { ["catalog_read", "watch_history"] }
//!
//! // Every catalog query this plugin will read, named. Run once, up front.
//! fn sources() {
//!     #{
//!         movies:   `item.type == "movie"`,
//!         episodes: `item.type == "episode"`,
//!     }
//! }
//!
//! // Returns entry_ids, most-wanted first — or, per entry, a record
//! // widening what a bare id can say (#166). Both shapes may appear in the
//! // same returned array.
//! fn pick(ctx) {
//!     // ctx.sets.movies   — array of item maps, one per match
//!     // ctx.pool          — the name of the pool asking
//!     // ctx.config        — the pool's `config:` block, passed through unread
//!     // ctx.target_count  — how many items the generation needs
//!     // ctx.history       — recent server-wide watch events
//!     // ctx.recent        — entry_ids this channel aired most recently
//!     // ctx.now           — unix seconds at generation time
//!
//!     // A bare id — everything before #166 already means this:
//!     // "ghost"
//!     //
//!     // A record — same id, plus why it was picked and how much of its
//!     // series to take. `entry_id` is required; `metadata` and `take` are
//!     // each optional and independent of one another.
//!     // #{ entry_id: "ghost", metadata: #{ reason: "won an Oscar" }, take: 3 }
//! }
//! ```
//!
//! `metadata` rides `serde_json::Value` (#166), reaches
//! `PlayoutItem::metadata` in the emitted playout JSON untouched, and refuses
//! a non-finite float at the moment it is picked, naming the key — the same
//! refusal [`crate::config::Pool::config`] makes at load, reusing
//! `etv_overlay::config_carrier` rather than a second implementation of that
//! rule. `take` overrides the pattern step's own `take` for that entry's
//! series; carrying it through pool resolution is this slice's job, honouring
//! it in the pattern walk is #173's.
//!
//! `ctx.config` is the one input the station does not construct: it is whatever
//! the channel author wrote under that pool, converted and handed over with
//! nothing read out of it. Its keys are the *script's* vocabulary — a scorer
//! decides what `affinity_window_days` means, and ETV never learns. That keeps a
//! script swappable at the cost of a mistyped key being silent, which is the
//! trade [`crate::config::Pool::config`] documents in full.
//!
//! Queries are declared up front rather than callable mid-`pick` so that a
//! malformed expression fails before any ranking work, and so the catalog is
//! read exactly once per generation no matter how the script is written.
//!
//! Each item map carries every column on `entries` plus every tag namespace
//! (genres, cast, labels, …), so extending an algorithm to weigh a new signal
//! is a script edit, never a rebuild.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use etv_overlay::config_carrier;
use rhai::serde::DynamicDeserializer;
use rhai::{Array, Dynamic, Engine, EvalAltResult, Map, Scope};

use crate::catalog::{Catalog, TagNs};

/// One watch event from the server's history, as handed to a plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchEvent {
    /// The catalog entry watched, when it could be matched to one. History
    /// rows that match nothing in the catalog are dropped before they get here.
    pub entry_id: String,
    /// Unix seconds when the watch stopped.
    pub watched_at: i64,
    /// Who watched it, as Tautulli reports them — the account's friendly name
    /// when it has one, else its username (#113).
    ///
    /// `None` when the history row named nobody, which is what a Tautulli that
    /// has anonymised the row looks like. Nothing ranks on this; it exists so a
    /// channel can say on screen and in the guide who has been watching. A
    /// `single_user` channel's rows all carry the same name, which is why the
    /// attribution line is only interesting on a pooled channel.
    pub watcher: Option<String>,
}

/// Everything the station hands a plugin besides the catalog itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScoreInputs {
    /// How many items this generation needs. The plugin chooses its own
    /// corpus, so nothing else can size it, and the script cannot derive the
    /// window duration itself. Overshooting is harmless — the pattern simply
    /// never reaches the tail.
    pub target_count: usize,
    /// Recent watch activity for the whole server, pooled with no user
    /// dimension. Empty when the history source is unreachable: a plugin still
    /// has release dates, `last_seen`, tags, and `recent` to rank on, so a
    /// history outage degrades the ranking instead of failing the generation.
    ///
    /// Shared rather than owned (#126): one fetch+join per station tick is
    /// handed to every channel, and every generation inside a channel's
    /// catch-up rebuilds these inputs. A `Vec` here made both of those a deep
    /// copy of up to a thousand events; behind an `Arc` they are a refcount.
    pub history: Arc<[WatchEvent]>,
    /// What this channel aired most recently, newest last, from the
    /// play-history ledger.
    pub recent: Vec<String>,
    /// Unix seconds at generation time. Passed in rather than read inside the
    /// script so a generation is reproducible from its inputs.
    pub now: i64,
    /// The station's configured tz (ADR 0004) — how a `sequencer` block
    /// ([`crate::sequence`]) converts `ctx.window.from` and other unix-second
    /// instants into the local weekday/hour/minute a daypart script places its
    /// pools against. Unused by a `pool_provider` scorer, which reads no
    /// clock. `None` at the stateless entry points
    /// ([`crate::resolve::resolve_channel`], [`crate::determinism`]'s own
    /// snapshot before this field existed) falls back to UTC inside
    /// [`crate::sequence`] — the same "least surprising" tolerance
    /// `resolve_channel` already gives an unpinned `window_start`.
    pub tz: Option<&'static time_tz::Tz>,
}

/// The tag namespaces exposed to a plugin, each as an array under its own key.
const EXPOSED_TAGS: &[(&str, TagNs)] = &[
    ("genres", TagNs::Genre),
    ("labels", TagNs::Label),
    ("cast", TagNs::Cast),
    ("directors", TagNs::Director),
    ("writers", TagNs::Writer),
    ("producers", TagNs::Producer),
    ("countries", TagNs::Country),
];

/// One generation's compiled scripts and resolved query sets, keyed by script
/// path.
///
/// A channel commonly points several pools at the same script — a `movies` pool
/// and a `shows` pool ranked by the same taste — and `sources()` takes no
/// arguments, so what it declares cannot vary by pool. Without this each pool
/// would recompile the script and re-run every declared query, materializing
/// the same slice of the library once per pool.
///
/// Scoped to a single generation and dropped with it, so a catalog that changes
/// between generations is picked up on the next one.
#[derive(Debug, Default)]
pub struct ScoreCache {
    entries: HashMap<PathBuf, CachedScript>,
    /// The metadata/take-override half of what a plugin pool's `pick()` just
    /// returned (#166), keyed by `(pool name, entry_id)`. `resolve_pool_sources`
    /// still returns bare ids, so widening it would ripple into every caller.
    /// Instead [`crate::pattern::build`] and [`crate::sequence::build`] (#201)
    /// each read the metadata half back from here once pool resolution
    /// finishes and flatten their own copy for their own return.
    ///
    /// The take-override half has only one reader, `pattern::PoolRuntime`
    /// (#173) — see the sequencer's own "Out of scope" note (#201) on why a
    /// per-entry take has no meaning there yet.
    ///
    /// Keyed on the pool too, not on `entry_id` alone: two plugin pools in one
    /// block may resolve the same id from overlapping catalog queries, and
    /// without the pool half of the key, the second pool's record would
    /// silently overwrite the first's in this map (a corruption that could
    /// bite whichever pool actually draws the id, whether or not it was the
    /// one that wrote here last). [`pattern::PoolRuntime::take_overrides`]
    /// reads its own pool's slice only, so this closes that hole for takes;
    /// the flattened `entry_id -> metadata` map [`pattern::build`] returns is
    /// necessarily flat again by the time two pools' *outputs* — not just
    /// their candidate sets — collide on one id, which is the same "blind
    /// across pools" limit `Pool::constraints` already documents.
    ///
    /// Only ids that actually attached something get an entry — a plugin
    /// returning bare ids leaves this empty, which is the whole point of the
    /// widening being additive.
    pub(crate) picked_extras: HashMap<(String, String), PickedExtra>,
}

/// The metadata blob and/or take override one `entry_id` carried in a plugin
/// pool's returned record (#166). See [`ScoreCache::picked_extras`].
#[derive(Debug, Clone, Default)]
pub(crate) struct PickedExtra {
    pub metadata: Option<serde_json::Value>,
    pub take_override: Option<usize>,
}

#[derive(Debug)]
struct CachedScript {
    ast: rhai::AST,
    /// The resolved sets, already a shared Rhai value, so handing it to a
    /// second pool costs a refcount rather than a deep copy of every item map.
    sets: Dynamic,
}

/// A scorer plugin's inputs plus the directory its path is relative to.
///
/// `base_dir` is the channel config file's directory, matching how a `block:`
/// include resolves: a config's paths mean what they mean relative to the file
/// they are written in, not to wherever the daemon happens to be launched from.
#[derive(Debug, Clone, Copy)]
pub struct ScoreEnv<'a> {
    pub inputs: &'a ScoreInputs,
    pub base_dir: &'a Path,
}

impl ScoreEnv<'_> {
    /// Where a `plugin:` path actually lives. An absolute path is used as
    /// written; a relative one hangs off the channel config's directory.
    pub fn resolve_path(&self, plugin: &Path) -> PathBuf {
        resolve_plugin_path(self.base_dir, plugin)
    }
}

/// Where a `plugin:` path actually lives relative to `base_dir` (the channel
/// config file's directory). An absolute path is used as written.
///
/// Free rather than only a `ScoreEnv` method because load-time hook validation
/// ([`declared_hooks`]) needs the same resolution with no [`ScoreInputs`] to
/// build a `ScoreEnv` around.
pub fn resolve_plugin_path(base_dir: &Path, plugin: &Path) -> PathBuf {
    if plugin.is_absolute() {
        plugin.to_path_buf()
    } else {
        base_dir.join(plugin)
    }
}

/// Hook names the station understands (#159). `pool_provider` is wired up —
/// it is what a `plugin:` pool has always meant. `sequencer` is declarable
/// only; nothing calls it yet, so a script may claim it but the station does
/// not act on it until a later slice implements the hook itself.
pub const KNOWN_HOOKS: &[&str] = &["pool_provider", "sequencer"];

/// Read the hook names a plugin script declares, without running its scoring
/// path or touching the catalog.
///
/// Compiles the script and calls `hooks()` alone — `sources()` and `pick()`
/// are never invoked here, which is what lets a load-time refusal happen
/// without a catalog handle or a full generation.
///
/// Every failure names the script: an unreadable or uncompilable file, a
/// missing or throwing `hooks()`, a non-string entry, an empty array (a plugin
/// must declare at least one hook), or a name outside [`KNOWN_HOOKS`].
pub fn declared_hooks(script_path: &Path) -> Result<Vec<String>, String> {
    let source = std::fs::read_to_string(script_path)
        .map_err(|e| format!("read plugin {}: {e}", script_path.display()))?;
    let engine = engine();
    let ast = engine
        .compile(&source)
        .map_err(|e| format!("compile plugin {}: {e}", script_path.display()))?;

    let mut scope = Scope::new();
    let declared: Array = engine
        .call_fn(&mut scope, &ast, "hooks", ())
        .map_err(|e| format!("plugin {}: hooks(): {e}", script_path.display()))?;

    if declared.is_empty() {
        return Err(format!(
            "plugin {} declares no hooks — a plugin must declare at least one \
             of: {}",
            script_path.display(),
            KNOWN_HOOKS.join(", ")
        ));
    }

    let mut hooks = Vec::with_capacity(declared.len());
    for value in declared {
        let name = value.into_string().map_err(|actual| {
            format!(
                "plugin {}: hooks() must return an array of strings, got {actual}",
                script_path.display()
            )
        })?;
        if !KNOWN_HOOKS.contains(&name.as_str()) {
            return Err(format!(
                "plugin {} declares unknown hook {name:?} — known hooks are: {}",
                script_path.display(),
                KNOWN_HOOKS.join(", ")
            ));
        }
        hooks.push(name);
    }
    Ok(hooks)
}

/// A host capability a plugin script can declare via `capabilities()` (#167).
/// `CatalogRead` gates `ctx.sets`; `WatchHistory` gates `ctx.history`.
/// `Datastore` names an external handle the channel config must separately
/// grant — what is behind that handle is out of scope for this station (see
/// [`crate::config::Pool::datastores`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Capability {
    CatalogRead,
    WatchHistory,
    Datastore(String),
}

impl Capability {
    /// The one name this capability answers to — the string a script's
    /// `capabilities()` returns and the string a pool grants under
    /// `capabilities:` or as a `datastores:` entry name. Simple capabilities
    /// and datastore names share a single namespace, which is what lets the
    /// load-time cross-check ([`crate::config::validate`]) compare the two
    /// sides as plain sets of names.
    pub fn name(&self) -> &str {
        match self {
            Capability::CatalogRead => "catalog_read",
            Capability::WatchHistory => "watch_history",
            Capability::Datastore(name) => name,
        }
    }
}

/// The simple (non-parameterized) capability names `capabilities()` may
/// return as bare strings. A named datastore is declared separately, as
/// `#{ datastore: "name" }` — see [`declared_capabilities`].
pub const KNOWN_CAPABILITIES: &[&str] = &["catalog_read", "watch_history"];

/// Read the capabilities a plugin script declares, without running its
/// scoring path or touching the catalog — the same load-time-only shape as
/// [`declared_hooks`].
///
/// Unlike `hooks()`, `capabilities()` is optional: a script with no such
/// function declares nothing, which is a valid answer for a plugin that only
/// reads the ambient inputs (`ctx.pool`, `ctx.config`, `ctx.target_count`,
/// `ctx.now`, `ctx.recent`). Presence is checked against the compiled AST
/// rather than by calling and catching a "function not found" error, so a
/// script whose `capabilities()` genuinely throws still fails loudly instead
/// of being read as "declares nothing".
///
/// Every failure names the script: an unreadable or uncompilable file, a
/// throwing `capabilities()`, an entry that is neither a string nor a
/// `#{ datastore: "name" }` map, an unknown string capability, or a datastore
/// entry with an empty or non-string name.
pub fn declared_capabilities(script_path: &Path) -> Result<Vec<Capability>, String> {
    let source = std::fs::read_to_string(script_path)
        .map_err(|e| format!("read plugin {}: {e}", script_path.display()))?;
    let engine = engine();
    let ast = engine
        .compile(&source)
        .map_err(|e| format!("compile plugin {}: {e}", script_path.display()))?;

    if !ast.iter_functions().any(|f| f.name == "capabilities") {
        return Ok(Vec::new());
    }

    let mut scope = Scope::new();
    let declared: Array = engine
        .call_fn(&mut scope, &ast, "capabilities", ())
        .map_err(|e| format!("plugin {}: capabilities(): {e}", script_path.display()))?;

    let mut out = Vec::with_capacity(declared.len());
    for value in declared {
        if value.is_map() {
            let map = value.cast::<Map>();
            let ds_value = map.get("datastore").ok_or_else(|| {
                format!(
                    "plugin {}: capabilities() entry {map:?} is not a recognized capability — \
                     expected a string or `#{{ datastore: \"name\" }}`",
                    script_path.display()
                )
            })?;
            let ds_name = ds_value.clone().into_string().map_err(|actual| {
                format!(
                    "plugin {}: capabilities() datastore name must be a string, got {actual}",
                    script_path.display()
                )
            })?;
            if ds_name.trim().is_empty() {
                return Err(format!(
                    "plugin {}: capabilities() declares a datastore with an empty name",
                    script_path.display()
                ));
            }
            out.push(Capability::Datastore(ds_name));
            continue;
        }

        let name = value.into_string().map_err(|actual| {
            format!(
                "plugin {}: capabilities() must return an array of strings or \
                 `#{{ datastore: \"name\" }}` maps, got {actual}",
                script_path.display()
            )
        })?;
        match name.as_str() {
            "catalog_read" => out.push(Capability::CatalogRead),
            "watch_history" => out.push(Capability::WatchHistory),
            _ => {
                return Err(format!(
                    "plugin {} declares unknown capability {name:?} — known capabilities \
                     are: {}, or `#{{ datastore: \"name\" }}`",
                    script_path.display(),
                    KNOWN_CAPABILITIES.join(", ")
                ));
            }
        }
    }
    Ok(out)
}

/// Which capabilities a channel config granted a plugin pool (#167),
/// resolved from [`crate::config::Pool::capabilities`] before [`pick`] runs.
/// Datastore grants carry no runtime handle in this slice — a grant only
/// proves at load time that its location is openable
/// ([`crate::config::validate`]); nothing here exposes it to the script.
#[derive(Debug, Clone, Copy, Default)]
pub struct GrantedCapabilities {
    pub catalog_read: bool,
    pub watch_history: bool,
}

impl GrantedCapabilities {
    /// Read a pool's granted capability names back into the two flags [`pick`]
    /// acts on, so a caller never has to know how a capability is spelled.
    /// Names that are not simple capabilities — a datastore grant — are
    /// ignored: nothing in this slice hands a script a datastore handle.
    pub fn from_names(names: &[String]) -> Self {
        let has = |want: &str| names.iter().any(|n| n == want);
        Self {
            catalog_read: has("catalog_read"),
            watch_history: has("watch_history"),
        }
    }
}

/// The `ctx` argument handed to a plugin's `pick()`.
///
/// A registered custom type rather than a plain map, so that `ctx.sets` and
/// `ctx.history` — the two capability-gated fields (#167) — can refuse a
/// script that reaches for either without having declared and been granted
/// it, at the moment of the read, with a message that names the capability.
/// Every other field is an ordinary infallible getter; nothing about their
/// values or names changes from before this existed, so a script that never
/// touches a gated field is unaffected.
#[derive(Debug, Clone)]
struct ScoreCtx {
    /// Filled only when `catalog_read` was granted; `None` is what turns a
    /// read of `ctx.sets` into the error [`ungranted`] words.
    sets: Option<Dynamic>,
    pool: String,
    config: Dynamic,
    target_count: i64,
    now: i64,
    /// Filled only when `watch_history` was granted — same gate as `sets`.
    history: Option<Dynamic>,
    recent: Dynamic,
}

/// What a script gets back for reading a `ctx` field its pool was not granted
/// (#167) — one place, so both gated fields refuse in the same words.
fn ungranted(capability: &str) -> Box<EvalAltResult> {
    format!("capability `{capability}` not declared").into()
}

impl ScoreCtx {
    fn get_sets(&mut self) -> Result<Dynamic, Box<EvalAltResult>> {
        self.sets.clone().ok_or_else(|| ungranted("catalog_read"))
    }
    fn get_pool(&mut self) -> String {
        self.pool.clone()
    }
    fn get_config(&mut self) -> Dynamic {
        self.config.clone()
    }
    fn get_target_count(&mut self) -> i64 {
        self.target_count
    }
    fn get_now(&mut self) -> i64 {
        self.now
    }
    fn get_history(&mut self) -> Result<Dynamic, Box<EvalAltResult>> {
        self.history
            .clone()
            .ok_or_else(|| ungranted("watch_history"))
    }
    fn get_recent(&mut self) -> Dynamic {
        self.recent.clone()
    }
}

/// The Rhai engine every plugin call uses.
///
/// The nesting limits are set explicitly because Rhai's defaults are lower in a
/// debug build than a release one, so leaving them alone means a plugin that
/// compiles for the daemon can fail under `cargo test` — a difference a plugin
/// author has no way to see coming. These are generous enough for ordinary
/// scripts and still bounded, so a runaway nesting depth fails to compile rather
/// than overflowing the stack.
///
/// Compiling and calling must use the same configuration, which is the whole
/// reason this is one function rather than two call sites that drifted.
///
/// [`ScoreCtx`] is registered here unconditionally, including for the
/// `hooks()`/`capabilities()`-only compiles in [`declared_hooks`] and
/// [`declared_capabilities`] — harmless, since nothing there ever constructs
/// one, and it keeps every plugin call sharing one engine configuration.
///
/// `pub(crate)` so [`crate::sequence`] (#169) shares it rather than a second
/// engine configuration that could drift from this one.
pub(crate) fn engine() -> Engine {
    let mut engine = Engine::new();
    engine.set_max_expr_depths(128, 64);
    engine
        .register_type_with_name::<ScoreCtx>("ScoreCtx")
        .register_get("sets", ScoreCtx::get_sets)
        .register_get("pool", ScoreCtx::get_pool)
        .register_get("config", ScoreCtx::get_config)
        .register_get("target_count", ScoreCtx::get_target_count)
        .register_get("now", ScoreCtx::get_now)
        .register_get("history", ScoreCtx::get_history)
        .register_get("recent", ScoreCtx::get_recent);
    engine
}

impl ScoreCache {
    /// Compile `script_path` and resolve every query its `sources()` declares,
    /// if this cache has not already done so.
    ///
    /// **This is the only half of scoring that touches the catalog.** Callers
    /// run every pool's `prepare` up front, while the catalog handle is in
    /// scope, and then drop that handle before any [`pick`] runs — so the
    /// database cannot be held open across a plugin's ranking work, which is
    /// unbounded and entirely the script author's to spend. Before this split,
    /// a scorer that took six minutes to rank a library held the station's only
    /// database connection for all six of them and every other channel queued
    /// behind it.
    pub fn prepare(&mut self, catalog: &Catalog, script_path: &Path) -> Result<(), String> {
        if self.entries.contains_key(script_path) {
            return Ok(());
        }
        let cached = compile_and_resolve(catalog, &engine(), script_path)?;
        self.entries.insert(script_path.to_path_buf(), cached);
        Ok(())
    }

    /// Stash the metadata/take-override half of what a plugin pool's `pick()`
    /// just returned (#166) — see [`Self::picked_extras`]. Ids that attached
    /// neither are skipped, so a plugin returning only bare ids leaves this
    /// untouched. `pool_name` is part of the key, not just the id, so a
    /// second pool resolving the same id from an overlapping query cannot
    /// overwrite the first pool's record.
    pub(crate) fn record_picked(&mut self, pool_name: &str, picked: &[PickedItem]) {
        for item in picked {
            if item.metadata.is_some() || item.take_override.is_some() {
                self.picked_extras.insert(
                    (pool_name.to_string(), item.id.clone()),
                    PickedExtra {
                        metadata: item.metadata.clone(),
                        take_override: item.take_override,
                    },
                );
            }
        }
    }
}

/// One entry a plugin's `pick()` returned, in the widened record shape
/// (#166). A bare `entry_id` string arrives as one of these with both
/// extras `None` — the whole point of the widening being additive, and why
/// `taste-engine.rhai` needs no edit to keep returning what it always has.
#[derive(Debug, Clone)]
pub struct PickedItem {
    pub id: String,
    /// Opaque per-airing data the plugin attached, carried untouched to
    /// `PlayoutItem::metadata`. A non-finite float anywhere inside is
    /// refused at the moment it is picked, naming the key — the same
    /// refusal [`crate::config::Pool::config`] makes at load, reusing
    /// `etv_overlay::config_carrier` rather than a second implementation of
    /// that rule.
    pub metadata: Option<serde_json::Value>,
    /// A take override for this entry's series (#166), carried through pool
    /// resolution to the pattern draw but not yet honoured there — that is
    /// #173. Refused at pick time if zero or negative, naming the entry.
    pub take_override: Option<usize>,
}

/// Run a prepared script's `pick()` and return what it chose, in the order it
/// chose them.
///
/// Takes no catalog: everything the script reads was materialized by
/// [`ScoreCache::prepare`]. Ranking is pure computation over that snapshot.
///
/// Every failure is a config error phrased against the script: a missing
/// function, a runtime error, or a returned item that is neither an
/// `entry_id` string nor a record naming one. A plugin that returns nothing
/// is an error too — an empty pool would silently shorten the channel, and a
/// scorer that finds nothing worth playing is a broken scorer, not an empty
/// schedule.
///
/// `granted` (#167) is trusted as already reconciled against what the script
/// declares — [`crate::config::validate`] rejects a mismatch at config load —
/// so this only decides what `ctx.sets` / `ctx.history` resolve to for *this*
/// call; it never re-reads `capabilities()`.
pub fn pick(
    cache: &ScoreCache,
    script_path: &Path,
    inputs: &ScoreInputs,
    pool_name: &str,
    pool_config: Option<&serde_json::Value>,
    granted: GrantedCapabilities,
) -> Result<Vec<PickedItem>, String> {
    let engine = engine();

    let cached = cache.entries.get(script_path).ok_or_else(|| {
        format!(
            "scorer plugin {}: pick() called before prepare() — this is a bug in \
             etv-station, not in the script",
            script_path.display()
        )
    })?;
    let ast = &cached.ast;

    let mut scope = Scope::new();

    // Capability-gated (#167): filled only when granted, so a reach for
    // `ctx.sets` or `ctx.history` fails the moment the script reads it,
    // naming the capability. An ungranted field is also never built.
    let sets = granted.catalog_read.then(|| cached.sets.clone());
    let history = granted.watch_history.then(|| {
        Dynamic::from_array(
            inputs
                .history
                .iter()
                .map(|e| {
                    let mut m = Map::new();
                    m.insert("entry_id".into(), e.entry_id.clone().into());
                    m.insert("watched_at".into(), e.watched_at.into());
                    // Empty string rather than `()` so a script can compare it
                    // without a type check; Rhai has no null-safe operator.
                    m.insert(
                        "watcher".into(),
                        e.watcher.clone().unwrap_or_default().into(),
                    );
                    Dynamic::from_map(m)
                })
                .collect(),
        )
    });

    let config = pool_config_dynamic(pool_config, "scorer", script_path, pool_name)?;

    let ctx = ScoreCtx {
        sets,
        // Which pool is asking. One script commonly serves several pools of
        // the same channel — a "movies" pool and a "shows" pool ranked by the
        // same taste — and without this the script cannot tell them apart, so
        // both would get the same list.
        pool: pool_name.to_string(),
        config,
        target_count: inputs.target_count as i64,
        now: inputs.now,
        history,
        recent: Dynamic::from_array(
            inputs
                .recent
                .iter()
                .map(|id| Dynamic::from(id.clone()))
                .collect(),
        ),
    };

    let picked: Array = engine
        .call_fn(&mut scope, ast, "pick", (Dynamic::from(ctx),))
        .map_err(|e| format!("scorer plugin {}: pick(): {e}", script_path.display()))?;

    let mut out = Vec::with_capacity(picked.len());
    let mut seen = std::collections::HashSet::new();
    for (i, value) in picked.into_iter().enumerate() {
        let item = parse_picked_item(script_path, i, value)?;
        // A duplicate would give one item two positions in the same pool and
        // two cursors under one series key. Cheaper to reject than to explain.
        if !seen.insert(item.id.clone()) {
            let id = item.id;
            return Err(format!(
                "scorer plugin {}: pick() returned {id:?} more than once",
                script_path.display()
            ));
        }
        out.push(item);
    }

    if out.is_empty() {
        return Err(format!(
            "scorer plugin {} picked nothing — an empty pool would silently shorten \
             the channel",
            script_path.display()
        ));
    }
    Ok(out)
}

/// Parse one array element `pick()` returned into the widened record shape
/// (#166): a bare `entry_id` string — unchanged from before this existed —
/// or a record naming `entry_id` plus an optional `metadata` blob and/or
/// `take` override. Anything else names the index and what arrived instead.
fn parse_picked_item(
    script_path: &Path,
    index: usize,
    value: Dynamic,
) -> Result<PickedItem, String> {
    if !value.is_map() {
        let id = value.into_string().map_err(|actual| {
            format!(
                "scorer plugin {}: pick() item #{index} must be an entry_id string or a \
                 record naming one, got {actual}",
                script_path.display()
            )
        })?;
        return Ok(PickedItem {
            id,
            metadata: None,
            take_override: None,
        });
    }

    let map = value.cast::<Map>();
    let entry_id = map
        .get("entry_id")
        .ok_or_else(|| {
            format!(
                "scorer plugin {}: pick() item #{index} is a record but names no `entry_id`",
                script_path.display()
            )
        })?
        .clone()
        .into_string()
        .map_err(|actual| {
            format!(
                "scorer plugin {}: pick() item #{index}'s `entry_id` must be a string, got \
                 {actual}",
                script_path.display()
            )
        })?;

    // Reuses the exact non-finite-float refusal `Pool::config` makes at load
    // (#130) rather than a second implementation of that rule — the record's
    // `metadata` is the same opaque-config carrier wearing a Rhai front end
    // instead of YAML/TOML.
    let metadata = match map.get("metadata") {
        Some(v) => {
            config_carrier::deserialize_config(DynamicDeserializer::new(v)).map_err(|e| {
                format!(
                    "scorer plugin {}: pick() item #{index} ({entry_id:?}) metadata: {e}",
                    script_path.display()
                )
            })?
        }
        None => None,
    };

    let take_override = match map.get("take") {
        Some(v) => {
            let n = v.as_int().map_err(|actual| {
                format!(
                    "scorer plugin {}: pick() item #{index} ({entry_id:?})'s `take` must be a \
                     whole number, got {actual}",
                    script_path.display()
                )
            })?;
            if n <= 0 {
                return Err(format!(
                    "scorer plugin {}: pick() item #{index} ({entry_id:?}) names a take \
                     override of {n}, which must be positive",
                    script_path.display()
                ));
            }
            Some(n as usize)
        }
        None => None,
    };

    Ok(PickedItem {
        id: entry_id,
        metadata,
        take_override,
    })
}

/// Compile a script and resolve every query its `sources()` declares.
///
/// Runs once per script per generation. A bad expression fails here — before
/// any ranking work, and before any pool has drawn — naming the source it came
/// from rather than surfacing as a mystery empty pool later.
fn compile_and_resolve(
    catalog: &Catalog,
    engine: &Engine,
    script_path: &Path,
) -> Result<CachedScript, String> {
    let source = std::fs::read_to_string(script_path)
        .map_err(|e| format!("read scorer plugin {}: {e}", script_path.display()))?;
    let ast = engine
        .compile(&source)
        .map_err(|e| format!("compile scorer plugin {}: {e}", script_path.display()))?;

    let mut scope = Scope::new();
    let sources: Map = engine
        .call_fn(&mut scope, &ast, "sources", ())
        .map_err(|e| format!("scorer plugin {}: sources(): {e}", script_path.display()))?;

    let mut sets = Map::new();
    for (name, expr) in sources {
        let cel = expr.into_string().map_err(|actual| {
            format!(
                "scorer plugin {}: source {name:?} must be a CEL string, got {actual}",
                script_path.display()
            )
        })?;
        let ids = catalog.resolve_query(&cel).map_err(|e| {
            format!(
                "scorer plugin {}: source {name:?} ({cel}): {e}",
                script_path.display()
            )
        })?;
        let items = load_items(catalog, &ids).map_err(|e| {
            format!(
                "scorer plugin {}: source {name:?}: {e}",
                script_path.display()
            )
        })?;
        sets.insert(name, Dynamic::from_array(items));
    }

    Ok(CachedScript {
        ast,
        // Shared, so a second pool pointed at this script gets a refcount bump
        // rather than a deep copy of every item map.
        sets: Dynamic::from_map(sets).into_shared(),
    })
}

/// A pool's own `config`, handed over verbatim. The station reads nothing out
/// of it: whatever the author wrote is whatever the script sees, nested to any
/// depth. An absent config becomes an empty map rather than a missing key, so a
/// script can read `ctx.config.whatever` unconditionally and get unit for
/// anything unset — which is also why a mistyped key is silent.
///
/// `plugin_kind` names the hook in the error (`"scorer"` / `"sequencer"`), the
/// only thing that differs between a scorer's `ctx.config` and a sequencer's
/// `ctx.pool_config.<name>` (#169) — both marshal the same carrier (ADR 0002)
/// through this one function rather than a second, driftable copy.
pub(crate) fn pool_config_dynamic(
    pool_config: Option<&serde_json::Value>,
    plugin_kind: &str,
    script_path: &Path,
    pool_name: &str,
) -> Result<Dynamic, String> {
    match pool_config {
        Some(value) => rhai::serde::to_dynamic(value).map_err(|e| {
            format!(
                "{plugin_kind} plugin {}: pool {pool_name:?} config is not representable in \
                 Rhai: {e}",
                script_path.display()
            )
        }),
        None => Ok(Dynamic::from_map(Map::new())),
    }
}

/// Load each id as a Rhai map: every column on `entries`, plus every exposed
/// tag namespace as an array.
///
/// `pub(crate)` so [`crate::sequence`] (#169) builds a sequencer's
/// `ctx.pools.<name>` item maps with the exact same marshalling a scorer's
/// `ctx.sets.<name>` already uses (issue #169, decision 1) rather than a
/// second, driftable implementation.
pub(crate) fn load_items(catalog: &Catalog, ids: &[String]) -> Result<Array, String> {
    let mut out = Array::with_capacity(ids.len());
    for id in ids {
        let entry = catalog
            .entry(id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("entry {id:?} vanished from the catalog mid-resolution"))?;

        let mut m = Map::new();
        m.insert("entry_id".into(), entry.entry_id.into());
        m.insert("type".into(), entry.kind.into());
        m.insert("title".into(), entry.title.into());
        insert_opt_str(&mut m, "title_sort", entry.title_sort);
        insert_opt_str(&mut m, "show", entry.show);
        insert_opt_str(&mut m, "show_id", entry.show_id);
        insert_opt_int(&mut m, "season", entry.season);
        insert_opt_int(&mut m, "episode", entry.episode);
        insert_opt_int(&mut m, "absolute_episode", entry.absolute_episode);
        insert_opt_str(&mut m, "edition", entry.edition);
        insert_opt_str(&mut m, "studio", entry.studio);
        insert_opt_int(&mut m, "year", entry.year);
        insert_opt_str(&mut m, "release_date", entry.release_date);
        insert_opt_int(&mut m, "duration_ms", entry.duration_ms);
        insert_opt_str(&mut m, "content_rating", entry.content_rating);
        insert_opt_str(&mut m, "library", entry.library);

        for (key, ns) in EXPOSED_TAGS {
            let values = catalog.tags_for(id, *ns).map_err(|e| e.to_string())?;
            m.insert(
                (*key).into(),
                Dynamic::from_array(values.into_iter().map(Dynamic::from).collect()),
            );
        }

        out.push(Dynamic::from_map(m));
    }
    Ok(out)
}

/// Absent columns arrive as `()`, Rhai's unit — so a script can test them with
/// `item.year == ()` rather than having to know a sentinel value.
fn insert_opt_str(m: &mut Map, key: &str, value: Option<String>) {
    m.insert(
        key.into(),
        value.map(Dynamic::from).unwrap_or(Dynamic::UNIT),
    );
}

fn insert_opt_int(m: &mut Map, key: &str, value: Option<i64>) {
    m.insert(
        key.into(),
        value.map(Dynamic::from).unwrap_or(Dynamic::UNIT),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Entry, Source};

    /// `prepare` then `pick`, the two halves a caller always runs in order.
    /// Grants both gated capabilities (#167) — every test in this module
    /// predates capability gating and reads `ctx.sets`/`ctx.history`
    /// unconditionally, so this preserves that. Tests exercising the gate
    /// itself call [`pick`] directly with a narrower [`GrantedCapabilities`].
    ///
    /// Test-only on purpose. The daemon deliberately has no such function: the
    /// whole point of the split is that the catalog-reading half and the
    /// unbounded ranking half happen in separate passes, and a convenience that
    /// does both would let a caller hold a database handle across the ranking
    /// again. Tests have one pool and no other channel to starve.
    fn run(
        catalog: &Catalog,
        script_path: &Path,
        inputs: &ScoreInputs,
        pool_name: &str,
        pool_config: Option<&serde_json::Value>,
        cache: &mut ScoreCache,
    ) -> Result<Vec<String>, String> {
        cache.prepare(catalog, script_path)?;
        pick(
            cache,
            script_path,
            inputs,
            pool_name,
            pool_config,
            GrantedCapabilities {
                catalog_read: true,
                watch_history: true,
            },
        )
        // Tests written before #166 assert against bare ids; unwrap the
        // record shape back down to that for them.
        .map(|items| items.into_iter().map(|p| p.id).collect())
    }

    fn catalog() -> Catalog {
        let c = Catalog::open_in_memory().unwrap();
        for (id, title, year) in [("m1", "Alpha", 2001), ("m2", "Beta", 2002)] {
            let mut e = Entry::new(id, "movie", title, Source::Plex);
            e.year = Some(year);
            c.upsert_entry(&e).unwrap();
        }
        c.add_tag("m1", TagNs::Genre, "Fantasy").unwrap();
        c
    }

    fn write(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
        let p = dir.path().join("plugin.rhai");
        std::fs::write(&p, body).unwrap();
        p
    }

    /// Parse a config bag exactly as a channel file does — YAML text in,
    /// `serde_json::Value` out, with no intermediate step, which is the whole
    /// reason the field can name one carrier type across both surfaces.
    fn yaml(src: &str) -> serde_json::Value {
        serde_norway::from_str(src).unwrap()
    }

    /// A script that reports what it saw in `ctx.config` by picking the ids the
    /// config names, so an assertion about the returned order is an assertion
    /// about what actually arrived inside the script.
    #[test]
    fn config_arrives_nested_with_types_intact() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            r#"
fn sources() { #{ movies: `item.type == "movie"` } }
fn pick(ctx) {
    let c = ctx.config;
    // Three levels down, through a map, a map, and an array — and the scalars
    // must still be their own types, not strings.
    let deep = c.weights.nested.values;
    if deep[0] != 42 { throw "int did not survive: " + deep[0]; }
    if deep[1] != 1.5 { throw "float did not survive: " + deep[1]; }
    if deep[2] != true { throw "bool did not survive: " + deep[2]; }
    if deep[3] != "four" { throw "string did not survive: " + deep[3]; }
    if c.weights.affinity != 3.0 { throw "nested float wrong"; }
    if c.name != "taste" { throw "top-level string wrong"; }
    [c.first, c.second]
}
"#,
        );
        let cfg = yaml(
            r#"
name: taste
first: m2
second: m1
weights:
  affinity: 3.0
  nested:
    values: [42, 1.5, true, "four"]
"#,
        );
        let got = run(
            &catalog(),
            &p,
            &ScoreInputs::default(),
            "test",
            Some(&cfg),
            &mut Default::default(),
        )
        .unwrap();
        assert_eq!(got, vec!["m2", "m1"]);
    }

    /// `item.library` (#128) has to reach a scorer's item maps, not just the CEL
    /// surface — the maps are built from an explicit column list, so a new
    /// column only arrives there if it is added by hand.
    #[test]
    fn item_maps_carry_the_library() {
        let c = Catalog::open_in_memory().unwrap();
        for (id, library) in [("m1", Some("4K Movies")), ("m2", None)] {
            let mut e = Entry::new(id, "movie", id, Source::Plex);
            e.library = library.map(str::to_string);
            c.upsert_entry(&e).unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            r#"
fn sources() { #{ movies: `item.type == "movie"` } }
fn pick(ctx) {
    let ids = [];
    for item in ctx.sets.movies {
        if item.library == "4K Movies" { ids.push(item.entry_id); }
    }
    // A column with no value arrives as unit, same as every other absent one.
    for item in ctx.sets.movies {
        if item.entry_id == "m2" && item.library != () { throw "expected unit"; }
    }
    ids
}
"#,
        );
        let got = run(
            &c,
            &p,
            &ScoreInputs::default(),
            "test",
            None,
            &mut Default::default(),
        )
        .unwrap();
        assert_eq!(got, vec!["m1"]);
    }

    #[test]
    fn an_absent_config_is_an_empty_map_not_a_missing_key() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            r#"
fn sources() { #{ movies: `item.type == "movie"` } }
fn pick(ctx) {
    // Reading straight through an unset config must yield unit rather than
    // erroring, so a script never has to guard before looking.
    if ctx.config.anything != () { throw "expected unit"; }
    if ctx.config.len() != 0 { throw "expected an empty map"; }
    ["m1"]
}
"#,
        );
        let got = run(
            &catalog(),
            &p,
            &ScoreInputs::default(),
            "test",
            None,
            &mut Default::default(),
        )
        .unwrap();
        assert_eq!(got, vec!["m1"]);
    }

    #[test]
    fn an_unrecognised_key_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            r#"
fn sources() { #{ movies: `item.type == "movie"` } }
fn pick(ctx) { ["m1"] }
"#,
        );
        // Nothing in the station knows what any of these mean, and a script that
        // reads none of them is not a config error — the whole bag is opaque.
        let cfg = yaml("afinity_window_days: 14\nutter_nonsense: [1, 2]\n");
        let got = run(
            &catalog(),
            &p,
            &ScoreInputs::default(),
            "test",
            Some(&cfg),
            &mut Default::default(),
        )
        .unwrap();
        assert_eq!(got, vec!["m1"]);
    }

    #[test]
    fn each_pool_gets_its_own_config_from_one_script() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            r#"
fn sources() { #{ movies: `item.type == "movie"` } }
fn pick(ctx) { [ctx.config.want] }
"#,
        );
        let mut cache = ScoreCache::default();
        let movies = yaml("want: m1\n");
        let shows = yaml("want: m2\n");
        let cat = catalog();
        // One compiled script, two pools, two different configs — the cache is
        // keyed on the script path, so this is where a config wrongly cached
        // alongside the AST would show up.
        assert_eq!(
            run(
                &cat,
                &p,
                &ScoreInputs::default(),
                "movies",
                Some(&movies),
                &mut cache
            )
            .unwrap(),
            ["m1"]
        );
        assert_eq!(
            run(
                &cat,
                &p,
                &ScoreInputs::default(),
                "shows",
                Some(&shows),
                &mut cache
            )
            .unwrap(),
            ["m2"]
        );
    }

    #[test]
    fn picks_in_the_order_the_script_returns() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            r#"
fn sources() { #{ movies: `item.type == "movie"` } }
fn pick(ctx) {
    let ids = [];
    for item in ctx.sets.movies { ids.push(item.entry_id); }
    ids.reverse();
    ids
}
"#,
        );
        let got = run(
            &catalog(),
            &p,
            &ScoreInputs::default(),
            "test",
            None,
            &mut Default::default(),
        )
        .unwrap();
        assert_eq!(got, vec!["m2", "m1"]);
    }

    #[test]
    fn items_carry_columns_and_tags() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            r#"
fn sources() { #{ movies: `item.type == "movie"` } }
fn pick(ctx) {
    let out = [];
    for item in ctx.sets.movies {
        if item.year == 2001 && item.genres.contains("Fantasy") && item.season == () {
            out.push(item.entry_id);
        }
    }
    out
}
"#,
        );
        assert_eq!(
            run(
                &catalog(),
                &p,
                &ScoreInputs::default(),
                "test",
                None,
                &mut Default::default()
            )
            .unwrap(),
            ["m1"]
        );
    }

    #[test]
    fn inputs_reach_the_script() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            r#"
fn sources() { #{ movies: `item.type == "movie"` } }
fn pick(ctx) {
    let out = [];
    for item in ctx.sets.movies {
        if !ctx.recent.contains(item.entry_id) { out.push(item.entry_id); }
    }
    out.truncate(ctx.target_count);
    out
}
"#,
        );
        let inputs = ScoreInputs {
            target_count: 1,
            recent: vec!["m1".into()],
            ..Default::default()
        };
        assert_eq!(
            run(
                &catalog(),
                &p,
                &inputs,
                "test",
                None,
                &mut Default::default()
            )
            .unwrap(),
            ["m2"]
        );
    }

    #[test]
    fn a_bad_source_expression_names_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            "fn sources() { #{ broken: `item.nope == 1` } }\nfn pick(ctx) { [] }\n",
        );
        let e = run(
            &catalog(),
            &p,
            &ScoreInputs::default(),
            "test",
            None,
            &mut Default::default(),
        )
        .unwrap_err();
        assert!(e.contains("broken"), "got {e}");
    }

    #[test]
    fn an_empty_pick_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, "fn sources() { #{} }\nfn pick(ctx) { [] }\n");
        let e = run(
            &catalog(),
            &p,
            &ScoreInputs::default(),
            "test",
            None,
            &mut Default::default(),
        )
        .unwrap_err();
        assert!(e.contains("picked nothing"), "got {e}");
    }

    #[test]
    fn a_duplicate_pick_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            "fn sources() { #{} }\nfn pick(ctx) { [\"m1\", \"m1\"] }\n",
        );
        let e = run(
            &catalog(),
            &p,
            &ScoreInputs::default(),
            "test",
            None,
            &mut Default::default(),
        )
        .unwrap_err();
        assert!(e.contains("more than once"), "got {e}");
    }

    #[test]
    fn a_missing_pick_function_names_the_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, "fn sources() { #{} }\n");
        let e = run(
            &catalog(),
            &p,
            &ScoreInputs::default(),
            "test",
            None,
            &mut Default::default(),
        )
        .unwrap_err();
        assert!(e.contains("pick()"), "got {e}");
    }

    // ---- hook declaration (#159) -------------------------------------------

    #[test]
    fn declared_hooks_reads_pool_provider() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, r#"fn hooks() { ["pool_provider"] }"#);
        assert_eq!(declared_hooks(&p).unwrap(), vec!["pool_provider"]);
    }

    #[test]
    fn declared_hooks_reads_more_than_one() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, r#"fn hooks() { ["pool_provider", "sequencer"] }"#);
        assert_eq!(
            declared_hooks(&p).unwrap(),
            vec!["pool_provider", "sequencer"]
        );
    }

    /// A script declaring no hooks fails to load, saying at least one is
    /// required — one of the issue's three named error cases.
    #[test]
    fn declared_hooks_rejects_an_empty_declaration() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, "fn hooks() { [] }");
        let e = declared_hooks(&p).unwrap_err();
        assert!(e.contains("declares no hooks"), "got {e}");
        assert!(e.contains("pool_provider"), "got {e}");
    }

    /// A script naming a hook the station has never heard of fails to load,
    /// and the error lists the hook names that do exist — the second named
    /// error case.
    #[test]
    fn declared_hooks_rejects_an_unknown_name() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, r#"fn hooks() { ["scoreboard"] }"#);
        let e = declared_hooks(&p).unwrap_err();
        assert!(e.contains("scoreboard"), "got {e}");
        assert!(e.contains("pool_provider"), "got {e}");
        assert!(e.contains("sequencer"), "got {e}");
    }

    #[test]
    fn declared_hooks_names_the_script_when_hooks_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, "fn sources() { #{} }\n");
        let e = declared_hooks(&p).unwrap_err();
        assert!(e.contains("hooks()"), "got {e}");
        assert!(e.contains(&p.display().to_string()), "got {e}");
    }

    /// `declared_hooks` never runs `sources()` or `pick()` — a script whose
    /// scoring path would blow up (reads the catalog, throws, whatever) still
    /// answers `hooks()` cleanly, which is what lets this run with no catalog
    /// at config-load time.
    #[test]
    fn declared_hooks_never_runs_sources_or_pick() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            r#"
fn hooks() { ["pool_provider"] }
fn sources() { throw "sources must not run"; }
fn pick(ctx) { throw "pick must not run"; }
"#,
        );
        assert_eq!(declared_hooks(&p).unwrap(), vec!["pool_provider"]);
    }

    // ---- capability declaration (#167) -------------------------------------

    #[test]
    fn declared_capabilities_reads_simple_names() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            r#"fn capabilities() { ["catalog_read", "watch_history"] }"#,
        );
        assert_eq!(
            declared_capabilities(&p).unwrap(),
            vec![Capability::CatalogRead, Capability::WatchHistory]
        );
    }

    #[test]
    fn declared_capabilities_reads_a_named_datastore() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            r#"fn capabilities() { ["catalog_read", #{ datastore: "taste_db" }] }"#,
        );
        assert_eq!(
            declared_capabilities(&p).unwrap(),
            vec![
                Capability::CatalogRead,
                Capability::Datastore("taste_db".into())
            ]
        );
    }

    /// A script with no `capabilities()` at all declares nothing — the field
    /// is optional, unlike `hooks()`. A plugin that only reads the ambient
    /// inputs (`ctx.recent`, `ctx.config`, …) needs no declaration.
    #[test]
    fn declared_capabilities_defaults_to_empty_when_the_function_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, r#"fn hooks() { ["pool_provider"] }"#);
        assert_eq!(declared_capabilities(&p).unwrap(), Vec::new());
    }

    /// Unlike an empty `hooks()`, an empty `capabilities()` is not an error —
    /// it is the explicit way to say "needs nothing gated".
    #[test]
    fn declared_capabilities_accepts_an_explicit_empty_array() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, "fn capabilities() { [] }");
        assert_eq!(declared_capabilities(&p).unwrap(), Vec::new());
    }

    #[test]
    fn declared_capabilities_rejects_an_unknown_name() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, r#"fn capabilities() { ["telepathy"] }"#);
        let e = declared_capabilities(&p).unwrap_err();
        assert!(e.contains("telepathy"), "got {e}");
        assert!(e.contains("catalog_read"), "got {e}");
        assert!(e.contains("watch_history"), "got {e}");
    }

    #[test]
    fn declared_capabilities_rejects_a_datastore_entry_with_an_empty_name() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, r#"fn capabilities() { [#{ datastore: "" }] }"#);
        let e = declared_capabilities(&p).unwrap_err();
        assert!(e.contains("empty"), "got {e}");
    }

    #[test]
    fn declared_capabilities_rejects_an_unrecognized_map_shape() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, r#"fn capabilities() { [#{ wat: "taste_db" }] }"#);
        let e = declared_capabilities(&p).unwrap_err();
        assert!(e.contains("not a recognized capability"), "got {e}");
    }

    /// `declared_capabilities` never runs `sources()` or `pick()`, exactly
    /// like `declared_hooks` — a load-time check must not need a catalog.
    #[test]
    fn declared_capabilities_never_runs_sources_or_pick() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            r#"
fn capabilities() { ["catalog_read"] }
fn sources() { throw "sources must not run"; }
fn pick(ctx) { throw "pick must not run"; }
"#,
        );
        assert_eq!(
            declared_capabilities(&p).unwrap(),
            vec![Capability::CatalogRead]
        );
    }

    // ---- runtime capability gate (#167) ------------------------------------

    /// A script that reaches for `ctx.history` without being granted
    /// `watch_history` fails at the `pick()` call, naming the capability —
    /// the "reach" error case the issue describes as only catchable at run
    /// time.
    #[test]
    fn reaching_for_an_ungranted_watch_history_fails_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            r#"
fn sources() { #{} }
fn pick(ctx) {
    let out = [];
    for e in ctx.history { out.push(e.entry_id); }
    out
}
"#,
        );
        let mut cache = ScoreCache::default();
        cache.prepare(&catalog(), &p).unwrap();
        let err = pick(
            &cache,
            &p,
            &ScoreInputs::default(),
            "test",
            None,
            GrantedCapabilities::default(),
        )
        .unwrap_err();
        assert!(err.contains("watch_history"), "got {err}");
    }

    /// Same shape, for `ctx.sets` gated behind `catalog_read`.
    #[test]
    fn reaching_for_ungranted_catalog_read_fails_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            r#"
fn sources() { #{ movies: `item.type == "movie"` } }
fn pick(ctx) {
    let out = [];
    for item in ctx.sets.movies { out.push(item.entry_id); }
    out
}
"#,
        );
        let mut cache = ScoreCache::default();
        cache.prepare(&catalog(), &p).unwrap();
        let err = pick(
            &cache,
            &p,
            &ScoreInputs::default(),
            "test",
            None,
            GrantedCapabilities::default(),
        )
        .unwrap_err();
        assert!(err.contains("catalog_read"), "got {err}");
    }

    /// The ambient fields — `ctx.recent`, `ctx.config`, `ctx.target_count`,
    /// `ctx.now`, `ctx.pool` — stay available with no grant at all; only
    /// `sets` and `history` are gated.
    #[test]
    fn ungated_fields_stay_available_with_no_capabilities_granted() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            r#"
fn sources() { #{} }
fn pick(ctx) {
    if ctx.pool != "test" { throw "pool wrong"; }
    if ctx.target_count != 3 { throw "target_count wrong"; }
    if ctx.now != 42 { throw "now wrong"; }
    if ctx.recent.len != 1 { throw "recent wrong"; }
    if ctx.config.want != "m1" { throw "config wrong"; }
    [ctx.config.want]
}
"#,
        );
        let mut cache = ScoreCache::default();
        cache.prepare(&catalog(), &p).unwrap();
        let inputs = ScoreInputs {
            target_count: 3,
            recent: vec!["m2".into()],
            now: 42,
            ..Default::default()
        };
        let cfg = yaml("want: m1\n");
        let got = pick(
            &cache,
            &p,
            &inputs,
            "test",
            Some(&cfg),
            GrantedCapabilities::default(),
        )
        .unwrap();
        assert_eq!(
            got.into_iter().map(|p| p.id).collect::<Vec<_>>(),
            vec!["m1"]
        );
    }

    // ---- record return shape (#166) ----------------------------------------

    /// A plugin returning bare ids works exactly as before the record shape
    /// existed — the widening is additive.
    #[test]
    fn a_bare_id_carries_no_metadata_or_take_override() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, "fn sources() { #{} }\nfn pick(ctx) { [\"m1\"] }\n");
        let mut cache = ScoreCache::default();
        cache.prepare(&catalog(), &p).unwrap();
        let got = pick(
            &cache,
            &p,
            &ScoreInputs::default(),
            "test",
            None,
            GrantedCapabilities::default(),
        )
        .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "m1");
        assert!(got[0].metadata.is_none());
        assert!(got[0].take_override.is_none());
    }

    /// A record's `metadata` lands on the returned item, nested and typed
    /// exactly like `Pool::config` (#166) — the same carrier, reached from
    /// the other side.
    #[test]
    fn a_records_metadata_arrives_nested_with_types_intact() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            r#"
fn sources() { #{} }
fn pick(ctx) {
    [#{ entry_id: "m1", metadata: #{ reason: "won an Oscar", score: 3.5, tags: ["a", "b"] } }]
}
"#,
        );
        let mut cache = ScoreCache::default();
        cache.prepare(&catalog(), &p).unwrap();
        let got = pick(
            &cache,
            &p,
            &ScoreInputs::default(),
            "test",
            None,
            GrantedCapabilities::default(),
        )
        .unwrap();
        let meta = got[0].metadata.as_ref().unwrap();
        assert_eq!(meta["reason"], serde_json::json!("won an Oscar"));
        assert_eq!(meta["score"], serde_json::json!(3.5));
        assert_eq!(meta["tags"][1], serde_json::json!("b"));
    }

    /// A record's `take` survives onto the returned item, and bare-id
    /// entries in the same array carry none — both shapes may mix freely.
    #[test]
    fn a_records_take_override_arrives_and_bare_ids_still_carry_none() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            r#"
fn sources() { #{} }
fn pick(ctx) { [#{ entry_id: "m1", take: 3 }, "m2"] }
"#,
        );
        let mut cache = ScoreCache::default();
        cache.prepare(&catalog(), &p).unwrap();
        let got = pick(
            &cache,
            &p,
            &ScoreInputs::default(),
            "test",
            None,
            GrantedCapabilities::default(),
        )
        .unwrap();
        assert_eq!(got[0].take_override, Some(3));
        assert!(got[0].metadata.is_none());
        assert_eq!(got[1].id, "m2");
        assert!(got[1].take_override.is_none());
    }

    /// A non-finite float anywhere inside a record's `metadata` fails the
    /// pick and names the key, in the same wording `Pool::config` uses
    /// (#130) — reusing `etv_overlay::config_carrier` rather than a second
    /// implementation of the rule.
    #[test]
    fn a_non_finite_float_in_a_records_metadata_fails_and_names_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            r#"
fn sources() { #{} }
fn pick(ctx) { [#{ entry_id: "m1", metadata: #{ weight: 1.0 / 0.0 } }] }
"#,
        );
        let mut cache = ScoreCache::default();
        cache.prepare(&catalog(), &p).unwrap();
        let err = pick(
            &cache,
            &p,
            &ScoreInputs::default(),
            "test",
            None,
            GrantedCapabilities::default(),
        )
        .unwrap_err();
        assert!(err.contains("weight"), "got {err}");
        assert!(err.contains("finite"), "got {err}");
        assert!(err.contains("m1"), "got {err}");
    }

    /// A take override of zero or negative fails the pick and names the
    /// entry.
    #[test]
    fn a_zero_or_negative_take_override_fails_and_names_the_entry() {
        for take in ["0", "-1"] {
            let dir = tempfile::tempdir().unwrap();
            let p = write(
                &dir,
                &format!(
                    "fn sources() {{ #{{}} }}\nfn pick(ctx) {{ [#{{ entry_id: \"m1\", take: {take} }}] }}\n"
                ),
            );
            let mut cache = ScoreCache::default();
            cache.prepare(&catalog(), &p).unwrap();
            let err = pick(
                &cache,
                &p,
                &ScoreInputs::default(),
                "test",
                None,
                GrantedCapabilities::default(),
            )
            .unwrap_err();
            assert!(err.contains("m1"), "take={take}, got {err}");
            assert!(err.contains("positive"), "take={take}, got {err}");
        }
    }

    /// A record naming no `entry_id` fails and says so, naming the index.
    #[test]
    fn a_record_with_no_entry_id_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            "fn sources() { #{} }\nfn pick(ctx) { [#{ metadata: #{ x: 1 } }] }\n",
        );
        let mut cache = ScoreCache::default();
        cache.prepare(&catalog(), &p).unwrap();
        let err = pick(
            &cache,
            &p,
            &ScoreInputs::default(),
            "test",
            None,
            GrantedCapabilities::default(),
        )
        .unwrap_err();
        assert!(err.contains("#0"), "got {err}");
        assert!(err.contains("entry_id"), "got {err}");
    }

    /// The same id twice is still refused when the two mentions wear
    /// different shapes — a bare id and a record naming it are the same
    /// entry, and one entry cannot hold two positions in a pool.
    #[test]
    fn duplicate_detection_still_works_across_mixed_bare_and_record_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            r#"fn sources() { #{} }
fn pick(ctx) { ["m1", #{ entry_id: "m1" }] }
"#,
        );
        let mut cache = ScoreCache::default();
        cache.prepare(&catalog(), &p).unwrap();
        let err = pick(
            &cache,
            &p,
            &ScoreInputs::default(),
            "test",
            None,
            GrantedCapabilities::default(),
        )
        .unwrap_err();
        assert!(err.contains("more than once"), "got {err}");
    }
}

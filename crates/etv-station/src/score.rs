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
//! // `config::Pool::datastores`. A granted datastore is reached as
//! // `ctx.datastore("name")` (#181), which returns a handle exposing the
//! // plex-db-ex reader crate's six accessors — `enrichment_for`,
//! // `enrichment_for_many`, `edges_from`, `edges_to`, `taste_vector_for`,
//! // `pooled_taste_vector` — or fails the pick() call naming the
//! // datastore if it was never granted.
//! fn capabilities() { ["catalog_read", "watch_history"] }
//!
//! // Every catalog query this plugin will read, named. Run once, up front —
//! // unless the pool authored its own `sources:` table (#210), which replaces
//! // this wholesale and leaves the function uncalled. Either way the script
//! // reads the results back the same way, as `ctx.sets.<name>`.
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
//!     // ctx.seed          — this pool's reproducible seed (#255, ADR 0005):
//!     //                     the channel's resolved seed mixed with the pool's
//!     //                     name, so a random choice made from it reproduces
//!     //                     exactly on a second generation
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
use std::sync::{Arc, Mutex};

use etv_overlay::config_carrier;
use rhai::serde::DynamicDeserializer;
use rhai::{Array, Dynamic, Engine, EvalAltResult, Map, Scope};

use crate::catalog::{Catalog, TagNs};
use crate::pattern::MAX_TAKE;

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

/// The catalog queries one prepared script was resolved against, when the pool
/// authored them itself (#210) — set name to CEL expression. `None` anywhere
/// this appears means "whatever the script's own `sources()` declares".
///
/// A `BTreeMap` rather than a `HashMap` so it can key [`ScoreCache`]: two pools
/// that wrote the same table in a different order are the same table, and share
/// one resolved set.
pub type PoolSources = std::collections::BTreeMap<String, String>;

/// One generation's compiled scripts and resolved query sets, keyed by script
/// path and the sources it was resolved against.
///
/// A channel commonly points several pools at the same script — a `movies` pool
/// and a `shows` pool ranked by the same taste — and a script's `sources()`
/// takes no arguments, so what *it* declares cannot vary by pool. Without this
/// each pool would recompile the script and re-run every declared query,
/// materializing the same slice of the library once per pool.
///
/// The sources are half the key because a pool may now author its own table
/// (#210), and then the resolved sets are that pool's, not the script's. Keyed
/// on the path alone, a `movies` pool narrowed to one Plex library would hand
/// its narrowed sets to every sibling pool naming the same script — the
/// `episodes` half of a For You channel would silently inherit
/// `library != "Concerts"` and, worse, whichever pool resolved second would
/// rank the *other* pool's candidates. Two pools that authored nothing, or
/// authored the same table, still share one entry — the extra compile and the
/// extra catalog pass are paid only by a pool that asked for something
/// different, which is exactly the pool that needs them.
///
/// Scoped to a single generation and dropped with it, so a catalog that changes
/// between generations is picked up on the next one.
#[derive(Debug, Default)]
pub struct ScoreCache {
    entries: HashMap<(PathBuf, Option<PoolSources>), CachedScript>,
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
/// grant (see [`crate::config::Pool::datastores`]) — reached at runtime as
/// `ctx.datastore("name")` (#181), which returns the plex-db-ex reader
/// crate's accessors for that store.
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
#[derive(Debug, Clone, Default)]
pub struct GrantedCapabilities {
    pub catalog_read: bool,
    pub watch_history: bool,
    /// Opened handles for every datastore this pool was granted (#181), keyed
    /// by the grant's name — the same name a script's `capabilities()`
    /// declares via `#{ datastore: "name" }`. Empty for a pool that grants
    /// none, which is every pool before this field existed.
    ///
    /// `plexdb_reader::Reader` wraps a `rusqlite` connection, which is
    /// [`Send`] but not [`Sync`]; the [`Mutex`] is what lets one [`Arc`] live
    /// inside a Rhai [`Dynamic`] under this engine's `sync` feature, which
    /// requires every registered type to be `Send + Sync`. Contention is not
    /// a concern — [`pick`] runs one script call at a time on one thread, so
    /// the lock is never held by more than one caller.
    pub datastores: HashMap<String, Arc<Mutex<plexdb_reader::Reader>>>,
}

impl GrantedCapabilities {
    /// Read a pool's granted capability names back into the two flags [`pick`]
    /// acts on, so a caller never has to know how a capability is spelled.
    /// Names that are not simple capabilities — a datastore grant — are
    /// ignored here; [`Self::with_datastores`] handles those separately,
    /// since opening one can fail and this constructor cannot.
    pub fn from_names(names: &[String]) -> Self {
        let has = |want: &str| names.iter().any(|n| n == want);
        Self {
            catalog_read: has("catalog_read"),
            watch_history: has("watch_history"),
            datastores: HashMap::new(),
        }
    }

    /// Open every grant in `grants` and fold the handles into this capability
    /// set, keyed by grant name. Called once per plugin pool per generation
    /// (`pattern::resolve_pool_sources`), right before [`pick`] runs —
    /// load-time validation ([`crate::config::validate`]) already proved each
    /// path opens and is at the schema version this build understands; this
    /// reopens it fresh each generation rather than caching a handle across
    /// them, since a `Reader::open` is one file open plus one `SELECT`, not a
    /// network round trip.
    ///
    /// Fails naming the datastore, its path, and the underlying error if a
    /// store that validated at load can no longer be opened — e.g. deleted or
    /// moved mid-run.
    pub fn with_datastores(
        mut self,
        grants: &[crate::config::DatastoreGrant],
    ) -> Result<Self, String> {
        for grant in grants {
            let reader = plexdb_reader::Reader::open(&grant.path)
                .map_err(|e| format!("datastore {:?} at {:?}: {e}", grant.name, grant.path))?;
            self.datastores
                .insert(grant.name.clone(), Arc::new(Mutex::new(reader)));
        }
        Ok(self)
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
/// touches a gated field is unaffected. `ctx.datastore("name")` (#181) is
/// gated the same way, except by name rather than by a single flag — see
/// [`ScoreCtx::get_datastore`].
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
    /// Every datastore this pool was granted (#181), keyed by name — the same
    /// map [`GrantedCapabilities::datastores`] carries, moved across whole and
    /// wrapped in the registered [`Datastore`] type only when a script asks
    /// for one. A name absent here is indistinguishable to the script from one
    /// that was never declared: both fail [`ScoreCtx::get_datastore`] the same
    /// way.
    datastores: HashMap<String, Arc<Mutex<plexdb_reader::Reader>>>,
    /// The channel's resolved generation seed, mixed with this pool's name
    /// (#255, ADR 0005) — the one source of reproducible entropy inside the
    /// sandbox. Not gated: a `pool_provider` script always receives it, the
    /// same as `now` or `target_count`.
    seed: i64,
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

    /// `ctx.datastore("name")` (#181). A name this pool was not granted —
    /// whether never declared, never granted, or misspelled — fails here
    /// naming the datastore, the same load-bearing error shape
    /// [`ungranted`] gives the two boolean-gated fields.
    fn get_datastore(&mut self, name: &str) -> Result<Dynamic, Box<EvalAltResult>> {
        self.datastores
            .get(name)
            .cloned()
            .map(|reader| Dynamic::from(Datastore(reader)))
            .ok_or_else(|| format!("datastore {name:?} not declared or granted").into())
    }

    fn get_seed(&mut self) -> i64 {
        self.seed
    }
}

/// A live handle to one granted datastore (#181), registered as its own Rhai
/// type so a script calls `ctx.datastore("name").enrichment_for(...)` and the
/// crate's other five accessors directly. Wraps the same
/// `Arc<Mutex<plexdb_reader::Reader>>` [`GrantedCapabilities::datastores`]
/// carries — see that field's doc for why the mutex exists.
#[derive(Debug, Clone)]
struct Datastore(Arc<Mutex<plexdb_reader::Reader>>);

impl Datastore {
    fn lock(&self) -> std::sync::MutexGuard<'_, plexdb_reader::Reader> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn enrichment_for(
        &mut self,
        item_id: &str,
        namespace: &str,
    ) -> Result<Array, Box<EvalAltResult>> {
        let facts = self
            .lock()
            .enrichment_for(item_id, namespace)
            .map_err(|e| format!("datastore: enrichment_for({item_id:?}, {namespace:?}): {e}"))?;
        Ok(facts.into_iter().map(enrichment_fact_to_dynamic).collect())
    }

    /// One host call for a whole candidate set, rather than
    /// [`Self::enrichment_for`] once per id — see
    /// [`plexdb_reader::Reader::enrichment_for_many`] for why that call shape
    /// exists at all (plex-db-ex#40).
    fn enrichment_for_many(
        &mut self,
        item_ids: Array,
        namespace: &str,
    ) -> Result<Map, Box<EvalAltResult>> {
        let ids: Vec<String> = item_ids
            .into_iter()
            .map(|id| {
                id.into_string().map_err(|actual| {
                    format!(
                        "datastore: enrichment_for_many: item id must be a string, got {actual}"
                    )
                })
            })
            .collect::<Result<_, String>>()?;
        let by_item = self
            .lock()
            .enrichment_for_many(ids.iter().map(String::as_str), namespace)
            .map_err(|e| format!("datastore: enrichment_for_many({namespace:?}): {e}"))?;
        Ok(by_item
            .into_iter()
            .map(|(item_id, facts)| {
                let facts: Array = facts.into_iter().map(enrichment_fact_to_dynamic).collect();
                (item_id.into(), Dynamic::from_array(facts))
            })
            .collect())
    }

    fn edges_from(&mut self, item_id: &str, edge_type: &str) -> Result<Array, Box<EvalAltResult>> {
        let edges = self
            .lock()
            .edges_from(item_id, edge_type)
            .map_err(|e| format!("datastore: edges_from({item_id:?}, {edge_type:?}): {e}"))?;
        Ok(edges.into_iter().map(edge_to_dynamic).collect())
    }

    fn edges_to(&mut self, item_id: &str, edge_type: &str) -> Result<Array, Box<EvalAltResult>> {
        let edges = self
            .lock()
            .edges_to(item_id, edge_type)
            .map_err(|e| format!("datastore: edges_to({item_id:?}, {edge_type:?}): {e}"))?;
        Ok(edges.into_iter().map(edge_to_dynamic).collect())
    }

    fn taste_vector_for(&mut self, plex_account_id: i64) -> Result<Dynamic, Box<EvalAltResult>> {
        let vector = self
            .lock()
            .taste_vector_for(plex_account_id)
            .map_err(|e| format!("datastore: taste_vector_for({plex_account_id}): {e}"))?;
        Ok(taste_vector_to_dynamic(vector))
    }

    /// The server-wide taste vector — see
    /// [`plexdb_reader::Reader::pooled_taste_vector`] for what "pooled" means
    /// (plex-db-ex#39).
    fn pooled_taste_vector(&mut self) -> Result<Dynamic, Box<EvalAltResult>> {
        let vector = self
            .lock()
            .pooled_taste_vector()
            .map_err(|e| format!("datastore: pooled_taste_vector(): {e}"))?;
        Ok(taste_vector_to_dynamic(vector))
    }
}

/// One `enrichment` row (#181), as a Rhai map: `namespace`, `key`, `value`,
/// `fetched_at` — the same four fields [`plexdb_reader::EnrichmentFact`]
/// carries, untouched.
fn enrichment_fact_to_dynamic(fact: plexdb_reader::EnrichmentFact) -> Dynamic {
    let mut m = Map::new();
    m.insert("namespace".into(), fact.namespace.into());
    m.insert("key".into(), fact.key.into());
    m.insert("value".into(), fact.value.into());
    m.insert("fetched_at".into(), fact.fetched_at.into());
    Dynamic::from_map(m)
}

/// One `edges` row (#181), as a Rhai map — mirrors [`plexdb_reader::Edge`]
/// field for field.
fn edge_to_dynamic(edge: plexdb_reader::Edge) -> Dynamic {
    let mut m = Map::new();
    m.insert("from_id".into(), edge.from_id.into());
    m.insert("to_id".into(), edge.to_id.into());
    m.insert("edge_type".into(), edge.edge_type.into());
    m.insert("rank".into(), edge.rank.into());
    m.insert("fetched_at".into(), edge.fetched_at.into());
    Dynamic::from_map(m)
}

/// One [`plexdb_reader::TasteVector`] (#181), as a Rhai map: `attributes` (an
/// array of `{namespace, key, value, weight}` maps) and
/// `shows_without_seasons` (an array of strings) — mirrors the Rust struct
/// field for field, nothing renamed or dropped.
fn taste_vector_to_dynamic(vector: plexdb_reader::TasteVector) -> Dynamic {
    let attributes: Array = vector
        .attributes
        .into_iter()
        .map(|a| {
            let mut m = Map::new();
            m.insert("namespace".into(), a.namespace.into());
            m.insert("key".into(), a.key.into());
            m.insert("value".into(), a.value.into());
            m.insert("weight".into(), a.weight.into());
            Dynamic::from_map(m)
        })
        .collect();
    let shows_without_seasons: Array = vector
        .shows_without_seasons
        .into_iter()
        .map(Dynamic::from)
        .collect();
    let mut m = Map::new();
    m.insert("attributes".into(), Dynamic::from_array(attributes));
    m.insert(
        "shows_without_seasons".into(),
        Dynamic::from_array(shows_without_seasons),
    );
    Dynamic::from_map(m)
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
///
/// [`Datastore`] (#181) is registered here unconditionally too, for the same
/// reason `ScoreCtx` is — harmless when nothing constructs one, and it keeps
/// every plugin call sharing one engine configuration.
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
        .register_get("recent", ScoreCtx::get_recent)
        .register_get("seed", ScoreCtx::get_seed)
        .register_fn("datastore", ScoreCtx::get_datastore);
    engine
        .register_type_with_name::<Datastore>("Datastore")
        .register_fn("enrichment_for", Datastore::enrichment_for)
        .register_fn("enrichment_for_many", Datastore::enrichment_for_many)
        .register_fn("edges_from", Datastore::edges_from)
        .register_fn("edges_to", Datastore::edges_to)
        .register_fn("taste_vector_for", Datastore::taste_vector_for)
        .register_fn("pooled_taste_vector", Datastore::pooled_taste_vector);
    engine
}

impl ScoreCache {
    /// Compile `script_path` and resolve every query it will read, if this
    /// cache has not already done so for this `sources` table.
    ///
    /// `sources` is the pool's own table when it authored one (#210) and `None`
    /// when it did not, in which case the script's `sources()` is called for
    /// them. [`pick`] must be handed the same value, since the two together are
    /// the cache key.
    ///
    /// **This is the only half of scoring that touches the catalog.** Callers
    /// run every pool's `prepare` up front, while the catalog handle is in
    /// scope, and then drop that handle before any [`pick`] runs — so the
    /// database cannot be held open across a plugin's ranking work, which is
    /// unbounded and entirely the script author's to spend. Before this split,
    /// a scorer that took six minutes to rank a library held the station's only
    /// database connection for all six of them and every other channel queued
    /// behind it.
    pub fn prepare(
        &mut self,
        catalog: &Catalog,
        script_path: &Path,
        sources: Option<&PoolSources>,
    ) -> Result<(), String> {
        let key = (script_path.to_path_buf(), sources.cloned());
        if self.entries.contains_key(&key) {
            return Ok(());
        }
        let cached = compile_and_resolve(catalog, &engine(), script_path, sources)?;
        self.entries.insert(key, cached);
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

/// `ctx.seed` for one pool: the channel's resolved generation seed folded
/// with a stable hash of the pool's own name, so two pools naming the same
/// script draw independent sequences while one pool's value is stable across
/// two generations of the same seed (#255, ADR 0005).
///
/// The top bit is cleared so the result is always a non-negative `i64` — a
/// script picking with `ctx.seed % n` gets an ordinary non-negative modulus
/// without first needing to know Rhai's `%` follows Rust's sign-of-dividend
/// rule for negative operands. That costs one bit of the mix's 64, which is
/// not a meaningful loss of entropy.
fn mixed_seed(seed: u64, pool_name: &str) -> i64 {
    let mixed = crate::pattern::mix_seed(seed, crate::catalog::identity::fnv1a_64(pool_name));
    (mixed & (i64::MAX as u64)) as i64
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
/// so this only decides what `ctx.sets` / `ctx.history` / `ctx.datastore(name)`
/// (#181) resolve to for *this* call; it never re-reads `capabilities()`.
///
/// `seed` is the channel's resolved generation seed — the same value
/// [`crate::pattern::RollKey`] mixes into the pattern walk's own draws.
/// [`ctx.seed`](ScoreCtx) mixes it with `pool_name` via
/// [`crate::pattern::mix_seed`] (#255, ADR 0005), so two pools naming the same
/// script never draw the same sequence, while one pool's value stays fixed
/// across two generations of the same seed.
#[allow(clippy::too_many_arguments)]
pub fn pick(
    cache: &ScoreCache,
    script_path: &Path,
    sources: Option<&PoolSources>,
    inputs: &ScoreInputs,
    seed: u64,
    pool_name: &str,
    pool_config: Option<&serde_json::Value>,
    granted: GrantedCapabilities,
) -> Result<Vec<PickedItem>, String> {
    let engine = engine();

    // `sources` is half the cache key (#210), so the same value the pool handed
    // `prepare` has to come back here — a mismatch would find no entry and
    // report the bug below rather than quietly ranking another pool's set.
    let key = (script_path.to_path_buf(), sources.cloned());
    let cached = cache.entries.get(&key).ok_or_else(|| {
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
        // Datastore grants (#181) are keyed by name, not gated behind one flag
        // — `ScoreCtx::get_datastore` fails a lookup of any name absent here,
        // whether the pool never declared it or `validate` never granted it.
        datastores: granted.datastores,
        seed: mixed_seed(seed, pool_name),
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
            if n as usize > MAX_TAKE {
                return Err(format!(
                    "scorer plugin {}: pick() item #{index} ({entry_id:?}) names a take \
                     override of {n}, which exceeds the maximum of {MAX_TAKE}",
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

/// Compile a script and resolve every query it will read — the pool's own
/// `sources` table when it authored one (#210), otherwise the script's
/// `sources()`.
///
/// Runs once per script-and-sources pair per generation. A bad expression fails
/// here — before any ranking work, and before any pool has drawn — naming the
/// source it came from rather than surfacing as a mystery empty pool later, and
/// saying which file that source was written in so the author knows whether to
/// open the channel or the script.
fn compile_and_resolve(
    catalog: &Catalog,
    engine: &Engine,
    script_path: &Path,
    pool_sources: Option<&PoolSources>,
) -> Result<CachedScript, String> {
    let source = std::fs::read_to_string(script_path)
        .map_err(|e| format!("read scorer plugin {}: {e}", script_path.display()))?;
    let ast = engine
        .compile(&source)
        .map_err(|e| format!("compile scorer plugin {}: {e}", script_path.display()))?;

    // The pool's table replaces the script's function wholesale, which is also
    // why `sources()` is not called at all when one is present: a script that
    // declares none, or whose own would fail, is still usable by a pool that
    // brought its own queries.
    let declared: Vec<(String, String)> = match pool_sources {
        Some(table) => table
            .iter()
            .map(|(name, cel)| (name.clone(), cel.clone()))
            .collect(),
        None => {
            let mut scope = Scope::new();
            let from_script: Map = engine
                .call_fn(&mut scope, &ast, "sources", ())
                .map_err(|e| format!("scorer plugin {}: sources(): {e}", script_path.display()))?;
            from_script
                .into_iter()
                .map(|(name, expr)| {
                    let cel = expr.into_string().map_err(|actual| {
                        format!(
                            "scorer plugin {}: source {name:?} must be a CEL string, got {actual}",
                            script_path.display()
                        )
                    })?;
                    Ok((name.to_string(), cel))
                })
                .collect::<Result<_, String>>()?
        }
    };

    // Names the file the author has to go and edit. A channel-authored source
    // that fails is the channel's line, not the script's, and saying "scorer
    // plugin taste-engine.rhai" about an expression that is not in
    // taste-engine.rhai sends them to the wrong file.
    let whose = match pool_sources {
        Some(_) => "channel-authored source".to_string(),
        None => format!("scorer plugin {}: source", script_path.display()),
    };

    let mut sets = Map::new();
    for (name, cel) in declared {
        let ids = catalog
            .resolve_query(&cel)
            .map_err(|e| format!("{whose} {name:?} ({cel}): {e}"))?;
        let items = load_items(catalog, &ids).map_err(|e| format!("{whose} {name:?}: {e}"))?;
        sets.insert(name.into(), Dynamic::from_array(items));
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
        cache.prepare(catalog, script_path, None)?;
        pick(
            cache,
            script_path,
            None,
            inputs,
            0,
            pool_name,
            pool_config,
            GrantedCapabilities {
                catalog_read: true,
                watch_history: true,
                ..Default::default()
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
        cache.prepare(&catalog(), &p, None).unwrap();
        let err = pick(
            &cache,
            &p,
            None,
            &ScoreInputs::default(),
            0,
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
        cache.prepare(&catalog(), &p, None).unwrap();
        let err = pick(
            &cache,
            &p,
            None,
            &ScoreInputs::default(),
            0,
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
        cache.prepare(&catalog(), &p, None).unwrap();
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
            None,
            &inputs,
            0,
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

    // ---- ctx.seed (#255, ADR 0005) ------------------------------------------

    /// A script reads back whatever `ctx.seed` resolved to by returning it as
    /// the (fabricated, catalog-unchecked) picked id — `pick` never validates
    /// a returned id against the catalog, so this is a normal way to assert
    /// on a `ctx` field's value, the same trick `ungated_fields_stay_available…`
    /// above uses for `ctx.config`.
    fn seed_pick(seed: u64, pool_name: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            "fn sources() { #{} }\nfn pick(ctx) { [ctx.seed.to_string()] }\n",
        );
        let mut cache = ScoreCache::default();
        cache.prepare(&catalog(), &p, None).unwrap();
        let got = pick(
            &cache,
            &p,
            None,
            &ScoreInputs::default(),
            seed,
            pool_name,
            None,
            GrantedCapabilities::default(),
        )
        .unwrap();
        got.into_iter().next().unwrap().id
    }

    /// Two pools of one channel naming the same script must not draw the
    /// same sequence (ADR 0005) — `examples/samples/foryou.yaml` points a
    /// `movies` and a `shows` pool at one script, and both receiving the
    /// same seed would put their exploration slots in lockstep on air.
    #[test]
    fn ctx_seed_differs_by_pool_name() {
        assert_ne!(seed_pick(42, "movies"), seed_pick(42, "shows"));
    }

    /// The same channel, the same authored seed, two generations: `ctx.seed`
    /// is identical — the property #168's generate-twice-and-diff check
    /// depends on.
    #[test]
    fn ctx_seed_is_stable_across_two_calls() {
        assert_eq!(seed_pick(42, "movies"), seed_pick(42, "movies"));
    }

    /// Changing the channel's seed changes `ctx.seed` for the same pool —
    /// the acceptance criterion "changing the seed changes which titles
    /// landed in those slots" has no chance of holding otherwise.
    #[test]
    fn ctx_seed_differs_when_the_channel_seed_differs() {
        assert_ne!(seed_pick(42, "movies"), seed_pick(43, "movies"));
    }

    /// Pins the exact mix `ctx.seed` computes — the channel seed and a
    /// stable hash of the pool name folded through the same SplitMix64 step
    /// the pattern engine's own `RollKey` uses, with the top bit cleared so
    /// script arithmetic never has to reckon with a negative `ctx.seed`.
    /// A regression here is a silent reshuffle of every exploration slot in
    /// production, not just a test failure.
    #[test]
    fn ctx_seed_is_the_channel_seed_mixed_with_the_pool_name() {
        let mixed = crate::pattern::mix_seed(42, crate::catalog::identity::fnv1a_64("movies"));
        let expected = (mixed & (i64::MAX as u64)) as i64;
        assert_eq!(seed_pick(42, "movies"), expected.to_string());
    }

    // ---- record return shape (#166) ----------------------------------------

    /// A plugin returning bare ids works exactly as before the record shape
    /// existed — the widening is additive.
    #[test]
    fn a_bare_id_carries_no_metadata_or_take_override() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, "fn sources() { #{} }\nfn pick(ctx) { [\"m1\"] }\n");
        let mut cache = ScoreCache::default();
        cache.prepare(&catalog(), &p, None).unwrap();
        let got = pick(
            &cache,
            &p,
            None,
            &ScoreInputs::default(),
            0,
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
        cache.prepare(&catalog(), &p, None).unwrap();
        let got = pick(
            &cache,
            &p,
            None,
            &ScoreInputs::default(),
            0,
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
        cache.prepare(&catalog(), &p, None).unwrap();
        let got = pick(
            &cache,
            &p,
            None,
            &ScoreInputs::default(),
            0,
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
        cache.prepare(&catalog(), &p, None).unwrap();
        let err = pick(
            &cache,
            &p,
            None,
            &ScoreInputs::default(),
            0,
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
            cache.prepare(&catalog(), &p, None).unwrap();
            let err = pick(
                &cache,
                &p,
                None,
                &ScoreInputs::default(),
                0,
                "test",
                None,
                GrantedCapabilities::default(),
            )
            .unwrap_err();
            assert!(err.contains("m1"), "take={take}, got {err}");
            assert!(err.contains("positive"), "take={take}, got {err}");
        }
    }

    /// A take override past `MAX_TAKE` fails the pick and names the entry,
    /// rather than reaching the pattern draw's `Vec::with_capacity(take)`
    /// (#202).
    #[test]
    fn a_take_override_past_the_maximum_fails_and_names_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!(
                "fn sources() {{ #{{}} }}\nfn pick(ctx) {{ [#{{ entry_id: \"m1\", take: {} }}] }}\n",
                MAX_TAKE + 1
            ),
        );
        let mut cache = ScoreCache::default();
        cache.prepare(&catalog(), &p, None).unwrap();
        let err = pick(
            &cache,
            &p,
            None,
            &ScoreInputs::default(),
            0,
            "test",
            None,
            GrantedCapabilities::default(),
        )
        .unwrap_err();
        assert!(err.contains("m1"), "got {err}");
        assert!(err.contains("maximum"), "got {err}");
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
        cache.prepare(&catalog(), &p, None).unwrap();
        let err = pick(
            &cache,
            &p,
            None,
            &ScoreInputs::default(),
            0,
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
        cache.prepare(&catalog(), &p, None).unwrap();
        let err = pick(
            &cache,
            &p,
            None,
            &ScoreInputs::default(),
            0,
            "test",
            None,
            GrantedCapabilities::default(),
        )
        .unwrap_err();
        assert!(err.contains("more than once"), "got {err}");
    }

    /// Four movies across three Plex libraries — the shape that made #210 a
    /// bug. `item.type == "movie"` is true of all four, so a script that
    /// declares only that hands a taste channel the concert film and the power
    /// hour along with the two features.
    fn library_catalog() -> Catalog {
        let c = Catalog::open_in_memory().unwrap();
        for (id, title, library) in [
            ("m1", "Alpha", "Movies"),
            ("m2", "Beta", "Movies"),
            ("c1", "Live at Wembley", "Concerts"),
            ("p1", "Power Hour Vol. 1", "Power Hours"),
        ] {
            let mut e = Entry::new(id, "movie", title, Source::Plex);
            e.library = Some(library.into());
            c.upsert_entry(&e).unwrap();
        }
        c
    }

    /// A script that returns every id it was handed, so the assertion is
    /// directly about which items reached `ctx.sets` — the one thing #210
    /// moves.
    const ECHO_SET: &str = r#"fn hooks() { ["pool_provider"] }
fn capabilities() { ["catalog_read"] }
fn sources() { #{ movies: `item.type == "movie"` } }
fn pick(ctx) {
    let out = [];
    for item in ctx.sets.movies { out.push(item.entry_id); }
    out
}
"#;

    fn sources(pairs: &[(&str, &str)]) -> PoolSources {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn echo(cache: &ScoreCache, path: &Path, src: Option<&PoolSources>) -> Vec<String> {
        pick(
            cache,
            path,
            src,
            &ScoreInputs::default(),
            0,
            "movies",
            None,
            GrantedCapabilities {
                catalog_read: true,
                watch_history: false,
                ..Default::default()
            },
        )
        .unwrap()
        .into_iter()
        .map(|p| p.id)
        .collect()
    }

    /// The pool's table replaces the script's `sources()` rather than adding to
    /// it: the script still declares `item.type == "movie"`, and the pool still
    /// gets only what its own expression matched.
    #[test]
    fn a_pool_authored_sources_table_replaces_the_scripts_own() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, ECHO_SET);
        let cat = library_catalog();
        let narrowed = sources(&[(
            "movies",
            r#"item.type == "movie" && item.library != "Concerts" && item.library != "Power Hours""#,
        )]);

        let mut cache = ScoreCache::default();
        cache.prepare(&cat, &p, Some(&narrowed)).unwrap();
        assert_eq!(
            echo(&cache, &p, Some(&narrowed)),
            vec!["m1", "m2"],
            "the concert and the power hour are not candidates"
        );
    }

    /// The script's own `sources()` is not even called when the pool brought
    /// its own — so a set name the script never declares still arrives, and a
    /// script whose `sources()` would fail is still usable.
    #[test]
    fn a_pool_authored_table_leaves_the_scripts_sources_uncalled() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            r#"fn sources() { throw "sources() must not run when the pool authored its own" }
fn pick(ctx) {
    let out = [];
    for item in ctx.sets.features { out.push(item.entry_id); }
    out
}
"#,
        );
        let cat = library_catalog();
        let table = sources(&[("features", r#"item.library == "Movies""#)]);

        let mut cache = ScoreCache::default();
        cache.prepare(&cat, &p, Some(&table)).unwrap();
        assert_eq!(echo(&cache, &p, Some(&table)), vec!["m1", "m2"]);
    }

    /// The cache is keyed on the script *and* its sources. Two pools naming one
    /// script with different tables each rank their own candidates — without
    /// the sources half of the key, whichever prepared first would hand its set
    /// to the other, and a channel that narrowed one pool would silently narrow
    /// its sibling.
    #[test]
    fn two_pools_naming_one_script_keep_their_own_sources() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, ECHO_SET);
        let cat = library_catalog();
        let features = sources(&[("movies", r#"item.library == "Movies""#)]);
        let concerts = sources(&[("movies", r#"item.library == "Concerts""#)]);

        let mut cache = ScoreCache::default();
        cache.prepare(&cat, &p, Some(&features)).unwrap();
        cache.prepare(&cat, &p, Some(&concerts)).unwrap();
        // And a third pool that authored nothing still gets the script's own.
        cache.prepare(&cat, &p, None).unwrap();

        assert_eq!(echo(&cache, &p, Some(&features)), vec!["m1", "m2"]);
        assert_eq!(echo(&cache, &p, Some(&concerts)), vec!["c1"]);
        // Catalog order, not the narrowed pools' — all four libraries.
        assert_eq!(echo(&cache, &p, None), vec!["c1", "m1", "m2", "p1"]);
    }

    /// Two pools that wrote the same table share one resolved set, whatever
    /// order they wrote the keys in — the reason the key is a `BTreeMap`.
    #[test]
    fn the_same_table_written_twice_resolves_once() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, ECHO_SET);
        let cat = library_catalog();
        let one = sources(&[
            ("movies", r#"item.library == "Movies""#),
            ("extra", r#"item.library == "Concerts""#),
        ]);
        let other = sources(&[
            ("extra", r#"item.library == "Concerts""#),
            ("movies", r#"item.library == "Movies""#),
        ]);

        let mut cache = ScoreCache::default();
        cache.prepare(&cat, &p, Some(&one)).unwrap();
        cache.prepare(&cat, &p, Some(&other)).unwrap();
        assert_eq!(cache.entries.len(), 1);
    }

    /// A broken expression the *channel* wrote must not be reported as the
    /// script's — the author has to know which file to open.
    #[test]
    fn a_bad_channel_authored_source_names_the_set_and_not_the_script() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, ECHO_SET);
        let err = ScoreCache::default()
            .prepare(
                &library_catalog(),
                &p,
                Some(&sources(&[("movies", "item.library ==== 3")])),
            )
            .expect_err("a malformed CEL expression must fail preparation");
        assert!(err.contains("channel-authored source"), "got {err}");
        assert!(err.contains("\"movies\""), "got {err}");
        assert!(
            !err.contains("plugin.rhai"),
            "the channel wrote this expression, not the script: {err}"
        );
    }
}

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
//! A plugin declares two functions:
//!
//! ```rhai
//! // Every catalog query this plugin will read, named. Run once, up front.
//! fn sources() {
//!     #{
//!         movies:   `item.type == "movie"`,
//!         episodes: `item.type == "episode"`,
//!     }
//! }
//!
//! // Returns entry_ids, most-wanted first.
//! fn pick(ctx) {
//!     // ctx.sets.movies   — array of item maps, one per match
//!     // ctx.pool          — the name of the pool asking
//!     // ctx.config        — the pool's `config:` block, passed through unread
//!     // ctx.target_count  — how many items the generation needs
//!     // ctx.history       — recent server-wide watch events
//!     // ctx.recent        — entry_ids this channel aired most recently
//!     // ctx.now           — unix seconds at generation time
//! }
//! ```
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

use rhai::{Array, Dynamic, Engine, Map, Scope};

use crate::catalog::{Catalog, TagNs};

/// One watch event from the server's history, as handed to a plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchEvent {
    /// The catalog entry watched, when it could be matched to one. History
    /// rows that match nothing in the catalog are dropped before they get here.
    pub entry_id: String,
    /// Unix seconds when the watch stopped.
    pub watched_at: i64,
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
    pub fn resolve_path(&self, plugin: &Path) -> std::path::PathBuf {
        if plugin.is_absolute() {
            plugin.to_path_buf()
        } else {
            self.base_dir.join(plugin)
        }
    }
}

/// Run `script_path` against the catalog and return the `entry_id`s it picked,
/// in the order it picked them.
///
/// Every failure is a config error phrased against the script: a missing file,
/// a compile error, a missing function, a query the catalog rejects, or a
/// returned id that is not in the catalog. A plugin that returns nothing is an
/// error too — an empty pool would silently shorten the channel, and a scorer
/// that finds nothing worth playing is a broken scorer, not an empty schedule.
pub fn run(
    catalog: &Catalog,
    script_path: &Path,
    inputs: &ScoreInputs,
    pool_name: &str,
    pool_config: Option<&serde_json::Value>,
    cache: &mut ScoreCache,
) -> Result<Vec<String>, String> {
    let mut engine = Engine::new();
    // Set the nesting limits explicitly. Rhai's defaults are lower in a debug
    // build than a release one, so leaving them alone means a plugin that
    // compiles for the daemon can fail under `cargo test` — a difference a
    // plugin author has no way to see coming. These are generous enough for
    // ordinary scripts and still bounded, so a runaway nesting depth fails to
    // compile rather than overflowing the stack.
    engine.set_max_expr_depths(128, 64);

    if !cache.entries.contains_key(script_path) {
        let cached = compile_and_resolve(catalog, &engine, script_path)?;
        cache.entries.insert(script_path.to_path_buf(), cached);
    }
    let cached = &cache.entries[script_path];
    let ast = &cached.ast;
    let sets = cached.sets.clone();

    let mut scope = Scope::new();

    let mut ctx = Map::new();
    ctx.insert("sets".into(), sets);
    // Which pool is asking. One script commonly serves several pools of the
    // same channel — a "movies" pool and a "shows" pool ranked by the same
    // taste — and without this the script cannot tell them apart, so both
    // would get the same list.
    ctx.insert("pool".into(), pool_name.to_string().into());
    // The pool's own `config`, handed over verbatim. The station reads nothing
    // out of it: whatever the author wrote is whatever the script sees, nested
    // to any depth. An absent config becomes an empty map rather than a missing
    // key, so a script can read `ctx.config.whatever` unconditionally and get
    // unit for anything unset — which is also why a mistyped key is silent.
    ctx.insert(
        "config".into(),
        match pool_config {
            Some(value) => rhai::serde::to_dynamic(value).map_err(|e| {
                format!(
                    "scorer plugin {}: pool {pool_name:?} config is not representable in Rhai: {e}",
                    script_path.display()
                )
            })?,
            None => Dynamic::from_map(Map::new()),
        },
    );
    ctx.insert("target_count".into(), (inputs.target_count as i64).into());
    ctx.insert("now".into(), inputs.now.into());
    ctx.insert(
        "history".into(),
        Dynamic::from_array(
            inputs
                .history
                .iter()
                .map(|e| {
                    let mut m = Map::new();
                    m.insert("entry_id".into(), e.entry_id.clone().into());
                    m.insert("watched_at".into(), e.watched_at.into());
                    Dynamic::from_map(m)
                })
                .collect(),
        ),
    );
    ctx.insert(
        "recent".into(),
        Dynamic::from_array(
            inputs
                .recent
                .iter()
                .map(|id| Dynamic::from(id.clone()))
                .collect(),
        ),
    );

    let picked: Array = engine
        .call_fn(&mut scope, ast, "pick", (Dynamic::from_map(ctx),))
        .map_err(|e| format!("scorer plugin {}: pick(): {e}", script_path.display()))?;

    let mut out = Vec::with_capacity(picked.len());
    let mut seen = std::collections::HashSet::new();
    for (i, value) in picked.into_iter().enumerate() {
        let id = value.into_string().map_err(|actual| {
            format!(
                "scorer plugin {}: pick() item #{i} must be an entry_id string, got {actual}",
                script_path.display()
            )
        })?;
        // A duplicate would give one item two positions in the same pool and
        // two cursors under one series key. Cheaper to reject than to explain.
        if !seen.insert(id.clone()) {
            return Err(format!(
                "scorer plugin {}: pick() returned {id:?} more than once",
                script_path.display()
            ));
        }
        out.push(id);
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

/// Load each id as a Rhai map: every column on `entries`, plus every exposed
/// tag namespace as an array.
fn load_items(catalog: &Catalog, ids: &[String]) -> Result<Array, String> {
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
}

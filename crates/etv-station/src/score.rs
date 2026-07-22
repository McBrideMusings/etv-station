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
//!     // ctx.target_count  — how many items the generation needs
//!     // ctx.history       — recent server-wide watch events
//!     // ctx.recent        — entry_ids this channel aired most recently
//!     // ctx.now           — unix seconds at generation time
//! }
//! ```
//!
//! Queries are declared up front rather than callable mid-`pick` so that a
//! malformed expression fails before any ranking work, and so the catalog is
//! read exactly once per generation no matter how the script is written.
//!
//! Each item map carries every column on `entries` plus every tag namespace
//! (genres, cast, labels, …), so extending an algorithm to weigh a new signal
//! is a script edit, never a rebuild.

use std::path::Path;

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
    pub history: Vec<WatchEvent>,
    /// What this channel aired most recently, newest last, from the
    /// play-history ledger.
    pub recent: Vec<String>,
    /// Unix seconds at generation time. Passed in rather than read inside the
    /// script so a generation is reproducible from its inputs.
    pub now: i64,
}

impl ScoreInputs {
    /// A const-constructible empty set of inputs — no history, no airings, no
    /// target. `Default` cannot be used in a const context, and tests that
    /// exercise pools which never reach a plugin still have to name something.
    pub const fn new_empty() -> Self {
        Self {
            target_count: 0,
            history: Vec::new(),
            recent: Vec::new(),
            now: 0,
        }
    }
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
) -> Result<Vec<String>, String> {
    let source = std::fs::read_to_string(script_path)
        .map_err(|e| format!("read scorer plugin {}: {e}", script_path.display()))?;

    let mut engine = Engine::new();
    // Set the nesting limits explicitly. Rhai's defaults are lower in a debug
    // build than a release one, so leaving them alone means a plugin that
    // compiles for the daemon can fail under `cargo test` — a difference a
    // plugin author has no way to see coming. These are generous enough for
    // ordinary scripts and still bounded, so a runaway nesting depth fails to
    // compile rather than overflowing the stack.
    engine.set_max_expr_depths(128, 64);
    let ast = engine
        .compile(&source)
        .map_err(|e| format!("compile scorer plugin {}: {e}", script_path.display()))?;

    let mut scope = Scope::new();
    let sources: Map = engine
        .call_fn(&mut scope, &ast, "sources", ())
        .map_err(|e| format!("scorer plugin {}: sources(): {e}", script_path.display()))?;

    // Resolve every declared query once, up front. A bad expression fails here,
    // before any ranking work, and names the source it came from.
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

    let mut ctx = Map::new();
    ctx.insert("sets".into(), Dynamic::from_map(sets));
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
        .call_fn(&mut scope, &ast, "pick", (Dynamic::from_map(ctx),))
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
        let got = run(&catalog(), &p, &ScoreInputs::default()).unwrap();
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
            run(&catalog(), &p, &ScoreInputs::default()).unwrap(),
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
        assert_eq!(run(&catalog(), &p, &inputs).unwrap(), ["m2"]);
    }

    #[test]
    fn a_bad_source_expression_names_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            "fn sources() { #{ broken: `item.nope == 1` } }\nfn pick(ctx) { [] }\n",
        );
        let e = run(&catalog(), &p, &ScoreInputs::default()).unwrap_err();
        assert!(e.contains("broken"), "got {e}");
    }

    #[test]
    fn an_empty_pick_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, "fn sources() { #{} }\nfn pick(ctx) { [] }\n");
        let e = run(&catalog(), &p, &ScoreInputs::default()).unwrap_err();
        assert!(e.contains("picked nothing"), "got {e}");
    }

    #[test]
    fn a_duplicate_pick_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            "fn sources() { #{} }\nfn pick(ctx) { [\"m1\", \"m1\"] }\n",
        );
        let e = run(&catalog(), &p, &ScoreInputs::default()).unwrap_err();
        assert!(e.contains("more than once"), "got {e}");
    }

    #[test]
    fn a_missing_pick_function_names_the_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, "fn sources() { #{} }\n");
        let e = run(&catalog(), &p, &ScoreInputs::default()).unwrap_err();
        assert!(e.contains("pick()"), "got {e}");
    }
}

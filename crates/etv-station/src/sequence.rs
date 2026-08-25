//! Sequencer hook (#169) — a block names a `sequencer` plugin script instead
//! of a `pattern`. The station resolves the block's pools exactly as it does
//! for a pattern block — `expr`/`plugin`/`groups`, then `order`,
//! `bucket_order`, and `constraints` — and hands the resolved pools plus the
//! generation window to the script's `arrange()`, which returns the block's
//! final ordered timeline. Nothing about pool *resolution* differs from the
//! pattern walk; only what draws from the result does.
//!
//! See ADR 0002 for why a pool's items cross the boundary as full item maps
//! rather than bare ids (the same marshalling a scorer's `ctx.sets` already
//! uses — [`crate::score::load_items`]), and ADR 0004 for why wall-clock
//! dayparting lives here rather than at channel composition.
//!
//! # The contract
//!
//! A plugin declares `sequencer` in its `hooks()` (#159) and implements one
//! function:
//!
//! ```rhai
//! fn hooks() { ["sequencer"] }
//!
//! // Returns entry_ids, in the order they should air.
//! fn arrange(ctx) {
//!     // ctx.pools.<name>       — this pool's resolved items, in the order
//!     //                          `order` / `bucket_order` / `constraints`
//!     //                          left them (full item maps: entry_id,
//!     //                          title, duration_ms, show, season, tags, …)
//!     // ctx.pool_config.<name> — that pool's own `config:` block, passed
//!     //                          through unread, same as a scorer's
//!     //                          `ctx.config` (empty map when unset) — a
//!     //                          daypart script's "which hours/weekdays does
//!     //                          this pool claim" table lives here, not in
//!     //                          the schema (#14, ADR 0004)
//!     // ctx.window.from        — unix seconds this generation begins airing
//!     // ctx.window.fill_secs   — how many seconds of airtime this
//!     //                          generation should cover, or () when
//!     //                          unbounded (the stateless entry point)
//!     // ctx.resume.<pool>.next — the series whose turn was next in that
//!     //                          pool as of the last generation, or ()
//!     // ctx.cursor.<series>    — the last entry_id the play-history ledger
//!     //                          recorded for that series, or absent
//!     // ctx.now                — unix seconds at generation time
//!
//!     // local_time(unix_secs) -> #{ weekday, hour, minute } — the station's
//!     // configured tz (ADR 0004) applied to any unix-second instant, not
//!     // just ctx.window.from: a daypart script walks its own cursor forward
//!     // by each item's duration and re-reads the clock at the new instant to
//!     // find the next boundary. `weekday` is one of "mon".."sun"; `hour` is
//!     // 0-23; both roll on the same local-midnight grid `chunk_hours` does.
//!     // Global function, not a ctx field — registered on the engine this
//!     // script runs under, nowhere else.
//!     let t = local_time(ctx.window.from);
//! }
//! ```
//!
//! `ctx.resume` / `ctx.cursor` are read-only inputs, never a second output
//! channel: the script decides, per pool, whether to honor them (an
//! `advance: "resume"`-style pool) or ignore them and draw from the top (an
//! `advance: "restart"`-style pool) — which is how two pools in one block get
//! different advance semantics under one script (the "For You" case). The
//! station derives the new `.resume` state from *which items came back*,
//! exactly as it does for the pattern walk: see [`build`]'s resume
//! derivation below.
//!
//! # Bounds the station enforces regardless of the script
//!
//! - An id `arrange()` returns that is in none of this block's pools fails
//!   the generation, naming the item — the pools are the sequencer's entire
//!   universe.
//! - A timeline shorter than the window is not padded; it falls through to
//!   the channel's normal short-generation handling (the daemon's own
//!   catch-up loop simply asks for the remainder next tick).
//! - A timeline longer than the window is truncated at the item that crosses
//!   the boundary — the same "the crossing unit finishes" leniency the
//!   pattern walk gives a cycle, translated to the sequencer's atomic unit
//!   (an item, since there is no cycle).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use rhai::{Array, Dynamic, Map, Scope};
use time::{OffsetDateTime, Weekday};
use time_tz::{OffsetDateTimeExt, Tz};

use crate::catalog::Catalog;
use crate::config::{Constraints, Pool, ShowGroup};
use crate::guide::GuideConfig;
pub use crate::pattern::BlockBuild;
use crate::resume::{GenerationState, PoolResume};

/// The window bounds a sequencer script sees (ADR 0004).
#[derive(Debug, Clone, Copy)]
pub struct Window {
    /// Unix seconds this generation begins airing at — the same `from` the
    /// daemon lays items from, so a script computing "what hour is it" gets
    /// the instant this timeline will actually occupy, not the instant the
    /// tick happened to run.
    pub from: i64,
    /// How much airtime this generation should cover. `None` for the
    /// stateless entry point ([`crate::resolve::resolve_channel`]), which
    /// asks for however long the script chooses to return.
    pub fill: Option<Duration>,
}

/// Resolve a sequencer block: resolve its pools exactly as [`crate::pattern`]
/// would for a `pattern` block, then hand them to `sequencer`'s `arrange()`
/// and return the timeline it picks, truncated to `window.fill` when the
/// script overruns it.
///
/// `block_constraints` is the block's own `[constraints]` table, which every
/// pool that declares none inherits — the same rule a pattern block's pools
/// follow.
///
/// `seed` is the channel's resolved generation seed — the same value a
/// `pattern` block's `RollKey` mixes into its own draws. A pool inside a
/// sequencer block is resolved through the same [`crate::pattern::resolve_pool_sources`]
/// a pattern block uses, so a `plugin:` pool here gets `ctx.seed` exactly as
/// one under `pattern:` does (#255).
#[allow(clippy::too_many_arguments)]
pub fn build(
    catalog: &Catalog,
    pools: &[Pool],
    groups: &[ShowGroup],
    block_constraints: Option<&Constraints>,
    sequencer: &Path,
    state: &GenerationState,
    seed: u64,
    score_env: crate::score::ScoreEnv<'_>,
    window: Window,
) -> Result<BlockBuild, String> {
    let mut score_cache = crate::score::ScoreCache::default();
    let pool_id_lists = crate::pattern::resolve_pool_sources(
        catalog,
        pools,
        groups,
        &mut score_cache,
        seed,
        score_env,
    )?;

    // Per pool: its bucket_order-sequenced series (kept for the resume
    // derivation below) and its item maps (handed to the script as
    // `ctx.pools.<name>`). `item_location` is every id's home — which pool,
    // and which series inside it — so the "returned an item in no pool"
    // check and the resume derivation are both O(1) per returned id rather
    // than a scan per lookup.
    let mut series_by_pool: Vec<(String, Vec<crate::pattern::Series>)> =
        Vec::with_capacity(pools.len());
    let mut item_location: HashMap<String, (usize, usize)> = HashMap::new();
    let mut union_ids: HashSet<String> = HashSet::new();
    let mut pools_ctx = Map::new();
    let mut pool_config_ctx = Map::new();

    for (cfg, ids) in pools.iter().zip(pool_id_lists) {
        let own: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let seam: Vec<String> = state
            .tail
            .iter()
            .filter(|id| own.contains(id.as_str()))
            .cloned()
            .collect();
        let (series, _limits, _keys, _durations, _mean) = crate::pattern::resolve_pool_series(
            catalog,
            cfg,
            ids,
            block_constraints,
            &seam,
            false,
        )?;

        let flat: Vec<String> = series.iter().flat_map(|s| s.ids.iter().cloned()).collect();
        let items = crate::score::load_items(catalog, &flat)
            .map_err(|e| format!("pool {:?}: {e}", cfg.name))?;
        pools_ctx.insert(cfg.name.clone().into(), Dynamic::from_array(items));

        // This pool's own `config:` block, passed through unread exactly as a
        // scorer's `ctx.config` is — the existing generic pool carrier (ADR
        // 0002), now doubling as where a daypart script's hour/weekday table
        // lives, so #14 needs no new schema field to say what hours a pool
        // claims.
        let config = crate::score::pool_config_dynamic(
            cfg.config.as_ref(),
            "sequencer",
            sequencer,
            &cfg.name,
        )?;
        pool_config_ctx.insert(cfg.name.clone().into(), config);

        let pool_idx = series_by_pool.len();
        for (si, s) in series.iter().enumerate() {
            for id in &s.ids {
                union_ids.insert(id.clone());
                item_location.insert(id.clone(), (pool_idx, si));
            }
        }
        series_by_pool.push((cfg.name.clone(), series));
    }

    let arranged_raw = call_arrange(
        sequencer,
        &pools_ctx,
        &pool_config_ctx,
        pools,
        state,
        score_env,
        window,
    )?;

    for id in &arranged_raw {
        if !union_ids.contains(id) {
            return Err(format!(
                "sequencer plugin {}: arrange() returned {id:?}, which is not in any of this \
                 block's pools",
                sequencer.display()
            ));
        }
    }

    let arranged = truncate_to_window(catalog, arranged_raw, window.fill)?;

    // Derive the resume state from which items came back, exactly as the
    // pattern walk does — walk the timeline in order, and for each pool note
    // the last series it drew from. That series' successor is what the next
    // generation's `ctx.resume.<pool>.next` reports; a pool the script never
    // drew from reports `None`, same as a freshly-resolved pool the pattern
    // walk has not yet visited.
    let mut last_series: HashMap<usize, usize> = HashMap::new();
    for id in &arranged {
        if let Some(&(pool_idx, si)) = item_location.get(id) {
            last_series.insert(pool_idx, si);
        }
    }
    let resume_out = series_by_pool
        .iter()
        .enumerate()
        .map(|(pool_idx, (name, series))| {
            let next = last_series.get(&pool_idx).and_then(|&si| {
                (!series.is_empty()).then(|| series[(si + 1) % series.len()].key.clone())
            });
            (name.clone(), PoolResume { next })
        })
        .collect();

    // The metadata half of what plugin pools picked (#166) — `arrange()` sees
    // a pool's `pick()` record only as the bare `entry_id`s in `ctx.pools`,
    // with the record's `metadata` stashed on the side by `crate::score::pick`
    // and recovered here by id, through the same shared helper `pattern::build`
    // uses (#203) so this path can't drop a field the pattern path keeps.
    let metadata_out = crate::pattern::flatten_picked_extras(score_cache.picked_extras);

    // This pool's own `guide:` (#289), the same rung the pattern walk
    // records — read here from `item_location`, which already knows which
    // pool supplied each id in the script's chosen timeline.
    let mut guide_out: HashMap<String, GuideConfig> = HashMap::new();
    for id in &arranged {
        if let Some(&(pool_idx, _)) = item_location.get(id)
            && let Some(guide) = &pools[pool_idx].guide
        {
            guide_out.insert(id.clone(), guide.clone());
        }
    }

    Ok(BlockBuild {
        ids: arranged,
        resume: resume_out,
        metadata: metadata_out,
        guides: guide_out,
    })
}

/// Compile `sequencer` and call its `arrange(ctx)`, returning the raw
/// entry_id list it picked — unvalidated against the block's pools and
/// untruncated to the window, both of which [`build`] does once this
/// returns.
fn call_arrange(
    sequencer: &Path,
    pools_ctx: &Map,
    pool_config_ctx: &Map,
    pools: &[Pool],
    state: &GenerationState,
    score_env: crate::score::ScoreEnv<'_>,
    window: Window,
) -> Result<Vec<String>, String> {
    let source = std::fs::read_to_string(sequencer)
        .map_err(|e| format!("read sequencer plugin {}: {e}", sequencer.display()))?;
    // The station's configured tz (ADR 0004), or UTC at the stateless entry
    // points that carry no station config — see the doc note on
    // `ScoreInputs::tz`. Baked into `local_time` via closure rather than
    // threaded through `ctx`, so a script reads the clock at any instant it
    // computes (a running cursor), not only at `ctx.window.from`.
    let tz = score_env.inputs.tz.unwrap_or(time_tz::timezones::db::UTC);
    let mut engine = crate::score::engine();
    engine.register_fn("local_time", move |secs: i64| local_time_map(secs, tz));
    let ast = engine
        .compile(&source)
        .map_err(|e| format!("compile sequencer plugin {}: {e}", sequencer.display()))?;

    let mut window_map = Map::new();
    window_map.insert("from".into(), window.from.into());
    window_map.insert(
        "fill_secs".into(),
        match window.fill {
            Some(d) => (d.as_secs() as i64).into(),
            None => Dynamic::UNIT,
        },
    );

    // Per-pool resume input: which series' turn was next as of the last
    // generation. Read-only — the station derives the new value itself from
    // what `arrange()` actually returns (#169 decision 2).
    let mut resume_map = Map::new();
    for cfg in pools {
        let next = state.resume.pool(&cfg.name).and_then(|p| p.next.clone());
        let mut m = Map::new();
        m.insert(
            "next".into(),
            next.map(Dynamic::from).unwrap_or(Dynamic::UNIT),
        );
        resume_map.insert(cfg.name.clone().into(), Dynamic::from_map(m));
    }

    // Per-series cursor from the play-history ledger, keyed exactly as
    // `HistoryDb::series_cursor` returns it (#109's business, not this
    // slice's — whatever it resolves to is what the script sees).
    let mut cursor_map = Map::new();
    for (key, entry_id) in &state.cursor {
        cursor_map.insert(key.clone().into(), entry_id.clone().into());
    }

    let mut ctx = Map::new();
    ctx.insert("pools".into(), Dynamic::from_map(pools_ctx.clone()));
    ctx.insert(
        "pool_config".into(),
        Dynamic::from_map(pool_config_ctx.clone()),
    );
    ctx.insert("window".into(), Dynamic::from_map(window_map));
    ctx.insert("resume".into(), Dynamic::from_map(resume_map));
    ctx.insert("cursor".into(), Dynamic::from_map(cursor_map));
    ctx.insert("now".into(), score_env.inputs.now.into());

    let mut scope = Scope::new();
    let picked: Array = engine
        .call_fn(&mut scope, &ast, "arrange", (Dynamic::from_map(ctx),))
        .map_err(|e| format!("sequencer plugin {}: arrange(): {e}", sequencer.display()))?;

    let mut out = Vec::with_capacity(picked.len());
    for (i, value) in picked.into_iter().enumerate() {
        let id = value.into_string().map_err(|actual| {
            format!(
                "sequencer plugin {}: arrange() item #{i} must be an entry_id string, got {actual}",
                sequencer.display()
            )
        })?;
        out.push(id);
    }
    Ok(out)
}

/// `local_time(unix_secs)` — the Rhai global function registered onto the
/// engine every `arrange()` call runs under (ADR 0004). Converts any
/// unix-second instant to the station's configured tz, so a daypart script
/// can re-read the clock at a cursor it has advanced by summed item
/// durations, not only at `ctx.window.from`.
///
/// `weekday` is a lowercase three-letter abbreviation (`"mon"` .. `"sun"`) —
/// vocabulary this hook owns outright, since no other part of the schema
/// names a weekday today. Rolls on the same local-midnight grid
/// [`crate::tz::add_chunk`] does, via the same `to_timezone` conversion.
///
/// An out-of-range `unix_secs` (a script computing nonsense) falls back to
/// the Unix epoch rather than panicking — the resulting weekday/hour is
/// visibly wrong to whoever reads the script's output, which is a better
/// failure than crashing the generation over an integer a script built by
/// arithmetic.
fn local_time_map(secs: i64, tz: &'static Tz) -> Map {
    let utc = OffsetDateTime::from_unix_timestamp(secs).unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let local = utc.to_timezone(tz);
    let weekday = match local.weekday() {
        Weekday::Monday => "mon",
        Weekday::Tuesday => "tue",
        Weekday::Wednesday => "wed",
        Weekday::Thursday => "thu",
        Weekday::Friday => "fri",
        Weekday::Saturday => "sat",
        Weekday::Sunday => "sun",
    };
    let mut m = Map::new();
    m.insert("weekday".into(), weekday.into());
    m.insert("hour".into(), (local.hour() as i64).into());
    m.insert("minute".into(), (local.minute() as i64).into());
    m
}

/// Truncate `ids` so their summed catalog duration does not exceed `fill` —
/// the item that crosses the boundary is kept whole, matching how the
/// pattern walk lets its own atomic unit (a cycle) finish rather than
/// splitting it. `None` leaves `ids` untouched: an unbounded generation has
/// nothing to truncate against.
///
/// An id the catalog has no measured duration for is charged the mean of the
/// ones that do (zero when none do), on the same reasoning
/// [`crate::pattern::PoolRuntime::runtime_of`] documents: counting it as free
/// would let a run of unmeasured items walk straight past the window.
fn truncate_to_window(
    catalog: &Catalog,
    ids: Vec<String>,
    fill: Option<Duration>,
) -> Result<Vec<String>, String> {
    let Some(fill) = fill else {
        return Ok(ids);
    };
    if ids.is_empty() {
        return Ok(ids);
    }
    let raw = catalog.durations_for(&ids).map_err(|e| e.to_string())?;
    let durations: HashMap<String, Duration> = raw
        .into_iter()
        .filter(|(_, ms)| *ms > 0 && *ms <= crate::resolve::MAX_CATALOG_DURATION_MS)
        .map(|(id, ms)| (id, Duration::from_millis(ms as u64)))
        .collect();
    let mean = if durations.is_empty() {
        Duration::ZERO
    } else {
        durations.values().sum::<Duration>() / durations.len() as u32
    };

    let mut laid = Duration::ZERO;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if laid >= fill {
            break;
        }
        laid += durations.get(&id).copied().unwrap_or(mean);
        out.push(id);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::catalog::{Entry as CatEntry, EntrySource, Source};
    use crate::config::{Advance, GroupBy, OnShort, Order, Rotate, Select};
    use crate::resume::ResumeMap;

    fn catalog() -> Catalog {
        let c = Catalog::open_in_memory().unwrap();
        let add = |id: &str, kind: &str, show: Option<(&str, i64, i64)>, ms: i64| {
            let mut e = CatEntry::new(id, kind, format!("Title {id}"), Source::Plex);
            e.duration_ms = Some(ms);
            if let Some((show_id, season, episode)) = show {
                e.show_id = Some(show_id.to_string());
                e.show = Some(show_id.trim_start_matches("show:").to_string());
                e.season = Some(season);
                e.episode = Some(episode);
            }
            c.upsert_entry(&e).unwrap();
            c.add_source(&EntrySource {
                source: Source::LocalFs,
                source_id: format!("fs-{id}"),
                entry_id: id.to_string(),
                playback_path: format!("/media/{id}.mkv"),
                last_seen: None,
                missing_since: None,
            })
            .unwrap();
        };
        for n in 1..=2 {
            add(&format!("mov-{n}"), "movie", None, 30_000);
        }
        for n in 1..=4 {
            add(
                &format!("got-e{n}"),
                "episode",
                Some(("show:got", 1, n)),
                30_000,
            );
        }
        for n in 1..=4 {
            add(
                &format!("inv-e{n}"),
                "episode",
                Some(("show:inv", 1, n)),
                30_000,
            );
        }
        c
    }

    fn pool(name: &str, expr: &str) -> Pool {
        Pool {
            name: name.into(),
            expr: Some(expr.into()),
            plugin: None,
            sources: None,
            groups: Vec::new(),
            order: Some(Order::parse("title:asc").unwrap()),
            bucket_order: None,
            group_by: GroupBy::Show,
            select: Select::RoundRobin,
            rotate: Rotate::Visit,
            advance: Advance::Restart,
            on_short: OnShort::Next,
            constraints: None,
            config: None,
            // An `expr` pool reaches no plugin, so it needs no grant (#167).
            capabilities: Vec::new(),
            datastores: Vec::new(),
            guide: None,
        }
    }

    fn test_env() -> crate::score::ScoreEnv<'static> {
        static EMPTY: std::sync::LazyLock<crate::score::ScoreInputs> =
            std::sync::LazyLock::new(crate::score::ScoreInputs::default);
        crate::score::ScoreEnv {
            inputs: &EMPTY,
            base_dir: std::path::Path::new("."),
        }
    }

    fn write(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
        let p = dir.path().join("sequencer.rhai");
        std::fs::write(&p, body).unwrap();
        p
    }

    /// The core promise of the hook: `arrange()` decides the order, not the
    /// pattern walk — reversing the movies pool here is something no pattern
    /// step can express.
    #[test]
    fn arrange_decides_the_order() {
        let dir = tempfile::tempdir().unwrap();
        let script = write(
            &dir,
            r#"
fn hooks() { ["sequencer"] }
fn arrange(ctx) {
    let ids = [];
    for item in ctx.pools.movies { ids.push(item.entry_id); }
    ids.reverse();
    ids
}
"#,
        );
        let cat = catalog();
        let pools = vec![pool("movies", "item.type == \"movie\"")];
        let BlockBuild { ids, .. } = build(
            &cat,
            &pools,
            &[],
            None,
            &script,
            &GenerationState::empty(),
            0,
            test_env(),
            Window {
                from: 0,
                fill: None,
            },
        )
        .unwrap();
        assert_eq!(ids, vec!["mov-2", "mov-1"]);
    }

    /// The item maps carry duration — the ADR 0004 fact a daypart sequencer
    /// needs to fit items against a clock boundary, and #169 decision 1's
    /// whole reason bare ids were rejected.
    #[test]
    fn item_maps_carry_duration_and_window_carries_the_clock() {
        let dir = tempfile::tempdir().unwrap();
        let script = write(
            &dir,
            r#"
fn hooks() { ["sequencer"] }
fn arrange(ctx) {
    let item = ctx.pools.movies[0];
    if item.duration_ms != 30000 { throw "duration missing: " + item.duration_ms; }
    if ctx.window.from != 12345 { throw "window.from missing"; }
    if ctx.window.fill_secs != 600 { throw "window.fill_secs missing"; }
    ["mov-1"]
}
"#,
        );
        let cat = catalog();
        let pools = vec![pool("movies", "item.type == \"movie\"")];
        let BlockBuild { ids, .. } = build(
            &cat,
            &pools,
            &[],
            None,
            &script,
            &GenerationState::empty(),
            0,
            test_env(),
            Window {
                from: 12345,
                fill: Some(Duration::from_secs(600)),
            },
        )
        .unwrap();
        assert_eq!(ids, vec!["mov-1"]);
    }

    /// The headline capability: two pools inside one sequencer block advance
    /// differently under one script — one restarts from the top every
    /// generation, the other resumes from `ctx.resume`/`ctx.cursor` — which a
    /// single `pattern` block cannot express (issue #169, "the For You
    /// case").
    #[test]
    fn two_pools_advance_differently_under_one_sequencer() {
        let dir = tempfile::tempdir().unwrap();
        let script = write(
            &dir,
            r#"
fn hooks() { ["sequencer"] }
fn arrange(ctx) {
    // `movies` always restarts: ignore ctx.resume/ctx.cursor entirely.
    let out = [];
    for item in ctx.pools.movies { out.push(item.entry_id); }
    // `shows` resumes: start after whatever the ledger last recorded for
    // this series, ignoring the freshly-resolved top.
    let last = ctx.cursor["show:got"];
    let started = last == ();
    for item in ctx.pools.shows {
        if started { out.push(item.entry_id); }
        if item.entry_id == last { started = true; }
    }
    out
}
"#,
        );
        let cat = catalog();
        let pools = vec![pool("movies", "item.type == \"movie\""), {
            let mut p = pool("shows", "item.type == \"episode\" && item.show == \"got\"");
            p.order = Some(Order::parse("season:asc,episode:asc").unwrap());
            p
        }];
        let mut state = GenerationState::empty();
        state.cursor.insert("show:got".into(), "got-e2".to_string());

        let BlockBuild { ids, .. } = build(
            &cat,
            &pools,
            &[],
            None,
            &script,
            &state,
            0,
            test_env(),
            Window {
                from: 0,
                fill: None,
            },
        )
        .unwrap();
        // movies restarted from the top; shows resumed after got-e2.
        assert_eq!(ids, vec!["mov-1", "mov-2", "got-e3", "got-e4"]);
    }

    /// Error case: an id `arrange()` invents outside its pools fails the
    /// generation, naming the item.
    #[test]
    fn an_item_outside_every_pool_fails_and_names_the_item() {
        let dir = tempfile::tempdir().unwrap();
        let script = write(
            &dir,
            r#"
fn hooks() { ["sequencer"] }
fn arrange(ctx) { ["nonexistent-id"] }
"#,
        );
        let cat = catalog();
        let pools = vec![pool("movies", "item.type == \"movie\"")];
        let err = build(
            &cat,
            &pools,
            &[],
            None,
            &script,
            &GenerationState::empty(),
            0,
            test_env(),
            Window {
                from: 0,
                fill: None,
            },
        )
        .unwrap_err();
        assert!(err.contains("nonexistent-id"), "got {err}");
        assert!(err.contains("not in any"), "got {err}");
    }

    /// A short timeline is not padded — the caller (the daemon's own
    /// catch-up loop) simply asks for the remainder next tick.
    #[test]
    fn a_short_timeline_is_returned_as_is() {
        let dir = tempfile::tempdir().unwrap();
        let script = write(
            &dir,
            r#"
fn hooks() { ["sequencer"] }
fn arrange(ctx) { ["mov-1"] }
"#,
        );
        let cat = catalog();
        let pools = vec![pool("movies", "item.type == \"movie\"")];
        let BlockBuild { ids, .. } = build(
            &cat,
            &pools,
            &[],
            None,
            &script,
            &GenerationState::empty(),
            0,
            test_env(),
            Window {
                from: 0,
                fill: Some(Duration::from_secs(3600)),
            },
        )
        .unwrap();
        assert_eq!(ids, vec!["mov-1"]);
    }

    /// An overrunning timeline is truncated at the item that crosses the
    /// window boundary — everything after it is dropped.
    #[test]
    fn an_overrunning_timeline_is_truncated_at_the_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let script = write(
            &dir,
            r#"
fn hooks() { ["sequencer"] }
fn arrange(ctx) {
    let ids = [];
    for item in ctx.pools.shows { ids.push(item.entry_id); }
    ids
}
"#,
        );
        let cat = catalog();
        let pools = vec![{
            let mut p = pool("shows", "item.type == \"episode\" && item.show == \"got\"");
            p.order = Some(Order::parse("season:asc,episode:asc").unwrap());
            p
        }];
        // Every episode is 30s; a 65s window fits two whole episodes and
        // then the one that crosses the boundary — three, not all four.
        let BlockBuild { ids, .. } = build(
            &cat,
            &pools,
            &[],
            None,
            &script,
            &GenerationState::empty(),
            0,
            test_env(),
            Window {
                from: 0,
                fill: Some(Duration::from_secs(65)),
            },
        )
        .unwrap();
        assert_eq!(ids, vec!["got-e1", "got-e2", "got-e3"]);
    }

    /// The resume state the station derives after a draw: the pool's series
    /// successor to whichever series the script last drew from, so a
    /// following generation's `ctx.resume.<pool>.next` reports something
    /// useful even though the script — not a rotation — chose the order.
    #[test]
    fn resume_reports_the_series_after_the_last_one_drawn() {
        let dir = tempfile::tempdir().unwrap();
        let script = write(
            &dir,
            r#"
fn hooks() { ["sequencer"] }
fn arrange(ctx) {
    // Draw one episode from the `got` series only.
    ["got-e1"]
}
"#,
        );
        let cat = catalog();
        let pools = vec![{
            let mut p = pool("shows", "item.type == \"episode\"");
            p.order = Some(Order::parse("show:asc,season:asc,episode:asc").unwrap());
            p
        }];
        let BlockBuild { resume, .. } = build(
            &cat,
            &pools,
            &[],
            None,
            &script,
            &GenerationState::empty(),
            0,
            test_env(),
            Window {
                from: 0,
                fill: None,
            },
        )
        .unwrap();
        // Two series in the pool: got and inv. Having drawn from got, the
        // successor series is inv.
        assert_eq!(
            resume.get("shows").unwrap().next.as_deref(),
            Some("show:inv")
        );
    }

    /// A pool the script never drew from reports no resume state — the same
    /// "start at the top" meaning a freshly-resolved pool already carries.
    #[test]
    fn an_undrawn_pool_reports_no_resume_state() {
        let dir = tempfile::tempdir().unwrap();
        let script = write(
            &dir,
            r#"
fn hooks() { ["sequencer"] }
fn arrange(ctx) { ["mov-1"] }
"#,
        );
        let cat = catalog();
        let pools = vec![
            pool("movies", "item.type == \"movie\""),
            pool("shows", "item.type == \"episode\""),
        ];
        let BlockBuild { resume, .. } = build(
            &cat,
            &pools,
            &[],
            None,
            &script,
            &GenerationState::empty(),
            0,
            test_env(),
            Window {
                from: 0,
                fill: None,
            },
        )
        .unwrap();
        assert!(resume.get("shows").unwrap().next.is_none());
    }

    /// The `.resume` sidecar carries `ctx.resume.<pool>.next` back in on the
    /// next generation, verbatim as the last generation reported it.
    #[test]
    fn resume_next_reaches_arrange_as_ctx_resume() {
        let dir = tempfile::tempdir().unwrap();
        let script = write(
            &dir,
            r#"
fn hooks() { ["sequencer"] }
fn arrange(ctx) {
    if ctx.resume.movies.next != "show:whatever" { throw "resume.next missing"; }
    ["mov-1"]
}
"#,
        );
        let cat = catalog();
        let pools = vec![pool("movies", "item.type == \"movie\"")];
        let mut resume_map = ResumeMap::new();
        resume_map.pools.insert(
            "movies".into(),
            PoolResume {
                next: Some("show:whatever".into()),
            },
        );
        let state = GenerationState {
            resume: resume_map,
            cursor: BTreeMap::new(),
            tail: Vec::new(),
        };
        let BlockBuild { ids, .. } = build(
            &cat,
            &pools,
            &[],
            None,
            &script,
            &state,
            0,
            test_env(),
            Window {
                from: 0,
                fill: None,
            },
        )
        .unwrap();
        assert_eq!(ids, vec!["mov-1"]);
    }

    /// A pool's `config:` block reaches the script as `ctx.pool_config.<name>`
    /// — the existing generic pool carrier (ADR 0002), not a new schema
    /// field, doubling as where a daypart script's hour/weekday table lives
    /// (#14, ADR 0004).
    #[test]
    fn pool_config_reaches_arrange_as_ctx_pool_config() {
        let dir = tempfile::tempdir().unwrap();
        let script = write(
            &dir,
            r#"
fn hooks() { ["sequencer"] }
fn arrange(ctx) {
    if ctx.pool_config.movies.hour_start != 20 { throw "pool_config missing"; }
    ["mov-1"]
}
"#,
        );
        let cat = catalog();
        let mut movies = pool("movies", "item.type == \"movie\"");
        movies.config = Some(serde_json::json!({"hour_start": 20}));
        let BlockBuild { ids, .. } = build(
            &cat,
            &[movies],
            &[],
            None,
            &script,
            &GenerationState::empty(),
            0,
            test_env(),
            Window {
                from: 0,
                fill: None,
            },
        )
        .unwrap();
        assert_eq!(ids, vec!["mov-1"]);
    }

    /// A pool with no `config:` reads back an empty map, same convention a
    /// scorer's `ctx.config` follows — a script can read
    /// `ctx.pool_config.<name>.anything` unconditionally and get unit back
    /// rather than handling a missing key specially.
    #[test]
    fn an_unset_pool_config_is_an_empty_map() {
        let dir = tempfile::tempdir().unwrap();
        let script = write(
            &dir,
            r#"
fn hooks() { ["sequencer"] }
fn arrange(ctx) {
    if ctx.pool_config.movies.anything != () { throw "expected unit"; }
    ["mov-1"]
}
"#,
        );
        let cat = catalog();
        let pools = vec![pool("movies", "item.type == \"movie\"")];
        let BlockBuild { ids, .. } = build(
            &cat,
            &pools,
            &[],
            None,
            &script,
            &GenerationState::empty(),
            0,
            test_env(),
            Window {
                from: 0,
                fill: None,
            },
        )
        .unwrap();
        assert_eq!(ids, vec!["mov-1"]);
    }

    /// `local_time()` resolves against the station's configured tz
    /// (`ScoreInputs::tz`), not UTC — the fact a daypart script depends on to
    /// place its pools against the local clock rather than whatever zone the
    /// generation happened to run in (#14, ADR 0004).
    #[test]
    fn local_time_resolves_in_the_stations_configured_tz() {
        let dir = tempfile::tempdir().unwrap();
        let script = write(
            &dir,
            r#"
fn hooks() { ["sequencer"] }
fn arrange(ctx) {
    let t = local_time(ctx.window.from);
    if t.weekday != "mon" { throw "weekday wrong: " + t.weekday; }
    if t.hour != 7 { throw "hour wrong: " + t.hour; }
    if t.minute != 0 { throw "minute wrong: " + t.minute; }
    ["mov-1"]
}
"#,
        );
        let cat = catalog();
        let pools = vec![pool("movies", "item.type == \"movie\"")];
        let chicago = crate::tz::parse("America/Chicago").unwrap();
        // 2026-04-13 12:00 UTC is a Monday, and equals 07:00 CDT — the same
        // instant `crate::tz`'s own tests anchor on.
        let from = time::macros::datetime!(2026-04-13 12:00:00 UTC).unix_timestamp();
        let inputs = crate::score::ScoreInputs {
            tz: Some(chicago),
            ..Default::default()
        };
        let env = crate::score::ScoreEnv {
            inputs: &inputs,
            base_dir: std::path::Path::new("."),
        };
        let BlockBuild { ids, .. } = build(
            &cat,
            &pools,
            &[],
            None,
            &script,
            &GenerationState::empty(),
            0,
            env,
            Window { from, fill: None },
        )
        .unwrap();
        assert_eq!(ids, vec!["mov-1"]);
    }
}

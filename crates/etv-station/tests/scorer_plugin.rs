//! Acceptance test for plugin scoring (#74): a pool whose items come from a
//! Rhai script instead of a CEL expression.
//!
//! Proves the four things the issue asks for — the plugin drives the ordering
//! and the station computes no score of its own; a different script produces a
//! different channel with no change to etv-station; the watch history and the
//! recently-aired tail reach the script as inputs; and a plugin pool composes
//! with the pattern machinery (`take`, `advance`, rotation) exactly as a CEL
//! pool does.

use std::path::{Path, PathBuf};

use etv_station::catalog::{Catalog, Entry, EntrySource, Source, TagNs};
use etv_station::config::{
    Advance, BlockInclude, ChannelConfig, Mode, PatternStep, Pool, RuleConfig,
};
use etv_station::resolve::resolve_channel_with_resume;
use etv_station::resume::GenerationState;
use etv_station::score::{ScoreInputs, WatchEvent};

/// Four movies and one two-episode show, all locally sourced so resolution
/// reaches a playable item.
fn catalog() -> Catalog {
    let cat = Catalog::open_in_memory().unwrap();
    let add = |id: &str, kind: &str, title: &str, year: i64, show: Option<(i64, i64)>| {
        let mut e = Entry::new(id, kind, title, Source::Plex);
        e.year = Some(year);
        if let Some((season, episode)) = show {
            e.show_id = Some("show:pilot".to_string());
            e.show = Some("Pilot".to_string());
            e.season = Some(season);
            e.episode = Some(episode);
        }
        cat.upsert_entry(&e).unwrap();
        cat.add_source(&EntrySource {
            source: Source::LocalFs,
            source_id: format!("fs-{id}"),
            entry_id: id.to_string(),
            playback_path: format!("/media/{id}.mkv"),
            last_seen: None,
        })
        .unwrap();
    };

    add("mov-a", "movie", "Arrival", 2016, None);
    add("mov-b", "movie", "Blade Runner", 1982, None);
    add("mov-c", "movie", "Contact", 1997, None);
    add("mov-d", "movie", "Dune", 2021, None);
    add("ep-1", "episode", "Pilot S1E1", 2020, Some((1, 1)));
    add("ep-2", "episode", "Pilot S1E2", 2020, Some((1, 2)));
    cat.add_tag("mov-b", TagNs::Genre, "Sci-Fi").unwrap();
    cat
}

fn write_plugin(dir: &tempfile::TempDir, name: &str, body: &str) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, body).unwrap();
    path
}

/// A one-pool, one-step pattern channel whose pool draws from `plugin`.
fn plugin_channel(plugin: &Path, take: usize, cycles: usize) -> ChannelConfig {
    ChannelConfig {
        name: None,
        scoring: None,
        window_days: 1,
        chunk_hours: 6,
        roll_interval: std::time::Duration::from_secs(60),
        retention_days: 1,
        seed: Some(7),
        overlay: None,
        rule: RuleConfig {
            blocks: vec![BlockInclude {
                block: None,
                program: None,
                duplicates: None,
                constraints: None,
                entries: Vec::new(),
                filter: None,
                mode: Mode::All,
                order: Default::default(),
                pools: vec![Pool {
                    name: "foryou".into(),
                    expr: None,
                    plugin: Some(plugin.to_path_buf()),
                    order: None,
                    select: Default::default(),
                    rotate: Default::default(),
                    advance: Advance::Restart,
                    on_short: Default::default(),
                }],
                pattern: vec![PatternStep {
                    pool: "foryou".into(),
                    take,
                    chance: 1.0,
                }],
                cycles: Some(cycles),
            }],
        },
    }
}

fn resolve_with(cfg: &ChannelConfig, cat: &Catalog, inputs: ScoreInputs) -> Vec<String> {
    let state = GenerationState::default();
    let (items, _) = resolve_channel_with_resume(
        cfg,
        Path::new("foryou.yaml"),
        &[],
        None,
        Some(cat),
        &state,
        &inputs,
    )
    .unwrap();
    items.into_iter().map(|i| i.id).collect()
}

/// Ranking by `year` descending is a judgment that lives entirely in the
/// script — nothing in etv-station knows what "newest first" means for a
/// scorer, and no score value ever crosses back over the boundary.
#[test]
fn the_script_decides_the_order() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_plugin(
        &dir,
        "newest.rhai",
        r#"
fn sources() { #{ movies: `item.type == "movie"` } }
fn pick(ctx) {
    let rows = [];
    for item in ctx.sets.movies { rows.push(item); }
    rows.sort(|a, b| if a.year > b.year { -1 } else if a.year < b.year { 1 } else { 0 });
    let out = [];
    for r in rows { out.push(r.entry_id); }
    out
}
"#,
    );
    let got = resolve_with(
        &plugin_channel(&p, 1, 4),
        &catalog(),
        ScoreInputs::default(),
    );
    assert_eq!(got, vec!["mov-d", "mov-a", "mov-c", "mov-b"]);
}

/// The interface boundary holds: swapping the script changes the channel, and
/// the only thing that differs between these two runs is the file on disk.
#[test]
fn swapping_the_plugin_swaps_the_channel() {
    let dir = tempfile::tempdir().unwrap();
    let cat = catalog();
    let newest = write_plugin(
        &dir,
        "newest.rhai",
        r#"
fn sources() { #{ movies: `item.type == "movie"` } }
fn pick(ctx) {
    let rows = [];
    for item in ctx.sets.movies { rows.push(item); }
    rows.sort(|a, b| if a.year > b.year { -1 } else if a.year < b.year { 1 } else { 0 });
    let out = [];
    for r in rows { out.push(r.entry_id); }
    out
}
"#,
    );
    let scifi = write_plugin(
        &dir,
        "scifi.rhai",
        r#"
fn sources() { #{ movies: `item.type == "movie"` } }
fn pick(ctx) {
    let out = [];
    for item in ctx.sets.movies {
        if item.genres.contains("Sci-Fi") { out.push(item.entry_id); }
    }
    out
}
"#,
    );

    let a = resolve_with(&plugin_channel(&newest, 1, 4), &cat, ScoreInputs::default());
    let b = resolve_with(&plugin_channel(&scifi, 1, 1), &cat, ScoreInputs::default());
    assert_eq!(b, vec!["mov-b"], "the second script picks by tag");
    assert_ne!(a, b, "same station, same catalog, different plugin");
}

/// Watch history and the recently-aired tail are inputs, not catalog fields: a
/// channel cannot express either in CEL, and the plugin is the only thing that
/// decides what they mean.
#[test]
fn history_and_recent_airings_reach_the_script() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_plugin(
        &dir,
        "taste.rhai",
        r#"
fn sources() { #{ movies: `item.type == "movie"` } }
fn pick(ctx) {
    let watched = [];
    for e in ctx.history { watched.push(e.entry_id); }
    let out = [];
    // Anything watched lately, minus anything this channel just aired.
    for item in ctx.sets.movies {
        if watched.contains(item.entry_id) && !ctx.recent.contains(item.entry_id) {
            out.push(item.entry_id);
        }
    }
    out
}
"#,
    );
    let inputs = ScoreInputs {
        target_count: 10,
        history: vec![
            WatchEvent {
                entry_id: "mov-a".into(),
                watched_at: 1000,
            },
            WatchEvent {
                entry_id: "mov-c".into(),
                watched_at: 2000,
            },
        ],
        recent: vec!["mov-c".into()],
        now: 3000,
    };
    let got = resolve_with(&plugin_channel(&p, 1, 1), &catalog(), inputs);
    assert_eq!(
        got,
        vec!["mov-a"],
        "watched and not recently aired is the only survivor"
    );
}

/// A plugin pool is a pool: the pattern's `take` paces it and rotation groups
/// episodes by `show_id` exactly as it does for a CEL-resolved set.
#[test]
fn a_plugin_pool_feeds_the_pattern_like_any_other() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_plugin(
        &dir,
        "everything.rhai",
        r#"
fn sources() { #{ all: `item.year > 0` } }
fn pick(ctx) {
    let out = [];
    for item in ctx.sets.all { out.push(item.entry_id); }
    out
}
"#,
    );
    // take = 2 with the default rotate = "visit": one visit draws two items
    // from one series, and the two episodes share a show_id, so they are one
    // series while each movie is its own.
    let got = resolve_with(
        &plugin_channel(&p, 2, 1),
        &catalog(),
        ScoreInputs::default(),
    );
    assert_eq!(got.len(), 2, "take caps the visit");
    assert!(
        got.iter().all(|id| id.starts_with("mov-")) || got == vec!["ep-1", "ep-2"],
        "a visit stays inside one series: got {got:?}"
    );
}

/// The committed worked example runs. Without this, `examples/plugins/
/// taste-engine.rhai` is documentation that nothing proves still compiles —
/// and a scorer only fails at generation time, on a running channel.
#[test]
fn the_committed_example_plugin_runs() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/plugins/taste-engine.rhai")
        .canonicalize()
        .expect("examples/plugins/taste-engine.rhai must exist");

    let cat = catalog();
    let inputs = ScoreInputs {
        target_count: 3,
        history: vec![WatchEvent {
            entry_id: "mov-a".into(),
            watched_at: 900_000,
        }],
        // mov-d aired most recently, so the example's replay TTL drops it.
        recent: vec!["mov-d".into()],
        now: 900_000 + 3600,
    };
    let picked = etv_station::score::run(&cat, &path, &inputs, "movies").unwrap();

    assert!(
        !picked.contains(&"mov-d".to_string()),
        "a just-aired item must be suppressed: {picked:?}"
    );
    assert_eq!(
        picked.first().map(String::as_str),
        Some("mov-a"),
        "the freshly-watched item should rank first: {picked:?}"
    );
}

/// A relative `plugin:` path means what it means relative to the channel config
/// file, not to the daemon's working directory. Without this, a config that
/// works when launched from the repo root breaks under systemd or Docker, and
/// the failure is a file-not-found at generation time on a live channel.
#[test]
fn a_relative_plugin_path_resolves_against_the_channel_config() {
    let dir = tempfile::tempdir().unwrap();
    let plugins = dir.path().join("plugins");
    std::fs::create_dir(&plugins).unwrap();
    std::fs::write(
        plugins.join("pick-one.rhai"),
        r#"
fn sources() { #{ movies: `item.type == "movie"` } }
fn pick(ctx) { ["mov-a"] }
"#,
    )
    .unwrap();

    // The pool names the path the way a config author would — relative, and
    // relative to the channel file, which lives one level above `plugins/`.
    let mut cfg = plugin_channel(Path::new("plugins/pick-one.rhai"), 1, 1);
    cfg.rule.blocks[0].pools[0].plugin = Some(PathBuf::from("plugins/pick-one.rhai"));

    let state = GenerationState::default();
    let (items, _) = resolve_channel_with_resume(
        &cfg,
        &dir.path().join("foryou.yaml"),
        &[],
        None,
        Some(&catalog()),
        &state,
        &ScoreInputs::default(),
    )
    .expect("a relative plugin path must resolve against the channel config's directory");

    assert_eq!(
        items.into_iter().map(|i| i.id).collect::<Vec<_>>(),
        vec!["mov-a"]
    );
}

/// The station refuses a plugin that picks nothing rather than quietly emitting
/// a short channel.
#[test]
fn a_plugin_that_picks_nothing_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_plugin(
        &dir,
        "empty.rhai",
        "fn sources() { #{} }\nfn pick(ctx) { [] }\n",
    );
    let state = GenerationState::default();
    let err = resolve_channel_with_resume(
        &plugin_channel(&p, 1, 1),
        Path::new("foryou.yaml"),
        &[],
        None,
        Some(&catalog()),
        &state,
        &Default::default(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("picked nothing"), "got {err}");
}

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
use etv_station::errors::ConfigError;
use etv_station::resolve::{ResolvedItem, resolve_channel_with_resume};
use etv_station::resume::GenerationState;
use etv_station::score::{ScoreInputs, WatchEvent};
use time::macros::datetime;

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

/// A chunk boundary in UTC, arbitrary but fixed (#270): chunks are cut on
/// aligned boundaries (00:00, 06:00, 12:00, 18:00 for a 6-hour chunk), so how
/// many files a short emit window produces depends on distance to the next
/// boundary, not on the window's own width. The two tests below build their
/// `start` from this fixed instant rather than `now_utc()`, so the file count
/// they assert is a property of the instant they chose, not of when the
/// suite happens to run.
const CHUNK_BOUNDARY: time::OffsetDateTime = datetime!(2026-04-13 00:00:00 UTC);

/// The chunk width the two tests below emit against. The loop bound in
/// `assert_one_file_across_the_chunk` and the `chunk_hours` it hands
/// `emit_window` are both this number, so the two can't drift apart.
const CHUNK_HOURS: u32 = 6;

/// Runs `emit_window` at fixed starts spread one hour apart across one chunk
/// (#270), asserting exactly one chunk file at each — proof that the file
/// count is a property of the fixed start, not a lucky roll of whatever the
/// wall clock read when the suite ran. Each start gets its own output
/// directory: every one falls in the same chunk (starting 00:00), and
/// `emit_window` folds new items onto whatever a chunk file already holds, so
/// one shared directory would pile each run's items onto the last rather than
/// prove anything. Returns the last run's written files, for the caller's
/// content assertions.
async fn assert_one_file_across_the_chunk(
    dir: &tempfile::TempDir,
    rule: &etv_station::rule::Sequential<'_>,
    tz: &'static time_tz::Tz,
) -> Vec<PathBuf> {
    let mut written = Vec::new();
    for hours in 0..i64::from(CHUNK_HOURS) {
        let start = CHUNK_BOUNDARY + time::Duration::hours(hours);
        let out_dir = dir.path().join(format!("output-{hours}"));
        written = etv_station::emit::emit_window(
            &out_dir,
            rule,
            start,
            tz,
            CHUNK_HOURS,
            start,
            start + rule.total_duration(),
        )
        .await
        .unwrap();
        assert_eq!(
            written.len(),
            1,
            "one chunk file: the window opening at {start} ends inside the same chunk"
        );
    }
    written
}

/// A one-pool, one-step pattern channel whose pool draws from `plugin`.
fn plugin_channel(plugin: &Path, take: usize, cycles: usize) -> ChannelConfig {
    ChannelConfig {
        name: None,
        display_name: None,
        guide: None,
        scoring: None,
        anchor: None,
        window_days: 1,
        chunk_hours: 6,
        roll_interval: std::time::Duration::from_secs(60),
        retention_days: 1,
        seed: Some(7),
        overlay: None,
        groups: Vec::new(),
        rule: RuleConfig {
            blocks: vec![BlockInclude {
                block: None,
                program: None,
                guide: None,
                duplicates: None,
                constraints: None,
                entries: Vec::new(),
                fallback: None,
                filter: None,
                mode: Mode::All,
                order: Default::default(),
                pools: vec![Pool {
                    name: "foryou".into(),
                    expr: None,
                    plugin: Some(plugin.to_path_buf()),
                    sources: None,
                    groups: Vec::new(),
                    order: None,
                    bucket_order: None,
                    group_by: Default::default(),
                    select: Default::default(),
                    rotate: Default::default(),
                    advance: Advance::Restart,
                    on_short: Default::default(),
                    constraints: None,
                    config: None,
                    // Every fixture script in this file reads `ctx.sets` /
                    // `ctx.history` unconditionally (#167 predates this file),
                    // so this helper grants both for every test that uses it.
                    capabilities: vec!["catalog_read".into(), "watch_history".into()],
                    datastores: Vec::new(),
                }],
                pattern: vec![PatternStep {
                    pool: "foryou".into(),
                    take: etv_station::config::Take::Count(take),
                    from: Default::default(),
                    chance: 1.0,
                }],
                cycles: Some(cycles),
                sequencer: None,
            }],
        },
    }
}

/// Every resolved airing for `cfg` — what a test that reads a field other
/// than `id` needs, and what [`resolve_with`] narrows for the tests that only
/// care about the order.
fn resolve_items(
    cfg: &ChannelConfig,
    cat: &Catalog,
    inputs: &ScoreInputs,
) -> Result<Vec<ResolvedItem>, ConfigError> {
    let state = GenerationState::default();
    resolve_channel_with_resume(
        cfg,
        Path::new("foryou.yaml"),
        &[],
        None,
        Some(cat),
        &state,
        inputs,
        None,
        time::OffsetDateTime::now_utc(),
    )
    .map(|(items, _)| items)
}

fn resolve_with(cfg: &ChannelConfig, cat: &Catalog, inputs: ScoreInputs) -> Vec<String> {
    resolve_items(cfg, cat, &inputs)
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect()
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
        history: [
            WatchEvent {
                entry_id: "mov-a".into(),
                watched_at: 1000,
                watcher: None,
            },
            WatchEvent {
                entry_id: "mov-c".into(),
                watched_at: 2000,
                watcher: None,
            },
        ]
        .into(),
        recent: vec!["mov-c".into()],
        now: 3000,
        tz: None,
        account_id: None,
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
        None,
        time::OffsetDateTime::now_utc(),
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
        None,
        time::OffsetDateTime::now_utc(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("picked nothing"), "got {err}");
}

// ---- record return shape (#166) --------------------------------------------

/// One record carrying a `metadata` blob and one bare id — the two return
/// shapes side by side in a single `pick()`.
const MIXED_SHAPES: &str = r#"
fn sources() { #{ movies: `item.type == "movie"` } }
fn pick(ctx) {
    [
        #{ entry_id: "mov-a", metadata: #{ reason: "won an Oscar" } },
        "mov-b",
    ]
}
"#;

/// The whole point of the widening: a plugin can mix bare ids with records
/// naming a `metadata` blob, and only the records' airings carry one all the
/// way to `ResolvedItem::metadata` — the field `crate::rule::build_playout_item`
/// copies onto `PlayoutItem::metadata` for the emitted playout JSON. An id
/// with no record attaches nothing, which is what keeps a plugin that only
/// ever returns bare ids producing byte-identical output to before this
/// existed.
#[test]
fn a_records_metadata_reaches_the_resolved_item_and_a_bare_id_carries_none() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_plugin(&dir, "metadata.rhai", MIXED_SHAPES);
    let items = resolve_items(&plugin_channel(&p, 2, 1), &catalog(), &Default::default()).unwrap();

    let a = items.iter().find(|i| i.id == "mov-a").expect("mov-a airs");
    assert_eq!(
        a.metadata,
        Some(serde_json::json!({ "reason": "won an Oscar" })),
        "the record's metadata must reach the resolved item"
    );
    let b = items.iter().find(|i| i.id == "mov-b").expect("mov-b airs");
    assert!(
        b.metadata.is_none(),
        "a bare id must attach no metadata — the widening is additive"
    );
}

/// A non-finite float anywhere inside a record's `metadata` fails the whole
/// channel resolution and names the key, in the same wording `Pool::config`
/// uses (#130) — the metadata carrier refuses the value it cannot hold rather
/// than let it silently reach the playout JSON as unit.
#[test]
fn a_non_finite_float_in_metadata_fails_channel_resolution_and_names_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_plugin(
        &dir,
        "bad_metadata.rhai",
        r#"
fn sources() { #{ movies: `item.type == "movie"` } }
fn pick(ctx) { [#{ entry_id: "mov-a", metadata: #{ weight: 1.0 / 0.0 } }] }
"#,
    );
    let err = resolve_items(&plugin_channel(&p, 1, 1), &catalog(), &Default::default())
        .expect_err("a non-finite float must fail the resolve");
    let msg = err.to_string();
    assert!(msg.contains("weight"), "got {msg}");
    assert!(msg.contains("finite"), "got {msg}");
    assert!(msg.contains("mov-a"), "got {msg}");
}

/// The acceptance criterion, driven end to end through the real emission
/// path: resolve a plugin pool's record through `resolve_channel_with_resume`,
/// lay the result with the real `Sequential` rule (`crate::rule::build_playout_item`
/// is what copies `ResolvedItem::metadata` onto `PlayoutItem::metadata`), and
/// write it with the real `emit::emit_window` — then read the actual bytes
/// back off disk. `mov-a`'s airing carries the blob; `mov-b`'s carries no
/// `metadata` key at all, which is what keeps a plugin that only ever
/// returns bare ids producing byte-identical playout JSON to before #166.
#[tokio::test]
async fn a_records_metadata_is_readable_in_the_emitted_playout_json() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_plugin(&dir, "metadata.rhai", MIXED_SHAPES);
    let items = resolve_items(&plugin_channel(&p, 2, 1), &catalog(), &Default::default()).unwrap();

    // Fixed durations rather than probed ones — this proves the metadata
    // plumbing through `Sequential`/`emit_window`, not ffprobe against fake
    // paths, which is a different concern this test does not own.
    let durations = vec![std::time::Duration::from_secs(60); items.len()];
    let rule = etv_station::rule::Sequential::new(&items, &durations);
    let tz = etv_station::tz::parse("UTC").unwrap();

    // The content assertions below read back the last of the fixed-start runs (#270).
    let written = assert_one_file_across_the_chunk(&dir, &rule, tz).await;
    let bytes = tokio::fs::read(&written[0]).await.unwrap();
    let playout: ersatztv_playout::playout::Playout = serde_json::from_slice(&bytes).unwrap();
    let a = playout
        .items
        .iter()
        .find(|i| i.id == "mov-a")
        .expect("mov-a's airing is in the emitted file");
    assert_eq!(
        a.metadata,
        Some(serde_json::json!({ "reason": "won an Oscar" })),
        "the record's metadata must be readable in the emitted playout JSON"
    );
    let b = playout
        .items
        .iter()
        .find(|i| i.id == "mov-b")
        .expect("mov-b's airing is in the emitted file");
    assert!(
        b.metadata.is_none(),
        "a bare id's airing must carry no metadata key at all"
    );

    // The raw bytes make the same point the struct assertion does, in the
    // words of the acceptance criterion: the key is present once, for the
    // one airing that earned it.
    let raw = String::from_utf8(bytes).unwrap();
    assert_eq!(raw.matches("\"metadata\"").count(), 1, "raw JSON: {raw}");
}

// ---- per-entry take override reaches the draw (#173) -----------------------

/// A two-episode show whose first episode's record overrides the step's own
/// `take` down to 1, so `ep-2` never airs.
const TAKE_OVERRIDE: &str = r#"
fn sources() { #{} }
fn pick(ctx) { [#{ entry_id: "ep-1", take: 1 }, "ep-2"] }
"#;

/// The acceptance criterion, driven through the real resolve path: a
/// plugin's per-entry `take` override caps what airs for that series,
/// beating the step's own `take`. The step asks for 2 — the show's whole
/// two-episode run — but `ep-1`'s record overrides it to 1, so `ep-2` never
/// airs this visit.
#[test]
fn a_take_override_caps_the_series_through_the_real_resolve_path() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_plugin(&dir, "sampler.rhai", TAKE_OVERRIDE);
    let got = resolve_with(
        &plugin_channel(&p, 2, 1),
        &catalog(),
        ScoreInputs::default(),
    );
    assert_eq!(
        got,
        vec!["ep-1"],
        "the override, not the step's take=2, decided how many aired"
    );
}

/// The same override, driven all the way to the emitted playout JSON on
/// disk — exactly as the `metadata` acceptance test above proves #166's
/// carrier, this proves #173's spend of it: `ep-2` must never reach disk,
/// because the visit that would have carried it stopped at the override.
#[tokio::test]
async fn a_take_override_caps_what_the_emitted_playout_json_holds() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_plugin(&dir, "sampler.rhai", TAKE_OVERRIDE);
    let items = resolve_items(&plugin_channel(&p, 2, 1), &catalog(), &Default::default()).unwrap();
    assert_eq!(
        items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
        vec!["ep-1"],
        "the override, not the step's take=2, decided how many aired"
    );

    let durations = vec![std::time::Duration::from_secs(60); items.len()];
    let rule = etv_station::rule::Sequential::new(&items, &durations);
    let tz = etv_station::tz::parse("UTC").unwrap();

    // The content assertions below read back the last of the fixed-start runs (#270).
    let written = assert_one_file_across_the_chunk(&dir, &rule, tz).await;
    let bytes = tokio::fs::read(&written[0]).await.unwrap();
    let playout: ersatztv_playout::playout::Playout = serde_json::from_slice(&bytes).unwrap();
    let ids: Vec<&str> = playout.items.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["ep-1"],
        "ep-2 must never reach disk: the override cut the visit short"
    );
}

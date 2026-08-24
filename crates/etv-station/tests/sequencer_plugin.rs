//! Acceptance test for the sequencer hook (#169): a block names a plugin
//! script that draws its final timeline from the block's resolved pools,
//! instead of walking `pattern`.
//!
//! Proves the committed worked example (`examples/plugins/foryou-sequencer.rhai`)
//! actually runs, and that its two pools genuinely advance differently under
//! one script — a `spotlight` pool that resumes a show from the ledger's
//! cursor, and a `discovery` pool that restarts fresh every generation — the
//! case a single `pattern` block cannot express.

use std::path::{Path, PathBuf};

use etv_station::catalog::{Catalog, Entry, EntrySource, Source};
use etv_station::config::{
    BlockInclude, ChannelConfig, GroupBy, Mode, Order, Pool, RuleConfig, Select,
};
use etv_station::resolve::resolve_channel_with_resume;
use etv_station::resume::GenerationState;
use etv_station::score::{ScoreEnv, ScoreInputs};
use etv_station::sequence::{Window, build};
use time::macros::datetime;

/// One two-episode show (`spotlight`'s pool) and four movies (`discovery`'s
/// pool), all locally sourced so resolution reaches a playable item.
fn catalog() -> Catalog {
    let cat = Catalog::open_in_memory().unwrap();
    let add_movie = |id: &str, title: &str, year: i64| {
        let mut e = Entry::new(id, "movie", title, Source::Plex);
        e.year = Some(year);
        cat.upsert_entry(&e).unwrap();
        cat.add_source(&EntrySource {
            source: Source::LocalFs,
            source_id: format!("fs-{id}"),
            entry_id: id.to_string(),
            playback_path: format!("/media/{id}.mkv"),
            last_seen: None,
            missing_since: None,
        })
        .unwrap();
    };
    add_movie("mov-a", "Arrival", 2016);
    add_movie("mov-b", "Blade Runner", 1982);
    add_movie("mov-c", "Contact", 1997);
    add_movie("mov-d", "Dune", 2021);

    let add_episode = |id: &str, season: i64, episode: i64| {
        let mut e = Entry::new(id, "episode", format!("Episode {episode}"), Source::Plex);
        e.show_id = Some("show:pilot".to_string());
        e.show = Some("Pilot".to_string());
        e.season = Some(season);
        e.episode = Some(episode);
        cat.upsert_entry(&e).unwrap();
        cat.add_source(&EntrySource {
            source: Source::LocalFs,
            source_id: format!("fs-{id}"),
            entry_id: id.to_string(),
            playback_path: format!("/media/{id}.mkv"),
            last_seen: None,
            missing_since: None,
        })
        .unwrap();
    };
    for n in 1..=4 {
        add_episode(&format!("ep-{n}"), 1, n);
    }
    cat
}

fn discovery_pool() -> Pool {
    Pool {
        name: "discovery".into(),
        expr: Some("item.type == \"movie\"".into()),
        plugin: None,
        sources: None,
        groups: Vec::new(),
        order: Some(Order::parse("year:desc").unwrap()),
        bucket_order: None,
        group_by: GroupBy::Show,
        select: Select::default(),
        rotate: Default::default(),
        advance: Default::default(),
        on_short: Default::default(),
        constraints: None,
        config: None,
        // Both pools are `expr`, so no plugin is reached and no capability
        // grant is needed (#167).
        capabilities: Vec::new(),
        datastores: Vec::new(),
        guide: None,
    }
}

fn spotlight_pool() -> Pool {
    Pool {
        name: "spotlight".into(),
        expr: Some("item.type == \"episode\"".into()),
        plugin: None,
        sources: None,
        groups: Vec::new(),
        order: Some(Order::parse("season:asc,episode:asc").unwrap()),
        bucket_order: None,
        group_by: GroupBy::Show,
        select: Select::default(),
        rotate: Default::default(),
        advance: Default::default(),
        on_short: Default::default(),
        constraints: None,
        config: None,
        // Both pools are `expr`, so no plugin is reached and no capability
        // grant is needed (#167).
        capabilities: Vec::new(),
        datastores: Vec::new(),
        guide: None,
    }
}

fn script_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/plugins/foryou-sequencer.rhai")
        .canonicalize()
        .expect("examples/plugins/foryou-sequencer.rhai must exist")
}

fn test_env(inputs: &ScoreInputs) -> ScoreEnv<'_> {
    ScoreEnv {
        inputs,
        base_dir: Path::new("."),
    }
}

/// The committed example runs, and it demonstrates the headline capability:
/// `discovery` restarts (newest-first, every generation, ignoring the
/// ledger) while `spotlight` resumes the show from its cursor — one script,
/// two advance policies, interleaved 1 spotlight then 2 discovery.
#[test]
fn the_committed_example_interleaves_a_resuming_and_a_restarting_pool() {
    let cat = catalog();
    let pools = vec![spotlight_pool(), discovery_pool()];
    let inputs = ScoreInputs::default();

    let mut state = GenerationState::empty();
    state.cursor.insert("show:pilot".into(), "ep-1".to_string());

    let (ids, resume, _, _) = build(
        &cat,
        &pools,
        &[],
        None,
        &script_path(),
        &state,
        0,
        test_env(&inputs),
        Window {
            from: 0,
            fill: None,
        },
    )
    .unwrap();

    // Spotlight resumed after ep-1 (the ledger cursor); discovery restarted
    // at its newest-first order, unaffected by any cursor at all.
    assert_eq!(
        ids,
        vec!["ep-2", "mov-d", "mov-a", "ep-3", "mov-c", "mov-b", "ep-4"],
        "got {ids:?}"
    );

    // The station derived new resume state from what was actually drawn.
    assert!(resume.contains_key("spotlight"));
    assert!(resume.contains_key("discovery"));
}

/// With no ledger cursor at all, `spotlight` starts at its first episode —
/// the same "start at the top" meaning a freshly-resolved pool always
/// carries.
#[test]
fn the_committed_example_starts_spotlight_at_the_top_with_no_cursor() {
    let cat = catalog();
    let pools = vec![spotlight_pool(), discovery_pool()];
    let inputs = ScoreInputs::default();

    let (ids, _, _, _) = build(
        &cat,
        &pools,
        &[],
        None,
        &script_path(),
        &GenerationState::empty(),
        0,
        test_env(&inputs),
        Window {
            from: 0,
            fill: None,
        },
    )
    .unwrap();

    assert_eq!(ids[0], "ep-1", "no cursor yet, so spotlight opens at S1E1");
}

// ---- plugin record shape reaches a sequencer block's output (#166 gap, #201) ----

fn write_plugin(dir: &tempfile::TempDir, name: &str, body: &str) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, body).unwrap();
    path
}

/// A chunk boundary in UTC, arbitrary but fixed (#361): chunks are cut on
/// aligned boundaries (00:00, 06:00, 12:00, 18:00 for a 6-hour chunk), so how
/// many files a short emit window produces depends on distance to the next
/// boundary, not on the window's own width. The generation instant below is
/// built from this fixed instant rather than `now_utc()`, so the file count
/// the test asserts is a property of the instant it chose, not of when the
/// suite happens to run.
const CHUNK_BOUNDARY: time::OffsetDateTime = datetime!(2026-04-13 00:00:00 UTC);

/// The chunk width the test below emits against. The loop bound in
/// `assert_one_file_across_the_chunk` and the `chunk_hours` it hands
/// `emit_window` are both this number, and it must equal
/// `forwarding_sequencer_channel`'s `chunk_hours: 6` below, so the two can't
/// drift apart.
const CHUNK_HOURS: u32 = 6;

/// Runs `emit_window` at fixed starts spread one hour apart across one chunk
/// (#361), asserting exactly one chunk file at each — proof that the file
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

/// A `plugin:` pool feeding a sequencer block — same pool contract as a
/// pattern block's, only the block draws its final order from `arrange()`
/// instead of a `pattern:` template.
fn foryou_pool(plugin: &Path) -> Pool {
    Pool {
        name: "foryou".into(),
        expr: None,
        plugin: Some(plugin.to_path_buf()),
        sources: None,
        groups: Vec::new(),
        order: None,
        bucket_order: None,
        group_by: GroupBy::Show,
        select: Select::default(),
        rotate: Default::default(),
        advance: Default::default(),
        on_short: Default::default(),
        constraints: None,
        config: None,
        // The fixture scripts below never read `ctx.sets`/`ctx.history`, so no
        // capability grant is needed (#167).
        capabilities: Vec::new(),
        datastores: Vec::new(),
        guide: None,
    }
}

/// A one-pool sequencer block whose `arrange()` just forwards the pool's
/// resolved ids in order — the minimum a sequencer needs to exist, so the
/// test is only exercising the plugin-record plumbing, not any arrangement
/// logic.
fn forwarding_sequencer_channel(sequencer: &Path, pool: Pool) -> ChannelConfig {
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
                overlay: None,
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
                pools: vec![pool],
                pattern: Vec::new(),
                cycles: None,
                sequencer: Some(sequencer.to_path_buf()),
            }],
        },
    }
}

const FORWARD_ARRANGE: &str = r#"
fn hooks() { ["sequencer"] }
fn arrange(ctx) {
    let out = [];
    for item in ctx.pools.foryou { out.push(item.entry_id); }
    out
}
"#;

/// One record carrying a `metadata` blob and one bare id — the same mixed
/// shape `tests/scorer_plugin.rs` proves reaches a `pattern:` block's output
/// (#166), now proved for a `sequencer:` block. Before #201 this metadata
/// vanished silently: `sequence::build` never read `ScoreCache::picked_extras`
/// back, so `mov-a`'s `ResolvedItem::metadata` was always `None` regardless
/// of what the plugin returned.
const MIXED_SHAPES: &str = r#"
fn sources() { #{ movies: `item.type == "movie"` } }
fn pick(ctx) {
    [
        #{ entry_id: "mov-a", metadata: #{ reason: "won an Oscar" } },
        "mov-b",
    ]
}
"#;

/// The acceptance criterion: a `sequencer:` block whose plugin pool returns a
/// record with `metadata` has that blob land on the resolved airing, exactly
/// as a `pattern:` block already does — and a bare id's airing carries none,
/// which is what keeps a sequencer plugin that only ever returns bare ids
/// (`foryou-sequencer.rhai`) producing byte-identical output to before #201.
#[test]
fn a_records_metadata_reaches_the_resolved_item_on_a_sequencer_block() {
    let dir = tempfile::tempdir().unwrap();
    let plugin = write_plugin(&dir, "metadata.rhai", MIXED_SHAPES);
    let arrange = write_plugin(&dir, "arrange.rhai", FORWARD_ARRANGE);
    let cfg = forwarding_sequencer_channel(&arrange, foryou_pool(&plugin));

    let state = GenerationState::default();
    let (items, _) = resolve_channel_with_resume(
        &cfg,
        Path::new("foryou.yaml"),
        &[],
        None,
        Some(&catalog()),
        &state,
        &ScoreInputs::default(),
        None,
        CHUNK_BOUNDARY,
    )
    .unwrap();

    let a = items.iter().find(|i| i.id == "mov-a").expect("mov-a airs");
    assert_eq!(
        a.metadata,
        Some(serde_json::json!({ "reason": "won an Oscar" })),
        "the record's metadata must reach the resolved item on a sequencer block"
    );
    let b = items.iter().find(|i| i.id == "mov-b").expect("mov-b airs");
    assert!(
        b.metadata.is_none(),
        "a bare id must attach no metadata — the widening is additive"
    );
}

/// The acceptance criterion driven end to end through the real emission path
/// — the actual product surface etv-station exists to write: resolve a
/// sequencer block's plugin record through `resolve_channel_with_resume`, lay
/// the result with the real `Sequential` rule, and write it with the real
/// `emit::emit_window` — then read the actual bytes back off disk, exactly as
/// `tests/scorer_plugin.rs` proves for a `pattern:` block (#166). `mov-a`'s
/// airing carries the blob in the `{start}_{finish}.json` chunk file; `mov-b`'s
/// carries no `metadata` key at all.
#[tokio::test]
async fn a_records_metadata_is_readable_in_the_emitted_playout_json_for_a_sequencer_block() {
    let dir = tempfile::tempdir().unwrap();
    let plugin = write_plugin(&dir, "metadata.rhai", MIXED_SHAPES);
    let arrange = write_plugin(&dir, "arrange.rhai", FORWARD_ARRANGE);
    let cfg = forwarding_sequencer_channel(&arrange, foryou_pool(&plugin));

    let state = GenerationState::default();
    let (items, _) = resolve_channel_with_resume(
        &cfg,
        Path::new("foryou.yaml"),
        &[],
        None,
        Some(&catalog()),
        &state,
        &ScoreInputs::default(),
        None,
        CHUNK_BOUNDARY,
    )
    .unwrap();

    // Fixed durations rather than probed ones — this proves the metadata
    // plumbing through `Sequential`/`emit_window`, not ffprobe against fake
    // paths, which is a different concern this test does not own.
    let durations = vec![std::time::Duration::from_secs(60); items.len()];
    let rule = etv_station::rule::Sequential::new(&items, &durations);
    let tz = etv_station::tz::parse("UTC").unwrap();
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
        "the record's metadata must be readable in the emitted playout JSON for a sequencer block"
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

    let raw = String::from_utf8(bytes).unwrap();
    assert_eq!(raw.matches("\"metadata\"").count(), 1, "raw JSON: {raw}");
}

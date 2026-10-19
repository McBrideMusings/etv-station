//! `simulate::simulate` replays the daemon's generation loop with the airing
//! history carried forward, so a scorer's repeat suppression (`ctx.recent`)
//! takes effect between generations — which a single ranking walked with a
//! cursor cannot show.
//!
//! The plugin below ranks every title it has not seen in `ctx.recent` ahead of
//! every title it has, in id order. With the history fed forward, the schedule
//! visits each title once before any title repeats. With `ctx.recent` empty it
//! would return the same ranking every generation and open each one with `m0`.

use std::path::{Path, PathBuf};

use etv_station::catalog::{Catalog, Entry, EntrySource, Source};
use etv_station::config::{
    Advance, BlockInclude, ChannelConfig, Mode, NoRepeatWithin, PatternStep, Pool, RuleConfig,
    ScoringConfig, Take,
};
use etv_station::simulate::simulate;
use time::macros::datetime;

const TITLES: usize = 100;

fn catalog() -> Catalog {
    let cat = Catalog::open_in_memory().unwrap();
    for n in 0..TITLES {
        let id = format!("m{n}");
        let mut e = Entry::new(&id, "movie", &id, Source::Plex);
        // Half an hour each: without a measured length the resolver's window
        // bound counts an item as zero airtime and lays the whole pool.
        e.duration_ms = Some(30 * 60 * 1000);
        cat.upsert_entry(&e).unwrap();
        cat.add_source(&EntrySource {
            source: Source::LocalFs,
            source_id: format!("fs-{id}"),
            entry_id: id.clone(),
            playback_path: format!("/media/{id}.mkv"),
            last_seen: None,
            missing_since: None,
        })
        .unwrap();
    }
    cat
}

const RECENCY_PLUGIN: &str = r#"
fn hooks() { ["pool_provider"] }
fn capabilities() { ["catalog_read"] }
fn sources() { #{ movies: `item.type == "movie"` } }
fn pick(ctx) {
    let seen = #{};
    for id in ctx.recent { seen[id] = true; }
    let fresh = [];
    let stale = [];
    for item in ctx.sets.movies {
        if seen[item.entry_id] == () { fresh.push(item.entry_id); } else { stale.push(item.entry_id); }
    }
    let by_id = |a, b| if a < b { -1 } else if a > b { 1 } else { 0 };
    fresh.sort(by_id);
    stale.sort(by_id);
    let out = [];
    for id in fresh { out.push(id); }
    for id in stale { out.push(id); }
    #{ picks: out, workspace: () }
}
fn audit(ctx, picks, workspace) { #{} }
"#;

fn write_plugin(dir: &tempfile::TempDir) -> PathBuf {
    let path = dir.path().join("recency.rhai");
    std::fs::write(&path, RECENCY_PLUGIN).unwrap();
    path
}

fn channel(plugin: &Path) -> ChannelConfig {
    ChannelConfig {
        logo: None,
        temperature: None,
        subtitle: None,
        tracks: None,
        number: 1,
        name: "test".into(),
        display_name: None,
        guide: None,
        scoring: None,
        anchor: None,
        window_days: 1,
        chunk_hours: 6,
        roll_interval: std::time::Duration::from_secs(3600),
        retention_days: 1,
        seed: Some(etv_station::config::SeedSpec::Fixed(7)),
        overlay: None,
        annotate: None,
        hooks: Vec::new(),
        groups: Vec::new(),
        rule: RuleConfig {
            blocks: vec![BlockInclude {
                tracks: None,
                overlay: None,
                block: None,
                program: None,
                guide: None,
                duplicates: None,
                constraints: None,
                entries: Vec::new(),
                fallback: None,
                filter: None,
                slots: Vec::new(),
                select: None,
                weight: Default::default(),
                align: None,
                min_fill: None,
                fill: Vec::new(),
                mode: Mode::All,
                order: Default::default(),
                pools: vec![Pool {
                    seed: None,
                    temperature: None,
                    tracks: None,
                    label: None,
                    advisory: None,
                    name: "movies".into(),
                    expr: None,
                    plugin: Some(plugin.to_path_buf()),
                    sources: None,
                    profile: Vec::new(),
                    profile_files: Vec::new(),
                    ignore_keywords: Vec::new(),
                    groups: Vec::new(),
                    order: None,
                    bucket_order: None,
                    group_by: Default::default(),
                    select: Default::default(),
                    rotate: Default::default(),
                    advance: Advance::Restart,
                    cursor_scope: Default::default(),
                    on_short: Default::default(),
                    constraints: None,
                    config: None,
                    capabilities: vec!["catalog_read".into()],
                    datastores: Vec::new(),
                    guide: None,
                }],
                pattern: vec![PatternStep {
                    pool: "movies".into(),
                    take: Take::Count(1),
                    from: Default::default(),
                    chance: 1.0,
                }],
                cycles: None,
                sequencer: None,
            }],
            ..Default::default()
        },
    }
}

#[test]
fn history_carries_forward_so_no_title_repeats_before_every_title_has_aired() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = channel(&write_plugin(&dir));
    let cat = catalog();

    let sim = simulate(
        &cfg,
        Path::new("channel.yaml"),
        &cat,
        None,
        4,
        datetime!(2026-01-01 00:00 UTC),
        None,
    )
    .unwrap();

    assert!(
        sim.generations.len() >= 2,
        "four days of hourly ticks must need more than one generation, got {}",
        sim.generations.len()
    );

    let aired: Vec<&str> = sim
        .generations
        .iter()
        .flat_map(|g| g.airings.iter().map(|a| a.id.as_str()))
        .collect();
    let first_round: Vec<&str> = aired.iter().copied().take(TITLES).collect();
    let mut distinct = first_round.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(
        distinct.len(),
        TITLES,
        "the first {TITLES} airings should be {TITLES} different titles: {first_round:?}"
    );
}

#[test]
fn each_generation_is_asked_for_a_chunks_worth_of_items() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = channel(&write_plugin(&dir));
    cfg.scoring = Some(ScoringConfig {
        nominal_item_secs: 5400,
        ..ScoringConfig::default()
    });
    let cat = catalog();

    let sim = simulate(
        &cfg,
        Path::new("channel.yaml"),
        &cat,
        None,
        1,
        datetime!(2026-01-01 00:00 UTC),
        None,
    )
    .unwrap();

    // 6h chunk / 5400s nominal item = 4, the daemon's `target_count`.
    assert_eq!(sim.generations[0].target_count, 4);
}

/// Ranks by id alone, ignoring `ctx.recent`, so every generation opens with the
/// same title unless the pool's cursor or the seam's adjacency tail moves it.
const ID_ORDER_PLUGIN: &str = r#"
fn hooks() { ["pool_provider"] }
fn capabilities() { ["catalog_read"] }
fn sources() { #{ movies: `item.type == "movie" || item.type == "episode"` } }
fn pick(ctx) {
    let ids = [];
    for item in ctx.sets.movies { ids.push(item.entry_id); }
    ids.sort(|a, b| if a < b { -1 } else if a > b { 1 } else { 0 });
    #{ picks: ids, workspace: () }
}
fn audit(ctx, picks, workspace) { #{} }
"#;

fn write_id_order_plugin(dir: &tempfile::TempDir) -> PathBuf {
    let path = dir.path().join("id-order.rhai");
    std::fs::write(&path, ID_ORDER_PLUGIN).unwrap();
    path
}

fn add_local_file(cat: &Catalog, e: &Entry) {
    cat.upsert_entry(e).unwrap();
    cat.add_source(&EntrySource {
        source: Source::LocalFs,
        source_id: format!("fs-{}", e.entry_id),
        entry_id: e.entry_id.clone(),
        playback_path: format!("/media/{}.mkv", e.entry_id),
        last_seen: None,
        missing_since: None,
    })
    .unwrap();
}

fn airing_ids(sim: &etv_station::simulate::Simulation) -> Vec<String> {
    sim.generations
        .iter()
        .flat_map(|g| g.airings.iter().map(|a| a.id.clone()))
        .collect()
}

/// A pool on `advance: resume` continues a show where the previous generation
/// stopped. The ledger's cursor is keyed on the airing's `show_id`, which is
/// what the pool's series looks up; seven episodes against a generation that
/// is not a multiple of seven long makes a lookup miss restart at `e1`.
#[test]
fn advance_resume_continues_a_season_across_generations() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = channel(&write_id_order_plugin(&dir));
    cfg.rule.blocks[0].pools[0].advance = Advance::Resume;
    let cat = Catalog::open_in_memory().unwrap();
    const EPISODES: usize = 7;
    for n in 1..=EPISODES {
        let id = format!("e{n}");
        let mut e = Entry::new(&id, "episode", &id, Source::Plex);
        e.show_id = Some("show:x".into());
        e.season = Some(1);
        e.episode = Some(n as i64);
        e.duration_ms = Some(30 * 60 * 1000);
        add_local_file(&cat, &e);
    }

    let sim = simulate(
        &cfg,
        Path::new("channel.yaml"),
        &cat,
        None,
        3,
        datetime!(2026-01-01 00:00 UTC),
        None,
    )
    .unwrap();

    assert!(sim.generations.len() >= 2, "need a generation seam to test");
    let aired = airing_ids(&sim);
    let expected: Vec<String> = (0..aired.len())
        .map(|i| format!("e{}", i % EPISODES + 1))
        .collect();
    assert_eq!(aired, expected, "the season must run on across generations");
}

/// The adjacency tail carries the last `adjacency_reach()` airings into the next
/// generation, so `no_repeat_within` holds across the seam. Every generation
/// ranks `m0` first; without the tail the one after a seam opens with it
/// however recently it aired.
#[test]
fn adjacency_tail_holds_no_repeat_within_across_a_generation_seam() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = channel(&write_id_order_plugin(&dir));
    const GAP: usize = 4;
    cfg.rule.no_repeat_within = Some(NoRepeatWithin::Positions(GAP));
    let cat = Catalog::open_in_memory().unwrap();
    for n in 0..(GAP + 1) {
        let id = format!("m{n}");
        let mut e = Entry::new(&id, "movie", &id, Source::Plex);
        e.duration_ms = Some(30 * 60 * 1000);
        add_local_file(&cat, &e);
    }

    let sim = simulate(
        &cfg,
        Path::new("channel.yaml"),
        &cat,
        None,
        3,
        datetime!(2026-01-01 00:00 UTC),
        None,
    )
    .unwrap();

    assert!(sim.generations.len() >= 2, "need a generation seam to test");
    let aired = airing_ids(&sim);
    for (i, id) in aired.iter().enumerate() {
        let from = i.saturating_sub(GAP);
        assert!(
            !aired[from..i].contains(id),
            "{id} at position {i} repeats within {GAP}: {:?}",
            &aired[from..=i]
        );
    }
}

/// Near the end of the window the remaining span is shorter than a chunk, so
/// `target_count` drops from a chunk's worth to the floor of 1.
#[test]
fn target_count_shrinks_to_one_near_the_end_of_the_window() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = channel(&write_plugin(&dir));
    cfg.scoring = Some(ScoringConfig {
        nominal_item_secs: 5400,
        ..ScoringConfig::default()
    });
    let cat = catalog();

    let sim = simulate(
        &cfg,
        Path::new("channel.yaml"),
        &cat,
        None,
        1,
        datetime!(2026-01-01 00:00 UTC),
        None,
    )
    .unwrap();

    let counts: Vec<usize> = sim.generations.iter().map(|g| g.target_count).collect();
    assert_eq!(counts[0], 4, "a full chunk: 6h / 5400s");
    assert_eq!(
        *counts.last().unwrap(),
        1,
        "less than one nominal item left: {counts:?}"
    );
}

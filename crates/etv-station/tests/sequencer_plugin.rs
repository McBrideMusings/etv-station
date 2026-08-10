//! Acceptance test for the sequencer hook (#169): a block names a plugin
//! script that draws its final timeline from the block's resolved pools,
//! instead of walking `pattern`.
//!
//! Proves the committed worked example (`examples/plugins/foryou-sequencer.rhai`)
//! actually runs, and that its two pools genuinely advance differently under
//! one script — a `spotlight` pool that resumes a show from the ledger's
//! cursor, and a `discovery` pool that restarts fresh every generation — the
//! case a single `pattern` block cannot express.

use std::path::Path;

use etv_station::catalog::{Catalog, Entry, EntrySource, Source};
use etv_station::config::{GroupBy, Order, Pool, Select};
use etv_station::resume::GenerationState;
use etv_station::score::{ScoreEnv, ScoreInputs};
use etv_station::sequence::{Window, build};

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
    }
}

fn spotlight_pool() -> Pool {
    Pool {
        name: "spotlight".into(),
        expr: Some("item.type == \"episode\"".into()),
        plugin: None,
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

    let (ids, resume) = build(
        &cat,
        &pools,
        &[],
        None,
        &script_path(),
        &state,
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

    let (ids, _) = build(
        &cat,
        &pools,
        &[],
        None,
        &script_path(),
        &GenerationState::empty(),
        test_env(&inputs),
        Window {
            from: 0,
            fill: None,
        },
    )
    .unwrap();

    assert_eq!(ids[0], "ep-1", "no cursor yet, so spotlight opens at S1E1");
}

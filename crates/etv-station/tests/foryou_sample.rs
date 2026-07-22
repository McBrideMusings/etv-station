//! Acceptance test for Sample S8 (#82): the committed
//! `examples/samples/foryou.yaml` channel weaves two movies and a three-episode
//! run, where *what* plays is chosen by `examples/plugins/taste-engine.rhai`
//! rather than by any query the config author wrote.
//!
//! The deepest sample in the set: it stacks plugin scoring (#74) on pools +
//! pattern (#72) and the per-`show_id` resume map, and it is the one place the
//! whole claim is checked end to end — the station supplies the catalog, the
//! pooled watch history, and what the channel already aired, and holds no taste
//! or replay rule of its own.

use std::path::Path;

use etv_station::catalog::{Catalog, Entry, EntrySource, Source};
use etv_station::config::{ChannelConfig, read_channel};
use etv_station::history::{Ledger, PlayRecord};
use etv_station::resolve::{ResolvedItem, resolve_channel_with_resume};
use etv_station::resume::{GenerationState, ResumeMap};
use etv_station::score::{ScoreInputs, WatchEvent};
use time::OffsetDateTime;

/// Ten movies and three shows of unequal length, all locally sourced so
/// resolution reaches a playable item.
///
/// The shows differ in length on purpose: the pattern draws three episodes per
/// visit, so a two-episode show cannot serve a visit alone and exercises
/// `on_short: next`.
fn library() -> Catalog {
    let cat = Catalog::open_in_memory().unwrap();
    let add = |id: &str, kind: &str, title: &str, year: i64, show: Option<(&str, i64, i64)>| {
        let mut e = Entry::new(id, kind, title, Source::Plex);
        e.year = Some(year);
        if let Some((show_id, season, episode)) = show {
            e.show_id = Some(show_id.to_string());
            e.show = Some(show_id.trim_start_matches("show:").to_string());
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

    // Ten movies, not four: the plugin suppresses what recently aired, so the
    // library has to be deeper than one generation's draw or every run would
    // exhaust it and fall through to the replay-the-stalest path (which
    // `an_exhausted_library_replays_the_stalest` covers deliberately).
    for (id, title, year) in [
        ("mov-arrival", "Arrival", 2016),
        ("mov-blade", "Blade Runner", 1982),
        ("mov-contact", "Contact", 1997),
        ("mov-dune", "Dune", 2021),
        ("mov-exmachina", "Ex Machina", 2014),
        ("mov-gattaca", "Gattaca", 1997),
        ("mov-her", "Her", 2013),
        ("mov-interstellar", "Interstellar", 2014),
        ("mov-moon", "Moon", 2009),
        ("mov-solaris", "Solaris", 1972),
    ] {
        add(id, "movie", title, year, None);
    }

    for ep in 1..=6 {
        add(
            &format!("sev-{ep}"),
            "episode",
            &format!("Severance S1E{ep}"),
            2022,
            Some(("show:severance", 1, ep)),
        );
    }
    for ep in 1..=4 {
        add(
            &format!("exp-{ep}"),
            "episode",
            &format!("Expanse S1E{ep}"),
            2015,
            Some(("show:expanse", 1, ep)),
        );
    }
    for ep in 1..=2 {
        add(
            &format!("dev-{ep}"),
            "episode",
            &format!("Devs S1E{ep}"),
            2020,
            Some(("show:devs", 1, ep)),
        );
    }
    cat
}

fn sample_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/samples/foryou.yaml")
}

fn config() -> ChannelConfig {
    read_channel(&sample_path()).expect("load the For You sample")
}

fn ids(items: &[ResolvedItem]) -> Vec<String> {
    items.iter().map(|i| i.id.clone()).collect()
}

fn resolve(
    cat: &Catalog,
    state: &GenerationState,
    inputs: &ScoreInputs,
) -> (Vec<ResolvedItem>, ResumeMap) {
    resolve_channel_with_resume(
        &config(),
        &sample_path(),
        &[],
        None,
        Some(cat),
        state,
        inputs,
    )
    .expect("resolve the For You sample")
}

/// Project the state the next window is handed, exactly as the daemon does: the
/// pools' rotation from this resolve, plus the per-series cursor and the
/// recently-aired tail read back out of the play-history ledger these airings
/// were recorded in.
fn advance(
    cat: &Catalog,
    prev: &GenerationState,
    resume: ResumeMap,
    items: &[ResolvedItem],
) -> (GenerationState, Vec<String>) {
    let aired = ids(items);
    let show_ids = cat.show_ids_for(&aired).unwrap();
    let mut ledger = Ledger::new();
    ledger.extend(prev.cursor.iter().map(|(key, entry_id)| PlayRecord {
        entry_id: entry_id.clone(),
        show_id: Some(key.clone()),
        start: OffsetDateTime::UNIX_EPOCH,
        played_at: OffsetDateTime::UNIX_EPOCH,
    }));
    ledger.extend(aired.iter().map(|id| PlayRecord {
        entry_id: id.clone(),
        show_id: show_ids.get(id).cloned(),
        start: OffsetDateTime::UNIX_EPOCH,
        played_at: OffsetDateTime::UNIX_EPOCH,
    }));
    let recent = ledger.tail(200);
    (
        GenerationState {
            resume,
            cursor: ledger.series_cursor(),
            tail: ledger.tail(etv_station::constrain::DEFAULT_SEAM_TAIL),
        },
        recent,
    )
}

fn is_movie(id: &str) -> bool {
    id.starts_with("mov-")
}

/// The committed sample loads and resolves at all. Everything below depends on
/// this, and it is also the check that the sample has not drifted from the
/// schema — a config-only edit that breaks it fails here rather than on air.
#[test]
fn the_sample_resolves() {
    let items = resolve(
        &library(),
        &GenerationState::default(),
        &ScoreInputs::default(),
    )
    .0;
    assert!(!items.is_empty(), "the sample must resolve to something");
}

/// Acceptance criterion 1: the shape is 2 movies, then 3 episodes, repeated —
/// and the two pools stay disjoint, which is what proves `ctx.pool` reaches the
/// script. Without it one plugin serving two pools would hand both the same
/// list and movies would appear in the episode slots.
#[test]
fn two_movies_then_three_episodes_from_disjoint_pools() {
    let got = ids(&resolve(
        &library(),
        &GenerationState::default(),
        &ScoreInputs::default(),
    )
    .0);

    assert_eq!(
        got.len() % 5,
        0,
        "every cycle contributes 5 items (2 movies + 3 episodes), got {got:?}"
    );
    for (i, chunk) in got.chunks(5).enumerate() {
        assert!(
            chunk[0..2].iter().all(|id| is_movie(id)),
            "cycle {i} should open with two movies, got {chunk:?}"
        );
        assert!(
            chunk[2..5].iter().all(|id| !is_movie(id)),
            "cycle {i} should continue with three episodes, got {chunk:?}"
        );
    }
}

/// Acceptance criterion 2: the plugin drives what plays. Watch history is an
/// input no CEL expression could express, and moving it changes the schedule —
/// with the config byte-for-byte identical between the two runs.
#[test]
fn watch_history_changes_what_plays() {
    let cat = library();
    let watched = |id: &str| ScoreInputs {
        target_count: 20,
        history: vec![WatchEvent {
            entry_id: id.into(),
            watched_at: 900_000,
        }],
        recent: Vec::new(),
        now: 900_000 + 3600,
    };

    let a = ids(&resolve(&cat, &GenerationState::default(), &watched("mov-dune")).0);
    let b = ids(&resolve(&cat, &GenerationState::default(), &watched("mov-contact")).0);

    assert_ne!(
        a, b,
        "the same config with different watch history must schedule differently"
    );
    assert_eq!(
        a.first().map(String::as_str),
        Some("mov-dune"),
        "the freshly-watched movie should lead: {a:?}"
    );
    assert_eq!(
        b.first().map(String::as_str),
        Some("mov-contact"),
        "the freshly-watched movie should lead: {b:?}"
    );
}

/// Acceptance criterion 3, first half: recently-played content is suppressed by
/// the plugin's own TTL, not by anything in etv-station. What the channel aired
/// last generation comes back as `ctx.recent`, and the example script drops it
/// — for as long as the library has anything else to offer.
#[test]
fn what_just_aired_is_suppressed_next_generation() {
    let cat = library();
    let first_inputs = ScoreInputs {
        target_count: 2,
        now: 900_000,
        ..Default::default()
    };
    let (items, resume) = resolve(&cat, &GenerationState::default(), &first_inputs);
    let first = ids(&items);
    let (state, recent) = advance(&cat, &GenerationState::default(), resume, &items);

    let second_inputs = ScoreInputs {
        target_count: 2,
        recent,
        now: 900_000 + 3600,
        ..Default::default()
    };
    let second = ids(&resolve(&cat, &state, &second_inputs).0);

    let repeated_movies: Vec<&String> = second
        .iter()
        .filter(|id| is_movie(id) && first.contains(id))
        .collect();
    assert!(
        repeated_movies.is_empty(),
        "a movie aired last generation must not come back while it is inside the \
         plugin's replay window: {repeated_movies:?}"
    );
}

/// Acceptance criterion 3, second half: a show the plugin keeps surfacing
/// continues through its episodes instead of replaying its first three.
/// `advance: resume` keys on `show_id`, so this holds even though the plugin
/// returns a freshly-ranked list each generation.
#[test]
fn an_in_progress_show_continues_across_generations() {
    let cat = library();
    let inputs = ScoreInputs {
        target_count: 20,
        now: 900_000,
        ..Default::default()
    };
    let (items, resume) = resolve(&cat, &GenerationState::default(), &inputs);
    let first: Vec<String> = ids(&items).into_iter().filter(|id| !is_movie(id)).collect();
    let (state, recent) = advance(&cat, &GenerationState::default(), resume, &items);

    let next_inputs = ScoreInputs {
        target_count: 20,
        recent,
        now: 900_000 + 3600,
        ..Default::default()
    };
    let second: Vec<String> = ids(&resolve(&cat, &state, &next_inputs).0)
        .into_iter()
        .filter(|id| !is_movie(id))
        .collect();

    // Whichever show leads the second generation, it must not be starting over
    // from episode 1 if it already played episodes in the first.
    for id in &second {
        let show = id.split('-').next().unwrap();
        let played_this_show: Vec<&String> = first.iter().filter(|f| f.starts_with(show)).collect();
        if played_this_show.is_empty() {
            // A show the first generation never touched legitimately starts at
            // its beginning — that is the "new shows start at S1E1" half.
            continue;
        }
        assert!(
            !played_this_show.contains(&id),
            "{id} already aired in the first generation; a resumed show must \
             advance rather than replay ({first:?} then {second:?})"
        );
    }
}

/// Acceptance criterion 4: swapping the scorer keeps the channel working. The
/// station holds no taste rule, so a script with entirely different judgment
/// still produces a valid schedule from the same config shape.
#[test]
fn swapping_the_scorer_keeps_the_channel_working() {
    let dir = tempfile::tempdir().unwrap();
    let plugins = dir.path().join("plugins");
    std::fs::create_dir(&plugins).unwrap();
    // Oldest-first, and deliberately ignores watch history entirely — about as
    // far from taste-engine's judgment as a scorer can get.
    std::fs::write(
        plugins.join("taste-engine.rhai"),
        r#"
fn sources() { #{ movies: `item.type == "movie"`, episodes: `item.type == "episode"` } }
fn pick(ctx) {
    let name = if ctx.pool == "movies" { "movies" } else { "episodes" };
    let rows = [];
    for item in ctx.sets[name] { rows.push(item); }
    rows.sort(|a, b| if a.year < b.year { -1 } else if a.year > b.year { 1 } else { 0 });
    let out = [];
    for r in rows { out.push(r.entry_id); }
    out
}
"#,
    )
    .unwrap();

    // Same channel config, resolved with its `../plugins/taste-engine.rhai`
    // pointing at the replacement — the config file is not edited at all.
    let samples = dir.path().join("samples");
    std::fs::create_dir(&samples).unwrap();
    let channel = samples.join("foryou.yaml");
    std::fs::copy(sample_path(), &channel).unwrap();

    let cat = library();
    let (items, _) = resolve_channel_with_resume(
        &read_channel(&channel).expect("load the copied sample"),
        &channel,
        &[],
        None,
        Some(&cat),
        &GenerationState::default(),
        &ScoreInputs::default(),
    )
    .expect("a different scorer must still produce a schedule");

    let got = ids(&items);
    assert_eq!(got.len() % 5, 0, "the 2+3 shape survives the swap: {got:?}");
    assert_eq!(
        got.first().map(String::as_str),
        Some("mov-solaris"),
        "the replacement ranks oldest-first, so 1972 leads: {got:?}"
    );
}

/// The plugin's replay suppression is a preference, not a reason to take the
/// channel off the air. When the recently-aired window covers the entire
/// library there is nothing left to prefer, and the example script falls back
/// to the stalest thing rather than picking nothing — which the station would
/// (correctly) reject as an empty pool.
#[test]
fn an_exhausted_library_replays_the_stalest() {
    let cat = library();
    // Everything aired, oldest first — so `mov-arrival` is the stalest movie.
    let everything: Vec<String> = [
        "mov-arrival",
        "mov-blade",
        "mov-contact",
        "mov-dune",
        "mov-exmachina",
        "mov-gattaca",
        "mov-her",
        "mov-interstellar",
        "mov-moon",
        "mov-solaris",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    let inputs = ScoreInputs {
        target_count: 4,
        recent: everything,
        now: 900_000,
        ..Default::default()
    };
    let got = ids(&resolve(&cat, &GenerationState::default(), &inputs).0);

    assert!(
        !got.is_empty(),
        "a fully-suppressed library must still schedule something"
    );
    assert_eq!(
        got.first().map(String::as_str),
        Some("mov-arrival"),
        "the longest-ago airing leads the fallback: {got:?}"
    );
}

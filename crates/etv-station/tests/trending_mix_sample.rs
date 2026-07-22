//! Acceptance test for Sample S7 (#81): the committed
//! `examples/samples/trending-mix.yaml` pattern channel weaves one trending
//! movie and three trending episodes, repeated, drawing both halves from a
//! single "Trending" collection split by `type`.
//!
//! Proves the deepest part of the Phase C schema: pools + pattern, the
//! per-`show_id` resume map under `advance = "resume"`, `select = "round_robin"`
//! rotating across shows of different lengths, a series that reaches its end
//! starting over rather than running dry, and the window-continuation model —
//! stateful-feeling progression with **no live cursor**, only a resume map
//! carried across the seam.

use std::path::Path;

use etv_station::catalog::{Catalog, Collection, Entry, EntrySource, Source};
use etv_station::config::{ChannelConfig, read_channel};
use etv_station::history::{Ledger, PlayRecord};
use etv_station::resolve::{resolve_channel, resolve_channel_with_resume};
use etv_station::resume::{GenerationState, ResumeMap};
use time::OffsetDateTime;

/// A "Trending" collection holding two movies and two shows of deliberately
/// unequal length — Game of Thrones with 8 episodes, Invincible with 3 — plus a
/// movie and an episode outside the collection that must never resolve.
///
/// The length mismatch is the point: 8 episodes cannot be consumed in the same
/// number of three-episode visits as 3, so the two shows can only stay correct
/// if each carries its own resume point.
fn trending_catalog() -> Catalog {
    let cat = Catalog::open_in_memory().unwrap();
    cat.upsert_collection(&Collection {
        collection_id: "coll-trending".to_string(),
        name: "Trending".to_string(),
        source: Source::Plex,
    })
    .unwrap();

    let add = |id: &str, kind: &str, title: &str, show: Option<(&str, i64, i64)>| {
        let mut e = Entry::new(id, kind, title, Source::Plex);
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

    let mut pos = 0;
    let mut join = |id: &str| {
        cat.add_collection_item("coll-trending", id, pos).unwrap();
        pos += 1;
    };

    // Two trending movies. `title:asc` orders them, so the titles — not the
    // ids — decide which plays first.
    add("mov-dune", "movie", "Dune", None);
    join("mov-dune");
    add("mov-arrival", "movie", "Arrival", None);
    join("mov-arrival");

    // Game of Thrones: 8 episodes. Longer than one window's worth of visits.
    for n in 1..=8 {
        let id = format!("got-e{n}");
        add(
            &id,
            "episode",
            &format!("GoT S1E{n}"),
            Some(("show:got", 1, n)),
        );
        join(&id);
    }
    // Invincible: 3 episodes. Exhausts and loops while GoT is still going.
    for n in 1..=3 {
        let id = format!("inv-e{n}");
        add(
            &id,
            "episode",
            &format!("Invincible S1E{n}"),
            Some(("show:inv", 1, n)),
        );
        join(&id);
    }

    // Outside the collection — must never appear.
    add("mov-untrending", "movie", "Untrending", None);
    add(
        "other-e1",
        "episode",
        "Other S1E1",
        Some(("show:other", 1, 1)),
    );
    cat
}

/// Project the state the next window is handed, exactly as the daemon does:
/// the pools' rotation from this resolve, plus the per-series cursor read back
/// out of the play-history ledger these airings were recorded in (#70). The
/// cursor is never carried directly — it only ever comes from the ledger.
fn advance_state(
    cat: &Catalog,
    prev: &GenerationState,
    resume: ResumeMap,
    items: &[etv_station::resolve::ResolvedItem],
) -> GenerationState {
    let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
    let show_ids = cat.show_ids_for(&ids).unwrap();
    let mut ledger = Ledger::new();
    ledger.extend(prev.cursor.iter().map(|(key, entry_id)| PlayRecord {
        entry_id: entry_id.clone(),
        show_id: Some(key.clone()),
        start: OffsetDateTime::UNIX_EPOCH,
        played_at: OffsetDateTime::UNIX_EPOCH,
    }));
    ledger.extend(ids.iter().map(|id| PlayRecord {
        entry_id: id.clone(),
        show_id: show_ids.get(id).cloned(),
        start: OffsetDateTime::UNIX_EPOCH,
        played_at: OffsetDateTime::UNIX_EPOCH,
    }));
    GenerationState {
        resume,
        cursor: ledger.series_cursor(),
        tail: ledger.tail(etv_station::constrain::DEFAULT_SEAM_TAIL),
    }
}

fn sample_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/samples/trending-mix.yaml")
}

fn config() -> ChannelConfig {
    read_channel(&sample_path()).expect("load trending-mix sample")
}

fn ids(items: &[etv_station::resolve::ResolvedItem]) -> Vec<String> {
    items.iter().map(|i| i.id.clone()).collect()
}

/// Acceptance criterion 1: the output is 1 movie, then 3 episodes, repeated to
/// fill the window.
#[test]
fn trending_mix_sample_weaves_one_movie_then_three_episodes() {
    let cat = trending_catalog();
    let items = resolve_channel(&config(), &sample_path(), &[], None, Some(&cat)).expect("resolve");
    let ids = ids(&items);

    assert_eq!(
        ids.len() % 4,
        0,
        "every cycle contributes exactly 4 items (1 movie + 3 episodes), got {ids:?}"
    );
    for (cycle, group) in ids.chunks(4).enumerate() {
        assert!(
            group[0].starts_with("mov-"),
            "cycle {cycle} must open on a movie, got {group:?}"
        );
        assert!(
            group[1..].iter().all(|id| id.contains("-e")),
            "cycle {cycle} must follow with three episodes, got {group:?}"
        );
    }
}

/// Acceptance criterion 3, first half: `select = "round_robin"` alternates
/// shows, and a three-episode visit stays within one show.
#[test]
fn trending_mix_sample_rotates_shows_and_keeps_a_visit_on_one_show() {
    let cat = trending_catalog();
    let items = resolve_channel(&config(), &sample_path(), &[], None, Some(&cat)).unwrap();
    let ids = ids(&items);

    // The show each three-episode run belonged to, cycle by cycle.
    let runs: Vec<String> = ids
        .chunks(4)
        .map(|group| {
            let shows: Vec<&str> = group[1..]
                .iter()
                .map(|id| id.split('-').next().unwrap())
                .collect();
            assert!(
                shows.windows(2).all(|w| w[0] == w[1]),
                "a visit must binge one show, got {shows:?}"
            );
            shows[0].to_string()
        })
        .collect();

    assert!(runs.len() >= 2, "need at least two runs to show rotation");
    assert!(
        runs.windows(2).all(|w| w[0] != w[1]),
        "consecutive runs must be different shows, got {runs:?}"
    );
}

/// Non-members never resolve — the collection is the whole source of truth for
/// both pools.
#[test]
fn trending_mix_sample_excludes_everything_outside_the_collection() {
    let cat = trending_catalog();
    let items = resolve_channel(&config(), &sample_path(), &[], None, Some(&cat)).unwrap();
    let ids = ids(&items);
    assert!(!ids.iter().any(|id| id == "mov-untrending"));
    assert!(!ids.iter().any(|id| id == "other-e1"));
}

/// Acceptance criterion 2 — the heart of the sample. Two series of different
/// lengths each advance independently across windows via their own `show_id`
/// resume points, and regenerating the next window continues progression by
/// reading the prior `resume_out`. Neither show resets the other.
#[test]
fn trending_mix_sample_continues_each_show_across_the_window_seam() {
    let cat = trending_catalog();
    let cfg = config();

    let (first, next) = resolve_channel_with_resume(
        &cfg,
        &sample_path(),
        &[],
        None,
        Some(&cat),
        &GenerationState::empty(),
    )
    .expect("first window");
    let first_ids = ids(&first);

    // Window 1 airs GoT from its start; 8 episodes cannot fit in the
    // three-at-a-time visits this window has, so some are left for later.
    let got_first: Vec<&String> = first_ids
        .iter()
        .filter(|id| id.starts_with("got-"))
        .collect();
    assert_eq!(got_first[0], "got-e1", "GoT starts at its first episode");
    assert!(
        got_first.len() < 8,
        "the sample is only meaningful if GoT outlasts one window; aired {} of 8",
        got_first.len()
    );

    // The sidecar records the rotation — whose turn is next — and nothing about
    // where any series stopped.
    let pool = next
        .pool("shows")
        .expect("the shows pool reports resume state");
    assert!(
        pool.next.is_some(),
        "round-robin must record whose turn is next"
    );

    // Where each show stopped comes from the play-history ledger, keyed by
    // show_id — one record, projected, rather than a second copy in the sidecar.
    let state = advance_state(&cat, &GenerationState::empty(), next, &first);
    assert_eq!(
        state.cursor.get("show:got").unwrap(),
        got_first.last().unwrap().as_str(),
        "the ledger's projection is GoT's last-played episode"
    );
    assert!(
        state.cursor.contains_key("show:inv"),
        "Invincible has its own entry, independent of GoT's"
    );

    // Window 2 is generated from that projection — no live cursor anywhere.
    let (second, _) =
        resolve_channel_with_resume(&cfg, &sample_path(), &[], None, Some(&cat), &state)
            .expect("second window");
    let second_ids = ids(&second);

    // GoT picks up exactly after where it stopped, rather than restarting.
    let last_got = got_first.last().unwrap().as_str();
    let next_got = second_ids
        .iter()
        .find(|id| id.starts_with("got-"))
        .expect("GoT airs again in window 2");
    let episode_of = |id: &str| id.trim_start_matches("got-e").parse::<u32>().unwrap();
    assert_eq!(
        episode_of(next_got),
        episode_of(last_got) + 1,
        "GoT must continue at the next episode across the seam, not reset"
    );

    // Concatenating both windows, GoT's episodes are strictly in order with no
    // gap and no repeat until it wraps past its finale.
    let got_all: Vec<u32> = first_ids
        .iter()
        .chain(second_ids.iter())
        .filter(|id| id.starts_with("got-"))
        .map(|id| episode_of(id))
        .collect();
    let expected: Vec<u32> = (0..got_all.len()).map(|i| (i as u32 % 8) + 1).collect();
    assert_eq!(
        got_all, expected,
        "GoT must advance 1..8 then loop, unbroken across the window seam"
    );
}

/// Acceptance criterion 3, second half: the shorter show reaches its end and
/// starts over so the channel never runs dry, while the longer show is still
/// going.
#[test]
fn trending_mix_sample_loops_the_shorter_show_without_disturbing_the_longer() {
    let cat = trending_catalog();
    let cfg = config();

    let (first, next) = resolve_channel_with_resume(
        &cfg,
        &sample_path(),
        &[],
        None,
        Some(&cat),
        &GenerationState::empty(),
    )
    .unwrap();
    let state = advance_state(&cat, &GenerationState::empty(), next, &first);
    let (second, _) =
        resolve_channel_with_resume(&cfg, &sample_path(), &[], None, Some(&cat), &state).unwrap();

    let inv: Vec<u32> = ids(&first)
        .iter()
        .chain(ids(&second).iter())
        .filter(|id| id.starts_with("inv-"))
        .map(|id| id.trim_start_matches("inv-e").parse::<u32>().unwrap())
        .collect();

    assert!(
        inv.len() > 3,
        "Invincible must air more slots than it has episodes for looping to be visible, got {inv:?}"
    );
    let expected: Vec<u32> = (0..inv.len()).map(|i| (i as u32 % 3) + 1).collect();
    assert_eq!(
        inv, expected,
        "Invincible must replay 1..3 on loop rather than running dry"
    );

    // The channel never emits nothing: both windows are full.
    assert!(!first.is_empty() && !second.is_empty());
}

/// Program metadata and playback paths come from the catalog, exactly as they
/// do for a `query` entry — a pattern block is not a second-class resolver.
#[test]
fn trending_mix_sample_items_carry_catalog_metadata() {
    let cat = trending_catalog();
    let items = resolve_channel(&config(), &sample_path(), &[], None, Some(&cat)).unwrap();

    let episode = items
        .iter()
        .find(|i| i.id == "got-e1")
        .expect("GoT S1E1 resolves");
    let program = episode.program.as_ref().expect("episode carries metadata");
    assert_eq!(program.season, Some(1));
    assert_eq!(program.episode, Some(1));
    assert_eq!(program.title.as_deref(), Some("GoT S1E1"));

    match &episode.source {
        etv_station::config::SourceConfig::Local { path } => {
            assert!(path.ends_with("got-e1.mkv"), "path = {path}")
        }
        other => panic!("expected a local source, got {other:?}"),
    }
}

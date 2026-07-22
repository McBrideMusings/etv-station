//! Acceptance test for Sample S10 (#84): the committed
//! `examples/samples/kungfu.yaml` pattern channel plays four martial-arts films
//! then one Jackie Chan film, repeated, and never the same film twice in a row.
//!
//! Proves three things no other sample does: `item.cast.contains(...)` filtering
//! by performer, two pools kept disjoint by their own expressions rather than by
//! scheduler policy, and the `no_repeat_within` adjacency pass (#73) — including
//! across a generation seam, where the boundary is read out of the play-history
//! ledger.

use std::collections::HashSet;
use std::path::Path;

use etv_station::catalog::{Catalog, Entry, EntrySource, Source, TagNs};
use etv_station::config::{ChannelConfig, read_channel};
use etv_station::history::{Ledger, PlayRecord};
use etv_station::resolve::{ResolvedItem, resolve_channel, resolve_channel_with_resume};
use etv_station::resume::{GenerationState, ResumeMap};
use time::OffsetDateTime;

/// Six martial-arts films — four without Jackie Chan, two with — plus two films
/// that must never resolve: one martial-arts-free Jackie Chan film (wrong
/// genre) and one non-martial-arts drama.
///
/// Two Jackie Chan films rather than one so `no_repeat_within` has a real
/// alternative at the injection slot; with only one it could never be satisfied
/// on consecutive cycles and the test would prove nothing about the constraint.
fn kungfu_catalog() -> Catalog {
    let cat = Catalog::open_in_memory().unwrap();

    let add = |id: &str, title: &str, genres: &[&str], cast: &[&str]| {
        let e = Entry::new(id, "movie", title, Source::Plex);
        cat.upsert_entry(&e).unwrap();
        cat.add_source(&EntrySource {
            source: Source::LocalFs,
            source_id: format!("fs-{id}"),
            entry_id: id.to_string(),
            playback_path: format!("/media/{id}.mkv"),
            last_seen: None,
        })
        .unwrap();
        for g in genres {
            cat.add_tag(id, TagNs::Genre, g).unwrap();
        }
        for c in cast {
            cat.add_tag(id, TagNs::Cast, c).unwrap();
        }
    };

    // The pile: martial arts, no Jackie Chan.
    add(
        "f-36th",
        "The 36th Chamber",
        &["Martial Arts"],
        &["Gordon Liu"],
    );
    add("f-fist", "Fist of Fury", &["Martial Arts"], &["Bruce Lee"]);
    add(
        "f-enter",
        "Enter the Dragon",
        &["Martial Arts"],
        &["Bruce Lee"],
    );
    add(
        "f-five",
        "Five Deadly Venoms",
        &["Martial Arts"],
        &["Lo Meng"],
    );
    add("f-crane", "Crane Fist", &["Martial Arts"], &["Ti Lung"]);

    // The cadence: martial arts, Jackie Chan.
    add(
        "f-drunken",
        "Drunken Master",
        &["Martial Arts"],
        &["Jackie Chan", "Yuen Siu-tien"],
    );
    add(
        "f-police",
        "Police Story",
        &["Martial Arts"],
        &["Jackie Chan", "Maggie Cheung"],
    );

    // Neither pool: Jackie Chan, but not a martial-arts film — proves the pile's
    // genre term and the cadence pool's reliance on cast are both real filters.
    add("f-cannon", "Cannonball Run", &["Comedy"], &["Jackie Chan"]);
    // Neither pool: martial-arts-free drama.
    add("f-drama", "A Quiet Drama", &["Drama"], &["Chow Yun-fat"]);

    cat
}

/// Project the state the next generation is handed, exactly as the daemon does:
/// the pools' rotation from this resolve, plus the cursor and the adjacency tail
/// read back out of the play-history ledger these airings were recorded in.
fn advance_state(
    cat: &Catalog,
    prev: &GenerationState,
    resume: ResumeMap,
    items: &[ResolvedItem],
) -> GenerationState {
    let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
    let show_ids = cat.show_ids_for(&ids).unwrap();
    let mut ledger = Ledger::new();
    ledger.extend(prev.tail.iter().map(|entry_id| PlayRecord {
        entry_id: entry_id.clone(),
        show_id: None,
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
        tail: ledger.tail(etv_station::constrain::SEAM_TAIL),
        ..Default::default()
    }
}

fn sample_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/samples/kungfu.yaml")
}

fn config() -> ChannelConfig {
    read_channel(&sample_path()).expect("load kungfu sample")
}

fn ids(items: &[ResolvedItem]) -> Vec<String> {
    items.iter().map(|i| i.id.clone()).collect()
}

const JACKIE: [&str; 2] = ["f-drunken", "f-police"];

fn is_jackie(id: &str) -> bool {
    JACKIE.contains(&id)
}

#[test]
fn kungfu_sample_follows_the_four_then_one_cadence() {
    let cat = kungfu_catalog();
    let items = resolve_channel(&config(), &sample_path(), &[], None, Some(&cat)).unwrap();
    let ids = ids(&items);

    // Every fifth slot is the Jackie Chan injection; the other four are pile.
    for (i, id) in ids.iter().enumerate() {
        if i % 5 == 4 {
            assert!(is_jackie(id), "slot {i} should be Jackie Chan: {ids:?}");
        } else {
            assert!(!is_jackie(id), "slot {i} should be from the pile: {ids:?}");
        }
    }
    assert!(ids.len() >= 5, "expected at least one full cycle: {ids:?}");
}

#[test]
fn kungfu_sample_pools_are_disjoint_by_construction() {
    let cat = kungfu_catalog();
    let items = resolve_channel(&config(), &sample_path(), &[], None, Some(&cat)).unwrap();
    let ids = ids(&items);

    // A film drawn at a pile slot is never one drawn at a cadence slot: the
    // expressions cannot both match the same film.
    let pile: HashSet<&String> = ids
        .iter()
        .enumerate()
        .filter(|(i, _)| i % 5 != 4)
        .map(|(_, s)| s)
        .collect();
    let cadence: HashSet<&String> = ids
        .iter()
        .enumerate()
        .filter(|(i, _)| i % 5 == 4)
        .map(|(_, s)| s)
        .collect();
    assert!(
        pile.is_disjoint(&cadence),
        "a film was drawn from both pools: {:?}",
        pile.intersection(&cadence).collect::<Vec<_>>()
    );
}

#[test]
fn kungfu_sample_filters_by_cast_and_genre() {
    let cat = kungfu_catalog();
    let items = resolve_channel(&config(), &sample_path(), &[], None, Some(&cat)).unwrap();
    let ids = ids(&items);

    // `!item.cast.contains("Jackie Chan")` on the pile AND
    // `item.genres.contains("Martial Arts")` on both pools have to hold, so a
    // Jackie Chan comedy belongs to neither pool.
    assert!(
        !ids.iter().any(|id| id == "f-cannon"),
        "a Jackie Chan film outside the Martial Arts genre resolved: {ids:?}"
    );
    assert!(
        !ids.iter().any(|id| id == "f-drama"),
        "a non-martial-arts film resolved: {ids:?}"
    );
    // The cadence slots only ever hold the two Jackie Chan martial-arts films.
    for (i, id) in ids.iter().enumerate().filter(|(i, _)| i % 5 == 4) {
        assert!(JACKIE.contains(&id.as_str()), "slot {i} is {id}: {ids:?}");
    }
}

#[test]
fn kungfu_sample_never_repeats_a_film_back_to_back() {
    let cat = kungfu_catalog();
    let items = resolve_channel(&config(), &sample_path(), &[], None, Some(&cat)).unwrap();
    let ids = ids(&items);

    for i in 1..ids.len() {
        assert_ne!(
            ids[i - 1],
            ids[i],
            "positions {} and {i} are the same film: {ids:?}",
            i - 1
        );
    }
}

#[test]
fn kungfu_sample_holds_no_repeat_across_the_generation_seam() {
    let cat = kungfu_catalog();
    let cfg = config();

    let (first, resume) = resolve_channel_with_resume(
        &cfg,
        &sample_path(),
        &[],
        None,
        Some(&cat),
        &GenerationState::empty(),
    )
    .unwrap();
    let first_ids = ids(&first);

    // Second generation, handed the ledger the first one wrote.
    let state = advance_state(&cat, &GenerationState::empty(), resume, &first);
    let (second, _) =
        resolve_channel_with_resume(&cfg, &sample_path(), &[], None, Some(&cat), &state).unwrap();
    let second_ids = ids(&second);

    assert!(
        !second_ids.is_empty(),
        "the channel stopped producing items"
    );
    assert_ne!(
        first_ids.last().unwrap(),
        second_ids.first().unwrap(),
        "the seam repeats a film: ...{:?} | {:?}...",
        &first_ids[first_ids.len().saturating_sub(2)..],
        &second_ids[..2.min(second_ids.len())]
    );
}

#[test]
fn kungfu_sample_keeps_broadcasting_after_playing_everything() {
    // Seven films, five per cycle: several generations run past the end of both
    // pools. The channel must keep resolving a full list rather than running dry.
    let cat = kungfu_catalog();
    let cfg = config();
    let mut state = GenerationState::empty();
    for pass in 0..5 {
        let (items, resume) =
            resolve_channel_with_resume(&cfg, &sample_path(), &[], None, Some(&cat), &state)
                .unwrap();
        assert!(!items.is_empty(), "generation {pass} resolved to nothing");
        state = advance_state(&cat, &state, resume, &items);
    }
}

//! Acceptance test for Sample S6 (#80): the committed
//! `examples/channels/marathon.yaml` collection channel plays every member of
//! the "Halloween Marathon" collection in its authored `collection_items.position`
//! order, excludes non-members, and is stable across generations. Proves
//! collections-as-order — the counterpart to Sample S5, which reads the same
//! stored structure for membership only and shuffles it.

use std::path::Path;

use etv_station::catalog::{Catalog, Collection, Entry, EntrySource, Source};
use etv_station::config::{ChannelConfig, read_channel};
use etv_station::resolve::resolve_channel;

/// The marathon's authored running order. Deliberately not alphabetical by id,
/// not release order, and not the order the entries are inserted in, so a
/// passing assertion can only come from reading `position`.
const AUTHORED: [(&str, i64); 4] = [
    ("m-scream", 0),
    ("m-alien", 1),
    ("m-carrie", 2),
    ("m-thething", 3),
];

/// Seed the marathon's four members plus one movie outside it, so the entry has
/// something it must not pick up. Members are inserted in an order that
/// contradicts their positions — insertion order must not leak into the result.
fn marathon_catalog() -> Catalog {
    let cat = Catalog::open_in_memory().unwrap();
    cat.upsert_collection(&Collection {
        collection_id: "coll-marathon".to_string(),
        name: "Halloween Marathon".to_string(),
        source: Source::Plex,
    })
    .unwrap();

    let seed_movie = |id: &str| {
        let e = Entry::new(id, "movie", format!("Movie {id}"), Source::Plex);
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

    // Insert back-to-front: if anything ever returned insertion order, or the
    // `entry_id` order the catalog falls back to elsewhere, the order assertion
    // below would fail rather than pass by luck.
    for (id, position) in AUTHORED.iter().rev() {
        seed_movie(id);
        cat.add_collection_item("coll-marathon", id, *position)
            .unwrap();
    }
    // A movie that is NOT in the collection — must never resolve.
    seed_movie("m-out");
    cat
}

fn sample_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/channels/marathon.yaml")
}

/// The whole point of the sample: authored position order, start to finish.
#[test]
fn marathon_sample_plays_the_collection_in_authored_position_order() {
    let config: ChannelConfig = read_channel(&sample_path()).expect("load marathon sample");
    let cat = marathon_catalog();
    let items = resolve_channel(&config, &sample_path(), &[], None, Some(&cat)).expect("resolve");

    let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
    let expected: Vec<&str> = AUTHORED.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, expected, "members must play in collection_items.position order");
}

/// The non-member never appears — the entry emits the collection, not the
/// catalog.
#[test]
fn marathon_sample_excludes_non_members() {
    let config: ChannelConfig = read_channel(&sample_path()).unwrap();
    let cat = marathon_catalog();
    let items = resolve_channel(&config, &sample_path(), &[], None, Some(&cat)).unwrap();

    assert_eq!(items.len(), AUTHORED.len());
    assert!(
        !items.iter().any(|i| i.id == "m-out"),
        "a movie outside the collection must never resolve"
    );
}

/// No shuffle, no seed, nothing time-dependent: the sample resolves identically
/// every generation, which is what makes "then it loops" a stable marathon
/// rather than a reshuffle.
#[test]
fn marathon_sample_is_stable_across_generations() {
    let config: ChannelConfig = read_channel(&sample_path()).unwrap();
    let cat = marathon_catalog();

    let first = resolve_channel(&config, &sample_path(), &[], None, Some(&cat)).unwrap();
    let second = resolve_channel(&config, &sample_path(), &[], None, Some(&cat)).unwrap();
    let ids1: Vec<&str> = first.iter().map(|i| i.id.as_str()).collect();
    let ids2: Vec<&str> = second.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids1, ids2, "an unseeded collection channel must not vary");
}

/// Re-ordering the marathon in Plex is a re-ingest, not a config edit: the same
/// committed YAML follows the new positions.
#[test]
fn reordering_the_collection_reorders_the_channel_without_touching_config() {
    let config: ChannelConfig = read_channel(&sample_path()).unwrap();
    let cat = marathon_catalog();

    // Drag "m-thething" from last to first, exactly as a re-ingest would rewrite
    // the positions.
    cat.add_collection_item("coll-marathon", "m-thething", -1)
        .unwrap();

    let items = resolve_channel(&config, &sample_path(), &[], None, Some(&cat)).unwrap();
    let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids, ["m-thething", "m-scream", "m-alien", "m-carrie"]);
}

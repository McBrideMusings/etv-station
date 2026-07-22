//! Acceptance test for Sample S5 (#79): the committed
//! `examples/samples/trending-shuffle.yaml` query channel resolves every member
//! of the "Trending" collection (membership EXISTS, position ignored), excludes
//! non-members, and shuffles — a fresh shuffle when unseeded, a reproducible one
//! when a `seed` is pinned. Proves collections-as-set + the resolve→collapse→
//! order (`random`) pipeline against a fixture catalog.

use std::path::Path;

use etv_station::catalog::{Catalog, Collection, Entry, EntrySource, Source};
use etv_station::config::{ChannelConfig, read_channel};
use etv_station::resolve::resolve_channel;

/// Seed four movies in a "Trending" collection plus one movie outside it, so the
/// membership filter has something to exclude. Position is set but irrelevant to
/// this sample (membership is a set, not an order).
fn trending_catalog() -> Catalog {
    let cat = Catalog::open_in_memory().unwrap();
    cat.upsert_collection(&Collection {
        collection_id: "coll-trending".to_string(),
        name: "Trending".to_string(),
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
    for (pos, id) in ["m-a", "m-b", "m-c", "m-d"].iter().enumerate() {
        seed_movie(id);
        cat.add_collection_item("coll-trending", id, pos as i64)
            .unwrap();
    }
    // A movie that is NOT in the collection — must never resolve.
    seed_movie("m-out");
    cat
}

fn sample_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/samples/trending-shuffle.yaml")
}

/// Every member of the collection plays; the non-member is excluded. Order is
/// unasserted here — an unseeded shuffle is fresh each generation.
#[test]
fn trending_shuffle_sample_plays_every_member_and_excludes_non_members() {
    let config: ChannelConfig = read_channel(&sample_path()).expect("load trending sample");
    let cat = trending_catalog();
    let items = resolve_channel(&config, &sample_path(), &[], None, Some(&cat)).expect("resolve");

    let mut ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
    ids.sort_unstable();
    // All four members, exactly once each; "m-out" is absent.
    assert_eq!(ids, ["m-a", "m-b", "m-c", "m-d"]);
}

/// A pinned `seed` reproduces the shuffle exactly (same seed → same order),
/// while still being a permutation of every member.
#[test]
fn trending_shuffle_sample_is_reproducible_with_a_pinned_seed() {
    let mut config: ChannelConfig = read_channel(&sample_path()).unwrap();
    config.seed = Some(42);
    let cat = trending_catalog();

    let first = resolve_channel(&config, &sample_path(), &[], None, Some(&cat)).unwrap();
    let second = resolve_channel(&config, &sample_path(), &[], None, Some(&cat)).unwrap();
    let ids1: Vec<&str> = first.iter().map(|i| i.id.as_str()).collect();
    let ids2: Vec<&str> = second.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids1, ids2, "a pinned seed must reproduce the shuffle");

    let mut sorted = ids1.clone();
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        ["m-a", "m-b", "m-c", "m-d"],
        "shuffle is a permutation of all members"
    );
}

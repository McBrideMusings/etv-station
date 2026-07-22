//! Acceptance test for Sample S4 (#78): the committed `examples/samples/dragonball.yaml`
//! manual block weaves query episode-ranges — each ordered by `absolute_episode`
//! — around an inline movie, playing entries in authored order. Proves the
//! hardest authored-order case: a `manual` block with per-entry query order
//! (#46) over the `absolute_episode` field (#47), and query/inline intermingling,
//! resolved against a fixture catalog.

use std::path::Path;

use etv_station::catalog::{Catalog, Entry, EntrySource, Source};
use etv_station::config::{ChannelConfig, read_channel};
use etv_station::resolve::resolve_channel;

/// Seed Dragon Ball episodes carrying franchise-wide `absolute_episode` numbers,
/// out of insertion order so the per-entry `absolute_episode:asc` sort — not the
/// seed order — decides the result. Values span both authored arcs (1–13, 14–28).
fn dragonball_catalog() -> Catalog {
    let cat = Catalog::open_in_memory().unwrap();
    for abs in [3, 1, 2, 13, 12, 15, 14, 28] {
        let id = format!("db:{abs}");
        let mut e = Entry::new(&id, "episode", format!("Dragon Ball Ep {abs}"), Source::Plex);
        e.show = Some("Dragon Ball".to_string());
        e.absolute_episode = Some(abs);
        cat.upsert_entry(&e).unwrap();
        cat.add_source(&EntrySource {
            source: Source::LocalFs,
            source_id: format!("fs-{id}"),
            entry_id: id.clone(),
            playback_path: format!("/media/db/{abs}.mkv"),
            last_seen: None,
        })
        .unwrap();
    }
    cat
}

fn sample_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/samples/dragonball.yaml")
}

#[test]
fn dragonball_sample_weaves_ranges_and_movie_in_authored_order() {
    let config: ChannelConfig = read_channel(&sample_path()).expect("load dragonball sample");
    let cat = dragonball_catalog();
    let items = resolve_channel(&config, &sample_path(), &[], None, Some(&cat)).expect("resolve");

    let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
    // Entry 1 (eps 1–13, absolute_episode:asc), then the inline movie (an `fs:`
    // id derived from its path), then entry 2 (eps 14–28, absolute_episode:asc).
    // The block is `manual`, so the entries stay in authored order while each
    // query entry sorts its own resolved episodes.
    assert_eq!(&ids[..5], ["db:1", "db:2", "db:3", "db:12", "db:13"].as_slice());
    assert!(
        ids[5].starts_with("fs:"),
        "the movie must sit between the two arcs, got {}",
        ids[5],
    );
    assert_eq!(&ids[6..], ["db:14", "db:15", "db:28"].as_slice());
    assert_eq!(items.len(), 9, "5 episodes + 1 movie + 3 episodes");
}

/// The woven order is deterministic — the same config + catalog resolve to the
/// same sequence every time (the per-entry sort has a stable tiebreak).
#[test]
fn dragonball_sample_order_is_deterministic() {
    let config: ChannelConfig = read_channel(&sample_path()).unwrap();
    let cat = dragonball_catalog();

    let first = resolve_channel(&config, &sample_path(), &[], None, Some(&cat)).unwrap();
    let second = resolve_channel(&config, &sample_path(), &[], None, Some(&cat)).unwrap();
    let ids1: Vec<&str> = first.iter().map(|i| i.id.as_str()).collect();
    let ids2: Vec<&str> = second.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids1, ids2);
}

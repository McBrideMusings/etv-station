//! Acceptance test for Sample S4 (#78): the committed `examples/samples/dragonball.yaml`
//! manual block weaves query episode-ranges — each ordered by `episode` within a
//! season — around an inline movie, playing entries in authored order. Proves the
//! hardest authored-order case: a `manual` block with per-entry query order
//! (#46), and query/inline intermingling, resolved against a fixture catalog.

use std::path::Path;

use etv_station::catalog::{Catalog, Entry, EntrySource, Source};
use etv_station::config::{ChannelConfig, read_channel};
use etv_station::resolve::resolve_channel;

/// Seed Dragon Ball episodes across the two authored arcs (season 1, then season
/// 2), inserted out of episode order so the per-entry `episode:asc` sort — not
/// the seed order — decides the result.
fn dragonball_catalog() -> Catalog {
    let cat = Catalog::open_in_memory().unwrap();
    for (season, episode) in [
        (1, 3),
        (1, 1),
        (1, 2),
        (1, 13),
        (1, 12),
        (2, 15),
        (2, 14),
        (2, 1),
    ] {
        let id = format!("db:s{season}e{episode}");
        let mut e = Entry::new(
            &id,
            "episode",
            format!("Dragon Ball S{season}E{episode}"),
            Source::Plex,
        );
        e.show = Some("Dragon Ball".to_string());
        e.season = Some(season);
        e.episode = Some(episode);
        cat.upsert_entry(&e).unwrap();
        cat.add_source(&EntrySource {
            source: Source::LocalFs,
            source_id: format!("fs-{id}"),
            entry_id: id.clone(),
            playback_path: format!("/media/db/s{season}e{episode}.mkv"),
            last_seen: None,
            missing_since: None,
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
    // Entry 1 (season 1, episode:asc), then the inline movie (an `fs:` id derived
    // from its path), then entry 2 (season 2, episode:asc). The block is
    // `manual`, so the entries stay in authored order while each query entry
    // sorts its own resolved episodes.
    assert_eq!(
        &ids[..5],
        ["db:s1e1", "db:s1e2", "db:s1e3", "db:s1e12", "db:s1e13"].as_slice()
    );
    assert!(
        ids[5].starts_with("fs:"),
        "the movie must sit between the two arcs, got {}",
        ids[5],
    );
    assert_eq!(&ids[6..], ["db:s2e1", "db:s2e14", "db:s2e15"].as_slice());
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

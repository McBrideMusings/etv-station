//! Acceptance test for Sample S2 (#76): the committed `examples/samples/lotr.yaml`
//! query channel resolves the LOTR films and plays them oldest-first by release
//! date, deterministically. Proves the query (#68) + order (#69) + resolve
//! pipeline (#71) path end-to-end against a fixture catalog — no live Plex.

use std::path::Path;

use etv_station::catalog::{Catalog, Entry, EntrySource, Source};
use etv_station::config::{ChannelConfig, read_channel};
use etv_station::resolve::resolve_channel;

/// Seed the three theatrical LOTR films. Titles carry the franchise prefix (as
/// Plex names them) so the sample's `item.title.contains("Lord of the Rings")`
/// query matches. Release dates are deliberately out of insertion order so the
/// sort — not the seed order — decides the result.
fn lotr_catalog() -> Catalog {
    let cat = Catalog::open_in_memory().unwrap();
    let films = [
        (
            "imdb:tt0167260",
            "The Lord of the Rings: The Return of the King",
            "2003-12-17",
        ),
        (
            "imdb:tt0120737",
            "The Lord of the Rings: The Fellowship of the Ring",
            "2001-12-19",
        ),
        (
            "imdb:tt0167261",
            "The Lord of the Rings: The Two Towers",
            "2002-12-18",
        ),
    ];
    for (id, title, release_date) in films {
        let mut e = Entry::new(id, "movie", title, Source::Plex);
        e.release_date = Some(release_date.to_string());
        cat.upsert_entry(&e).unwrap();
        cat.add_source(&EntrySource {
            source: Source::LocalFs,
            source_id: format!("fs-{id}"),
            entry_id: id.to_string(),
            playback_path: format!("/media/lotr/{id}.mkv"),
            last_seen: None,
        })
        .unwrap();
    }
    cat
}

fn sample_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/samples/lotr.yaml")
}

#[test]
fn lotr_sample_resolves_in_release_order() {
    let config: ChannelConfig = read_channel(&sample_path()).expect("load lotr sample");

    let cat = lotr_catalog();
    let items = resolve_channel(&config, &sample_path(), &[], None, Some(&cat)).expect("resolve");

    let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["imdb:tt0120737", "imdb:tt0167261", "imdb:tt0167260"],
        "LOTR films must play oldest-first by release date",
    );

    // Program metadata + a playable source came from the catalog.
    assert_eq!(
        items[0].program.as_ref().unwrap().title.as_deref(),
        Some("The Lord of the Rings: The Fellowship of the Ring"),
    );
}

#[test]
fn lotr_sample_order_is_deterministic() {
    let config: ChannelConfig = read_channel(&sample_path()).unwrap();
    let cat = lotr_catalog();

    let first = resolve_channel(&config, &sample_path(), &[], None, Some(&cat)).unwrap();
    let second = resolve_channel(&config, &sample_path(), &[], None, Some(&cat)).unwrap();
    let ids1: Vec<&str> = first.iter().map(|i| i.id.as_str()).collect();
    let ids2: Vec<&str> = second.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids1, ids2);
}

/// #76 also requires ties and null release dates to order deterministically via
/// the `entry_id` tiebreaker. The three real films have distinct dates, so this
/// drives the same sample query over a fixture that forces both: two entries
/// share a date, and one has none.
#[test]
fn lotr_sample_breaks_ties_by_entry_id_and_sorts_nulls_last() {
    let cat = Catalog::open_in_memory().unwrap();
    let seed = |id: &str, title: &str, date: Option<&str>| {
        let mut e = Entry::new(id, "movie", title, Source::Plex);
        e.release_date = date.map(str::to_string);
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
    // `bravo` and `alpha` tie on the same date; `null` has none. Seeded so the
    // tie pair is inserted in reverse entry_id order.
    seed(
        "lotr:bravo",
        "The Lord of the Rings: Bravo",
        Some("2001-01-01"),
    );
    seed(
        "lotr:alpha",
        "The Lord of the Rings: Alpha",
        Some("2001-01-01"),
    );
    seed("lotr:null", "The Lord of the Rings: Null", None);

    let config: ChannelConfig = read_channel(&sample_path()).unwrap();
    let items = resolve_channel(&config, &sample_path(), &[], None, Some(&cat)).unwrap();
    let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
    // Tie broken by entry_id ascending (alpha < bravo); null release date last.
    assert_eq!(ids, vec!["lotr:alpha", "lotr:bravo", "lotr:null"]);
}

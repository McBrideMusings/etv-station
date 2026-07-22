//! Acceptance test for Sample S9 (#83): the committed `examples/samples/ghibli.yaml`
//! query channel resolves a studio by the clean `item.studio` column, while the
//! two brand tiers (A24, Disney) resolve by `item.labels` — the same channel
//! shape over three metadata-reliability tiers. Proves why one `studio` string
//! isn't enough for a brand, against a fixture catalog.

use std::path::Path;

use etv_station::catalog::{Catalog, Entry, EntrySource, Source, TagNs};
use etv_station::config::{ChannelConfig, read_channel};
use etv_station::resolve::resolve_channel;

/// Seed three tiers into one catalog:
/// - Studio Ghibli films with a clean `studio` string.
/// - Disney-brand films whose `studio` is a *sub-studio* (Pixar/Marvel/Lucasfilm)
///   but which carry a user-applied "Disney" Label.
/// - An A24 film carrying an "A24" Label (A24 is a distributor, not the studio).
/// - A control film in neither.
fn brand_catalog() -> Catalog {
    let cat = Catalog::open_in_memory().unwrap();
    let seed = |id: &str, title: &str, studio: &str, labels: &[&str]| {
        let mut e = Entry::new(id, "movie", title, Source::Plex);
        e.studio = Some(studio.to_string());
        cat.upsert_entry(&e).unwrap();
        cat.add_source(&EntrySource {
            source: Source::LocalFs,
            source_id: format!("fs-{id}"),
            entry_id: id.to_string(),
            playback_path: format!("/media/{id}.mkv"),
            last_seen: None,
        })
        .unwrap();
        for label in labels {
            cat.add_tag(id, TagNs::Label, label).unwrap();
        }
    };

    // Ghibli — clean studio string. Seeded OUT of title order so `title:asc` (not
    // insertion order) is what the assertion proves.
    seed("ghibli:spirited", "Spirited Away", "Studio Ghibli", &[]);
    seed("ghibli:totoro", "My Neighbor Totoro", "Studio Ghibli", &[]);
    seed("ghibli:mononoke", "Princess Mononoke", "Studio Ghibli", &[]);
    // Disney brand — sub-studios, unified only by the Label.
    seed("dis:toystory", "Toy Story", "Pixar", &["Disney"]);
    seed("dis:ironman", "Iron Man", "Marvel Studios", &["Disney"]);
    seed("dis:starwars", "Star Wars", "Lucasfilm", &["Disney"]);
    // A24 — distributor label.
    seed("a24:ex", "Ex Machina", "Film4", &["A24"]);
    // Control — neither.
    seed("wb:matrix", "The Matrix", "Warner Bros", &[]);
    cat
}

fn sample_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/samples/ghibli.yaml")
}

/// The studio tier: `item.studio == "Studio Ghibli"` resolves exactly the Ghibli
/// films, alphabetically (`title:asc`), excluding every other studio/brand.
#[test]
fn ghibli_sample_resolves_the_studio_alphabetically() {
    let config: ChannelConfig = read_channel(&sample_path()).expect("load ghibli sample");
    let cat = brand_catalog();
    let items = resolve_channel(&config, &sample_path(), &[], None, Some(&cat)).expect("resolve");

    let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
    // title:asc → "My Neighbor Totoro", "Princess Mononoke", "Spirited Away".
    assert_eq!(ids, ["ghibli:totoro", "ghibli:mononoke", "ghibli:spirited"]);
}

/// The brand tier: a Disney Label resolves across sub-studios where a `studio`
/// string cannot — `item.studio == "Disney"` matches nothing, but
/// `item.labels.contains("Disney")` matches every sub-studio film.
#[test]
fn disney_label_spans_substudios_where_studio_string_fails() {
    let cat = brand_catalog();

    // A single `studio` string can't capture the brand — no film's studio IS
    // "Disney" (they're Pixar / Marvel / Lucasfilm).
    assert!(
        cat.resolve_query(r#"item.studio == "Disney""#)
            .unwrap()
            .is_empty(),
        "no film's studio column is literally \"Disney\"",
    );

    // The user-applied Label unifies them.
    let mut disney = cat
        .resolve_query(r#"item.labels.contains("Disney")"#)
        .unwrap();
    disney.sort();
    assert_eq!(disney, ["dis:ironman", "dis:starwars", "dis:toystory"]);
}

/// The A24 tier: a distributor Label, queryable the same way.
#[test]
fn a24_label_is_queryable() {
    let cat = brand_catalog();
    assert_eq!(
        cat.resolve_query(r#"item.labels.contains("A24")"#).unwrap(),
        vec!["a24:ex".to_string()],
    );
    // Both operators on the promoted `studio` column resolve too.
    let non_ghibli = cat
        .resolve_query(r#"item.studio != "Studio Ghibli""#)
        .unwrap();
    assert!(non_ghibli.contains(&"a24:ex".to_string()));
    assert!(!non_ghibli.contains(&"ghibli:totoro".to_string()));
}

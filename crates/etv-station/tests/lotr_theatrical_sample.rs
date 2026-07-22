//! Acceptance test for Sample S3 (#77): the committed
//! `examples/channels/lotr-theatrical.yaml` query channel resolves only the
//! theatrical LOTR films (edition NULL) and excludes the Extended Editions,
//! oldest-first. Proves the `item.edition` filter + the NULL-as-default `!=`
//! rule (#103) — a theatrical cut has no edition yet is still matched — against
//! a fixture catalog.

use std::path::Path;

use etv_station::catalog::{Catalog, Entry, EntrySource, Source};
use etv_station::config::{ChannelConfig, read_channel};
use etv_station::resolve::resolve_channel;

/// Seed each LOTR film twice — a theatrical cut (no edition = NULL) and an
/// Extended Edition — so the edition filter has something to exclude. Release
/// dates are out of insertion order so the sort, not the seed order, decides.
fn lotr_catalog() -> Catalog {
    let cat = Catalog::open_in_memory().unwrap();
    let seed = |id: &str, title: &str, date: &str, edition: Option<&str>| {
        let mut e = Entry::new(id, "movie", title, Source::Plex);
        e.release_date = Some(date.to_string());
        e.edition = edition.map(str::to_string);
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
    // Theatrical cuts — no edition.
    seed(
        "lotr:rotk",
        "The Lord of the Rings: The Return of the King",
        "2003-12-17",
        None,
    );
    seed(
        "lotr:fotr",
        "The Lord of the Rings: The Fellowship of the Ring",
        "2001-12-19",
        None,
    );
    seed(
        "lotr:ttt",
        "The Lord of the Rings: The Two Towers",
        "2002-12-18",
        None,
    );
    // Extended Editions — must be excluded.
    seed(
        "lotr:fotr-ext",
        "The Lord of the Rings: The Fellowship of the Ring",
        "2001-12-19",
        Some("Extended Edition"),
    );
    seed(
        "lotr:ttt-ext",
        "The Lord of the Rings: The Two Towers",
        "2002-12-18",
        Some("Extended Edition"),
    );
    cat
}

fn sample_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/channels/lotr-theatrical.yaml")
}

#[test]
fn lotr_theatrical_sample_excludes_extended_editions() {
    let config: ChannelConfig = read_channel(&sample_path()).expect("load lotr-theatrical sample");
    let cat = lotr_catalog();
    let items = resolve_channel(&config, &sample_path(), &[], None, Some(&cat)).expect("resolve");

    let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
    // Only the three theatrical cuts (edition NULL), oldest-first — no `-ext`.
    assert_eq!(ids, vec!["lotr:fotr", "lotr:ttt", "lotr:rotk"]);
}

/// The load-bearing assertion for #103: a theatrical film has edition NULL, and
/// `!=` must still include it. Without NULL-as-default it would be excluded and
/// the channel would resolve empty.
#[test]
fn lotr_theatrical_sample_keeps_null_edition_films() {
    let cat = lotr_catalog();
    let mut theatrical = cat
        .resolve_query(
            r#"item.title.contains("Lord of the Rings") && item.edition != "Extended Edition""#,
        )
        .unwrap();
    theatrical.sort();
    // Exactly the three NULL-edition cuts; neither `-ext` entry.
    assert_eq!(
        theatrical,
        vec![
            "lotr:fotr".to_string(),
            "lotr:rotk".to_string(),
            "lotr:ttt".to_string()
        ],
    );
}

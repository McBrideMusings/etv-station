//! Acceptance test for Sample S4 (#78, #164): the committed
//! `examples/samples/dragonball.yaml` plays the standalone chronology
//! `examples/chronologies/dragonball.yaml` — season 1, a film, season 2 — in the
//! chronology's position order. Proves a chronology file referenced by name
//! resolves episodes and a standalone film in one run, resolves identically for
//! every channel naming it, and fails loudly on a miss or a shared position.

use std::path::{Path, PathBuf};

use etv_station::catalog::{Catalog, Entry, EntrySource, Source};
use etv_station::config::{Station, load};
use etv_station::resolve::resolve_channel;

const FILM_ID: &str = "db:blood-rubies";

/// Every Dragon Ball episode the chronology names (13 in season 1, 15 in
/// season 2) plus the film, inserted newest-first so the chronology's
/// positions — not the insert order — decide the result. `skip` leaves one
/// episode out, to prove a miss fails the resolve.
fn dragonball_catalog(skip: Option<(i64, i64)>) -> Catalog {
    let cat = Catalog::open_in_memory().unwrap();
    let add = |e: Entry| {
        let id = e.entry_id.clone();
        cat.upsert_entry(&e).unwrap();
        cat.add_source(&EntrySource {
            source: Source::LocalFs,
            source_id: format!("fs-{id}"),
            entry_id: id.clone(),
            playback_path: format!("/media/{id}.mkv"),
            last_seen: None,
            missing_since: None,
        })
        .unwrap();
    };
    for (season, count) in [(2, 15), (1, 13)] {
        for episode in (1..=count).rev() {
            if skip == Some((season, episode)) {
                continue;
            }
            let mut e = Entry::new(
                format!("db:s{season}e{episode}"),
                "episode",
                format!("Dragon Ball S{season}E{episode}"),
                Source::Plex,
            );
            e.show = Some("Dragon Ball".to_string());
            e.season = Some(season);
            e.episode = Some(episode);
            add(e);
        }
    }
    let mut film = Entry::new(
        FILM_ID,
        "movie",
        "Dragon Ball: Curse of the Blood Rubies",
        Source::Plex,
    );
    film.year = Some(1986);
    add(film);
    cat
}

fn examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

/// The committed sample and chronology, loaded through a station (the only
/// path that attaches a chronology). `extra_channels` are `(file, body)` pairs
/// written beside it; `chronology` overrides the committed chronology file.
fn load_station(extra_channels: &[(&str, String)], chronology: Option<&str>) -> Station {
    let dir = tempfile::tempdir().unwrap().keep();
    let chronology_path = match chronology {
        Some(body) => {
            let p = dir.join("chronology.yaml");
            std::fs::write(&p, body).unwrap();
            p
        }
        None => examples().join("chronologies/dragonball.yaml"),
    };
    let mut channels = vec![examples().join("samples/dragonball.yaml")];
    for (file, body) in extra_channels {
        let p = dir.join(file);
        std::fs::write(&p, body).unwrap();
        channels.push(p);
    }
    let list = |paths: &[PathBuf]| {
        paths
            .iter()
            .map(|p| format!("  - \"{}\"\n", p.display()))
            .collect::<String>()
    };
    std::fs::write(
        dir.join("station.yaml"),
        format!(
            "output_base: out\nchannels:\n{}chronologies:\n{}normalization:\n  audio: {{}}\n  video: {{}}\n",
            list(&channels),
            list(&[chronology_path]),
        ),
    )
    .unwrap();
    load(&dir.join("station.yaml")).unwrap_or_else(|e| panic!("station load: {e}"))
}

fn ids(station: &Station, name: &str, cat: &Catalog) -> Result<Vec<String>, String> {
    let ch = station
        .channels
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("channel {name} not loaded: {:?}", station.dropped));
    resolve_channel(&ch.config, &ch.config_path, &[], None, Some(cat))
        .map(|items| items.into_iter().map(|i| i.id).collect())
        .map_err(|e| e.to_string())
}

fn expected_ids() -> Vec<String> {
    let mut want: Vec<String> = (1..=13).map(|e| format!("db:s1e{e}")).collect();
    want.push(FILM_ID.to_string());
    want.extend((1..=15).map(|e| format!("db:s2e{e}")));
    want
}

#[test]
fn dragonball_sample_plays_the_chronology_in_position_order() {
    let station = load_station(&[], None);
    let got = ids(&station, "dragonball", &dragonball_catalog(None)).expect("resolve");
    assert_eq!(got, expected_ids(), "13 episodes, the film, 15 episodes");
}

#[test]
fn dragonball_sample_order_is_deterministic() {
    let station = load_station(&[], None);
    let cat = dragonball_catalog(None);
    assert_eq!(
        ids(&station, "dragonball", &cat),
        ids(&station, "dragonball", &cat)
    );
}

#[test]
fn two_channels_naming_one_chronology_resolve_identically() {
    let second = std::fs::read_to_string(examples().join("samples/dragonball.yaml"))
        .unwrap()
        .replace("number: 9", "number: 90")
        .replace("\nname: dragonball\n", "\nname: dragonball-two\n");
    let station = load_station(&[("dragonball-two.yaml", second)], None);
    let cat = dragonball_catalog(None);
    assert_eq!(
        ids(&station, "dragonball", &cat).unwrap(),
        ids(&station, "dragonball-two", &cat).unwrap()
    );
}

#[test]
fn an_entry_matching_no_catalog_item_fails_naming_the_entry() {
    let station = load_station(&[], None);
    let err = ids(&station, "dragonball", &dragonball_catalog(Some((1, 5)))).unwrap_err();
    assert!(err.contains("Dragon Ball S1E5"), "{err}");
    assert!(err.contains("position 50"), "{err}");
    assert!(err.contains("matches no catalog item"), "{err}");
}

#[test]
fn two_entries_on_one_position_fail_the_station_load_naming_both() {
    let dir = tempfile::tempdir().unwrap().keep();
    std::fs::write(
        dir.join("chronology.yaml"),
        "name: dragonball\nentries:\n  - {position: 1, show: \"Dragon Ball\", season: 1, episode: 1}\n  - {position: 1, title: \"Dragon Ball: Curse of the Blood Rubies\"}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("station.yaml"),
        format!(
            "output_base: out\nchannels:\n  - \"{}\"\nchronologies:\n  - \"chronology.yaml\"\nnormalization:\n  audio: {{}}\n  video: {{}}\n",
            examples().join("samples/dragonball.yaml").display()
        ),
    )
    .unwrap();
    let err = match load(&dir.join("station.yaml")) {
        Ok(_) => panic!("a duplicate position must fail the load"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("position 1 is claimed twice"), "{err}");
    assert!(err.contains("Dragon Ball S1E1"), "{err}");
    assert!(
        err.contains("Dragon Ball: Curse of the Blood Rubies"),
        "{err}"
    );
}

#[test]
fn two_positions_naming_one_item_fail_the_resolve_naming_both() {
    let station = load_station(
        &[],
        Some(
            "name: dragonball\nentries:\n  - {position: 1, show: \"Dragon Ball\", season: 1, episode: 1}\n  - {position: 2, show: \"Dragon Ball\", season: 1, episode: 1}\n",
        ),
    );
    let err = ids(&station, "dragonball", &dragonball_catalog(None)).unwrap_err();
    assert!(
        err.contains("position 1") && err.contains("position 2"),
        "{err}"
    );
    assert!(err.contains("same catalog item"), "{err}");
}

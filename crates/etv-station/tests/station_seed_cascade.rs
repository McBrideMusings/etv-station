//! The station → channel seed cascade (#324), exercised through the same
//! `config::load` the daemon calls — not through the private helper that
//! implements it.
//!
//! Before this existed, 37 of 64 production channels set no `seed:`, so
//! `resolve.rs`'s `config.seed.unwrap_or_else(fresh_seed)` drew a wall-clock
//! seed on every generation. A catalog refresh every 900s that changed a
//! channel's candidate set therefore reshuffled that channel's whole future
//! window, silently.

use std::fs;
use std::path::{Path, PathBuf};

use etv_station::config::{self, derive_channel_seed};

const RULE: &str = "rule:\n\
                    \x20 blocks:\n\
                    \x20   - mode: all\n\
                    \x20     order: manual\n\
                    \x20     entries:\n\
                    \x20       - kind: item\n\
                    \x20         id: x\n\
                    \x20         out_point: 30s\n\
                    \x20         source:\n\
                    \x20           kind: lavfi\n\
                    \x20           params: testsrc\n";

/// A station file plus two channel files, laid out the way the deployed
/// station is: `channels/<folder>/channel.yaml`, so the identity each channel
/// is salted with is genuinely its folder name.
fn write_station(dir: &Path, station_seed: Option<&str>, beta_seed: Option<&str>) -> PathBuf {
    let mut station = String::from(
        "tz: \"UTC\"\n\
         output_base: out\n\
         channels:\n\
         \x20 - channels/*/channel.yaml\n\
         normalization:\n\
         \x20 audio: {}\n\
         \x20 video: {}\n",
    );
    if let Some(seed) = station_seed {
        station.push_str(&format!("seed: {seed}\n"));
    }
    let station_path = dir.join("station.yaml");
    fs::write(&station_path, station).unwrap();

    for (number, folder, own_seed) in [(1, "001-for-you", None), (2, "002-for-pierce", beta_seed)] {
        let folder_dir = dir.join("channels").join(folder);
        fs::create_dir_all(&folder_dir).unwrap();
        let body = match own_seed {
            Some(seed) => format!("number: {number}\nseed: {seed}\n{RULE}"),
            None => format!("number: {number}\n{RULE}"),
        };
        fs::write(folder_dir.join("channel.yaml"), body).unwrap();
    }
    station_path
}

fn seed_of(station: &config::Station, name: &str) -> Option<u64> {
    station
        .channels
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("channel {name} not loaded"))
        .config
        .seed
}

#[test]
fn unseeded_channels_inherit_the_station_seed_salted_by_folder_name() {
    let dir = tempfile::tempdir().unwrap();
    let station_path = write_station(dir.path(), Some("1138"), None);

    let station = config::load(&station_path).unwrap();
    let a = seed_of(&station, "001-for-you");
    let b = seed_of(&station, "002-for-pierce");

    assert_eq!(a, Some(derive_channel_seed(1138, "001-for-you")));
    assert_eq!(b, Some(derive_channel_seed(1138, "002-for-pierce")));

    // The salt's whole job: two channels under one station seed must not be
    // handed the same number, or an identical candidate multiset shuffles
    // identically on both.
    assert_ne!(a, b);
    assert_ne!(a, Some(1138));
    assert_ne!(b, Some(1138));
}

#[test]
fn a_channel_that_pins_its_own_seed_ignores_the_station_seed() {
    let dir = tempfile::tempdir().unwrap();
    let station_path = write_station(dir.path(), Some("1138"), Some("7"));

    let station = config::load(&station_path).unwrap();
    assert_eq!(seed_of(&station, "002-for-pierce"), Some(7));
    // ...and its neighbour still inherits.
    assert_eq!(
        seed_of(&station, "001-for-you"),
        Some(derive_channel_seed(1138, "001-for-you"))
    );
}

#[test]
fn with_no_station_seed_an_unseeded_channel_stays_unseeded() {
    let dir = tempfile::tempdir().unwrap();
    let station_path = write_station(dir.path(), None, None);

    let station = config::load(&station_path).unwrap();
    // Unchanged behavior: `resolve.rs` falls through to `fresh_seed()`.
    assert_eq!(seed_of(&station, "001-for-you"), None);
    assert_eq!(seed_of(&station, "002-for-pierce"), None);
}

/// Renumbering a channel must not reshuffle it — the reason the salt is the
/// folder name and not the channel number (#263 makes the number separately
/// declarable). Two loads of the same folder name produce the same seed.
#[test]
fn the_derived_seed_is_stable_across_loads() {
    let dir = tempfile::tempdir().unwrap();
    let station_path = write_station(dir.path(), Some("1138"), None);

    let first = config::load(&station_path).unwrap();
    let second = config::load(&station_path).unwrap();
    assert_eq!(
        seed_of(&first, "001-for-you"),
        seed_of(&second, "001-for-you")
    );
}

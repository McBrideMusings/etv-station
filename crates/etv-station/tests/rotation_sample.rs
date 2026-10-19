//! Acceptance test for slot rotations (etv-station-xm8p.1): `examples/samples/
//! rotation.yaml` airs a different member of its `prime` rotation on each of
//! three evenings, replayed through the same generation loop the Review view
//! simulates, and the preview names the same members before anything runs.

use std::path::{Path, PathBuf};

use etv_station::catalog::Catalog;
use etv_station::config::read_channel;
use etv_station::simulate::simulate;
use time::macros::datetime;

fn sample() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/samples/rotation.yaml")
}

/// The program title of the slot each evening's 19:00 airing came from.
fn evening_blocks(days: u32) -> Vec<usize> {
    let cfg = read_channel(&sample()).unwrap();
    let cat = Catalog::open_in_memory().unwrap();
    let sim = simulate(
        &cfg,
        &sample(),
        &cat,
        None,
        days,
        datetime!(2026-10-19 00:00 UTC),
        None,
    )
    .unwrap();
    let mut evenings = Vec::new();
    for day in 0..days {
        let prime = datetime!(2026-10-19 19:00 UTC) + time::Duration::days(i64::from(day));
        let airing = sim
            .generations
            .iter()
            .flat_map(|g| &g.airings)
            .find(|a| a.start >= prime && a.start < prime + time::Duration::hours(4))
            .unwrap_or_else(|| panic!("nothing aired in the evening of day {day}"));
        evenings.push(airing.block);
    }
    evenings
}

#[test]
fn three_evenings_air_three_different_members() {
    let blocks = evening_blocks(3);
    let mut distinct = blocks.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(distinct.len(), 3, "evening blocks {blocks:?}");
    let cfg = read_channel(&sample()).unwrap();
    for block in blocks {
        let name = cfg.rule.slot_name(block).expect("a slot airs");
        assert_ne!(name, "prime", "the rotation itself never airs");
    }
}

#[test]
fn the_preview_names_the_members_the_simulation_airs() {
    let cfg = read_channel(&sample()).unwrap();
    let rows = etv_station::rotation::preview_channel(
        &cfg,
        time_tz::timezones::db::UTC,
        &Default::default(),
        datetime!(2026-10-19 00:00 UTC),
        3,
    );
    assert_eq!(rows.len(), 3);
    let previewed: Vec<usize> = rows
        .iter()
        .map(|r| cfg.rule.slot_index(&r.airs).unwrap())
        .collect();
    assert_eq!(previewed, evening_blocks(3));
}

//! Acceptance test for #14: dayparting as a sequencer plugin, proved against
//! the committed reusable script (`examples/plugins/daypart-sequencer.rhai`)
//! rather than a channel-specific one — every network mirror points its
//! `sequencer:` at this same file and expresses its schedule through each
//! pool's own `config:` block (ADR 0002's generic carrier, not a new schema
//! field, per ADR 0004).
//!
//! Each test builds a tiny synthetic catalog with hand-picked, evenly-sized
//! durations so the exact air sequence is predictable by hand — the point is
//! to prove the placement/drift/overlap rules #14's acceptance criteria
//! state, not to exercise a real library.

use std::path::Path;
use std::time::Duration;

use etv_station::catalog::{Catalog, Entry, EntrySource, Source};
use etv_station::config::{GroupBy, Order, Pool, Select};
use etv_station::resume::GenerationState;
use etv_station::score::{ScoreEnv, ScoreInputs};
use etv_station::sequence::{Window, build};
use time::OffsetDateTime;
use time::macros::datetime;

/// Add a movie to the catalog. `duration_ms` of `None` leaves it with no
/// measured duration at all — the item the script sees as
/// `item.duration_ms == ()`.
fn add_movie(cat: &Catalog, id: &str, title: &str, duration_ms: Option<i64>) {
    let mut e = Entry::new(id, "movie", title, Source::Plex);
    e.duration_ms = duration_ms;
    cat.upsert_entry(&e).unwrap();
    cat.add_source(&EntrySource {
        source: Source::LocalFs,
        source_id: format!("fs-{id}"),
        entry_id: id.to_string(),
        playback_path: format!("/media/{id}.mkv"),
        last_seen: None,
    })
    .unwrap();
}

fn pool(name: &str, expr: &str, config: Option<serde_json::Value>) -> Pool {
    Pool {
        name: name.into(),
        expr: Some(expr.into()),
        plugin: None,
        groups: Vec::new(),
        order: Some(Order::parse("title:asc").unwrap()),
        bucket_order: None,
        group_by: GroupBy::Show,
        select: Select::default(),
        rotate: Default::default(),
        advance: Default::default(),
        on_short: Default::default(),
        constraints: None,
        config,
        // Every pool here is `expr`, so no plugin is reached and no
        // capability grant is needed (#167).
        capabilities: Vec::new(),
        datastores: Vec::new(),
    }
}

fn daypart(hour_start: i64, hour_end: i64) -> serde_json::Value {
    serde_json::json!({ "hour_start": hour_start, "hour_end": hour_end })
}

/// Run the committed daypart sequencer over `pools`, filling `fill_secs` of
/// airtime from `from`, and return the entry ids it lays down.
///
/// `tz` is the station's configured zone by IANA name; `None` leaves
/// `ScoreInputs::tz` unset, which is what the stateless entry points hand a
/// sequencer and which the script must read as UTC.
fn run(
    cat: &Catalog,
    pools: &[Pool],
    tz: Option<&str>,
    from: OffsetDateTime,
    fill_secs: u64,
) -> Result<Vec<String>, String> {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/plugins/daypart-sequencer.rhai")
        .canonicalize()
        .expect("examples/plugins/daypart-sequencer.rhai must exist");
    let inputs = ScoreInputs {
        tz: tz.map(|name| etv_station::tz::parse(name).unwrap()),
        ..Default::default()
    };
    build(
        cat,
        pools,
        &[],
        None,
        &script,
        &GenerationState::empty(),
        ScoreEnv {
            inputs: &inputs,
            base_dir: Path::new("."),
        },
        Window {
            from: from.unix_timestamp(),
            fill: Some(Duration::from_secs(fill_secs)),
        },
    )
    .map(|(ids, _)| ids)
}

/// A block naming the sequencer places each pool in its declared hour range,
/// and the default pool (no `hour_start`/`hour_end`) fills every hour no
/// daypart claims — the channel is never dark across a full day.
#[test]
fn pools_place_in_their_declared_hours_and_the_default_fills_the_rest() {
    let cat = Catalog::open_in_memory().unwrap();
    add_movie(&cat, "filler-1", "Filler", Some(3_600_000)); // 60 min
    add_movie(&cat, "morning-1", "Morning Show", Some(3_600_000)); // 60 min
    add_movie(&cat, "prime-1", "Prime Movie", Some(3_600_000)); // 60 min

    let pools = vec![
        pool(
            "morning",
            "item.title == \"Morning Show\"",
            Some(daypart(6, 12)),
        ),
        pool(
            "primetime",
            "item.title == \"Prime Movie\"",
            Some(daypart(20, 24)),
        ),
        pool("default", "item.title == \"Filler\"", None),
    ];

    // 2026-04-13 00:00 UTC is a Monday, and local midnight in UTC.
    let ids = run(
        &cat,
        &pools,
        Some("UTC"),
        datetime!(2026-04-13 00:00:00 UTC),
        24 * 3600,
    )
    .unwrap();

    // 60-minute items across a 24h day: 6h filler, 6h morning, 8h filler,
    // 4h primetime — 24 items, no gap and no item split across an hour.
    assert_eq!(ids.len(), 24, "got {ids:?}");
    for id in &ids[0..6] {
        assert_eq!(id, "filler-1", "hours 0-6 should be the default pool");
    }
    for id in &ids[6..12] {
        assert_eq!(id, "morning-1", "hours 6-12 should be the morning daypart");
    }
    for id in &ids[12..20] {
        assert_eq!(id, "filler-1", "hours 12-20 should fall back to default");
    }
    for id in &ids[20..24] {
        assert_eq!(id, "prime-1", "hours 20-24 should be the primetime daypart");
    }
}

/// An item that would run past its daypart's end still airs whole — never
/// truncated with `out_point` to hold the grid — and the next daypart starts
/// late by exactly that overrun (#14 decision 3, ADR 0004).
#[test]
fn an_overrunning_item_airs_whole_and_the_next_daypart_starts_late() {
    let cat = Catalog::open_in_memory().unwrap();
    // 100 minutes: overruns the 60-minute `early` slot by 40 minutes.
    add_movie(&cat, "a-item", "A Show", Some(100 * 60 * 1000));
    add_movie(&cat, "b-item", "B Show", Some(30 * 60 * 1000));

    let pools = vec![
        pool("early", "item.title == \"A Show\"", Some(daypart(0, 1))),
        pool("late", "item.title == \"B Show\"", Some(daypart(1, 2))),
        // A default is required by the script even though this window never
        // reaches an hour neither daypart claims.
        pool("default", "item.title == \"A Show\"", None),
    ];

    let ids = run(
        &cat,
        &pools,
        Some("UTC"),
        datetime!(2026-04-13 00:00:00 UTC),
        3 * 3600,
    )
    .unwrap();

    assert!(ids.len() >= 2, "got {ids:?}");
    assert_eq!(ids[0], "a-item", "the early daypart's item airs first");
    assert_eq!(
        ids[1], "b-item",
        "the late daypart's item plays next even though its 01:00 start already passed"
    );
}

/// Given a choice, the sequencer prefers an item that fits the time left
/// before the next boundary over one drawn earlier in the pool's own order —
/// the mechanism that keeps drift bounded instead of compounding (#14
/// decision 3, ADR 0004).
#[test]
fn the_sequencer_prefers_an_item_that_fits_the_remaining_slot() {
    let cat = Catalog::open_in_memory().unwrap();
    // "Long" is drawn first in pool order but never fits inside a one-hour
    // daypart; "short" always does, so it should be the only thing chosen.
    add_movie(&cat, "long-item", "A Long Film", Some(90 * 60 * 1000));
    add_movie(&cat, "short-item", "B Short Film", Some(20 * 60 * 1000));

    let pools = vec![
        pool(
            "test",
            "item.title in [\"A Long Film\", \"B Short Film\"]",
            Some(daypart(0, 1)),
        ),
        pool("default", "item.title == \"A Long Film\"", None),
    ];

    let ids = run(
        &cat,
        &pools,
        Some("UTC"),
        datetime!(2026-04-13 00:00:00 UTC),
        3600,
    )
    .unwrap();

    assert!(!ids.is_empty(), "got {ids:?}");
    assert!(
        ids.iter().all(|id| id == "short-item"),
        "the long film never fits a one-hour daypart and should never be chosen: {ids:?}"
    );
}

/// Two dayparts claiming the same local hour fail `arrange()`, naming both
/// pools rather than picking a winner (#14 decision 6).
#[test]
fn two_dayparts_claiming_the_same_hour_fail_naming_both() {
    let cat = Catalog::open_in_memory().unwrap();
    add_movie(&cat, "x-item", "X Show", Some(3_600_000));
    add_movie(&cat, "y-item", "Y Show", Some(3_600_000));
    add_movie(&cat, "d-item", "Default Show", Some(3_600_000));

    let pools = vec![
        pool("alpha", "item.title == \"X Show\"", Some(daypart(20, 23))),
        pool("bravo", "item.title == \"Y Show\"", Some(daypart(22, 24))),
        pool("default", "item.title == \"Default Show\"", None),
    ];

    // No tz: the UTC fallback, which is irrelevant to an overlap check.
    let err = run(&cat, &pools, None, datetime!(2026-04-13 00:00:00 UTC), 3600).unwrap_err();

    assert!(err.contains("alpha"), "got {err}");
    assert!(err.contains("bravo"), "got {err}");
}

/// A block with no default pool (every pool declares an hour range) fails
/// naming the problem, rather than the channel going dark the first hour
/// nothing claims.
#[test]
fn a_block_with_no_default_pool_fails_at_arrange() {
    let cat = Catalog::open_in_memory().unwrap();
    add_movie(&cat, "x-item", "X Show", Some(3_600_000));

    let pools = vec![pool(
        "alpha",
        "item.title == \"X Show\"",
        Some(daypart(0, 24)),
    )];

    let err = run(&cat, &pools, None, datetime!(2026-04-13 00:00:00 UTC), 3600).unwrap_err();

    assert!(err.contains("no default pool"), "got {err}");
}

/// Times resolve in the station's configured tz, not UTC — the same absolute
/// instant places a different pool depending on which zone the station runs
/// in (#14 decision 3, ADR 0004).
#[test]
fn placement_reads_the_stations_configured_tz_not_utc() {
    let cat = Catalog::open_in_memory().unwrap();
    add_movie(&cat, "morning-1", "Morning Show", Some(3_600_000));
    add_movie(&cat, "default-1", "Filler", Some(3_600_000));

    let pools = vec![
        pool(
            "morning",
            "item.title == \"Morning Show\"",
            Some(daypart(6, 12)),
        ),
        pool("default", "item.title == \"Filler\"", None),
    ];
    // 2026-04-13 10:00 UTC: 10:00 local in UTC (inside the 6-12 daypart) but
    // 05:00 local in America/Chicago (before it opens).
    let from = datetime!(2026-04-13 10:00:00 UTC);

    let utc_ids = run(&cat, &pools, Some("UTC"), from, 3600).unwrap();
    assert_eq!(utc_ids[0], "morning-1", "10:00 UTC is inside 6-12 in UTC");

    let chicago_ids = run(&cat, &pools, Some("America/Chicago"), from, 3600).unwrap();
    assert_eq!(
        chicago_ids[0], "default-1",
        "the same instant is 05:00 in America/Chicago, before the daypart opens"
    );
}

/// A daypart's `weekdays` list, not only its hours, gates when it claims a
/// slot — the same absolute hour falls to the default pool on a day the
/// daypart does not name (#14 decision 1: "on declared weekdays").
#[test]
fn a_daypart_only_claims_its_declared_weekdays() {
    let cat = Catalog::open_in_memory().unwrap();
    add_movie(&cat, "toonami-1", "Toonami Anime", Some(3_600_000));
    add_movie(&cat, "default-1", "Filler", Some(3_600_000));

    let pools = vec![
        pool(
            "toonami",
            "item.title == \"Toonami Anime\"",
            Some(serde_json::json!({
                "hour_start": 0, "hour_end": 3, "weekdays": ["sat"],
            })),
        ),
        pool("default", "item.title == \"Filler\"", None),
    ];

    // 2026-04-13 is a Monday: 01:00 local falls inside the daypart's hour
    // range but not its weekday list, so the default pool fills it.
    let monday_ids = run(
        &cat,
        &pools,
        Some("UTC"),
        datetime!(2026-04-13 01:00:00 UTC),
        3600,
    )
    .unwrap();
    assert_eq!(
        monday_ids[0], "default-1",
        "Monday 01:00 is inside 0-3 but not a Saturday, so the default fills it"
    );

    // 2026-04-18 is a Saturday: the same 01:00 local hour is now claimed.
    let saturday_ids = run(
        &cat,
        &pools,
        Some("UTC"),
        datetime!(2026-04-18 01:00:00 UTC),
        3600,
    )
    .unwrap();
    assert_eq!(
        saturday_ids[0], "toonami-1",
        "Saturday 01:00 is inside both the hour range and the weekday list"
    );
}

/// An item with no measured catalog duration is judged by the same 30-minute
/// nominal length the cursor actually advances it by, not treated as free —
/// it must not be preferred over a measured item that genuinely fits a short
/// remaining slot just because it looks like it costs nothing.
#[test]
fn an_unmeasured_item_is_not_treated_as_free_by_the_fit_check() {
    let cat = Catalog::open_in_memory().unwrap();
    add_movie(&cat, "unmeasured-item", "No Duration On File", None);
    add_movie(&cat, "short-item", "Five Minute Short", Some(5 * 60 * 1000));

    let pools = vec![
        pool(
            "test",
            "item.title in [\"No Duration On File\", \"Five Minute Short\"]",
            Some(daypart(0, 1)),
        ),
        pool("default", "item.title == \"No Duration On File\"", None),
    ];

    // A 10-minute window: the unmeasured item's real charge (30 min, the
    // same fallback `arrange()` advances the cursor by) does not fit, but
    // the measured 5-minute item does.
    let ids = run(
        &cat,
        &pools,
        Some("UTC"),
        datetime!(2026-04-13 00:00:00 UTC),
        10 * 60,
    )
    .unwrap();

    assert_eq!(
        ids[0], "short-item",
        "the unmeasured item must not look free to the fit-check: {ids:?}"
    );
}

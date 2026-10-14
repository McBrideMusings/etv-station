//! A pattern block's `schedule:` (ADR 0021), driven through the daemon's own
//! generation loop (`simulate::simulate`) for a week of hourly ticks: two
//! entries, each rotating two pools in three-episode runs. Across every
//! generation seam and every switch between entries, each show continues from
//! the episode after the one it last aired, each run starts inside its entry's
//! hours, and an entry returning the next day picks up its pattern at the step
//! it stopped on.

use std::collections::HashMap;

use etv_station::catalog::{Catalog, Entry, EntrySource, Source};
use etv_station::config::read_channel;
use etv_station::simulate::simulate;
use time::macros::datetime;
use time_tz::OffsetDateTimeExt;

const EPISODES: i64 = 120;

/// Pool → its shows. Every episode runs half an hour.
const POOLS: [(&str, [&str; 2]); 4] = [
    ("day-a", ["Ant", "Bee"]),
    ("day-b", ["Cat", "Dog"]),
    ("night-c", ["Eel", "Fox"]),
    ("night-d", ["Gnu", "Hen"]),
];

const CHANNEL: &str = r#"
number: 1
name: scheduled
window_days: 1
roll_interval: "3600s"
seed: 7
rule:
  blocks:
    - pools:
        - { name: day-a, expr: 'item.show in ["Ant", "Bee"]', order: "season:asc,episode:asc", rotate: visit, advance: resume }
        - { name: day-b, expr: 'item.show in ["Cat", "Dog"]', order: "season:asc,episode:asc", rotate: visit, advance: resume }
        - { name: night-c, expr: 'item.show in ["Eel", "Fox"]', order: "season:asc,episode:asc", rotate: visit, advance: resume }
        - { name: night-d, expr: 'item.show in ["Gnu", "Hen"]', order: "season:asc,episode:asc", rotate: visit, advance: resume }
      schedule:
        - name: day
          hours: { start: 6, end: 21 }
          pattern:
            - { pool: day-a, take: 3 }
            - { pool: day-b, take: 3 }
        - name: night
          hours: { start: 21, end: 6 }
          pattern:
            - { pool: night-c, take: 3 }
            - { pool: night-d, take: 3 }
"#;

fn catalog(duration_ms: Option<i64>) -> Catalog {
    let cat = Catalog::open_in_memory().unwrap();
    for (_, shows) in POOLS {
        for show in shows {
            for ep in 1..=EPISODES {
                let id = format!("{show}-{ep:03}");
                let e = Entry {
                    show: Some(show.to_string()),
                    show_id: Some(show.to_string()),
                    season: Some(1),
                    episode: Some(ep),
                    duration_ms,
                    ..Entry::new(&id, "episode", &id, Source::Plex)
                };
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
            }
        }
    }
    cat
}

fn pool_of(show: &str) -> &'static str {
    POOLS
        .iter()
        .find(|(_, shows)| shows.contains(&show))
        .map(|(pool, _)| *pool)
        .unwrap()
}

#[test]
fn a_week_of_scheduled_runs_continues_every_show_and_keeps_each_pool_in_its_hours() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("channel.yaml");
    std::fs::write(&path, CHANNEL).unwrap();
    let config = read_channel(&path).unwrap();
    let cat = catalog(Some(30 * 60 * 1000));
    let chicago = etv_station::tz::parse("America/Chicago").unwrap();

    // Local midnight, Monday 2026-03-02; the week crosses the 03-08 DST change.
    let start = datetime!(2026-03-02 06:00 UTC);
    let sim = simulate(&config, &path, &cat, None, 7, start, Some(chicago)).unwrap();
    assert!(
        sim.generations.len() > 7 * 12,
        "hourly ticks over a week: {} generations",
        sim.generations.len()
    );

    let airings: Vec<_> = sim
        .generations
        .iter()
        .flat_map(|g| &g.airings)
        .filter(|a| a.start < start + time::Duration::days(7))
        .collect();

    // Every show continues one episode at a time across the whole week.
    let mut last_episode: HashMap<String, i64> = HashMap::new();
    for a in &airings {
        let (show, ep) = a.id.split_once('-').unwrap();
        let ep: i64 = ep.parse().unwrap();
        if let Some(prev) = last_episode.insert(show.to_string(), ep) {
            assert_eq!(
                ep,
                prev + 1,
                "{show} jumped from episode {prev} to {ep} at {}",
                a.start
            );
        }
    }

    // Split the week into runs: consecutive airings from one pool.
    let mut runs: Vec<(&str, time::OffsetDateTime, usize)> = Vec::new();
    for a in &airings {
        let pool = pool_of(a.id.split_once('-').unwrap().0);
        match runs.last_mut() {
            Some((p, _, n)) if *p == pool => *n += 1,
            _ => runs.push((pool, a.start, 1)),
        }
    }
    assert!(
        runs[1..runs.len() - 1].iter().all(|(_, _, n)| *n == 3),
        "every run but the clipped edges is three episodes"
    );

    // Each run starts inside its own entry's local hours.
    for (pool, at, _) in &runs {
        let hour = at.to_timezone(chicago).hour();
        let day_hours = (6..21).contains(&hour);
        let is_day_pool = pool.starts_with("day");
        assert_eq!(
            is_day_pool, day_hours,
            "pool {pool} started a run at {hour:02}:00 local ({at})"
        );
    }

    // Each entry alternates its two steps across the whole week — including
    // across the 15 hours its rival holds the air — so a day that stopped
    // after day-a opens the next morning with day-b.
    for (entry, steps) in [
        ("day", ["day-a", "day-b"]),
        ("night", ["night-c", "night-d"]),
    ] {
        let order: Vec<&str> = runs
            .iter()
            .map(|(p, _, _)| *p)
            .filter(|p| p.starts_with(entry))
            .collect();
        for pair in order.windows(2) {
            assert_ne!(pair[0], pair[1], "{entry} repeated a step: {order:?}");
        }
        assert!(order.iter().all(|p| steps.contains(p)));
    }

    // And every airing's audit trail names the entry that drew it.
    let entry_of: HashMap<&str, &str> = POOLS
        .iter()
        .map(|(p, _)| (*p, if p.starts_with("day") { "day" } else { "night" }))
        .collect();
    for a in &airings {
        let pool = pool_of(a.id.split_once('-').unwrap().0);
        let named = a
            .metadata
            .as_ref()
            .and_then(|m| m["audit"].as_array())
            .and_then(|stages| {
                stages
                    .iter()
                    .find_map(|s| s["detail"]["schedule"].as_str().map(str::to_string))
            });
        assert_eq!(named.as_deref(), Some(entry_of[pool]), "{}", a.id);
    }
}

/// The schedule's clock is the runtime it has laid, so a library the catalog
/// has no lengths for must still move it — each item at the channel's
/// `nominal_item_secs` — or the walk never reaches the end of its window.
#[test]
fn a_schedule_over_unmeasured_items_still_reaches_the_end_of_its_window() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("channel.yaml");
    std::fs::write(
        &path,
        format!("{CHANNEL}scoring:\n  nominal_item_secs: 1800\n"),
    )
    .unwrap();
    let config = read_channel(&path).unwrap();
    let cat = catalog(None);

    let items =
        etv_station::resolve::resolve_channel(&config, &path, &[], None, Some(&cat)).unwrap();
    // The stateless entry point lays 24 hours: 48 half-hour items, plus the
    // rest of the visit that crosses the end.
    assert!(
        (48..=50).contains(&items.len()),
        "{} items for 24 hours of nominal half-hours",
        items.len()
    );
}

//! A projection started at a channel's frontier airs what the daemon's own
//! next generation airs from the same `.resume` state and play history — and
//! not what a fresh simulation from that instant would.
//!
//! The pool is a seeded-random list walked with `advance: resume`, so the
//! second generation continues down the first generation's list. A projection
//! that ignored the seed would reopen at the top of a new list.

use etv_station::catalog::{Catalog, Entry, EntrySource, Source};
use etv_station::config::read_channel;
use etv_station::generation::{
    AiringsLedger, ChannelLedger, GenerationEnv, generate, ledger_reach,
};
use etv_station::history::{HistoryDb, PlayRecord};
use etv_station::resume::ResumeMap;
use etv_station::simulate::{Ledger, simulate, simulate_from};
use time::Duration;
use time::macros::datetime;

const CHANNEL: &str = r#"
number: 1
name: ch
window_days: 1
roll_interval: "1h"
retention_days: 1
chunk_hours: 6
seed: 7
rule:
  blocks:
    - mode: "all"
      pools:
        - name: movies
          expr: 'item.type == "movie"'
          order: "random"
          advance: resume
      pattern:
        - pool: movies
          take: 6
"#;

fn catalog() -> Catalog {
    let cat = Catalog::open_in_memory().unwrap();
    for n in 0..60 {
        let id = format!("m{n:02}");
        let mut e = Entry::new(&id, "movie", &id, Source::Plex);
        e.duration_ms = Some(30 * 60 * 1000);
        cat.upsert_entry(&e).unwrap();
        cat.add_source(&EntrySource {
            source: Source::LocalFs,
            source_id: format!("fs-{id}"),
            entry_id: id.clone(),
            playback_path: format!("/media/{id}.mkv"),
            last_seen: None,
            missing_since: None,
            unairable_reason: None,
        })
        .unwrap();
    }
    cat
}

#[test]
fn a_projection_from_the_frontier_airs_what_the_daemon_airs_next() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("channel.yaml");
    std::fs::write(&path, CHANNEL).unwrap();
    let cfg = read_channel(&path).unwrap();
    let cat = catalog();
    let db = HistoryDb::open_in_memory().unwrap();
    let t0 = datetime!(2026-01-01 00:00 UTC);

    // The daemon's first generation, recorded the way its catch-up records it.
    let mut resume = ResumeMap::new();
    let env = |now| GenerationEnv {
        config: &cfg,
        config_path: &path,
        identity_roots: &[],
        path_index: None,
        history: Default::default(),
        account_id: None,
        tz: None,
        now,
    };
    let ledger = ChannelLedger {
        db: &db,
        channel: "ch",
    };
    let first = generate(
        &env(t0),
        Some(&cat),
        &ledger,
        &resume,
        t0,
        t0 + Duration::days(1),
    )
    .unwrap();
    let mut frontier = t0;
    let records: Vec<PlayRecord> = first
        .items
        .iter()
        .map(|item| {
            let start = frontier;
            frontier += Duration::minutes(30);
            PlayRecord {
                entry_id: item.id.clone(),
                show_id: first.show_ids.get(&item.id).cloned(),
                start,
                played_at: t0,
                ..Default::default()
            }
        })
        .collect();
    assert!(!records.is_empty());
    db.record("ch", &records).unwrap();
    resume.adopt(first.resume, t0);

    // The daemon's next generation, from the frontier.
    let next = generate(
        &env(frontier),
        Some(&cat),
        &ledger,
        &resume,
        frontier,
        frontier + Duration::days(1),
    )
    .unwrap();
    let daemon: Vec<&str> = next.items.iter().map(|i| i.id.as_str()).collect();

    // The projection, seeded the way the web UI seeds it from the same files.
    let seeded = Ledger::seeded(
        ledger.tail(ledger_reach(&cfg)).unwrap(),
        ledger.series_cursor().unwrap(),
        ledger.global_cursor().unwrap(),
    );
    let projected = simulate_from(
        &cfg,
        &path,
        &cat,
        None,
        1,
        frontier,
        None,
        seeded,
        resume.clone(),
    )
    .unwrap();
    let from_frontier: Vec<&str> = projected.generations[0]
        .airings
        .iter()
        .map(|a| a.id.as_str())
        .collect();
    assert_eq!(from_frontier, daemon);

    // A fresh simulation from the same instant reopens the list instead.
    let fresh = simulate(&cfg, &path, &cat, None, 1, frontier, None).unwrap();
    let from_scratch: Vec<&str> = fresh.generations[0]
        .airings
        .iter()
        .map(|a| a.id.as_str())
        .collect();
    assert_ne!(from_scratch, daemon);
}

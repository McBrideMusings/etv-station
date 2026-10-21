//! Acceptance test for show arcs (#163): the committed
//! `examples/samples/black-clover-arcs.yaml` plus `examples/arcs/black-clover.yaml`
//! cut Black Clover's 170 single-season episodes into named arcs. A
//! season-grouped pool airs one arc per visit, `filter.seasons` selects an arc
//! by name, and the load fails loudly on a bad arcs file or an unknown name.

use std::path::{Path, PathBuf};

use etv_station::catalog::{Catalog, Entry, EntrySource, Source};
use etv_station::config::{Station, load};
use etv_station::resolve::resolve_channel;
use etv_station::simulate::simulate;
use time::macros::datetime;

/// Black Clover as Plex serves it: every episode in season 1, numbered
/// absolutely 1..=`count`. The committed arcs end at episode 154, so a count
/// past that leaves a trailing run.
fn black_clover(count: i64) -> Catalog {
    let cat = Catalog::open_in_memory().unwrap();
    for n in 1..=count {
        let id = format!("bc-{n:03}");
        let mut e = Entry::new(&id, "episode", format!("Black Clover {n}"), Source::Plex);
        e.show = Some("Black Clover".to_string());
        e.show_id = Some("show:bc".to_string());
        e.season = Some(1);
        e.episode = Some(n);
        e.absolute_episode = Some(n);
        e.duration_ms = Some(24 * 60 * 1000);
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

fn examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

/// A station listing `channels` and the arcs files in `arcs`, written to a
/// fresh directory. Returns the load result so a test can expect a failure.
fn try_load(
    channels: &[PathBuf],
    arcs: &[PathBuf],
) -> Result<Station, etv_station::errors::ConfigError> {
    let dir = tempfile::tempdir().unwrap().keep();
    let list = |paths: &[PathBuf]| {
        paths
            .iter()
            .map(|p| format!("  - \"{}\"\n", p.display()))
            .collect::<String>()
    };
    std::fs::write(
        dir.join("station.yaml"),
        format!(
            "output_base: out\nchannels:\n{}arcs:\n{}normalization:\n  audio: {{}}\n  video: {{}}\n",
            list(channels),
            list(arcs),
        ),
    )
    .unwrap();
    load(&dir.join("station.yaml"))
}

fn write(name: &str, body: &str) -> PathBuf {
    let p = tempfile::tempdir().unwrap().keep().join(name);
    std::fs::write(&p, body).unwrap();
    p
}

fn committed() -> Station {
    try_load(
        &[examples().join("samples/black-clover-arcs.yaml")],
        &[examples().join("arcs/black-clover.yaml")],
    )
    .unwrap_or_else(|e| panic!("station load: {e}"))
}

/// The arc name an episode id falls in under the committed file.
fn arc_of(id: &str) -> &'static str {
    let n: i64 = id.trim_start_matches("bc-").parse().unwrap();
    match n {
        1..=51 => "Dungeon",
        52..=102 => "Royal Knights",
        103..=154 => "Witch Queen",
        _ => "trailing",
    }
}

#[test]
fn a_take_all_visit_airs_one_arc_end_to_end_never_the_whole_season() {
    let station = committed();
    let ch = &station.channels[0];
    let cat = black_clover(160);
    let sim = simulate(
        &ch.config,
        &ch.config_path,
        &cat,
        None,
        7,
        datetime!(2026-10-19 00:00 UTC),
        None,
    )
    .unwrap();
    let ids: Vec<&str> = sim
        .generations
        .iter()
        .flat_map(|g| &g.airings)
        .map(|a| a.id.as_str())
        .collect();

    // Collapse the air sequence to (arc, run length).
    let mut runs: Vec<(&str, usize)> = Vec::new();
    for id in &ids {
        match runs.last_mut() {
            Some((arc, n)) if *arc == arc_of(id) => *n += 1,
            _ => runs.push((arc_of(id), 1)),
        }
    }
    let lens: Vec<usize> = runs.iter().map(|r| r.1).collect();
    assert!(
        runs.iter().all(|(_, n)| *n <= 52),
        "no visit may run past one arc (longest arc is 52): {runs:?}"
    );
    assert_eq!(
        runs.iter().take(4).copied().collect::<Vec<_>>(),
        vec![
            ("Dungeon", 51),
            ("Royal Knights", 51),
            ("Witch Queen", 52),
            ("trailing", 6)
        ],
        "arcs in order, then the 6 episodes past the last arc as one bucket \
         (run lengths {lens:?})"
    );
}

#[test]
fn filter_seasons_keeps_only_the_named_arc() {
    let channel = write(
        "arc-filter.yaml",
        r#"number: 90
name: arc-filter
window_days: 1
roll_interval: "60s"
retention_days: 1
rule:
  blocks:
    - mode: "all"
      program: { title: "Black Clover" }
      entries:
        - kind: query
          query: 'item.show == "Black Clover"'
      filter:
        seasons: ["Royal Knights"]
"#,
    );
    let station = try_load(&[channel], &[examples().join("arcs/black-clover.yaml")]).unwrap();
    let ch = &station.channels[0];
    let got: Vec<String> = resolve_channel(
        &ch.config,
        &ch.config_path,
        &[],
        None,
        Some(&black_clover(160)),
    )
    .unwrap()
    .into_iter()
    .map(|i| i.id)
    .collect();
    let want: Vec<String> = (52..=102).map(|n| format!("bc-{n:03}")).collect();
    assert_eq!(got, want, "episodes 52-102 and nothing else");
}

#[test]
fn an_unknown_arc_name_fails_the_load_naming_it() {
    let channel = write(
        "arc-typo.yaml",
        r#"number: 91
name: arc-typo
window_days: 1
roll_interval: "60s"
retention_days: 1
rule:
  blocks:
    - mode: "all"
      program: { title: "x" }
      entries:
        - kind: query
          query: 'item.show == "Black Clover"'
      filter:
        seasons: ["Dungeoun"]
"#,
    );
    let station = try_load(&[channel], &[examples().join("arcs/black-clover.yaml")]).unwrap();
    assert!(
        station.channels.is_empty(),
        "the typo channel must not load"
    );
    let reason = &station.dropped[0].reason;
    assert!(reason.contains("\"Dungeoun\""), "{reason}");
    assert!(
        reason.contains("Dungeon"),
        "names the arcs that exist: {reason}"
    );
}

#[test]
fn a_bad_arcs_file_fails_the_station_load() {
    let channel = examples().join("samples/black-clover-arcs.yaml");
    let overlap = write(
        "overlap.yaml",
        "show: \"Naruto\"\narcs:\n  - { name: A, episodes: \"1-20\" }\n  - { name: B, episodes: \"18-30\" }\n",
    );
    let err = try_load(std::slice::from_ref(&channel), &[overlap])
        .unwrap_err()
        .to_string();
    assert!(err.contains("Naruto") && err.contains("overlap"), "{err}");

    let copy = write(
        "black-clover-again.yaml",
        &std::fs::read_to_string(examples().join("arcs/black-clover.yaml")).unwrap(),
    );
    let twice = try_load(
        &[channel],
        &[examples().join("arcs/black-clover.yaml"), copy],
    )
    .unwrap_err()
    .to_string();
    assert!(
        twice.contains("Black Clover") && twice.contains("twice"),
        "{twice}"
    );
}

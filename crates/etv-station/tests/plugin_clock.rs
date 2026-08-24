//! Verification for #357: closing the plugin determinism vector.
//!
//! A Rhai plugin's `timestamp()`/`elapsed()` read a clock seeded from the
//! generation's own `window_start` at the top of `resolve_channel_with_resume`
//! (`score::PLUGIN_CLOCK`, set via `score::set_plugin_clock`) — never the real
//! wall clock. This proves the three things that follow from that:
//!
//! - the same `window_start` resolved twice, with a plugin reading the clock,
//!   produces byte-identical output;
//! - two different `window_start` values produce different output — the clock
//!   is fixed *per generation*, not frozen globally;
//! - a plugin spin loop on `elapsed()` still terminates, because the clock
//!   keeps advancing by a nonzero step.
//!
//! Modelled on `scorer_plugin.rs`'s harness (in-memory catalog, a
//! `write_plugin` helper, direct `resolve_channel_with_resume` calls).

use std::path::{Path, PathBuf};

use etv_station::catalog::{Catalog, Entry, EntrySource, Source};
use etv_station::config::{
    Advance, BlockInclude, ChannelConfig, Mode, PatternStep, Pool, RuleConfig,
};
use etv_station::resolve::resolve_channel_with_resume;
use etv_station::resume::GenerationState;
use etv_station::score::ScoreInputs;
use time::macros::datetime;

/// Enough movies that a permutation is actually observable.
fn catalog() -> Catalog {
    let cat = Catalog::open_in_memory().unwrap();
    for id in ["m0", "m1", "m2", "m3", "m4", "m5", "m6", "m7"] {
        let e = Entry::new(id, "movie", id, Source::Plex);
        cat.upsert_entry(&e).unwrap();
        cat.add_source(&EntrySource {
            source: Source::LocalFs,
            source_id: format!("fs-{id}"),
            entry_id: id.to_string(),
            playback_path: format!("/media/{id}.mkv"),
            last_seen: None,
            missing_since: None,
        })
        .unwrap();
    }
    cat
}

fn write_plugin(dir: &tempfile::TempDir, body: &str) -> PathBuf {
    let path = dir.path().join("clock.rhai");
    std::fs::write(&path, body).unwrap();
    path
}

/// A pool-provider plugin that:
/// - spins on `elapsed(t0)` until 20ms of (plugin-visible) time has passed,
///   which only terminates because the injected clock keeps advancing;
/// - seeds a permutation from the *absolute* `timestamp()` reading `t0`, not
///   just from `elapsed()` — `elapsed()` alone is relative to `t0` and would
///   read the same regardless of `window_start`, which would make this
///   fixture blind to the very thing the second test below checks for.
const CLOCK_READING_PLUGIN: &str = r#"
fn hooks() { ["pool_provider"] }
fn capabilities() { ["catalog_read"] }
fn sources() { #{ movies: `item.type == "movie"` } }
fn pick(ctx) {
    let ids = [];
    for item in ctx.sets.movies { ids.push(item.entry_id); }

    let t0 = timestamp();
    let spins = 0;
    while elapsed(t0) < 0.02 { spins += 1; }

    // Absolute reading (t0), not just the relative spin count, so the seed
    // — and therefore the permutation — depends on which window_start this
    // generation was seeded from.
    let seed = ((t0 * 1000.0).to_int() + spins * 1000003) % 999999937;

    let scored = [];
    let idx = 0;
    for id in ids {
        let key = ((seed + 1) * (idx + 1) * 7919) % 999999937;
        scored.push(#{ id: id, key: key });
        idx += 1;
    }
    scored.sort(|a, b| if a.key < b.key { -1 } else if a.key > b.key { 1 } else { 0 });

    let out = [];
    for s in scored { out.push(s.id); }
    out
}
"#;

fn plugin_channel(plugin: &Path) -> ChannelConfig {
    ChannelConfig {
        name: None,
        display_name: None,
        guide: None,
        scoring: None,
        anchor: None,
        window_days: 1,
        chunk_hours: 6,
        roll_interval: std::time::Duration::from_secs(60),
        retention_days: 1,
        seed: Some(7),
        overlay: None,
        groups: Vec::new(),
        rule: RuleConfig {
            blocks: vec![BlockInclude {
                overlay: None,
                block: None,
                program: None,
                guide: None,
                duplicates: None,
                constraints: None,
                entries: Vec::new(),
                fallback: None,
                filter: None,
                mode: Mode::All,
                order: Default::default(),
                pools: vec![Pool {
                    name: "clockpool".into(),
                    expr: None,
                    plugin: Some(plugin.to_path_buf()),
                    sources: None,
                    groups: Vec::new(),
                    order: None,
                    bucket_order: None,
                    group_by: Default::default(),
                    select: Default::default(),
                    rotate: Default::default(),
                    advance: Advance::Restart,
                    on_short: Default::default(),
                    constraints: None,
                    config: None,
                    capabilities: vec!["catalog_read".into()],
                    datastores: Vec::new(),
                    guide: None,
                }],
                pattern: vec![PatternStep {
                    pool: "clockpool".into(),
                    take: etv_station::config::Take::Count(8),
                    from: Default::default(),
                    chance: 1.0,
                }],
                cycles: Some(1),
                sequencer: None,
            }],
        },
    }
}

/// Resolve `plugin_channel` at `window_start` and return the ordered ids.
fn resolve_at(
    cfg: &ChannelConfig,
    cat: &Catalog,
    window_start: time::OffsetDateTime,
) -> Vec<String> {
    let state = GenerationState::default();
    let (items, _) = resolve_channel_with_resume(
        cfg,
        Path::new("clock.yaml"),
        &[],
        None,
        Some(cat),
        &state,
        &ScoreInputs::default(),
        None,
        window_start,
    )
    .unwrap();
    items.into_iter().map(|i| i.id).collect()
}

/// A fixed instant, arbitrary but not tied to when the suite happens to run
/// (mirrors `scorer_plugin.rs`'s `CHUNK_BOUNDARY`).
const WINDOW_START: time::OffsetDateTime = datetime!(2026-04-13 00:00:00 UTC);

/// Two generations of the same channel at the same `window_start`, with a
/// plugin that reads `timestamp()`/`elapsed()`, must produce byte-identical
/// output — the acceptance criterion's core claim.
#[test]
fn same_window_start_resolves_identically_with_a_clock_reading_plugin() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_plugin(&dir, CLOCK_READING_PLUGIN);
    let cat = catalog();
    let cfg = plugin_channel(&p);

    let a = resolve_at(&cfg, &cat, WINDOW_START);
    let b = resolve_at(&cfg, &cat, WINDOW_START);

    assert_eq!(
        a, b,
        "two generations of the same channel at the same window_start must \
         see an identical plugin clock and so produce identical output"
    );
}

/// Two different `window_start` values must see a *different* clock — the
/// clock is fixed per generation, not globally frozen. Without this, a fix
/// that hard-codes the plugin clock to one constant would pass the identical-
/// output test above for the wrong reason.
#[test]
fn different_window_starts_see_a_different_clock() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_plugin(&dir, CLOCK_READING_PLUGIN);
    let cat = catalog();
    let cfg = plugin_channel(&p);

    let a = resolve_at(&cfg, &cat, WINDOW_START);
    let b = resolve_at(&cfg, &cat, WINDOW_START + time::Duration::hours(1));

    assert_ne!(
        a, b,
        "two generations an hour apart must see different timestamp() \
         readings and so produce a different permutation — the clock must \
         not be frozen to one global value"
    );
}

/// A plugin spinning `while elapsed(t0) < w { … }` must still terminate: the
/// injected clock advances by a nonzero step on every read, so the loop
/// cannot spin forever the way it would against a pinned constant.
#[test]
fn an_elapsed_spin_loop_terminates() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_plugin(&dir, CLOCK_READING_PLUGIN);
    let cat = catalog();
    let cfg = plugin_channel(&p);

    // No timeout wrapper needed: if the injected clock ever stopped
    // advancing, this call would hang and the test would fail by timing out
    // under the suite's own runner rather than asserting anything itself.
    let got = resolve_at(&cfg, &cat, WINDOW_START);
    assert_eq!(
        got.len(),
        8,
        "the spin loop must terminate and let all 8 items resolve"
    );
}

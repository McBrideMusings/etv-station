//! Determinism check (#168) — generate a channel twice from identical inputs
//! and diff the two schedules, so a plugin that breaks reproducible
//! generation is caught instead of shipping.
//!
//! Reproducible generation is a stated property of the system: pin a seed and
//! the same catalog, config, and resume state produce the same schedule. That
//! holds for everything the station does itself, because every random choice
//! is seeded from the channel `seed` plus a position. A plugin can break it
//! silently — a Rhai script that reads wall-clock time or iterates something
//! whose order is not stable produces a different schedule every run, and the
//! schedule still looks completely valid. This module measures that; it does
//! not fix it (out of scope — see the issue).
//!
//! Not part of normal generation. A debug check, wired to its own CLI flag in
//! `main.rs`, never called from [`crate::daemon`].

use std::sync::Arc;

use time::OffsetDateTime;

use crate::catalog::Catalog;
use crate::config::{LoadedChannel, Station};
use crate::daemon::window_duration;
use crate::errors::StationError;
use crate::resolve::resolve_channel_with_resume;
use crate::resume::GenerationState;
use crate::score::ScoreInputs;

/// Hint handed to a scorer plugin as `ctx.target_count`. The check has no
/// `from`/`target` chunk to size one from (unlike a real generation tick —
/// see `daemon::target_count`), so it uses a fixed, generous value: enough
/// for an ordinary pool, and it is only a hint a script may ignore.
const TARGET_COUNT_HINT: usize = 500;

/// Where two generations of the same channel first disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Difference {
    /// The airing position, zero-indexed, where the two schedules diverge.
    pub position: usize,
    /// The entry id pass A resolved at `position`, or `None` if pass A's
    /// schedule ended before reaching it.
    pub entry_a: Option<String>,
    /// The entry id pass B resolved at `position`, or `None` if pass B's
    /// schedule ended before reaching it.
    pub entry_b: Option<String>,
}

/// The result of generating one channel twice from identical inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeterminismReport {
    pub channel: String,
    pub pass_a_len: usize,
    pub pass_b_len: usize,
    /// `None` when the two schedules matched entry-for-entry.
    pub difference: Option<Difference>,
}

impl DeterminismReport {
    pub fn is_identical(&self) -> bool {
        self.difference.is_none()
    }
}

/// Generate `channel_name` twice from identical inputs and report whether the
/// two schedules match.
///
/// # What "identical inputs" means here
///
/// - **Catalog snapshot** — one read-only handle, opened once and reused for
///   both passes, so both see the exact same bytes on disk. Nothing here
///   ingests or writes to it.
/// - **Seed** — an authored `seed` is already the same for both passes (it is
///   read once, from the loaded config). An unset seed is intentionally
///   fresh-per-generation (#46: "unset = fresh per generation") —
///   [`resolve_channel_with_resume`] draws a new one from wall-clock nanos on
///   every call, which would make *every* unseeded channel read as
///   non-reproducible for a reason that has nothing to do with a plugin. So
///   an unset seed is pinned here, once, to one freshly-drawn value, and that
///   one value is used for both passes exactly as an authored seed would be.
/// - **Resume state** — [`GenerationState::empty`] for both passes: the same
///   stateless shape [`crate::resolve::resolve_channel`] itself uses, not the
///   channel's live `.resume` sidecar or play-history ledger. Nothing is read
///   from or written to either between the two calls, so there is no
///   persisted state for the second pass to observe — it literally cannot,
///   because neither pass touches any.
/// - **External-store snapshot** — one [`ScoreInputs`], including `now` (read
///   from the wall clock exactly once), reused verbatim for both passes.
///
/// # What this catches
///
/// A plugin whose `pick()` depends on anything outside those pinned inputs
/// disagrees between the two passes and is caught here. Rhai's sandbox has no
/// OS clock, environment, file I/O, or RNG of its own — `timestamp()` /
/// `elapsed()` (Rhai's own wall clock) is the one vector it exposes, which is
/// why "reads wall-clock time" and "uses unseeded randomness" are the same
/// underlying bug in this engine: neither is derivable from anything but that
/// clock.
///
/// # Hook coverage
///
/// This diffs the final resolved `entry_id` sequence
/// [`resolve_channel_with_resume`] produces — not any one hook's call — so it
/// already covers a `pool_provider` plugin and will cover a `sequencer`
/// plugin (#169, not wired into resolution yet) the moment that hook starts
/// contributing to the same resolved list, with no change needed here.
pub fn check(station: &mut Station, channel_name: &str) -> Result<DeterminismReport, StationError> {
    let idx = station
        .channels
        .iter()
        .position(|c| c.name == channel_name)
        .ok_or_else(|| StationError::UnknownChannel {
            name: channel_name.to_string(),
        })?;

    // Pin the seed once, before either pass — see the doc comment above.
    if station.channels[idx].config.seed.is_none() {
        station.channels[idx].config.seed = Some(fresh_seed());
    }

    let channel = &station.channels[idx];
    let catalog = open_catalog(station.station.catalog_path.as_deref())?;
    let fill = Some(window_duration(channel.config.window_days).unsigned_abs());
    // One state, one scoring snapshot, reused verbatim for both passes.
    let state = GenerationState::empty();
    let scoring = ScoreInputs {
        target_count: TARGET_COUNT_HINT,
        history: Arc::from(Vec::new()),
        recent: Vec::new(),
        now: OffsetDateTime::now_utc().unix_timestamp(),
        // Same station tz a real generation hands a sequencer block — a
        // daypart script that reads `local_time()` is exactly the kind of
        // wall-clock read this check exists to catch, so both passes need the
        // real zone rather than the UTC fallback.
        tz: Some(crate::tz::parse(&station.station.tz)?),
        // No live Tautulli fetch happens in this stateless check, the same
        // simplification `history` above already makes — a `single_user`
        // channel's `ctx.account_id` is untested here, not incorrectly
        // resolved (#278).
        account_id: None,
    };
    // Read once and handed to both passes, for the same reason `now` is: a
    // sequencer block (#169) reads this as `ctx.window.from`, so a daypart
    // script asks what hour the generation starts at. Reading the clock
    // separately per pass would straddle an hour boundary now and then and
    // report a correct daypart script as non-reproducible.
    let window_start = OffsetDateTime::now_utc();

    let a = generate_once(
        channel,
        &station.station.identity_roots,
        catalog.as_ref(),
        &state,
        &scoring,
        fill,
        window_start,
    )?;
    let b = generate_once(
        channel,
        &station.station.identity_roots,
        catalog.as_ref(),
        &state,
        &scoring,
        fill,
        window_start,
    )?;

    Ok(diff_report(channel_name, &a, &b))
}

/// One pass: resolve the channel and reduce it to the bare `entry_id`
/// sequence a diff needs. Takes everything by shared reference so the same
/// catalog handle, state, and scoring snapshot can be reused for both passes
/// with no clone and no risk of one pass's call mutating what the other sees.
fn generate_once(
    channel: &LoadedChannel,
    identity_roots: &[String],
    catalog: Option<&Catalog>,
    state: &GenerationState,
    scoring: &ScoreInputs,
    fill: Option<std::time::Duration>,
    window_start: OffsetDateTime,
) -> Result<Vec<String>, StationError> {
    let (items, _resume_out) = resolve_channel_with_resume(
        &channel.config,
        &channel.config_path,
        identity_roots,
        None,
        catalog,
        state,
        scoring,
        fill,
        window_start,
    )?;
    Ok(items.into_iter().map(|item| item.id).collect())
}

/// The first position (if any) where `a` and `b` disagree — including a
/// length mismatch, read as one side running out.
fn diff_report(channel: &str, a: &[String], b: &[String]) -> DeterminismReport {
    let max = a.len().max(b.len());
    let difference = (0..max).find_map(|i| {
        let entry_a = a.get(i).cloned();
        let entry_b = b.get(i).cloned();
        if entry_a == entry_b {
            None
        } else {
            Some(Difference {
                position: i,
                entry_a,
                entry_b,
            })
        }
    });
    DeterminismReport {
        channel: channel.to_string(),
        pass_a_len: a.len(),
        pass_b_len: b.len(),
        difference,
    }
}

fn open_catalog(path: Option<&str>) -> Result<Option<Catalog>, StationError> {
    match path.filter(|p| !p.trim().is_empty()) {
        Some(p) => Ok(Some(Catalog::open_readonly(p)?)),
        None => Ok(None),
    }
}

/// A seed drawn once, for a channel whose `seed` is unset (#46). Same shape
/// as `resolve::fresh_seed` (wall-clock nanos) but kept local rather than
/// shared: drawing one is this check's own need, not something the
/// resolver's other callers use.
fn fresh_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Entry, Source};
    use crate::config::{Advance, GroupBy, OnShort, PatternStep, Rotate, Select, Take, TakeFrom};
    use crate::config::{
        BlockInclude, ChannelConfig, Entry as ConfigEntry, ItemEntry, Mode, Order, Pool,
        RuleConfig, ScoringConfig, SourceConfig,
    };
    use std::path::{Path, PathBuf};
    use std::time::Duration as StdDuration;

    fn path() -> &'static Path {
        Path::new("/tmp/determinism-check-channel.toml")
    }

    fn base_channel(rule: RuleConfig, scoring: Option<ScoringConfig>) -> ChannelConfig {
        ChannelConfig {
            scoring,
            name: None,
            window_days: 1,
            chunk_hours: 24,
            roll_interval: StdDuration::from_secs(3600),
            retention_days: 1,
            seed: None,
            anchor: None,
            rule,
            groups: Vec::new(),
            overlay: None,
        }
    }

    fn station_with(name: &str, config: ChannelConfig, catalog_path: Option<String>) -> Station {
        Station {
            config_path: PathBuf::from("/tmp/station.toml"),
            station: crate::config::StationConfig {
                tz: "UTC".into(),
                device_id: None,
                output_base: PathBuf::from("/tmp/out"),
                channels: Vec::new(),
                source_roots: Vec::new(),
                identity_roots: Vec::new(),
                catalog_path,
                catalog_refresh_secs: 0,
                full_sweep_after_secs: 0,
            },
            channels: vec![LoadedChannel {
                name: name.to_string(),
                config_path: path().to_path_buf(),
                output_folder: PathBuf::from("/tmp/out").join(name),
                config,
            }],
        }
    }

    // ---- no plugin: a manual entries channel -------------------------------

    fn item_entry(id: &str) -> ItemEntry {
        ItemEntry {
            source: SourceConfig::Lavfi { params: id.into() },
            in_point: None,
            out_point: Some(StdDuration::from_secs(30)),
            program: None,
        }
    }

    fn manual_block(ids: &[&str]) -> BlockInclude {
        BlockInclude {
            block: None,
            program: None,
            duplicates: None,
            constraints: None,
            entries: ids
                .iter()
                .map(|id| ConfigEntry::Item(Box::new(item_entry(id))))
                .collect(),
            fallback: None,
            sequencer: None,
            pools: Vec::new(),
            pattern: Vec::new(),
            cycles: None,
            mode: Mode::All,
            order: Some(Order::Manual),
            filter: None,
        }
    }

    /// Each item declares its own long runtime via `out_point`
    /// (`estimated_runtimes`'s most-authoritative source), so the window cut
    /// below has something real to bite on rather than the channel's nominal
    /// stand-in length.
    fn item_entry_secs(id: &str, secs: u64) -> ItemEntry {
        ItemEntry {
            source: SourceConfig::Lavfi { params: id.into() },
            in_point: None,
            out_point: Some(StdDuration::from_secs(secs)),
            program: None,
        }
    }

    fn manual_block_secs(ids: &[(&str, u64)]) -> BlockInclude {
        BlockInclude {
            block: None,
            program: None,
            duplicates: None,
            constraints: None,
            entries: ids
                .iter()
                .map(|(id, secs)| ConfigEntry::Item(Box::new(item_entry_secs(id, *secs))))
                .collect(),
            fallback: None,
            sequencer: None,
            pools: Vec::new(),
            pattern: Vec::new(),
            cycles: None,
            mode: Mode::All,
            order: Some(Order::Manual),
            filter: None,
        }
    }

    /// A channel with no plugin at all passes the check — see the issue's
    /// acceptance criteria: two calls with pinned inputs and no scorer script
    /// anywhere in the config are a pure function of the same inputs both
    /// times.
    #[test]
    fn a_channel_with_no_plugin_passes() {
        let cfg = base_channel(
            RuleConfig {
                blocks: vec![manual_block(&["a", "b", "c"])],
            },
            None,
        );
        let mut station = station_with("plain", cfg, None);
        let report = check(&mut station, "plain").unwrap();
        assert!(
            report.is_identical(),
            "expected identical schedules, got {report:?}"
        );
        assert_eq!(report.pass_a_len, 3);
        assert_eq!(report.pass_b_len, 3);
    }

    /// An unknown channel name is reported rather than silently resolving
    /// nothing.
    #[test]
    fn an_unknown_channel_is_an_error() {
        let cfg = base_channel(
            RuleConfig {
                blocks: vec![manual_block(&["a"])],
            },
            None,
        );
        let mut station = station_with("plain", cfg, None);
        let err = check(&mut station, "nope").unwrap_err();
        assert!(matches!(err, StationError::UnknownChannel { .. }));
    }

    // ---- seed pinning: an unseeded `order = random` channel ----------------

    fn shuffled_block(ids: &[&str]) -> BlockInclude {
        BlockInclude {
            block: None,
            program: None,
            duplicates: None,
            constraints: None,
            entries: ids
                .iter()
                .map(|id| ConfigEntry::Item(Box::new(item_entry(id))))
                .collect(),
            fallback: None,
            sequencer: None,
            pools: Vec::new(),
            pattern: Vec::new(),
            cycles: None,
            mode: Mode::All,
            order: Some(Order::Random),
            filter: None,
        }
    }

    /// An unseeded `order = "random"` channel intentionally reshuffles every
    /// generation (#46) — that must not read as a plugin breaking
    /// determinism. `check` pins one seed for both passes, so the two
    /// schedules match even though the config itself never set one.
    ///
    /// `order = "random"` on an entries block resolves through the catalog
    /// (the order engine's seeded shuffle), so this fixture needs one —
    /// unlike the plain `manual_block` tests above, which stay catalog-free.
    #[test]
    fn an_unseeded_random_channel_still_passes_because_the_check_pins_one_seed() {
        let dir = tempfile::tempdir().unwrap();
        let ids = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];
        let catalog_path = dir.path().join("catalog.db");
        catalog_with_movies(&catalog_path, &ids);

        let cfg = base_channel(
            RuleConfig {
                blocks: vec![shuffled_block(&ids)],
            },
            None,
        );
        assert!(cfg.seed.is_none(), "fixture must start unseeded");
        let mut station = station_with(
            "shuffled",
            cfg,
            Some(catalog_path.to_string_lossy().into_owned()),
        );
        let report = check(&mut station, "shuffled").unwrap();
        assert!(
            report.is_identical(),
            "an unseeded channel must still be reproducible once the check pins a seed: {report:?}"
        );
    }

    // ---- resume-state isolation: a cut entries list -------------------------

    /// A manual entries list longer than one window: the first generation
    /// only covers part of it and would normally advance `.resume.position`
    /// for the next one. `check` never feeds pass A's resume map into pass
    /// B — both start from `GenerationState::empty()` — so if it *did* leak,
    /// pass B would start at item 3 instead of item 0 and the two schedules
    /// would visibly disagree at position 0. They must not.
    #[test]
    fn the_second_pass_does_not_observe_the_first_passs_resume_state() {
        // fill = window_days(1) * 86_400s; three 40_000s items cover it
        // (120_000s >= 86_400s), the other two do not.
        let mut cfg = base_channel(
            RuleConfig {
                blocks: vec![manual_block_secs(&[
                    ("a", 40_000),
                    ("b", 40_000),
                    ("c", 40_000),
                    ("d", 40_000),
                    ("e", 40_000),
                ])],
            },
            None,
        );
        cfg.window_days = 1;
        let mut station = station_with("cut", cfg, None);
        let report = check(&mut station, "cut").unwrap();
        assert!(
            report.is_identical(),
            "leaked resume state would show up as a divergence: {report:?}"
        );
        assert_eq!(
            report.pass_a_len, 3,
            "sanity: the fixture must actually cut short of the full list"
        );
        assert_eq!(report.pass_b_len, 3);
    }

    // ---- a plugin reading Rhai's own wall clock is caught -------------------

    /// Writes a file-backed catalog (not in-memory — the check reopens it by
    /// path, exactly as it would a real one) and drops the writable handle,
    /// matching how the daemon itself never holds a writer open once ingest
    /// finishes.
    fn catalog_with_movies(path: &Path, ids: &[&str]) {
        let c = Catalog::open(path).unwrap();
        for id in ids.iter().copied() {
            let e = Entry::new(id, "movie", id, Source::Plex);
            c.upsert_entry(&e).unwrap();
            // A row with no playback source is skipped by `catalog_item`
            // (resolve.rs) as unplayable, so every fixture entry needs one.
            c.add_source(&crate::catalog::EntrySource {
                source: Source::LocalFs,
                source_id: format!("fs-{id}"),
                entry_id: id.to_string(),
                playback_path: format!("/media/{id}.mkv"),
                last_seen: None,
            })
            .unwrap();
        }
    }

    fn write_plugin(dir: &tempfile::TempDir, body: &str) -> PathBuf {
        let p = dir.path().join("plugin.rhai");
        std::fs::write(&p, body).unwrap();
        p
    }

    /// A scorer that reorders its result from Rhai's own `timestamp()` /
    /// `elapsed()` instead of the deterministic `ctx.now` the station hands
    /// it — the exact contract violation #168 exists to catch. Rhai's
    /// sandbox has no OS clock, environment, file I/O, or RNG of its own, so
    /// this one vector stands in for both "reads wall-clock time" and "uses
    /// unseeded randomness": with nothing else nondeterministic to read in
    /// this engine, they are the same bug.
    ///
    /// The mixing (xor-shift over each item's index, folded with the raw
    /// nanosecond reading) is deliberate rather than a plain reverse-if-odd:
    /// a coin-flip decision has a real chance of landing the same way twice
    /// in a row, which would make this test flaky. Requiring the *whole*
    /// nanosecond reading (not a low bit of it) to match between two
    /// independent measurements to produce the same order is, in practice,
    /// never going to coincidentally pass.
    const WALL_CLOCK_PLUGIN: &str = r#"
fn hooks() { ["pool_provider"] }
fn capabilities() { ["catalog_read"] }
fn sources() { #{ movies: `item.type == "movie"` } }
fn pick(ctx) {
    let ids = [];
    for item in ctx.sets.movies { ids.push(item.entry_id); }

    // Spin until the clock has demonstrably moved, and seed from how many
    // iterations that took rather than from the reading itself. A fixed-size
    // busy loop plus `elapsed()` was the first shape of this and it is not
    // reliable: on an idle machine the loop finishes inside one tick of the
    // clock's resolution, both passes read the same elapsed value, and the
    // fixture is then perfectly deterministic - which fails this test for the
    // opposite of the reason it is checking. The spin count over a fixed wall
    // window varies with load and scheduling and is essentially never equal
    // twice.
    let t0 = timestamp();
    let spins = 0;
    while elapsed(t0) < 0.02 { spins += 1; }
    // The spin count carries essentially all of the entropy here: this clock's
    // resolution is coarse enough that the reading at loop exit is the same
    // number every run, so folding it in costs nothing and removes nothing.
    // The 20ms window is what makes the count reliable — at 2ms the counts
    // repeated outright about one run in fifteen, and an identical count is an
    // identical schedule, which fails this test for the opposite of the reason
    // it checks.
    // Reduced into the modulus immediately: unbounded, this reaches ~1e11,
    // and the key below then multiplies it past what an i64 holds.
    let seed = (spins * 1000003 + (elapsed(t0) * 1000000000.0).to_int()) % 999999937;

    // A per-item key that wraps around a large modulus as `idx` grows, so its
    // sort order is not just "seed's magnitude shifted by a constant" — two
    // different seeds land the wraparounds at different items and produce a
    // genuinely different permutation, not merely a different starting
    // rotation of the same one.
    //
    // The multiplier is what makes that true. Without it — plain
    // `seed * (idx + 1)` — nothing wraps until `seed` exceeds an eighth of the
    // modulus, so for every realistic seed the keys rise monotonically with
    // `idx` and the sort is the identity permutation no matter what the clock
    // said. That made this fixture perfectly deterministic on an idle machine,
    // which fails this test for the exact opposite of the reason it checks.
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

    /// A scorer that shuffles purely from `ctx.seed` (#255, ADR 0005) —
    /// no clock read at all — using the same key/sort mixing shape as
    /// [`WALL_CLOCK_PLUGIN`] above, so the only variable between the two
    /// fixtures is where the entropy comes from.
    const CTX_SEED_PLUGIN: &str = r#"
fn hooks() { ["pool_provider"] }
fn capabilities() { ["catalog_read"] }
fn sources() { #{ movies: `item.type == "movie"` } }
fn pick(ctx) {
    let ids = [];
    for item in ctx.sets.movies { ids.push(item.entry_id); }

    // Reduced into the modulus immediately, exactly as the wall-clock
    // fixture reduces its own reading — `ctx.seed` is a full 63-bit value
    // and multiplying it by `idx + 1` and 7919 unreduced overflows Rhai's
    // checked i64 multiplication.
    let seed = ctx.seed % 999999937;

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

    fn plugin_pool(script: &Path) -> Pool {
        Pool {
            name: "movies".into(),
            expr: None,
            plugin: Some(script.to_path_buf()),
            sources: None,
            groups: Vec::new(),
            order: None,
            bucket_order: None,
            group_by: GroupBy::Show,
            select: Select::RoundRobin,
            rotate: Rotate::Visit,
            advance: Advance::Restart,
            on_short: OnShort::Next,
            constraints: None,
            config: None,
            // The test script reads `ctx.sets`, which #167 gates behind a
            // granted `catalog_read`. This pool is built directly rather than
            // loaded, so nothing cross-checks the grant against the script's
            // own `capabilities()` here — the script declares it too, so the
            // pair matches what a real config would have to write.
            capabilities: vec!["catalog_read".into()],
            datastores: Vec::new(),
        }
    }

    fn pattern_block(script: &Path, take: usize) -> BlockInclude {
        BlockInclude {
            block: None,
            program: None,
            duplicates: None,
            constraints: None,
            entries: Vec::new(),
            fallback: None,
            sequencer: None,
            pools: vec![plugin_pool(script)],
            pattern: vec![PatternStep {
                pool: "movies".into(),
                take: Take::Count(take),
                from: TakeFrom::default(),
                chance: 1.0,
            }],
            cycles: Some(1),
            mode: Mode::All,
            order: None,
            filter: None,
        }
    }

    #[test]
    fn a_plugin_reading_wall_clock_time_is_caught() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_plugin(&dir, WALL_CLOCK_PLUGIN);
        let ids = ["m0", "m1", "m2", "m3", "m4", "m5", "m6", "m7"];
        let catalog_path = dir.path().join("catalog.db");
        catalog_with_movies(&catalog_path, &ids);

        let cfg = base_channel(
            RuleConfig {
                blocks: vec![pattern_block(&script, ids.len())],
            },
            None,
        );
        let mut station = station_with(
            "wobbly",
            cfg,
            Some(catalog_path.to_string_lossy().into_owned()),
        );
        let report = check(&mut station, "wobbly").unwrap();
        assert!(
            !report.is_identical(),
            "a plugin reading Rhai's own wall clock must be caught, but the two \
             passes matched — either the fixture failed to introduce real \
             per-call variation, or the check pinned something it should not have"
        );
        let diff = report.difference.unwrap();
        assert!(diff.entry_a.is_some());
        assert!(diff.entry_b.is_some());
        assert_ne!(diff.entry_a, diff.entry_b);
    }

    /// The reason `ctx.seed` exists: a scorer that shuffles from it instead
    /// of Rhai's own wall clock passes the check (#255, ADR 0005) — the
    /// opposite assertion from the wall-clock fixture above, over an
    /// otherwise identical mixing shape. The channel starts unseeded, so
    /// this also covers "an unseeded channel still works": `check` pins one
    /// fresh seed before either pass, and that pinned value is what reaches
    /// the script both times.
    #[test]
    fn a_plugin_picking_from_ctx_seed_passes() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_plugin(&dir, CTX_SEED_PLUGIN);
        let ids = ["m0", "m1", "m2", "m3", "m4", "m5", "m6", "m7"];
        let catalog_path = dir.path().join("catalog.db");
        catalog_with_movies(&catalog_path, &ids);

        let cfg = base_channel(
            RuleConfig {
                blocks: vec![pattern_block(&script, ids.len())],
            },
            None,
        );
        assert!(cfg.seed.is_none(), "fixture must start unseeded");
        let mut station = station_with(
            "reshuffled",
            cfg,
            Some(catalog_path.to_string_lossy().into_owned()),
        );
        let report = check(&mut station, "reshuffled").unwrap();
        assert!(
            report.is_identical(),
            "a plugin picking from ctx.seed must reproduce across two passes: {report:?}"
        );
    }
}

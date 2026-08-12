//! Acceptance test for Sample S8 (#82, rewritten for #254): the committed
//! `examples/samples/foryou.yaml` channel weaves two movies and a
//! three-episode run, where *which movies* play is chosen by
//! `examples/plugins/taste-cosine.rhai` ranking TMDB keywords against a
//! pooled taste vector read from a granted plex-db-ex datastore — not by any
//! query this config authors, and not by what this repo's own watch history
//! says.
//!
//! The deepest sample in the set: it stacks plugin scoring (#74) with a
//! granted external datastore (#181) on pools + pattern (#72) and the
//! per-`show_id` resume map, and it is the one place the whole claim is
//! checked end to end — the station supplies the catalog, the target count,
//! and the seed; the script supplies the ranking and holds no replay rule of
//! its own (ADR 0002).
//!
//! Every test here resolves against a hand-built plexdb fixture
//! ([`write_taste_fixture`]), never the real plex-db-ex snapshot — the same
//! posture `datastore_capability.rs` and `taste_cosine_scorer.rs` take.

use std::path::Path;
use std::sync::OnceLock;

use etv_station::catalog::{Catalog, Entry, EntrySource, Source};
use etv_station::config::{ChannelConfig, NoRepeatWithin, Pool, read_channel};
use etv_station::history::{HistoryDb, PlayRecord};
use etv_station::resolve::{ResolvedItem, resolve_channel_with_resume};
use etv_station::resume::{GenerationState, ResumeMap};
use etv_station::score::{ScoreInputs, WatchEvent};
use time::OffsetDateTime;

/// Ten movies and three shows of unequal length, all locally sourced so
/// resolution reaches a playable item.
///
/// The shows differ in length on purpose: the pattern draws three episodes
/// per visit, so a two-episode show cannot serve a visit alone and exercises
/// `on_short: next`.
fn library() -> Catalog {
    let cat = Catalog::open_in_memory().unwrap();
    let add = |id: &str, kind: &str, title: &str, year: i64, show: Option<(&str, i64, i64)>| {
        let mut e = Entry::new(id, kind, title, Source::Plex);
        e.year = Some(year);
        if let Some((show_id, season, episode)) = show {
            e.show_id = Some(show_id.to_string());
            e.show = Some(show_id.trim_start_matches("show:").to_string());
            e.season = Some(season);
            e.episode = Some(episode);
        }
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

    // Ten movies: enough that the ranking's own `+10` margin and the
    // pattern's 2-per-cycle draw have real headroom, and enough to carry a
    // meaningful mix of on-profile, off-profile, and keyword-less titles
    // (see `write_taste_fixture`).
    for (id, title, year) in [
        ("mov-arrival", "Arrival", 2016),
        ("mov-blade", "Blade Runner", 1982),
        ("mov-contact", "Contact", 1997),
        ("mov-dune", "Dune", 2021),
        ("mov-exmachina", "Ex Machina", 2014),
        ("mov-gattaca", "Gattaca", 1997),
        ("mov-her", "Her", 2013),
        ("mov-interstellar", "Interstellar", 2014),
        ("mov-moon", "Moon", 2009),
        ("mov-solaris", "Solaris", 1972),
    ] {
        add(id, "movie", title, year, None);
    }

    for ep in 1..=6 {
        add(
            &format!("sev-{ep}"),
            "episode",
            &format!("Severance S1E{ep}"),
            2022,
            Some(("show:severance", 1, ep)),
        );
    }
    for ep in 1..=4 {
        add(
            &format!("exp-{ep}"),
            "episode",
            &format!("Expanse S1E{ep}"),
            2015,
            Some(("show:expanse", 1, ep)),
        );
    }
    for ep in 1..=2 {
        add(
            &format!("dev-{ep}"),
            "episode",
            &format!("Devs S1E{ep}"),
            2020,
            Some(("show:devs", 1, ep)),
        );
    }
    cat
}

/// A minimal but real plexdb store (schema-7) for the ten movies above.
///
/// `mov-contact` is the one played title, carrying three keywords ("signal",
/// "alien", "thriller") — one play split three ways puts `1/3` on each, so
/// its own cosine score is `(1/3 * 3) / sqrt(3) = 1/sqrt(3) ≈ 0.577`.
/// `mov-dune` carries two of those three ("signal", "alien") and nothing
/// else: `(1/3 + 1/3) / sqrt(2) ≈ 0.471`. `mov-arrival` carries one on-profile
/// keyword ("signal") plus one the store has never seen ("linguistics"):
/// `(1/3 + 0) / sqrt(2) ≈ 0.236`. Every other movie carries no keywords at
/// all, so they are ineligible for the exploration slot and rank at the
/// tied bottom by entry_id. Three real levels — a played title leading its
/// own two closest, unwatched relatives — is enough to prove the ranking
/// comes from this store and not from `ScoreInputs.history`, which is what
/// the test below actually checks.
fn write_taste_fixture(path: &Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(&format!(
        "CREATE TABLE schema_version (version INTEGER NOT NULL);
         INSERT INTO schema_version (version) VALUES ({version});
         CREATE TABLE items (
             item_id TEXT PRIMARY KEY, type TEXT NOT NULL,
             show_item_id TEXT, season INTEGER, episode INTEGER
         );
         CREATE TABLE enrichment (
             item_id TEXT NOT NULL, namespace TEXT NOT NULL,
             key TEXT NOT NULL, value TEXT NOT NULL, fetched_at TEXT NOT NULL
         );
         CREATE TABLE plays (
             history_key TEXT PRIMARY KEY, item_id TEXT NOT NULL,
             plex_account_id INTEGER NOT NULL, viewed_at INTEGER NOT NULL
         );
         INSERT INTO items (item_id, type) VALUES
             ('mov-arrival', 'movie'), ('mov-blade', 'movie'), ('mov-contact', 'movie'),
             ('mov-dune', 'movie'), ('mov-exmachina', 'movie'), ('mov-gattaca', 'movie'),
             ('mov-her', 'movie'), ('mov-interstellar', 'movie'), ('mov-moon', 'movie'),
             ('mov-solaris', 'movie');
         INSERT INTO enrichment (item_id, namespace, key, value, fetched_at) VALUES
             ('mov-contact', 'tmdb_keywords', 'keyword', 'signal', '2026-01-01T00:00:00+00:00'),
             ('mov-contact', 'tmdb_keywords', 'keyword', 'alien', '2026-01-01T00:00:00+00:00'),
             ('mov-contact', 'tmdb_keywords', 'keyword', 'thriller', '2026-01-01T00:00:00+00:00'),
             ('mov-dune', 'tmdb_keywords', 'keyword', 'signal', '2026-01-01T00:00:00+00:00'),
             ('mov-dune', 'tmdb_keywords', 'keyword', 'alien', '2026-01-01T00:00:00+00:00'),
             ('mov-arrival', 'tmdb_keywords', 'keyword', 'signal', '2026-01-01T00:00:00+00:00'),
             ('mov-arrival', 'tmdb_keywords', 'keyword', 'linguistics', '2026-01-01T00:00:00+00:00');
         INSERT INTO plays (history_key, item_id, plex_account_id, viewed_at) VALUES
             ('h1', 'mov-contact', 42, 1700000000);",
        version = plexdb_reader::SUPPORTED_SCHEMA_VERSION,
    ))
    .unwrap();
}

/// The fixture store, built once and shared read-only across every test in
/// this binary — `Reader::open` never writes, so concurrent tests reading it
/// are safe, and rebuilding it per test would just be the same bytes again.
fn taste_store_path() -> &'static Path {
    static PATH: OnceLock<std::path::PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        // `keep()` hands back the directory and gives up deleting it: the
        // fixture has to outlive every test function in this binary, and a
        // `tempdir()` still owned here would clean itself up on drop.
        let path = tempfile::tempdir().unwrap().keep().join("taste.db");
        write_taste_fixture(&path);
        path
    })
}

fn sample_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/samples/foryou.yaml")
}

/// The committed sample, with the movies pool's `datastores` grant path
/// repointed at the local fixture store — `read_channel` is a raw parse
/// (docs on `config::load::read_channel`) that does not expand `${VAR}`, so
/// the committed `${PLEXDB_SNAPSHOT_PATH}` reference is overwritten here the
/// same way `config_with_movies_config` overwrites `config:` below, rather
/// than relying on a real snapshot or an environment variable no test
/// machine can be expected to have set.
///
/// Also pins `seed`, which the committed sample leaves unset (a real station
/// resolves one per generation). `resolve::resolve_channel_with_resume`
/// draws a fresh random seed for every call when it is unset
/// (`config.seed.unwrap_or_else(fresh_seed)`), and this scorer's
/// exploration slots and the shows pool's `select: random` both depend on
/// it — so two calls in the same test would draw two different schedules
/// for a reason that has nothing to do with what each test is actually
/// checking. A real generation pins one value for the whole run the same
/// way; this is that pin, held fixed across every test in this file.
fn config() -> ChannelConfig {
    let mut cfg = read_channel(&sample_path()).expect("load the For You sample");
    cfg.seed = Some(20260812);
    movies_pool(&mut cfg).datastores[0].path = taste_store_path().to_str().unwrap().to_string();
    cfg
}

/// The sample's movies pool — the one carrying the plugin and its datastore
/// grant, which is what every test below repoints or reconfigures.
fn movies_pool(cfg: &mut ChannelConfig) -> &mut Pool {
    cfg.rule.blocks[0]
        .pools
        .iter_mut()
        .find(|p| p.name == "movies")
        .expect("the sample declares a movies pool")
}

/// A movies-pool `config:` carrying just `exploration_fraction`, the one
/// tunable this scorer reads.
fn exploration(fraction: f64) -> Option<serde_json::Value> {
    Some(serde_json::json!({ "exploration_fraction": fraction }))
}

fn ids(items: &[ResolvedItem]) -> Vec<String> {
    items.iter().map(|i| i.id.clone()).collect()
}

/// One generation, with everything the daemon supplies that no test here
/// varies. The error travels back rather than being unwrapped, so each caller
/// keeps its own account of what it expected to resolve.
fn resolve_cfg(
    cfg: &ChannelConfig,
    path: &Path,
    cat: &Catalog,
    state: &GenerationState,
    inputs: &ScoreInputs,
) -> Result<(Vec<ResolvedItem>, ResumeMap), etv_station::errors::ConfigError> {
    resolve_channel_with_resume(
        cfg,
        path,
        &[],
        None,
        Some(cat),
        state,
        inputs,
        None,
        OffsetDateTime::now_utc(),
    )
}

fn resolve(
    cat: &Catalog,
    state: &GenerationState,
    inputs: &ScoreInputs,
) -> (Vec<ResolvedItem>, ResumeMap) {
    resolve_cfg(&config(), &sample_path(), cat, state, inputs).expect("resolve the For You sample")
}

/// The pattern's shape: cycles of two movies, then three episodes.
fn assert_two_movies_then_three_episodes(got: &[String]) {
    assert_eq!(
        got.len() % 5,
        0,
        "every cycle contributes 5 items (2 movies + 3 episodes), got {got:?}"
    );
    for (i, chunk) in got.chunks(5).enumerate() {
        assert!(
            chunk[0..2].iter().all(|id| is_movie(id)),
            "cycle {i} should open with two movies, got {chunk:?}"
        );
        assert!(
            chunk[2..5].iter().all(|id| !is_movie(id)),
            "cycle {i} should continue with three episodes, got {chunk:?}"
        );
    }
}

/// Project the state the next window is handed, exactly as the daemon does: the
/// pools' rotation from this resolve, plus the per-series cursor and the
/// recently-aired tail read back out of the play-history ledger these airings
/// were recorded in.
fn advance(
    cat: &Catalog,
    prev: &GenerationState,
    resume: ResumeMap,
    items: &[ResolvedItem],
) -> (GenerationState, Vec<String>) {
    const CHANNEL: &str = "test";
    let aired = ids(items);
    let show_ids = cat.show_ids_for(&aired).unwrap();
    let db = HistoryDb::open_in_memory().unwrap();
    let seed: Vec<PlayRecord> = prev
        .cursor
        .iter()
        .map(|(key, entry_id)| PlayRecord {
            entry_id: entry_id.clone(),
            show_id: Some(key.clone()),
            start: OffsetDateTime::UNIX_EPOCH,
            played_at: OffsetDateTime::UNIX_EPOCH,
            error_card: false,
        })
        .collect();
    db.record(CHANNEL, &seed).unwrap();
    let airings: Vec<PlayRecord> = aired
        .iter()
        .map(|id| PlayRecord {
            entry_id: id.clone(),
            show_id: show_ids.get(id).cloned(),
            start: OffsetDateTime::UNIX_EPOCH,
            played_at: OffsetDateTime::UNIX_EPOCH,
            error_card: false,
        })
        .collect();
    db.record(CHANNEL, &airings).unwrap();
    let recent = db.tail(CHANNEL, 200).unwrap();
    (
        GenerationState {
            resume,
            cursor: db.series_cursor(CHANNEL).unwrap(),
            tail: db
                .tail(CHANNEL, etv_station::constrain::DEFAULT_SEAM_TAIL)
                .unwrap(),
        },
        recent,
    )
}

fn is_movie(id: &str) -> bool {
    id.starts_with("mov-")
}

/// The committed sample loads and resolves at all. Everything below depends on
/// this, and it is also the check that the sample has not drifted from the
/// schema — a config-only edit that breaks it fails here rather than on air.
#[test]
fn the_sample_resolves() {
    let items = resolve(
        &library(),
        &GenerationState::default(),
        &ScoreInputs::default(),
    )
    .0;
    assert!(!items.is_empty(), "the sample must resolve to something");
}

/// Acceptance criterion 1: the shape is 2 movies, then 3 episodes, repeated —
/// and the two pools stay disjoint, which is what proves the movies pool's
/// plugin and the shows pool's plain `expr` each only ever produce their own
/// half.
#[test]
fn two_movies_then_three_episodes_from_disjoint_pools() {
    let got = ids(&resolve(
        &library(),
        &GenerationState::default(),
        &ScoreInputs::default(),
    )
    .0);

    assert_two_movies_then_three_episodes(&got);
}

/// The #115 regression, reproduced from the issue: `no_repeat_within: 10`
/// authored at *block* level on this pattern block used to reorder the finished
/// list and turn a cycle of `[mov, mov, ep, ep, ep]` into
/// `[mov, ep, ep, ep, mov]`. It is now the default the pools inherit, applied to
/// each pool's own list before the interleave, so the shape cannot move.
#[test]
fn a_block_level_constraint_is_inherited_without_reordering_the_pattern() {
    let mut cfg = config();
    cfg.rule.blocks[0].constraints = Some(etv_station::config::Constraints {
        no_repeat_within: Some(NoRepeatWithin::Positions(10)),
        separate_by: None,
        separate_min_gap: None,
    });

    let cat = library();
    let got = ids(&resolve_cfg(
        &cfg,
        &sample_path(),
        &cat,
        &GenerationState::default(),
        &ScoreInputs::default(),
    )
    .expect("resolve with a block-level constraint")
    .0);

    assert_two_movies_then_three_episodes(&got);
}

/// Acceptance criterion 2: the plugin's ranking comes from the plex-db-ex
/// datastore, not from this repo's own pooled watch history. `mov-contact`
/// leads (see `write_taste_fixture`'s hand-computed scores), and moving
/// `ScoreInputs.history` between two different runs changes nothing about
/// which movies air or in what order, because `taste-cosine.rhai` never
/// reads `ctx.history` at all — a stronger claim than "the order happens to
/// match", which the equality assertion below checks directly.
///
/// Exploration is turned off here (`exploration_fraction: 0.0`, overriding
/// the sample's own `0.2`): this test is checking the *rank*, and the
/// seeded surprise slot has its own tests below and in
/// `taste_cosine_scorer.rs`.
#[test]
fn the_datastore_taste_vector_decides_ranking_not_etv_stations_own_history() {
    let cat = library();
    let with_history = |watched: &str| ScoreInputs {
        target_count: 20,
        history: [WatchEvent {
            entry_id: watched.into(),
            watched_at: 900_000,
            watcher: None,
        }]
        .into(),
        recent: Vec::new(),
        now: 900_000 + 3600,
        tz: None,
    };
    let resolve_with = |inputs: &ScoreInputs| {
        let cfg = config_with_movies_config(exploration(0.0));
        ids(&resolve_cfg(
            &cfg,
            &sample_path(),
            &cat,
            &GenerationState::default(),
            inputs,
        )
        .expect("resolve the For You sample")
        .0)
        .into_iter()
        .filter(|id| is_movie(id))
        .collect::<Vec<_>>()
    };

    let movies_a = resolve_with(&with_history("mov-arrival"));
    let movies_b = resolve_with(&with_history("mov-blade"));

    assert_eq!(
        movies_a, movies_b,
        "etv-station's own watch history must not move the movie ranking at all"
    );
    assert_eq!(
        movies_a.first().map(String::as_str),
        Some("mov-contact"),
        "the highest cosine score should lead: {movies_a:?}"
    );
}

/// A pool's `config:` reaches the script and changes its judgment (#124),
/// exactly as it does for any plugin pool — but the specific tunable this
/// scorer reads is `exploration_fraction` (#254), not the affinity window an
/// older scorer used. Read back through the `source` field the script
/// attaches to each pick's `metadata` (#166): at `0.0` no slot can ever draw
/// explore (`unit_at` never returns a value below zero), and at `1.0` every
/// slot tries to. Checked this way rather than by comparing the two runs'
/// full orders, because with as few on-profile movies as this fixture
/// carries, two different fractions can coincidentally land on the same
/// order — the `source` tag says which mechanism actually filled each slot,
/// which an order comparison can't.
#[test]
fn a_pool_config_value_changes_the_scripts_ranking() {
    let cat = library();
    let inputs = ScoreInputs {
        target_count: 20,
        ..Default::default()
    };

    let movie_sources = |config: Option<serde_json::Value>| {
        resolve_cfg(
            &config_with_movies_config(config),
            &sample_path(),
            &cat,
            &GenerationState::default(),
            &inputs,
        )
        .expect("resolve the For You sample")
        .0
        .into_iter()
        .filter(|i| is_movie(&i.id))
        .map(|i| {
            i.metadata
                .as_ref()
                .and_then(|m| m.get("source"))
                .and_then(|s| s.as_str())
                .map(str::to_string)
        })
        .collect::<Vec<_>>()
    };

    let no_exploration = movie_sources(exploration(0.0));
    assert!(
        no_exploration.iter().all(|s| s.as_deref() == Some("rank")),
        "exploration_fraction: 0.0 must never draw explore: {no_exploration:?}"
    );

    let full_exploration = movie_sources(exploration(1.0));
    assert!(
        full_exploration
            .iter()
            .any(|s| s.as_deref() == Some("explore")),
        "exploration_fraction: 1.0 must draw explore at least once: {full_exploration:?}"
    );
}

/// The committed sample with an arbitrary `config` attached to its movies pool.
fn config_with_movies_config(pool_config: Option<serde_json::Value>) -> ChannelConfig {
    let mut cfg = config();
    movies_pool(&mut cfg).config = pool_config;
    cfg
}

/// Acceptance criterion 3, restated for this scorer: a movie that aired last
/// generation is *not* suppressed, and that is deliberate rather than a
/// regression. ADR 0002 records replay policy as the plugin's business, and
/// `taste-cosine.rhai` has none — its ranking barely moves generation to
/// generation, so the seeded exploration slot in `foryou.yaml`'s config is
/// what keeps two consecutive generations from being identical, not a
/// replay TTL. This pins that choice down as a test rather than leaving it
/// only in a comment: the top-ranked movie leads two generations running.
#[test]
fn a_movie_that_just_aired_is_not_suppressed_next_generation() {
    let cat = library();
    let inputs = ScoreInputs {
        target_count: 2,
        now: 900_000,
        ..Default::default()
    };
    let (items, resume) = resolve(&cat, &GenerationState::default(), &inputs);
    let first = ids(&items);
    let (state, recent) = advance(&cat, &GenerationState::default(), resume, &items);

    let second_inputs = ScoreInputs {
        target_count: 2,
        recent,
        now: 900_000 + 3600,
        ..Default::default()
    };
    let second = ids(&resolve(&cat, &state, &second_inputs).0);

    let first_movies: Vec<&String> = first.iter().filter(|id| is_movie(id)).collect();
    let second_movies: Vec<&String> = second.iter().filter(|id| is_movie(id)).collect();
    assert_eq!(
        first_movies, second_movies,
        "with no replay suppression in this scorer, the same top-ranked movies lead again: \
         {first_movies:?} then {second_movies:?}"
    );
}

/// Acceptance criterion 3's other half, unchanged by #254: a show the
/// station keeps surfacing continues through its episodes instead of
/// replaying its first three. `advance: resume` keys on `show_id`, so this
/// holds for the shows pool's plain `expr` exactly as it held when that pool
/// was a plugin, because the resume mechanism lives in pool resolution, not
/// in the script.
#[test]
fn an_in_progress_show_continues_across_generations() {
    let cat = library();
    let inputs = ScoreInputs {
        target_count: 20,
        now: 900_000,
        ..Default::default()
    };
    let (items, resume) = resolve(&cat, &GenerationState::default(), &inputs);
    let first: Vec<String> = ids(&items).into_iter().filter(|id| !is_movie(id)).collect();
    let (state, recent) = advance(&cat, &GenerationState::default(), resume, &items);

    let next_inputs = ScoreInputs {
        target_count: 20,
        recent,
        now: 900_000 + 3600,
        ..Default::default()
    };
    let second: Vec<String> = ids(&resolve(&cat, &state, &next_inputs).0)
        .into_iter()
        .filter(|id| !is_movie(id))
        .collect();

    // A show's own episode count — how many episodes it has to finish
    // before a fresh episode-1 draw is a legitimate restart rather than
    // resume failing to advance it.
    let episode_count = |show: &str| match show {
        "sev" => 6,
        "exp" => 4,
        "dev" => 2,
        other => panic!("unknown show prefix {other}"),
    };

    // Walk every airing across both generations, in air order, tracking
    // each show's highest episode aired so far. `advance: resume` means the
    // *next* airing of a show continues from there — episode N+1 — with
    // exactly one legitimate exception: a show already at its own last
    // episode is finished, and a fresh draw of it restarts at episode 1.
    // Anything else (a random skip backward, or forward past N+1) means
    // resume did not do its job.
    let mut highest: std::collections::HashMap<&str, i64> = std::collections::HashMap::new();
    for id in first.iter().chain(second.iter()) {
        let (show, ep) = id.split_once('-').unwrap();
        let ep: i64 = ep.parse().unwrap();
        match highest.get(show).copied() {
            None => {
                assert_eq!(
                    ep, 1,
                    "{id}: a show's first-ever airing must start at episode 1"
                );
            }
            Some(prev) if prev == episode_count(show) => {
                assert_eq!(
                    ep,
                    1,
                    "{id}: {show} already finished (episode {prev} of {}); a fresh draw \
                     restarts at 1, not {ep}",
                    episode_count(show)
                );
            }
            Some(prev) => {
                assert_eq!(
                    ep,
                    prev + 1,
                    "{id}: {show} was at episode {prev}; resume must continue at {}, not {ep} \
                     ({first:?} then {second:?})",
                    prev + 1
                );
            }
        }
        highest.insert(show, ep);
    }
}

/// Acceptance criterion 4: swapping the scorer keeps the channel working. The
/// station holds no taste rule, so a script with entirely different judgment
/// still produces a valid schedule from the same config shape — the movies
/// pool's `datastores` grant still opens (the station proves that before any
/// plugin runs, regardless of whether the plugin reaches for it), but the
/// replacement script below never calls `ctx.datastore` at all.
#[test]
fn swapping_the_scorer_keeps_the_channel_working() {
    let dir = tempfile::tempdir().unwrap();
    let plugins = dir.path().join("plugins");
    std::fs::create_dir(&plugins).unwrap();
    // Oldest-first, and deliberately ignores the datastore entirely — about
    // as far from taste-cosine's judgment as a scorer can get.
    std::fs::write(
        plugins.join("taste-cosine.rhai"),
        r#"
fn sources() { #{ movies: `item.type == "movie"` } }
fn pick(ctx) {
    let rows = [];
    for item in ctx.sets.movies { rows.push(item); }
    rows.sort(|a, b| if a.year < b.year { -1 } else if a.year > b.year { 1 } else { 0 });
    let out = [];
    for r in rows { out.push(r.entry_id); }
    out
}
"#,
    )
    .unwrap();

    // Same channel config, resolved with its `../plugins/taste-cosine.rhai`
    // pointing at the replacement — the config file is not edited at all.
    let samples = dir.path().join("samples");
    std::fs::create_dir(&samples).unwrap();
    let channel = samples.join("foryou.yaml");
    std::fs::copy(sample_path(), &channel).unwrap();

    let mut cfg = read_channel(&channel).expect("load the copied sample");
    movies_pool(&mut cfg).datastores[0].path = taste_store_path().to_str().unwrap().to_string();

    let cat = library();
    let (items, _) = resolve_cfg(
        &cfg,
        &channel,
        &cat,
        &GenerationState::default(),
        &ScoreInputs::default(),
    )
    .expect("a different scorer must still produce a schedule");

    let got = ids(&items);
    assert_eq!(got.len() % 5, 0, "the 2+3 shape survives the swap: {got:?}");
    assert_eq!(
        got.first().map(String::as_str),
        Some("mov-solaris"),
        "the replacement ranks oldest-first, so 1972 leads: {got:?}"
    );
}

//! Acceptance tests for the Endless pool provider (#176):
//! `examples/plugins/endless-distance.rhai` picks each item by keyword-cosine
//! distance from the item picked before it, inside a declared band that
//! drifts over the run, reading `tmdb_keywords` from a granted plex-db-ex
//! snapshot.
//!
//! Every fixture here is written fresh by this file (mirroring
//! `datastore_capability.rs`) rather than reading the real store — this
//! project's own convention (`vendor_plexdb_reader_pin.rs`'s doc comment) is
//! that a committed test never depends on a path outside the repository, so
//! these prove the algorithm's shape rather than the real library's numbers.
//! The band defaults baked into the script were tuned separately, by hand,
//! against a read-only copy of the real store.

use std::path::{Path, PathBuf};

use etv_station::catalog::{Catalog, Entry, Source};
use etv_station::config::DatastoreGrant;
use etv_station::score::{GrantedCapabilities, PickedItem, ScoreCache, ScoreInputs, pick};

const PLUGIN: &str = "../../examples/plugins/endless-distance.rhai";

fn plugin_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(PLUGIN)
}

/// Writes a minimal plexdb-schema store: one `movie` row per `(item_id,
/// keywords)` pair, each keyword landing as one `tmdb_keywords` enrichment
/// row with `key = "keyword"`. An entry with an empty keyword slice gets no
/// enrichment rows at all — a title the real store never tagged.
fn write_fixture(path: &Path, movies: &[(&str, &[&str])]) {
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
         CREATE TABLE edges (
             from_id TEXT NOT NULL, to_id TEXT NOT NULL, edge_type TEXT NOT NULL,
             rank INTEGER NOT NULL, fetched_at TEXT NOT NULL
         );
         CREATE TABLE plays (
             history_key TEXT PRIMARY KEY, item_id TEXT NOT NULL,
             plex_account_id INTEGER NOT NULL, viewed_at INTEGER NOT NULL
         );",
        version = plexdb_reader::SUPPORTED_SCHEMA_VERSION,
    ))
    .unwrap();
    for (id, keywords) in movies {
        conn.execute(
            "INSERT INTO items (item_id, type) VALUES (?1, 'movie')",
            (id,),
        )
        .unwrap();
        for kw in *keywords {
            conn.execute(
                "INSERT INTO enrichment (item_id, namespace, key, value, fetched_at) \
                 VALUES (?1, 'tmdb_keywords', 'keyword', ?2, '2026-01-01T00:00:00+00:00')",
                (id, kw),
            )
            .unwrap();
        }
    }
}

/// A catalog holding one `movie` entry per id in `movies`, plus (when
/// `episode` is set) one `episode` entry — used only by the movies-only
/// scoping test.
fn catalog(movies: &[&str], episode: Option<&str>) -> Catalog {
    let cat = Catalog::open_in_memory().unwrap();
    for id in movies {
        cat.upsert_entry(&Entry::new(*id, "movie", *id, Source::Plex))
            .unwrap();
    }
    if let Some(id) = episode {
        let mut e = Entry::new(id, "episode", id, Source::Plex);
        e.show_id = Some("show:x".into());
        e.season = Some(1);
        e.episode = Some(1);
        cat.upsert_entry(&e).unwrap();
    }
    cat
}

/// Runs the plugin once and returns the full `PickedItem` list, in the order
/// `pick()` chose them. `config` is the pool's `config:` bag — `None` takes
/// the script's own defaults. The shared entry point every test below (and
/// `run` below it) goes through, so none of them can drift apart on how a
/// generation is set up.
fn run_full(
    cat: &Catalog,
    db: &Path,
    recent: Vec<String>,
    target_count: usize,
    seed: u64,
    config: Option<serde_json::Value>,
) -> Vec<PickedItem> {
    let script = plugin_path();
    let mut cache = ScoreCache::default();
    cache.prepare(cat, &script, None).unwrap();

    let granted = GrantedCapabilities::from_names(&["catalog_read".to_string()])
        .with_datastores(&[DatastoreGrant {
            name: "taste_db".into(),
            path: db.to_str().unwrap().into(),
        }])
        .unwrap();

    let inputs = ScoreInputs {
        target_count,
        recent,
        ..Default::default()
    };

    pick(
        &cache,
        &script,
        None,
        &inputs,
        seed,
        "endless",
        config.as_ref(),
        granted,
    )
    .expect("pick() must succeed against a fixture with at least one keyworded candidate")
}

/// Runs the plugin once and returns `(entry_id, distance-from-previous)` in
/// the order `pick()` chose them — the shape every determinism/walk test
/// below needs.
fn run(
    cat: &Catalog,
    db: &Path,
    recent: Vec<String>,
    target_count: usize,
    seed: u64,
    config: Option<serde_json::Value>,
) -> Vec<(String, f64)> {
    run_full(cat, db, recent, target_count, seed, config)
        .into_iter()
        .map(|p| {
            let dist = p
                .metadata
                .as_ref()
                .and_then(|m| m.get("distance"))
                .and_then(|d| d.as_f64())
                .expect("every picked entry carries its distance from the previous one");
            (p.id, dist)
        })
        .collect()
}

/// `A`, `B`, `C`, `D` are built so every distance is exactly representable in
/// f64 (halves and whole numbers), and `E` carries no keywords at all:
///
/// | pair  | shared | n×n | cosine | distance |
/// |-------|--------|-----|--------|----------|
/// | A–B   | k1,k2  | 2×2 | 1.0    | 0.0      |
/// | A–C   | k1     | 2×2 | 0.5    | 0.5      |
/// | C–B   | k1     | 2×2 | 0.5    | 0.5      |
/// | A–D   | none   | —   | 0.0    | 1.0      |
///
/// With `min_distance: 0.3, max_distance: 0.7, drift: 0.0` and the walk
/// starting from `A` (already aired, so never itself returned): `B` is a
/// near-duplicate of `A` (0.0 < 0.3, excluded) and `D` shares no keyword with
/// `A` at all, so it is not even in `A`'s search space — only `C` (0.5) is
/// reachable, and it is the first pick. From `C`, `B` becomes reachable (0.5
/// again) and is picked next. From `B`, everything token-linked (`A`, `C`) is
/// already used, so the walk is forced to jump to whatever is left — `D` —
/// at the trivial-jump distance of exactly 1.0. `E` never appears: it has no
/// enrichment rows, so it is never a candidate at all.
#[test]
fn walks_the_band_then_jumps_when_the_local_neighbourhood_is_exhausted() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("taste.db");
    write_fixture(
        &db,
        &[
            ("A", &["k1", "k2"]),
            ("B", &["k1", "k2"]),
            ("C", &["k1", "k3"]),
            ("D", &["k4", "k5"]),
            ("E", &[]),
        ],
    );
    let cat = catalog(&["A", "B", "C", "D", "E"], None);

    let config = serde_json::json!({
        "min_distance": 0.3,
        "max_distance": 0.7,
        "drift": 0.0,
    });
    let out = run(&cat, &db, vec!["A".to_string()], 1, 0, Some(config));

    assert_eq!(
        out.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        vec!["C", "B", "D"],
        "expected the band to route through C and B before the exhausted \
         neighbourhood forces a jump to D",
    );
    assert_eq!(out[0].1, 0.5, "A→C");
    assert_eq!(out[1].1, 0.5, "C→B");
    assert_eq!(out[2].1, 1.0, "B→D: forced jump, no shared keyword left");
    assert!(
        !out.iter().any(|(id, _)| id == "A" || id == "E"),
        "A already aired and must not repeat; E has no keywords and must \
         never be a candidate: {out:?}"
    );
}

/// A title with zero `tmdb_keywords` rows is excluded from the candidate set
/// entirely — never picked, and never scored as if it were distance 1.0 from
/// everything (which would make it indistinguishable from a real, ordinary
/// far-away title).
#[test]
fn a_title_with_no_keywords_is_never_a_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("taste.db");
    write_fixture(
        &db,
        &[
            ("kw-1", &["k1", "k2"]),
            ("kw-2", &["k1", "k3"]),
            ("no-kw", &[]),
        ],
    );
    let cat = catalog(&["kw-1", "kw-2", "no-kw"], None);

    let out = run(&cat, &db, Vec::new(), 5, 3, None);
    assert!(
        !out.iter().any(|(id, _)| id == "no-kw"),
        "a keywordless title must never be picked: {out:?}"
    );
    assert!(!out.is_empty());
}

/// The same seed, config, catalog, and store snapshot must produce the
/// identical run — the property `--check-determinism` (#168) exercises at
/// the whole-channel level. Nothing in the script reads Rhai's own clock, so
/// two calls with identical inputs are pure repeats of each other.
#[test]
fn the_same_seed_and_snapshot_reproduce_the_same_run() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("taste.db");
    write_fixture(
        &db,
        &[
            ("m1", &["a", "b", "c"]),
            ("m2", &["a", "d"]),
            ("m3", &["b", "e"]),
            ("m4", &["c", "f"]),
            ("m5", &["d", "e", "f"]),
        ],
    );
    let cat = catalog(&["m1", "m2", "m3", "m4", "m5"], None);

    let a = run(&cat, &db, Vec::new(), 4, 42, None);
    let b = run(&cat, &db, Vec::new(), 4, 42, None);
    assert_eq!(a, b, "identical inputs must produce an identical run");
}

/// A pair that is each other's closest match, and shares no keyword with
/// anything else in the library, must not trap the walk: once both have been
/// used, `pick()` is forced onward into the rest of the library rather than
/// stopping. This is the failure named in #176's title — "Endless orbits two
/// neighbors" — reproduced structurally: `trap-x`/`trap-y` are mutually the
/// only thing in reach of each other, exactly the shape that makes a plain
/// nearest-neighbour walk with no exhaustion fallback stall at two items.
///
/// The rest of the library is an 18-movie keyword chain — `chain-0` and
/// `chain-1` share a keyword, `chain-1` and `chain-2` share the next one, and
/// so on — so the walk can keep finding an in-band neighbour step after
/// step. Asking for far more than the library holds (`target_count: 30`
/// against 20 titles total) means the only way to fall short of a near-full
/// sweep is to stall early.
#[test]
fn a_mutually_closest_pair_does_not_trap_the_walk() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("taste.db");

    let chain_ids: Vec<String> = (0..18).map(|i| format!("chain-{i}")).collect();
    let links: Vec<String> = (0..19).map(|i| format!("link-{i}")).collect();
    let chain_keywords: Vec<[&str; 2]> = (0..18)
        .map(|i| [links[i].as_str(), links[i + 1].as_str()])
        .collect();

    let mut movies: Vec<(&str, &[&str])> = vec![
        ("trap-x", &["trap_a", "trap_b"]),
        ("trap-y", &["trap_a", "trap_c", "trap_d", "trap_e"]),
    ];
    for i in 0..18 {
        movies.push((chain_ids[i].as_str(), &chain_keywords[i]));
    }
    write_fixture(&db, &movies);

    let ids: Vec<&str> = movies.iter().map(|(id, _)| *id).collect();
    let cat = catalog(&ids, None);

    let out = run(&cat, &db, Vec::new(), 30, 11, None);

    // 20 candidates total; one is consumed as the cold-start anchor and
    // never itself returned, so a full sweep is 19 distinct picks.
    assert_eq!(
        out.len(),
        19,
        "expected a full sweep of every candidate but the cold-start anchor: {out:?}"
    );
    let distinct: std::collections::HashSet<&str> = out.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(distinct.len(), out.len(), "no id repeats within one run");

    let chain_seen = out
        .iter()
        .filter(|(id, _)| id.starts_with("chain-"))
        .count();
    assert!(
        chain_seen >= 17,
        "the walk must reach well beyond the trap pair into the chain: saw {chain_seen} of 18 chain titles, run = {out:?}"
    );
}

/// Endless is scoped to movies (#176's resolution note: keywords sit on
/// movies and shows, never on episodes, and nothing maps an episode to its
/// show's keywords from inside a plugin today). An episode in the same
/// catalog — even a keyworded one, which the real library never actually
/// produces — must never reach `ctx.sets.movies` at all, because
/// `sources()` filters on `item.type == "movie"`.
#[test]
fn episodes_are_never_candidates() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("taste.db");
    write_fixture(
        &db,
        &[
            ("mov-1", &["k1", "k2"]),
            ("mov-2", &["k1", "k3"]),
            ("ep-1", &["k1", "k2"]),
        ],
    );
    let cat = catalog(&["mov-1", "mov-2"], Some("ep-1"));

    let out = run(&cat, &db, Vec::new(), 5, 1, None);
    assert!(
        !out.iter().any(|(id, _)| id == "ep-1"),
        "an episode must never be picked, even when it happens to carry \
         keywords: {out:?}"
    );
}

/// #392's acceptance criterion: each pick's audit record carries the
/// distance it was walked by, and it must agree with the `distance` the walk
/// already attaches under `metadata` — the two must never disagree.
#[test]
fn a_picks_audit_record_carries_the_distance_it_was_walked_by() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("taste.db");
    write_fixture(
        &db,
        &[
            ("m1", &["a", "b", "c"]),
            ("m2", &["a", "d"]),
            ("m3", &["b", "e"]),
            ("m4", &["c", "f"]),
            ("m5", &["d", "e", "f"]),
        ],
    );
    let cat = catalog(&["m1", "m2", "m3", "m4", "m5"], None);

    let picked = run_full(&cat, &db, Vec::new(), 4, 42, None);
    assert!(!picked.is_empty());
    for p in &picked {
        let meta = p.metadata.as_ref().unwrap();
        let meta_distance = meta["distance"].as_f64().unwrap();
        let detail_distance = meta["audit"][0]["detail"]["distance"].as_f64().unwrap();
        assert_eq!(
            detail_distance, meta_distance,
            "{}: audit detail.distance must agree with metadata.distance",
            p.id
        );
    }
}

/// #393's acceptance criteria: a step's runners-up get named in the pick's
/// audit trail, each with a distance and a script-authored reason, bounded
/// by the script's own `near_miss_limit` tunable, and the winner never names
/// itself.
#[test]
fn a_picks_near_misses_name_the_steps_runners_up() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("taste.db");
    write_fixture(
        &db,
        &[
            ("anchor", &["k1", "k2"]),
            // Three in-band ties (dist 0.5 from anchor) — one wins the
            // seeded tie-break, the other two must be named as runners-up.
            ("in-1", &["k1", "k3"]),
            ("in-2", &["k1", "k4"]),
            ("in-3", &["k1", "k5"]),
            // Out-of-band: shares only k1, closer than the 0.3 floor.
            ("close", &["k1"]),
        ],
    );
    let cat = catalog(&["anchor", "in-1", "in-2", "in-3", "close"], None);

    let config = serde_json::json!({
        "min_distance": 0.3,
        "max_distance": 0.7,
        "drift": 0.0,
    });
    let picked = run_full(&cat, &db, vec!["anchor".to_string()], 1, 0, Some(config));
    assert!(!picked.is_empty());

    let mut saw_non_empty = false;
    for p in &picked {
        let meta = p.metadata.as_ref().unwrap();
        let near_misses = meta["audit"][0]["detail"]["near_misses"]
            .as_array()
            .unwrap_or_else(|| panic!("{}: near_misses must be an array", p.id));
        assert!(
            near_misses.len() <= 3,
            "{}: near_misses exceeded the script's default bound of 3, got {}",
            p.id,
            near_misses.len()
        );
        for nm in near_misses {
            assert_ne!(
                nm["id"].as_str(),
                Some(p.id.as_str()),
                "{}: a pick must never name itself as its own near miss",
                p.id
            );
            assert!(
                nm["distance"].as_f64().is_some(),
                "{}: near_miss distance must be numeric",
                p.id
            );
            let reason = nm["reason"]
                .as_str()
                .unwrap_or_else(|| panic!("{}: near_miss reason must be a string", p.id));
            assert!(
                !reason.is_empty(),
                "{}: near_miss reason must not be empty",
                p.id
            );
        }
        if !near_misses.is_empty() {
            saw_non_empty = true;
        }
    }
    assert!(
        saw_non_empty,
        "expected at least one pick to name a runner-up: {picked:?}"
    );
}

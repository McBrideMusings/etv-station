//! Acceptance test for the keyword-cosine taste scorer (#254): a movies-only
//! plugin that ranks by how close a candidate's TMDB keywords sit to the
//! house's pooled taste vector, read from a granted plex-db-ex datastore,
//! with a seeded exploration slot layered on top of the ranking.
//!
//! Runs `score::pick` directly against `examples/plugins/taste-cosine.rhai`
//! (the committed script, proving it still compiles and runs) and a
//! hand-built `plexdb.db` fixture — the same style `datastore_capability.rs`
//! uses for the generic datastore-grant mechanism this file exercises for
//! one specific script.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use etv_station::catalog::{Catalog, Entry, EntrySource, Source};
use etv_station::config::DatastoreGrant;
use etv_station::score::{
    Capability, GrantedCapabilities, PickedItem, ScoreCache, ScoreInputs, declared_capabilities,
    declared_hooks, pick,
};

fn plugin_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/plugins/taste-cosine.rhai")
        .canonicalize()
        .expect("examples/plugins/taste-cosine.rhai must exist")
}

fn grant(db: &Path) -> GrantedCapabilities {
    GrantedCapabilities::from_names(&["catalog_read".into(), "watch_history".into()])
        .with_datastores(&[DatastoreGrant {
            name: "taste".into(),
            path: db.to_str().unwrap().into(),
        }])
        .expect("a fixture store that just validated must still open")
}

/// The tables `plexdb-reader` opens, at the schema version it understands —
/// the empty store each fixture below then fills its own way.
fn empty_store(path: &Path) -> rusqlite::Connection {
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
         );",
        version = plexdb_reader::SUPPORTED_SCHEMA_VERSION,
    ))
    .unwrap();
    conn
}

/// A minimal but real plexdb store: one played movie ("mov-a") defines the
/// pooled weights, and three more candidates carry a mix of on-profile,
/// off-profile, and absent keywords — enough to hand-compute every one of
/// their cosine scores.
fn write_taste_fixture(path: &Path) {
    empty_store(path)
        .execute_batch(
            "INSERT INTO items (item_id, type) VALUES
                 ('mov-a', 'movie'), ('mov-b', 'movie'), ('mov-c', 'movie'), ('mov-d', 'movie');
             -- mov-a: keywords contact + time, watched once below — this is what
             -- defines the pooled weights (0.5 each: sqrt(1 play) / 2 keywords).
             -- mov-c: contact only (on-profile) -> 0.5 / sqrt(1) = 0.5.
             -- mov-d: time (on-profile) + desert (off-profile) -> 0.5 / sqrt(2).
             -- mov-b: no keywords at all -> ineligible, score 0.0.
             INSERT INTO enrichment (item_id, namespace, key, value, fetched_at) VALUES
                 ('mov-a', 'tmdb_keywords', 'keyword', 'contact', '2026-01-01T00:00:00+00:00'),
                 ('mov-a', 'tmdb_keywords', 'keyword', 'time', '2026-01-01T00:00:00+00:00'),
                 ('mov-c', 'tmdb_keywords', 'keyword', 'contact', '2026-01-01T00:00:00+00:00'),
                 ('mov-d', 'tmdb_keywords', 'keyword', 'time', '2026-01-01T00:00:00+00:00'),
                 ('mov-d', 'tmdb_keywords', 'keyword', 'desert', '2026-01-01T00:00:00+00:00');
             INSERT INTO plays (history_key, item_id, plex_account_id, viewed_at) VALUES
                 ('h1', 'mov-a', 42, 1700000000);",
        )
        .unwrap();
}

/// Two accounts' plays in one store (#278): account 42 watches `acct-a`
/// once, account 99 watches `acct-b` twice (a rewatch, so its own weights
/// scale to `sqrt(2)/2` rather than coincidentally landing on the same
/// `0.5` account 42's single play produces — the whole point being that
/// account 42's vector, account 99's vector, and the vector pooling both
/// must be three genuinely different numbers, not just three different
/// orderings of the same ones).
///
/// `acct-a` and `acct-b` share the `contact` keyword and each carry one
/// keyword the other does not (`time`, `space`), so `acct-c` — `contact`
/// only, never watched by anyone — scores differently under all three
/// vectors: 42 alone, 99 alone, and the two pooled together.
fn write_two_account_fixture(path: &Path) {
    empty_store(path)
        .execute_batch(
            "INSERT INTO items (item_id, type) VALUES
                 ('acct-a', 'movie'), ('acct-b', 'movie'), ('acct-c', 'movie');
             INSERT INTO enrichment (item_id, namespace, key, value, fetched_at) VALUES
                 ('acct-a', 'tmdb_keywords', 'keyword', 'contact', '2026-01-01T00:00:00+00:00'),
                 ('acct-a', 'tmdb_keywords', 'keyword', 'time', '2026-01-01T00:00:00+00:00'),
                 ('acct-b', 'tmdb_keywords', 'keyword', 'contact', '2026-01-01T00:00:00+00:00'),
                 ('acct-b', 'tmdb_keywords', 'keyword', 'space', '2026-01-01T00:00:00+00:00'),
                 ('acct-c', 'tmdb_keywords', 'keyword', 'contact', '2026-01-01T00:00:00+00:00');
             INSERT INTO plays (history_key, item_id, plex_account_id, viewed_at) VALUES
                 ('h1', 'acct-a', 42, 1700000000),
                 ('h2', 'acct-b', 99, 1700000001),
                 ('h3', 'acct-b', 99, 1700000002);",
        )
        .unwrap();
}

/// One locally-sourced entry, so resolution reaches a playable item.
fn add(cat: &Catalog, id: &str, kind: &str) {
    let e = Entry::new(id, kind, id, Source::Plex);
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

fn catalog_of<'a>(movie_ids: impl IntoIterator<Item = &'a str>) -> Catalog {
    let cat = Catalog::open_in_memory().unwrap();
    for id in movie_ids {
        add(&cat, id, "movie");
    }
    cat
}

fn small_catalog() -> Catalog {
    catalog_of(["mov-a", "mov-b", "mov-c", "mov-d"])
}

/// The committed script prepared against a catalog, plus the fixture store its
/// `taste` grant opens — every test here needs all three together, and the
/// `TempDir` has to outlive the store's path.
struct Scorer {
    _dir: tempfile::TempDir,
    db: PathBuf,
    script: PathBuf,
    cache: ScoreCache,
}

impl Scorer {
    /// `write_store` fills the plexdb fixture the script reads; `cat` is the
    /// library it gets to rank.
    fn new(cat: &Catalog, write_store: impl FnOnce(&Path)) -> Self {
        let script = plugin_path();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("taste.db");
        write_store(&db);
        // The two halves, in the order the daemon runs them: `prepare` reads
        // the catalog, `pick` below ranks what it found.
        let mut cache = ScoreCache::default();
        cache.prepare(cat, &script, None).unwrap();
        Self {
            _dir: dir,
            db,
            script,
            cache,
        }
    }

    /// One generation on a pooled channel — the ordinary case, with no one
    /// account behind it. `exploration_fraction` reaches the script as the
    /// pool's `config:`; `None` leaves the script's own default in place.
    fn pick(
        &self,
        pool: &str,
        seed: u64,
        target_count: usize,
        exploration_fraction: Option<f64>,
    ) -> Vec<PickedItem> {
        self.pick_for(pool, seed, target_count, exploration_fraction, None)
    }

    /// One generation for a `single_user`-scoped channel (#278):
    /// `account_id` reaches the script as `ctx.account_id`, exactly as the
    /// station resolves it from a channel's `scoring: { taste_scope:
    /// single_user, user: … }`. No exploration, same as the ranking test
    /// above — this is checking which taste vector the script ranked
    /// against, not the surprise slot.
    fn pick_scoped(
        &self,
        pool: &str,
        target_count: usize,
        account_id: Option<i64>,
    ) -> Vec<PickedItem> {
        self.pick_for(pool, 0, target_count, Some(0.0), account_id)
    }

    /// The one call into `score::pick` both of the above make, so the two
    /// cannot drift apart on how a generation is set up.
    fn pick_for(
        &self,
        pool: &str,
        seed: u64,
        target_count: usize,
        exploration_fraction: Option<f64>,
        account_id: Option<i64>,
    ) -> Vec<PickedItem> {
        let config = exploration_fraction.map(|f| serde_json::json!({ "exploration_fraction": f }));
        let inputs = ScoreInputs {
            target_count,
            account_id,
            ..Default::default()
        };
        pick(
            &self.cache,
            &self.script,
            None,
            &inputs,
            seed,
            pool,
            config.as_ref(),
            grant(&self.db),
        )
        .unwrap()
    }
}

/// A scorer over a larger library: `n_eligible` movies each carrying one
/// keyword (never matched to any play, so every score is the neutral 0.0 —
/// only eligibility is under test here), plus `n_ineligible` with no
/// enrichment at all. Catalog and store are built from one id list, so the two
/// cannot drift apart.
fn large_scorer(n_eligible: usize, n_ineligible: usize) -> Scorer {
    let ids: Vec<String> = (0..n_eligible)
        .map(|i| format!("el-{i:04}"))
        .chain((0..n_ineligible).map(|i| format!("in-{i:04}")))
        .collect();
    let cat = catalog_of(ids.iter().map(String::as_str));
    Scorer::new(&cat, |db| {
        let conn = empty_store(db);
        for (i, id) in ids.iter().enumerate() {
            conn.execute(
                "INSERT INTO items (item_id, type) VALUES (?1, 'movie')",
                [id],
            )
            .unwrap();
            if id.starts_with("el-") {
                conn.execute(
                    "INSERT INTO enrichment (item_id, namespace, key, value, fetched_at) \
                     VALUES (?1, 'tmdb_keywords', 'keyword', ?2, 't')",
                    rusqlite::params![id, format!("kw{i}")],
                )
                .unwrap();
            }
        }
    })
}

fn source_of(p: &PickedItem) -> Option<&str> {
    p.metadata.as_ref()?.get("source")?.as_str()
}

fn score_of(picked: &[PickedItem], id: &str) -> f64 {
    picked
        .iter()
        .find(|p| p.id == id)
        .unwrap_or_else(|| {
            panic!(
                "{id} missing from the pick: {:?}",
                picked.iter().map(|p| &p.id).collect::<Vec<_>>()
            )
        })
        .metadata
        .as_ref()
        .unwrap()["score"]
        .as_f64()
        .unwrap()
}

/// A hand-computed cosine score, checked to within float noise. `what` names
/// whose score it is, so a failure reads as a sentence rather than as two
/// bare numbers.
fn assert_close(got: f64, expected: f64, what: &str) {
    assert!(
        (got - expected).abs() < 1e-9,
        "{what} = {got}, expected {expected}"
    );
}

/// #278's acceptance criterion: a `single_user` channel ranks on that
/// account's own vector, not the house's pooled one, and two different
/// accounts produce two genuinely different rankings — the failure this
/// ticket exists to prevent is exactly the case where all three land on the
/// same numbers because the branch silently never took.
///
/// `acct-c` (the `contact`-only candidate neither account watched) is the
/// clean single number to check: account 42 alone, account 99 alone, and the
/// two pooled together each define a different `contact` weight
/// (`write_two_account_fixture`'s own doc comment works the arithmetic), so
/// its score is the one place this test needs to hand-verify a value rather
/// than merely diff two runs.
#[test]
fn a_single_user_channel_ranks_on_that_accounts_vector_not_the_pooled_one() {
    let cat = catalog_of(["acct-a", "acct-b", "acct-c"]);
    let scorer = Scorer::new(&cat, write_two_account_fixture);

    let acct_42 = scorer.pick_scoped("movies", 3, Some(42));
    let acct_99 = scorer.pick_scoped("movies", 3, Some(99));
    let pooled = scorer.pick_scoped("movies", 3, None);

    // Account 42's own vector: `contact` and `time` at 0.5 each, from its one
    // play of `acct-a` (sqrt(1 play) / 2 keywords).
    let expected_42 = 0.5;
    let c42 = score_of(&acct_42, "acct-c");
    assert_close(c42, expected_42, "account 42's acct-c score");

    // Account 99's own vector: `contact` and `space` at sqrt(2)/2 each, from
    // its two plays (a rewatch) of `acct-b`.
    let expected_99 = 2.0_f64.sqrt() / 2.0;
    let c99 = score_of(&acct_99, "acct-c");
    assert_close(c99, expected_99, "account 99's acct-c score");

    // The pooled vector sums both accounts' contributions to `contact`
    // rather than averaging or picking one — plex-db-ex#39's rollup, summed
    // not normalised per account.
    let cpooled = score_of(&pooled, "acct-c");
    assert_close(
        cpooled,
        expected_42 + expected_99,
        "the pooled acct-c score",
    );

    // Three different numbers, not three labels on the same one — the
    // silent failure #278 exists to rule out.
    assert_ne!(c42, c99, "the two accounts must not rank identically");
    assert_ne!(c42, cpooled, "an account must not rank like the house pool");
    assert_ne!(
        c99, cpooled,
        "the other account must not rank like it either"
    );
}

/// The committed worked example runs. Without this, `taste-cosine.rhai` is
/// documentation that nothing proves still compiles — and this scorer only
/// fails at generation time, on a running channel.
///
/// Also the acceptance criterion asking for a score "verified against a
/// hand-computed value for one named title": `pick()` never returns a score
/// itself (ADR 0002 — the score stays internal to the plugin), so this reads
/// it back the one way it does cross the boundary, the `metadata` the script
/// attaches to each pick (#166).
#[test]
fn the_committed_example_plugin_runs_and_scores_correctly() {
    let scorer = Scorer::new(&small_catalog(), write_taste_fixture);
    // No exploration for this test — it is checking the ranking, not the
    // surprise slot, which has its own tests below.
    let picked = scorer.pick("movies", 0, 3, Some(0.0));

    let ids: Vec<String> = picked.iter().map(|p| p.id.clone()).collect();
    assert_eq!(
        ids,
        vec!["mov-a", "mov-c", "mov-d", "mov-b"],
        "highest cosine score first, entry_id breaking the score-0.0 tie: {ids:?}"
    );

    // mov-d carries "time" (on profile, weight 0.5 — mov-a's lone play
    // split across its two keywords) and "desert" (off profile, weight 0):
    // sum 0.5, divided by sqrt(2) keywords — #254's worked table, applied to
    // this fixture's own numbers rather than restated abstractly.
    let mov_d = picked.iter().find(|p| p.id == "mov-d").unwrap();
    let score = mov_d.metadata.as_ref().unwrap()["score"].as_f64().unwrap();
    let expected = 0.5 / 2.0_f64.sqrt();
    assert!(
        (score - expected).abs() < 1e-9,
        "mov-d score = {score}, expected {expected}"
    );
    let keywords: Vec<&str> = mov_d.metadata.as_ref().unwrap()["on_profile_keywords"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        keywords,
        vec!["time"],
        "only the on-profile keyword should be named — desert is off-profile"
    );

    // mov-b has no keywords in the store at all: ineligible, and its cosine
    // score is exactly zero rather than undefined.
    let mov_b = picked.iter().find(|p| p.id == "mov-b").unwrap();
    let mov_b_score = mov_b.metadata.as_ref().unwrap()["score"].as_f64().unwrap();
    assert_eq!(mov_b_score, 0.0);
}

/// `capabilities()` declares exactly the three things the sample channel
/// grants (#167, #181) — a channel pool that grants fewer fails to load
/// naming whichever is missing; that generic mechanism is covered by
/// `config::validate::tests` and `datastore_capability.rs`. This is the half
/// specific to this script: what it actually asks for.
#[test]
fn the_script_declares_pool_provider_and_all_three_capabilities() {
    let path = plugin_path();
    assert_eq!(declared_hooks(&path).unwrap(), vec!["pool_provider"]);
    let caps = declared_capabilities(&path).unwrap();
    assert_eq!(
        caps,
        vec![
            Capability::CatalogRead,
            Capability::WatchHistory,
            Capability::Datastore("taste".into()),
        ]
    );
}

/// Acceptance criterion: no episode is ever handed to the scorer. Movies
/// only, at the source — `sources()` declares nothing but `movies`, so
/// `ctx.sets` can never carry an episode. Proven the way a misconfiguration
/// would find out: a catalog holding both, resolved for a pool named
/// "shows" (as if a channel had mistakenly pointed its episode pool at this
/// script) still returns only movies, because there is no `episodes` set for
/// any pool name to draw from.
#[test]
fn only_movies_ever_reach_the_scorer_regardless_of_which_pool_asks() {
    let cat = small_catalog();
    add(&cat, "ep-1", "episode");
    let scorer = Scorer::new(&cat, write_taste_fixture);

    let picked = scorer.pick("shows", 0, 3, None);
    assert!(
        picked.iter().all(|p| p.id != "ep-1"),
        "an episode must never be returned, even for a pool named \"shows\": {:?}",
        picked.iter().map(|p| &p.id).collect::<Vec<_>>()
    );
}

/// Determinism (#168): the same seed, against the same fixture, reproduces
/// the exact same pick every time — this is what lets "generate twice from
/// a pinned seed" produce byte-identical playout JSON, since `pick()`'s
/// output is exactly what a generation writes into it (#166).
#[test]
fn the_same_seed_reproduces_the_same_pick_exactly() {
    let scorer = Scorer::new(&small_catalog(), write_taste_fixture);
    let run = || {
        scorer
            .pick("movies", 7, 3, Some(1.0))
            .into_iter()
            .map(|p| (p.id, p.metadata))
            .collect::<Vec<_>>()
    };

    assert_eq!(run(), run(), "the same seed must reproduce byte-for-byte");
}

/// Acceptance criterion: changing the seed changes which titles land in the
/// exploration slots.
#[test]
fn a_different_seed_changes_the_exploration_draw() {
    let scorer = Scorer::new(&small_catalog(), write_taste_fixture);
    // exploration_fraction 1.0 makes every slot an explore draw, so the
    // whole order is the seeded permutation — the clearest possible signal
    // that the seed, not chance, decides it.
    let run = |seed: u64| {
        scorer
            .pick("movies", seed, 3, Some(1.0))
            .into_iter()
            .map(|p| p.id)
            .collect::<Vec<_>>()
    };

    let a = run(7);
    let b = run(99);
    assert_ne!(
        a, b,
        "different seeds must draw a different order: {a:?} vs {b:?}"
    );
}

/// Acceptance criterion: no exploration slot is ever filled by a title with
/// no attributes in the scorer's namespace. `exploration_fraction: 1.0`
/// tries to explore on every slot, and `target_count` is sized so the
/// requested output exactly equals the eligible count — if a single
/// ineligible id ever appeared, either the eligibility filter or the
/// rank-fallback boundary would be wrong.
#[test]
fn no_exploration_slot_ever_gets_an_ineligible_title() {
    let scorer = large_scorer(15, 5);
    // out_len = target_count + 10 = 15, exactly the eligible count.
    let picked = scorer.pick("movies", 123, 5, Some(1.0));
    let ids: HashSet<String> = picked.into_iter().map(|p| p.id).collect();
    assert_eq!(
        ids.len(),
        15,
        "every eligible candidate should be used exactly once"
    );
    for id in &ids {
        assert!(
            id.starts_with("el-"),
            "an ineligible id must never appear: {id}"
        );
    }
}

/// Acceptance criterion: with `exploration_fraction: 0.2`, roughly one slot
/// in five comes from the seeded random pick rather than from rank. Counted
/// directly off `metadata.source` rather than by diffing against a
/// `fraction: 0.0` baseline — an early explore draw removes that candidate
/// from every later rank position too, so a position-by-position diff
/// overcounts by cascading one substitution into many.
#[test]
fn exploration_fraction_lands_roughly_one_in_five() {
    let scorer = large_scorer(400, 100);
    let picked = scorer.pick("movies", 42, 490, Some(0.2));
    let total = picked.len();
    let explore_count = picked
        .iter()
        .filter(|p| source_of(p) == Some("explore"))
        .count();
    let ratio = explore_count as f64 / total as f64;
    assert!(
        ratio > 0.10 && ratio < 0.30,
        "expected roughly 1 in 5 slots to come from the explore draw, got {ratio} \
         ({explore_count}/{total})"
    );
}

/// `taste-engine.rhai` is gone (#254) — this is the negative half of "the
/// committed example runs": nothing should still be able to load it.
#[test]
fn the_deleted_scorer_is_gone() {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins/taste-engine.rhai");
    assert!(
        !path.exists(),
        "taste-engine.rhai must be deleted, not left beside its replacement"
    );
}

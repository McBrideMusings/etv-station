//! Acceptance test for the keyword-cosine taste scorer (#254): a plugin that
//! ranks by how close a candidate's TMDB keywords sit to the house's pooled
//! taste vector, read from a granted plex-db-ex datastore, with a seeded
//! exploration slot layered on top of the ranking and a recency damping
//! under it.
//!
//! It ranks films or series, chosen by a pool's `unit:` — `"movie"`,
//! `"show"`, or `"season"`. Series became possible when #274 made the
//! catalog's `show_id` GUID-derived, so an episode (which carries no
//! keywords of its own) can be resolved to the show that does.
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
    Capability, GrantedCapabilities, PickedItem, PoolSources, ScoreCache, ScoreInputs,
    declared_capabilities, declared_hooks, pick,
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
             -- Of the four titles, three carry an in-namespace fact at all
             -- (mov-a, mov-c, mov-d; mov-b has none) so doc_count = 3, and the
             -- keyword document frequencies are contact=2 (mov-a, mov-c),
             -- time=2 (mov-a, mov-d), desert=1 (mov-d only) (#294).
             -- mov-c: contact only (on-profile, df 2 of 3 -> idf factor
             --   1 + ln(3/2)) -> 0.5 * (1 + ln(3/2)) / sqrt(1).
             -- mov-d: time (on-profile, df 2 of 3 -> idf factor 1 + ln(3/2))
             --   + desert (off-profile, contributes nothing to the sum, but
             --   still counts toward the sqrt(n) divisor) ->
             --   0.5 * (1 + ln(3/2)) / sqrt(2).
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

/// A pool `config:` block holding just `exploration_fraction`, or no block at
/// all — which is what most tests here want, and what leaves the script's own
/// default in place rather than overriding it with the same number.
fn tunables(exploration_fraction: Option<f64>) -> Option<serde_json::Value> {
    exploration_fraction.map(|f| serde_json::json!({ "exploration_fraction": f }))
}

/// The committed script prepared against a catalog, plus the fixture store its
/// `taste` grant opens — every test here needs all three together, and the
/// `TempDir` has to outlive the store's path.
struct Scorer {
    _dir: tempfile::TempDir,
    db: PathBuf,
    script: PathBuf,
    cache: ScoreCache,
    /// The pool's own `sources:` table (#210), or `None` to let the script's
    /// `sources()` stand. It is half the cache key, so the same value has to
    /// reach `prepare` and `pick` — holding it here is what stops those two
    /// from drifting apart.
    sources: Option<PoolSources>,
}

impl Scorer {
    /// `write_store` fills the plexdb fixture the script reads; `cat` is the
    /// library it gets to rank.
    fn new(cat: &Catalog, write_store: impl FnOnce(&Path)) -> Self {
        Self::new_sourced(cat, write_store, None)
    }

    /// The same, for a pool that writes its own candidate queries — the shape
    /// an influence-tilted pool needs (#396), since the `influence` set is
    /// channel-authored and the script declares no default for it.
    fn new_sourced(
        cat: &Catalog,
        write_store: impl FnOnce(&Path),
        sources: Option<PoolSources>,
    ) -> Self {
        let script = plugin_path();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("taste.db");
        write_store(&db);
        // The two halves, in the order the daemon runs them: `prepare` reads
        // the catalog, `pick` below ranks what it found.
        let mut cache = ScoreCache::default();
        cache.prepare(cat, &script, sources.as_ref()).unwrap();
        Self {
            _dir: dir,
            db,
            script,
            cache,
            sources,
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
        self.pick_for(
            pool,
            seed,
            target_count,
            tunables(exploration_fraction),
            None,
            &[],
        )
    }

    /// One generation on a pool tilted toward an influence set (#396).
    /// `influence_weight` is the pool's `config:` value; the set itself came
    /// from the `sources:` table this scorer was built with. Exploration is
    /// off, so the order under test is the tilted ranking itself.
    fn pick_tilted(&self, target_count: usize, influence_weight: f64) -> Vec<PickedItem> {
        let config = serde_json::json!({
            "exploration_fraction": 0.0,
            "influence_weight": influence_weight,
        });
        self.pick_for("movies", 0, target_count, Some(config), None, &[])
    }

    /// One generation on a channel that has already aired something (#254's
    /// recency damping). `recent` is the airing tail exactly as the station
    /// hands it over — oldest first, newest LAST — so a test naming the
    /// order it wants reads the same way the daemon's own
    /// `HistoryDb::tail` returns it.
    fn pick_recent(
        &self,
        target_count: usize,
        exploration_fraction: Option<f64>,
        recent: &[&str],
    ) -> Vec<PickedItem> {
        self.pick_for(
            "movies",
            0,
            target_count,
            tunables(exploration_fraction),
            None,
            recent,
        )
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
        self.pick_for(pool, 0, target_count, tunables(Some(0.0)), account_id, &[])
    }

    /// The one call into `score::pick` all of the above make, so they cannot
    /// drift apart on how a generation is set up. `config` is the pool's
    /// `config:` block verbatim; `None` hands the script an absent block,
    /// which reaches `tunable()` differently from an empty one.
    fn pick_for(
        &self,
        pool: &str,
        seed: u64,
        target_count: usize,
        config: Option<serde_json::Value>,
        account_id: Option<i64>,
        recent: &[&str],
    ) -> Vec<PickedItem> {
        let inputs = ScoreInputs {
            target_count,
            account_id,
            recent: recent.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        };
        pick(
            &self.cache,
            &self.script,
            self.sources.as_ref(),
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

fn detail_of(p: &PickedItem) -> &serde_json::Value {
    &p.metadata.as_ref().unwrap()["audit"][0]["detail"]
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

    // mov-d carries "time" (on profile, weight 0.5 — mov-a's lone play split
    // across its two keywords — scaled by the idf factor 1 + ln(doc_count /
    // df), where doc_count is 3 (mov-a, mov-c, mov-d each carry a fact;
    // mov-b carries none) and "time"'s own df is 2 (mov-a, mov-d)) and
    // "desert" (off profile, contributes 0 to the sum but still counts
    // toward the sqrt(n) divisor): sum 0.5 * (1 + ln(3/2)), divided by
    // sqrt(2) keywords — #254's worked table, extended by #294's idf term,
    // applied to this fixture's own numbers rather than restated
    // abstractly.
    let mov_d = picked.iter().find(|p| p.id == "mov-d").unwrap();
    let score = mov_d.metadata.as_ref().unwrap()["score"].as_f64().unwrap();
    let expected = 0.5 * (1.0 + (3.0_f64 / 2.0).ln()) / 2.0_f64.sqrt();
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

/// `unit:` decides what gets ranked — never the pool's name, and never what
/// happens to be in the catalog. Now that `sources()` offers both sets, a
/// pool that omits `unit:` must still rank movies, so a channel that points
/// its episode pool at this script and forgets the key gets an obviously
/// wrong schedule rather than a silent mixture of films and episodes.
///
/// Proven the way the misconfiguration would find out: a catalog holding
/// both, resolved for a pool actually named "shows", still returns only
/// movies.
#[test]
fn a_pool_that_omits_the_unit_key_ranks_movies_whatever_it_is_called() {
    let cat = small_catalog();
    add(&cat, "ep-1", "episode");
    let scorer = Scorer::new(&cat, write_taste_fixture);

    let picked = scorer.pick("shows", 0, 3, None);
    assert!(
        picked.iter().all(|p| p.id != "ep-1"),
        "no episode may come back without unit = \"show\", even for a pool named \"shows\": {:?}",
        picked.iter().map(|p| &p.id).collect::<Vec<_>>()
    );
}

/// An episode with no `show_id` is dropped rather than aired as a series of
/// one. A shows pool exists to air series, and #274's own note is that a
/// catalog with no `show_id` still emits television — it just draws a
/// different show every slot and nothing that groups episodes can work.
/// Silently mixing those in would reintroduce exactly that.
#[test]
fn an_episode_with_no_show_id_never_reaches_a_shows_pool() {
    let cat = shows_catalog();
    // An orphan: an episode the ingester never resolved a show for.
    let orphan = Entry::new("orphan-ep", "episode", "orphan-ep", Source::Plex);
    cat.upsert_entry(&orphan).unwrap();
    cat.add_source(&EntrySource {
        source: Source::LocalFs,
        source_id: "fs-orphan".into(),
        entry_id: "orphan-ep".into(),
        playback_path: "/media/orphan.mkv".into(),
        last_seen: None,
        missing_since: None,
    })
    .unwrap();

    let scorer = Scorer::new(&cat, write_shows_fixture);
    let picked = scorer.pick_shows(8, &[]);
    assert!(
        picked.iter().all(|p| p.id != "orphan-ep"),
        "an episode with no show_id must be dropped: {:?}",
        order_of(&picked)
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

/// #392's acceptance criterion: a pick's audit record carries its score,
/// rank, the candidate-set size, and its draw kind — and an exploration draw
/// reads as distinguishable from a ranked one rather than as a low-ranked
/// accident. A high `exploration_fraction` (the same knob
/// `exploration_fraction_lands_roughly_one_in_five` uses) guarantees at
/// least one of each kind lands in the picked set.
#[test]
fn a_picks_audit_record_carries_score_rank_candidate_count_and_draw_kind() {
    let scorer = large_scorer(400, 100);
    let picked = scorer.pick("movies", 42, 490, Some(0.2));

    let mut saw_ranked = false;
    let mut saw_exploration = false;
    for p in &picked {
        let detail = detail_of(p);
        let detail_score = detail["score"].as_f64().unwrap();
        let meta_score = p.metadata.as_ref().unwrap()["score"].as_f64().unwrap();
        assert_eq!(
            detail_score, meta_score,
            "{}: detail.score must agree with metadata.score",
            p.id
        );
        assert!(
            detail["rank"].as_i64().unwrap() >= 1,
            "{}: rank must be 1-based",
            p.id
        );
        assert_eq!(
            detail["candidate_count"].as_i64().unwrap(),
            500,
            "{}: candidate_count must cover every candidate scored, eligible or not",
            p.id
        );
        let draw = detail["draw"].as_str().unwrap();
        match (source_of(p), draw) {
            (Some("explore"), "exploration") => saw_exploration = true,
            (Some("explore"), other) => panic!(
                "{}: exploration pick's draw was {other:?}, not \"exploration\"",
                p.id
            ),
            (_, "ranked") => saw_ranked = true,
            (_, other) => panic!("{}: ranked pick's draw was {other:?}, not \"ranked\"", p.id),
        }
    }
    assert!(saw_ranked, "expected at least one ranked pick in the run");
    assert!(
        saw_exploration,
        "expected at least one exploration pick in the run"
    );
}

/// #393's acceptance criteria: a pick's audit `detail.near_misses` names the
/// candidates that ranked immediately above it, each with a score and a
/// script-authored reason, bounded regardless of how large the candidate set
/// is.
#[test]
fn a_picks_near_misses_name_the_candidates_ranked_above_it() {
    let scorer = large_scorer(400, 100);
    let picked = scorer.pick("movies", 42, 490, Some(0.2));

    for p in &picked {
        let detail = detail_of(p);
        let rank = detail["rank"].as_i64().unwrap();
        let own_score = detail["score"].as_f64().unwrap();
        let near_misses = detail["near_misses"]
            .as_array()
            .unwrap_or_else(|| panic!("{}: near_misses must be an array", p.id));

        let expected = std::cmp::min(3, (rank - 1) as usize);
        assert!(
            near_misses.len() <= 3,
            "{}: near_misses exceeded the script's stated bound of 3, got {}",
            p.id,
            near_misses.len()
        );
        assert_eq!(
            near_misses.len(),
            expected,
            "{}: expected {expected} near misses at rank {rank} (500-candidate set), got {}",
            p.id,
            near_misses.len()
        );

        for nm in near_misses {
            let nm_score = nm["score"]
                .as_f64()
                .unwrap_or_else(|| panic!("{}: near_miss score must be numeric", p.id));
            assert!(
                nm_score >= own_score,
                "{}: a named near miss must have ranked at or above this pick: {nm_score} < {own_score}",
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
            assert!(
                nm["id"].as_str().is_some(),
                "{}: near_miss must carry an id",
                p.id
            );
        }
    }
}

/// #393's acceptance criterion: the near-miss list must not blow the
/// audit-trail byte budget the design was costed against (ADR 0011) — the
/// budget is 400-800 bytes on an item measuring ~2.7KB before this work, so
/// growth here must also stay under a third of that. Measured directly on
/// one item's `metadata`, with and without the `near_misses` key, and both
/// numbers are printed so the number actually reached is on record.
#[test]
fn near_misses_stay_inside_the_audit_byte_budget() {
    let scorer = large_scorer(400, 100);
    let picked = scorer.pick("movies", 42, 490, Some(0.2));

    // The worst case for byte growth is a pick with the full 3-entry list —
    // any rank comfortably above 3 has one, by the previous test.
    let p = picked
        .iter()
        .find(|p| detail_of(p)["near_misses"].as_array().unwrap().len() == 3)
        .expect("expected at least one pick with a full 3-entry near-miss list");

    let with_near_misses = serde_json::to_string(p.metadata.as_ref().unwrap()).unwrap();

    let mut stripped = p.metadata.as_ref().unwrap().clone();
    stripped["audit"][0]["detail"]
        .as_object_mut()
        .unwrap()
        .remove("near_misses");
    let without_near_misses = serde_json::to_string(&stripped).unwrap();

    let grown = with_near_misses.len() - without_near_misses.len();
    println!(
        "taste-cosine near_misses byte growth: {grown} bytes ({} -> {} bytes for one item's metadata)",
        without_near_misses.len(),
        with_near_misses.len()
    );
    assert!(
        grown > 0,
        "near_misses must add some bytes to the record, got {grown}"
    );
    assert!(
        grown <= 800,
        "near_misses grew the record by {grown} bytes, exceeding ADR 0011's 800-byte audit budget"
    );
    assert!(
        grown < 2700 / 3,
        "near_misses grew the record by {grown} bytes, exceeding a third of a ~2.7KB item"
    );
}

/// The near-miss bound is the script's own tunable (#393), not a fixed
/// station constant — overriding it via the pool's `config:` changes how many
/// candidates get named, the same way `exploration_fraction` already does.
#[test]
fn near_miss_limit_is_overridable_via_pool_config() {
    let scorer = large_scorer(400, 100);
    let inputs = ScoreInputs {
        target_count: 490,
        ..Default::default()
    };
    let config = serde_json::json!({ "exploration_fraction": 0.0, "near_miss_limit": 1 });
    let picked = pick(
        &scorer.cache,
        &scorer.script,
        None,
        &inputs,
        42,
        "movies",
        Some(&config),
        grant(&scorer.db),
    )
    .unwrap();

    for p in &picked {
        let near_misses = detail_of(p)["near_misses"].as_array().unwrap();
        assert!(
            near_misses.len() <= 1,
            "{}: near_miss_limit: 1 must cap the list at 1, got {}",
            p.id,
            near_misses.len()
        );
    }
}

/// A store for #294's regression case: one played title ("seed") defines
/// equal weights for a common keyword and a rare one, four candidates carry
/// only the common keyword, one carries only the rare one, and one carries
/// both — enough to hand-compute every idf factor below.
fn write_idf_fixture(path: &Path) {
    empty_store(path)
        .execute_batch(
            "INSERT INTO items (item_id, type) VALUES
                 ('seed', 'movie'),
                 ('gen1', 'movie'), ('gen2', 'movie'), ('gen3', 'movie'), ('gen4', 'movie'),
                 ('rare1', 'movie'), ('multi', 'movie');
             -- seed: common + rare, watched once -> pooled weight 0.5 each
             -- (sqrt(1 play) / 2 keywords), same as write_taste_fixture above.
             -- Six candidates carry an in-namespace fact (gen1-4, rare1,
             -- multi), so doc_count = 6. df(common) = 5 (gen1-4, multi);
             -- df(rare) = 2 (rare1, multi).
             INSERT INTO enrichment (item_id, namespace, key, value, fetched_at) VALUES
                 ('seed',  'tmdb_keywords', 'keyword', 'common', '2026-01-01T00:00:00+00:00'),
                 ('seed',  'tmdb_keywords', 'keyword', 'rare',   '2026-01-01T00:00:00+00:00'),
                 ('gen1',  'tmdb_keywords', 'keyword', 'common', '2026-01-01T00:00:00+00:00'),
                 ('gen2',  'tmdb_keywords', 'keyword', 'common', '2026-01-01T00:00:00+00:00'),
                 ('gen3',  'tmdb_keywords', 'keyword', 'common', '2026-01-01T00:00:00+00:00'),
                 ('gen4',  'tmdb_keywords', 'keyword', 'common', '2026-01-01T00:00:00+00:00'),
                 ('rare1', 'tmdb_keywords', 'keyword', 'rare',   '2026-01-01T00:00:00+00:00'),
                 ('multi', 'tmdb_keywords', 'keyword', 'common', '2026-01-01T00:00:00+00:00'),
                 ('multi', 'tmdb_keywords', 'keyword', 'rare',   '2026-01-01T00:00:00+00:00');
             INSERT INTO plays (history_key, item_id, plex_account_id, viewed_at) VALUES
                 ('h1', 'seed', 42, 1700000000);",
        )
        .unwrap();
}

/// #294's regression: before the idf factor, `sum / sqrt(n)` scored every
/// single-keyword title purely on that keyword's pooled weight, so a
/// candidate carrying the pool's most generic keyword (on almost everything)
/// tied a candidate carrying a keyword only a couple of titles share. Under
/// the fixed scorer, `gen1`'s lone `common` keyword (df 5 of 6) and `rare1`'s
/// lone `rare` keyword (df 2 of 6) carry the same *weight* but different
/// idf, so `rare1` must outrank `gen1` even though the old code tied them —
/// and `multi`, which carries both keywords, must outrank both.
#[test]
fn a_rare_shared_keyword_outranks_a_common_one_and_no_tie_holds_rank_one() {
    let cat = catalog_of(["gen1", "gen2", "gen3", "gen4", "rare1", "multi"]);
    let scorer = Scorer::new(&cat, write_idf_fixture);
    let picked = scorer.pick("movies", 0, 6, Some(0.0));

    let multi = score_of(&picked, "multi");
    let rare1 = score_of(&picked, "rare1");
    let gen1 = score_of(&picked, "gen1");

    assert!(
        multi > rare1,
        "the multi-keyword title must outrank the rare-single-keyword one: \
         multi={multi}, rare1={rare1}"
    );
    assert!(
        rare1 > gen1,
        "a rare shared keyword must outrank a common one of equal weight: \
         rare1={rare1}, gen1={gen1}"
    );

    // Rank order: no candidate before it ties its score, i.e. rank 1 is held
    // by exactly one title, not a multi-way tie the old sqrt(n)-only
    // normalization would have produced here.
    let top_score = picked[0].metadata.as_ref().unwrap()["score"]
        .as_f64()
        .unwrap();
    let tied_at_top = picked
        .iter()
        .filter(|p| p.metadata.as_ref().unwrap()["score"].as_f64().unwrap() == top_score)
        .count();
    assert_eq!(
        tied_at_top, 1,
        "rank 1 must be held by exactly one title, not a tie: {picked:?}"
    );
}

/// The order the picks come back in, which with exploration off is the
/// ranking itself — the one thing the recency tests below are actually
/// asserting about.
fn order_of(picked: &[PickedItem]) -> Vec<&str> {
    picked.iter().map(|p| p.id.as_str()).collect()
}

fn detail_f64(p: &PickedItem, key: &str) -> f64 {
    detail_of(p)[key]
        .as_f64()
        .unwrap_or_else(|| panic!("detail.{key} missing or not a number: {:?}", detail_of(p)))
}

/// The acceptance criterion for recency damping. The cosine alone is a pure
/// function of (catalog, taste vector), and both hold still between
/// generations — so before this, the ranking came out identical every run
/// and the head of it aired on a loop. `ctx.recent` is the only input that
/// moves as the channel plays.
///
/// `write_taste_fixture`'s undamped ranking is mov-a (0.9938) > mov-c
/// (0.7027) > mov-d (0.4969) > mov-b (0.0), pinned by
/// `the_committed_example_plugin_runs_and_scores_correctly` above. Airing
/// mov-a puts it at distance 1 in the tail, so it keeps 1/(1+25) = 3.8% of
/// its cosine and lands third — behind both titles it beat a moment ago,
/// and still ahead of the one that has no keywords at all.
#[test]
fn a_recent_airing_drops_a_title_below_the_ones_it_was_beating() {
    let scorer = Scorer::new(&small_catalog(), write_taste_fixture);

    assert_eq!(
        order_of(&scorer.pick_recent(4, Some(0.0), &[])),
        ["mov-a", "mov-c", "mov-d", "mov-b"],
        "the undamped ranking, with nothing yet aired"
    );
    assert_eq!(
        order_of(&scorer.pick_recent(4, Some(0.0), &["mov-a"])),
        ["mov-c", "mov-d", "mov-a", "mov-b"],
        "mov-a aired last, so it must fall behind mov-c and mov-d"
    );
}

/// Damped, not excluded — the judgement a fixed cooldown window cannot
/// express. Pushed far enough back in the tail (distance 100, so it keeps
/// 100/(100+25) = 80% of its cosine), mov-a's 0.9938 * 0.8 = 0.795 still
/// clears mov-c's undamped 0.7027 and it returns to the top on its own.
#[test]
fn a_far_enough_back_airing_stops_mattering_and_the_title_returns() {
    let scorer = Scorer::new(&small_catalog(), write_taste_fixture);

    // mov-a oldest, then 99 ids this catalog does not contain — a real tail
    // carries this channel's episodes too, and they simply never match a
    // movie id.
    let filler: Vec<String> = (0..99).map(|i| format!("ep-{i:03}")).collect();
    let mut tail = vec!["mov-a"];
    tail.extend(filler.iter().map(String::as_str));

    let picked = scorer.pick_recent(4, Some(0.0), &tail);
    assert_eq!(
        order_of(&picked),
        ["mov-a", "mov-c", "mov-d", "mov-b"],
        "100 airings back, mov-a keeps 80% of its cosine and still leads"
    );
    assert_close(
        detail_f64(&picked[0], "recency_factor"),
        100.0 / 125.0,
        "mov-a's recency factor at distance 100",
    );
}

/// A title the tail does not hold is untouched: factor exactly 1.0 and the
/// damped score identical to the cosine. This is the common case — the tail
/// is 200 entries against an 11,543-movie library — so a regression that
/// damped everything would be invisible in the ordering tests above.
#[test]
fn a_title_outside_the_tail_keeps_its_whole_cosine() {
    let scorer = Scorer::new(&small_catalog(), write_taste_fixture);
    let picked = scorer.pick_recent(4, Some(0.0), &["mov-a"]);

    for p in &picked {
        if p.id == "mov-a" {
            continue;
        }
        assert!(
            detail_of(p)["aired_rank"].is_null(),
            "{} is not in the tail, so aired_rank must be null: {:?}",
            p.id,
            detail_of(p)
        );
        assert_close(
            detail_f64(p, "recency_factor"),
            1.0,
            &format!("{}'s recency factor", p.id),
        );
        assert_close(
            detail_f64(p, "score"),
            detail_f64(p, "base_score"),
            &format!("{}'s damped score against its cosine", p.id),
        );
    }
}

/// The audit has to show both halves. A title that fell twenty places
/// because it aired last night only reads as a decision if the undamped
/// cosine and the factor are both on the page; one number alone reads as
/// the scorer having changed its mind about the title.
#[test]
fn the_audit_shows_the_undamped_cosine_the_factor_and_the_verdict() {
    let scorer = Scorer::new(&small_catalog(), write_taste_fixture);
    let picked = scorer.pick_recent(4, Some(0.0), &["mov-a"]);
    let damped = picked.iter().find(|p| p.id == "mov-a").unwrap();

    assert_eq!(detail_of(damped)["aired_rank"].as_i64(), Some(1));
    assert_close(
        detail_f64(damped, "recency_factor"),
        1.0 / 26.0,
        "mov-a's recency factor at distance 1",
    );
    assert_close(
        detail_f64(damped, "score"),
        detail_f64(damped, "base_score") / 26.0,
        "the reported score against cosine times factor",
    );
    assert_eq!(
        damped.metadata.as_ref().unwrap()["audit"][0]["verdict"].as_str(),
        Some("ranked by keyword cosine, damped for a recent airing on this channel"),
        "a damped pick must say so rather than reading like a plain low rank"
    );
}

/// The exploration slot excludes the airing tail outright rather than
/// damping it: it draws in a fixed seeded order that ignores score, so
/// damping would buy it nothing, and spending the one off-profile slot in
/// five on a title from three days ago is the failure this whole change is
/// about.
#[test]
fn no_exploration_slot_ever_draws_a_title_from_the_airing_tail() {
    let scorer = large_scorer(40, 0);
    let aired: Vec<String> = (0..30).map(|i| format!("el-{i:04}")).collect();
    let tail: Vec<&str> = aired.iter().map(String::as_str).collect();
    let recent: HashSet<&str> = tail.iter().copied().collect();

    let picked = scorer.pick_recent(20, None, &tail);
    let explored: Vec<&str> = picked
        .iter()
        .filter(|p| source_of(p) == Some("explore"))
        .map(|p| p.id.as_str())
        .collect();

    assert!(
        !explored.is_empty(),
        "the default exploration fraction must have fired at least once in 30 slots"
    );
    for id in &explored {
        assert!(
            !recent.contains(id),
            "{id} aired inside the tail and must not be an exploration draw: {explored:?}"
        );
    }
}

/// What `admin audit` actually prints. The two halves this change touches
/// are written by `pick()` and rendered by `audit_report::render`, and the
/// tests above only check the first half — so this feeds real plugin output
/// straight into the real renderer and asserts the damping is legible on the
/// page rather than merely present in the JSON.
#[test]
fn admin_audit_prints_the_damping_for_a_recently_aired_pick() {
    let scorer = Scorer::new(&small_catalog(), write_taste_fixture);
    let picked = scorer.pick_recent(4, Some(0.0), &["mov-a"]);

    let start = time::OffsetDateTime::UNIX_EPOCH;
    let items: Vec<etv_station::audit_report::ReportItem> = picked
        .iter()
        .map(|p| etv_station::audit_report::ReportItem {
            id: p.id.clone(),
            start,
            finish: start + time::Duration::hours(2),
            title: Some(p.id.clone()),
            metadata: p.metadata.clone(),
        })
        .collect();
    let report = etv_station::audit_report::render("for-pierce", start, &items);
    println!("{report}");

    assert!(
        report.contains("ranked by keyword cosine, damped for a recent airing on this channel"),
        "the verdict must name the damping:\n{report}"
    );
    for line in ["base_score", "recency_factor", "aired_rank"] {
        assert!(
            report.contains(line),
            "the report must show {line}:\n{report}"
        );
    }
}

// ---------------------------------------------------------------------------
// Shows: the scorer ranks SERIES, not episodes.
//
// #274 made the catalog's `show_id` GUID-derived, which is what lets a script
// resolve an episode to its show and score the show's own keywords — the gap
// that made #254 ship movies-only. An episode carries no keywords, and
// ordering episodes by anything but season/episode would air a season
// shuffled, so a shows pool ranks the show and hands back its episodes in
// broadcast order.
// ---------------------------------------------------------------------------

/// Three shows whose ids are also the `item_id`s the plexdb fixture carries
/// keywords for, plus episodes pointing at them via `show_id` — the join
/// #274 unlocked, in miniature.
///
/// `sh-a` is watched, so it defines the pooled weights (contact + time, 0.5
/// each). `sh-b` carries `contact` only; `sh-c` carries `time` plus the
/// off-profile `desert`; `sh-d` carries nothing and is ineligible. Those are
/// exactly `write_taste_fixture`'s four movies with show ids, so the same
/// hand-computed cosines apply: sh-a 0.9938 > sh-b 0.7027 > sh-c 0.4969 >
/// sh-d 0.0.
fn write_shows_fixture(path: &Path) {
    empty_store(path)
        .execute_batch(
            "INSERT INTO items (item_id, type) VALUES
                 ('sh-a', 'show'), ('sh-b', 'show'), ('sh-c', 'show'), ('sh-d', 'show');
             INSERT INTO enrichment (item_id, namespace, key, value, fetched_at) VALUES
                 ('sh-a', 'tmdb_keywords', 'keyword', 'contact', 't'),
                 ('sh-a', 'tmdb_keywords', 'keyword', 'time', 't'),
                 ('sh-b', 'tmdb_keywords', 'keyword', 'contact', 't'),
                 ('sh-c', 'tmdb_keywords', 'keyword', 'time', 't'),
                 ('sh-c', 'tmdb_keywords', 'keyword', 'desert', 't');
             INSERT INTO plays (history_key, item_id, plex_account_id, viewed_at) VALUES
                 ('h1', 'sh-a', 42, 1700000000);",
        )
        .unwrap();
}

/// One episode, wired to its show the way the Plex ingester wires one.
fn add_episode(cat: &Catalog, show_id: &str, season: i64, episode: i64) {
    let id = format!("{show_id}-s{season}e{episode:02}");
    let mut e = Entry::new(&id, "episode", &id, Source::Plex);
    e.show = Some(show_id.to_string());
    e.show_id = Some(show_id.to_string());
    e.season = Some(season);
    e.episode = Some(episode);
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

/// Four shows, two seasons of two episodes each. Deliberately inserted in a
/// scrambled order so a test asserting broadcast order is asserting the
/// script's sort, not the catalog's insertion sequence.
fn shows_catalog() -> Catalog {
    let cat = Catalog::open_in_memory().unwrap();
    for show in ["sh-a", "sh-b", "sh-c", "sh-d"] {
        for (season, episode) in [(2, 2), (1, 1), (2, 1), (1, 2)] {
            add_episode(&cat, show, season, episode);
        }
    }
    cat
}

impl Scorer {
    /// One generation on a `unit = "show"` pool, with exploration off so the
    /// order under test is the ranking itself.
    fn pick_shows(&self, target_count: usize, recent: &[&str]) -> Vec<PickedItem> {
        let config = serde_json::json!({ "unit": "show", "exploration_fraction": 0.0 });
        let inputs = ScoreInputs {
            target_count,
            recent: recent.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        };
        pick(
            &self.cache,
            &self.script,
            None,
            &inputs,
            0,
            "shows",
            Some(&config),
            grant(&self.db),
        )
        .unwrap()
    }
}

/// The acceptance criterion. A shows pool ranks the series by the show's own
/// keyword cosine — the thing that was impossible before #274 — and hands
/// back each show's episodes in season/episode order, all of them, best show
/// first.
///
/// Both halves matter. The station will not re-sort what a plugin returns and
/// groups the list into series by first appearance (`pattern.rs:14`), so the
/// order shows come back in becomes the rotation order, and the order inside
/// a show becomes the order its episodes play.
#[test]
fn a_shows_pool_ranks_series_and_returns_episodes_in_broadcast_order() {
    let scorer = Scorer::new(&shows_catalog(), write_shows_fixture);
    let picked = scorer.pick_shows(8, &[]);

    // Shows in ranked order — the same cosines as the movie fixture, since
    // the keywords are the same.
    let show_order: Vec<&str> = picked
        .iter()
        .map(|p| &p.id[..4])
        .collect::<Vec<_>>()
        .windows(2)
        .filter(|w| w[0] != w[1])
        .map(|w| w[0])
        .chain(picked.last().map(|p| &p.id[..4]))
        .collect();
    assert_eq!(
        show_order,
        ["sh-a", "sh-b", "sh-c", "sh-d"],
        "series must come back in cosine order: {:?}",
        order_of(&picked)
    );

    // Every episode of every show, each show's block in broadcast order.
    assert_eq!(
        order_of(&picked),
        [
            "sh-a-s1e01",
            "sh-a-s1e02",
            "sh-a-s2e01",
            "sh-a-s2e02", //
            "sh-b-s1e01",
            "sh-b-s1e02",
            "sh-b-s2e01",
            "sh-b-s2e02", //
            "sh-c-s1e01",
            "sh-c-s1e02",
            "sh-c-s2e01",
            "sh-c-s2e02", //
            "sh-d-s1e01",
            "sh-d-s1e02",
            "sh-d-s2e01",
            "sh-d-s2e02",
        ],
        "episodes must be season-then-episode inside each show"
    );
}

/// A show hands back ALL of its episodes, not a slice. `advance: resume`
/// finds the last-aired episode in the returned list and continues past it,
/// so a list holding only the first few would never contain the episode the
/// ledger resumes from and the show would restart from episode one forever.
#[test]
fn every_episode_of_a_picked_show_comes_back_not_just_the_next_few() {
    let scorer = Scorer::new(&shows_catalog(), write_shows_fixture);
    // A target_count far smaller than one show's episode count: the unit
    // budget counts SHOWS, so this must not truncate a show mid-run.
    let picked = scorer.pick_shows(1, &[]);
    for show in ["sh-a", "sh-b", "sh-c", "sh-d"] {
        let n = picked.iter().filter(|p| p.id.starts_with(show)).count();
        assert_eq!(n, 4, "{show} must hand back all 4 of its episodes");
    }
}

/// Recency damps the SERIES, keyed on `show_id`, not the episode. A specific
/// episode almost never repeats, so damping the episode id would do nothing;
/// damping the series is what "Columbo has been on a lot lately" means.
///
/// `sh-a` leads on cosine (0.9938 against sh-b's 0.7027). Airing one of its
/// episodes puts the whole show at distance 1, so it keeps 1/(1+25) of its
/// score and drops to third — behind two shows whose episodes never aired.
#[test]
fn airing_one_episode_damps_the_whole_series() {
    let scorer = Scorer::new(&shows_catalog(), write_shows_fixture);

    let before = scorer.pick_shows(8, &[]);
    assert!(before[0].id.starts_with("sh-a"), "sh-a leads undamped");

    let after = scorer.pick_shows(8, &["sh-a-s1e01"]);
    let first_show = &after[0].id[..4];
    assert_eq!(first_show, "sh-b", "one sh-a episode must sink the series");

    let damped = after.iter().find(|p| p.id.starts_with("sh-a")).unwrap();
    assert_eq!(
        detail_of(damped)["aired_rank"].as_i64(),
        Some(1),
        "the series carries its most recently aired episode's distance"
    );
    assert_close(
        detail_f64(damped, "recency_factor"),
        1.0 / 26.0,
        "sh-a's recency factor at distance 1",
    );
    // The show that was ranked, named on every one of its episodes — without
    // it the audit reads as though the episode itself scored.
    assert_eq!(
        detail_of(damped)["unit"].as_str(),
        Some("sh-a"),
        "a shows pick must name the series it was ranked as"
    );
}

impl Scorer {
    /// One generation on a `unit = "season"` pool — 001-for-you's shape,
    /// where a visit is a whole season rather than three episodes.
    fn pick_seasons(&self, target_count: usize) -> Vec<PickedItem> {
        let config = serde_json::json!({ "unit": "season", "exploration_fraction": 0.0 });
        let inputs = ScoreInputs {
            target_count,
            ..Default::default()
        };
        pick(
            &self.cache,
            &self.script,
            None,
            &inputs,
            0,
            "shows",
            Some(&config),
            grant(&self.db),
        )
        .unwrap()
    }
}

/// `unit = "season"` lays the same ranking out season-major: every picked
/// show's season 1 in ranked order, then every show's season 2.
///
/// This is what 001-for-you needs and `unit = "show"` cannot give it. That
/// channel's pattern draws `take: all`, so a visit is a whole season. Under
/// show-major ordering the station's rotation — which follows first
/// appearance — would work through every season of the top-ranked show
/// before ever reaching the second show, turning a recommendation channel
/// into a marathon channel. Season-major makes each visit land on a
/// different series.
#[test]
fn season_units_interleave_shows_instead_of_marathoning_one() {
    let scorer = Scorer::new(&shows_catalog(), write_shows_fixture);

    assert_eq!(
        order_of(&scorer.pick_seasons(8)),
        [
            // Every show's season 1, in cosine order …
            "sh-a-s1e01",
            "sh-a-s1e02", //
            "sh-b-s1e01",
            "sh-b-s1e02", //
            "sh-c-s1e01",
            "sh-c-s1e02", //
            "sh-d-s1e01",
            "sh-d-s1e02", //
            // … then every show's season 2, same order.
            "sh-a-s2e01",
            "sh-a-s2e02", //
            "sh-b-s2e01",
            "sh-b-s2e02", //
            "sh-c-s2e01",
            "sh-c-s2e02", //
            "sh-d-s2e01",
            "sh-d-s2e02",
        ],
        "seasons must interleave across shows, not run one show to its end"
    );
}

/// The two layouts hold the same episodes and the same ranking — they differ
/// only in the order the blocks are laid down. A regression that dropped or
/// duplicated an episode in one layout would otherwise hide behind the
/// order assertions above.
#[test]
fn show_major_and_season_major_carry_the_same_episodes() {
    let scorer = Scorer::new(&shows_catalog(), write_shows_fixture);

    let shows = scorer.pick_shows(8, &[]);
    let seasons = scorer.pick_seasons(8);
    let mut by_show = order_of(&shows);
    let mut by_season = order_of(&seasons);
    assert_ne!(by_show, by_season, "the two layouts must differ in order");
    by_show.sort_unstable();
    by_season.sort_unstable();
    assert_eq!(
        by_show, by_season,
        "and must hold exactly the same episodes"
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

// ---------------------------------------------------------------------------
// #396 — a channel-authored INFLUENCE set tilts the ranking without gating it.
//
// The fixture is `write_taste_fixture`'s arithmetic with two extra films that
// are not candidates. Pooled taste comes from the one played movie: `mov-a`
// carries `contact` and `time`, so both weigh 0.5. The candidate set is the
// three `mov-*` titles, so doc_count = 3 and the document frequencies are
// contact = 2 (a, c), time = 2 (a, d), desert = 1 (d) — idf factors
// 1 + ln(3/2) for the first two and 1 + ln(3) for `desert`.
//
// The influence set is `gp-1` and `gp-2`, both carrying `desert` and nothing
// else, so the influence profile is exactly `desert -> 1.0` (both members of
// two carry it). `desert` is the keyword the account's own history has no
// opinion about at all, which is what makes the tilt visible rather than
// merely additive to an existing preference:
//
//   mov-a  taste (0.5·idfC + 0.5·idfT)/√2 = 0.993813   influence 0
//   mov-c  taste  0.5·idfC                = 0.702733   influence 0
//   mov-d  taste  0.5·idfT/√2             = 0.496905   influence idfD/√2 = 1.483946
//
// So `mov-d` is last untilted and first at influence_weight 0.5 — and `mov-c`,
// which shares nothing with the influence set, still airs. That is the whole
// claim: a guide, not a gate.
// ---------------------------------------------------------------------------

fn write_influence_fixture(path: &Path) {
    empty_store(path)
        .execute_batch(
            "INSERT INTO items (item_id, type) VALUES
                 ('mov-a', 'movie'), ('mov-c', 'movie'), ('mov-d', 'movie'),
                 ('gp-1', 'movie'), ('gp-2', 'movie');
             INSERT INTO enrichment (item_id, namespace, key, value, fetched_at) VALUES
                 ('mov-a', 'tmdb_keywords', 'keyword', 'contact', 't'),
                 ('mov-a', 'tmdb_keywords', 'keyword', 'time', 't'),
                 ('mov-c', 'tmdb_keywords', 'keyword', 'contact', 't'),
                 ('mov-d', 'tmdb_keywords', 'keyword', 'time', 't'),
                 ('mov-d', 'tmdb_keywords', 'keyword', 'desert', 't'),
                 ('gp-1', 'tmdb_keywords', 'keyword', 'desert', 't'),
                 ('gp-2', 'tmdb_keywords', 'keyword', 'desert', 't');
             INSERT INTO plays (history_key, item_id, plex_account_id, viewed_at) VALUES
                 ('h1', 'mov-a', 42, 1700000000);",
        )
        .unwrap();
}

/// The candidates and the influence set as a pool would author them: two CEL
/// expressions over one catalog, disjoint here only so the arithmetic above
/// stays hand-checkable — nothing requires them to be.
fn influence_sources() -> PoolSources {
    [
        (
            "movies".to_string(),
            r#"item.title.startsWith("mov-")"#.to_string(),
        ),
        (
            "influence".to_string(),
            r#"item.title.startsWith("gp-")"#.to_string(),
        ),
    ]
    .into_iter()
    .collect()
}

fn influence_scorer() -> Scorer {
    let cat = catalog_of(["mov-a", "mov-c", "mov-d", "gp-1", "gp-2"]);
    Scorer::new_sourced(&cat, write_influence_fixture, Some(influence_sources()))
}

/// The acceptance criterion. The same catalog and the same taste vector
/// produce a different order once the influence set is weighted, and the
/// title that moves is the one sharing the influence set's keyword.
#[test]
fn an_influence_set_reorders_the_ranking_it_shares_keywords_with() {
    let scorer = influence_scorer();

    let plain = scorer.pick_tilted(3, 0.0);
    assert_eq!(
        order_of(&plain),
        vec!["mov-a", "mov-c", "mov-d"],
        "at weight 0 the ranking must be the plain taste cosine"
    );

    let tilted = scorer.pick_tilted(3, 0.5);
    assert_eq!(
        order_of(&tilted),
        vec!["mov-d", "mov-a", "mov-c"],
        "at weight 0.5 the title carrying the influence set's keyword must lead"
    );
}

/// The tilt is a guide, not a gate: every candidate is still returned, and one
/// sharing nothing at all with the influence set still airs.
#[test]
fn a_tilted_pool_still_returns_candidates_the_influence_set_says_nothing_about() {
    let picked = influence_scorer().pick_tilted(3, 4.0);
    let mut ids = order_of(&picked);
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec!["mov-a", "mov-c", "mov-d"],
        "even at weight 4.0 the influence set must not narrow the candidates"
    );
}

/// The two halves of the score are hand-checkable and both reported, so a
/// pick that only aired because of the tilt can be told apart from one the
/// account's own history chose.
#[test]
fn the_audit_reports_both_cosines_and_the_weight_between_them() {
    let picked = influence_scorer().pick_tilted(3, 0.5);
    let d = picked.iter().find(|p| p.id == "mov-d").unwrap();

    let idf_time = 1.0 + (3.0f64 / 2.0).ln();
    let idf_desert = 1.0 + 3.0f64.ln();
    let taste = 0.5 * idf_time / 2.0f64.sqrt();
    let influence = idf_desert / 2.0f64.sqrt();

    assert_close(detail_f64(d, "taste_score"), taste, "mov-d's taste cosine");
    assert_close(
        detail_f64(d, "influence_score"),
        influence,
        "mov-d's influence cosine",
    );
    assert_close(detail_f64(d, "influence_weight"), 0.5, "the weight");
    assert_close(
        detail_f64(d, "base_score"),
        taste + 0.5 * influence,
        "base_score against taste + weight * influence",
    );
    assert_eq!(
        detail_of(d)["on_influence"][0].as_str(),
        Some("desert"),
        "the audit must name which keyword the influence set contributed"
    );
    assert_eq!(
        d.metadata.as_ref().unwrap()["audit"][0]["verdict"].as_str(),
        Some("ranked by keyword cosine against pooled taste, tilted toward the influence set"),
        "a tilted pick must say so"
    );
}

/// A candidate sharing nothing with the influence set gets no tilt clause and
/// no influence score, on the very same tilted pool — the verdict has to stay
/// true per pick, not per pool.
#[test]
fn a_candidate_the_influence_set_never_touched_reads_as_an_ordinary_pick() {
    let picked = influence_scorer().pick_tilted(3, 0.5);
    let c = picked.iter().find(|p| p.id == "mov-c").unwrap();

    assert_close(detail_f64(c, "influence_score"), 0.0, "mov-c's tilt");
    assert_eq!(
        c.metadata.as_ref().unwrap()["audit"][0]["verdict"].as_str(),
        Some("ranked by keyword cosine against pooled taste"),
        "an untouched candidate must not claim a tilt it never got"
    );
}

/// The default is zero, so every pool that predates #396 — and every pool that
/// authors no influence set — scores exactly what it scored before. Checked
/// against the untouched `write_taste_fixture` rather than by asserting the
/// default's value, so it fails if the influence branch ever runs unasked.
#[test]
fn a_pool_with_no_influence_set_is_untouched_by_the_feature() {
    let scorer = Scorer::new(&small_catalog(), write_taste_fixture);
    let picked = scorer.pick("movies", 0, 4, Some(0.0));
    let c = picked.iter().find(|p| p.id == "mov-c").unwrap();

    assert_close(detail_f64(c, "influence_score"), 0.0, "an untilted pool");
    assert_close(detail_f64(c, "influence_weight"), 0.0, "an untilted pool");
    assert_close(
        detail_f64(c, "base_score"),
        detail_f64(c, "taste_score"),
        "base_score must be the taste cosine alone when nothing tilts it",
    );
}

// ---------------------------------------------------------------------------
// #397 — `seen` partitions the candidates and `unusual_weight` pushes away
// from the house.
//
// The fixture is the influence one plus a second account, so "this account's
// taste" and "the house's taste" are genuinely different vectors rather than
// the same numbers twice. Account 42 played mov-a only; account 99 played
// mov-c twice, which puts `contact` heavily into the pooled vector and not at
// all into 42's own past `mov-a`'s share of it.
//
// The partition is the claim that matters: "only" and "exclude" cannot both
// return the same title, which is what makes a comfort pool and a discovery
// pool safe to run in one block.
// ---------------------------------------------------------------------------

fn write_seen_fixture(path: &Path) {
    empty_store(path)
        .execute_batch(
            "INSERT INTO items (item_id, type) VALUES
                 ('mov-a', 'movie'), ('mov-c', 'movie'), ('mov-d', 'movie'),
                 ('gp-1', 'movie'), ('gp-2', 'movie');
             INSERT INTO enrichment (item_id, namespace, key, value, fetched_at) VALUES
                 ('mov-a', 'tmdb_keywords', 'keyword', 'contact', 't'),
                 ('mov-a', 'tmdb_keywords', 'keyword', 'time', 't'),
                 ('mov-c', 'tmdb_keywords', 'keyword', 'contact', 't'),
                 ('mov-d', 'tmdb_keywords', 'keyword', 'time', 't'),
                 ('mov-d', 'tmdb_keywords', 'keyword', 'desert', 't'),
                 ('gp-1', 'tmdb_keywords', 'keyword', 'desert', 't'),
                 ('gp-2', 'tmdb_keywords', 'keyword', 'desert', 't');
             -- Account 42 has played mov-a and nothing else. Account 99's two
             -- plays of mov-c exist only to make the pooled vector differ
             -- from 42's; they must never make mov-c look watched to 42.
             INSERT INTO plays (history_key, item_id, plex_account_id, viewed_at) VALUES
                 ('h1', 'mov-a', 42, 1700000000),
                 ('h2', 'mov-c', 99, 1700000100),
                 ('h3', 'mov-c', 99, 1700000200);",
        )
        .unwrap();
}

impl Scorer {
    /// One generation on a pool that partitions on watch state and/or leans
    /// away from the house — the two halves of #397. `account_id` is required
    /// for either to mean anything, exactly as on a live `single_user`
    /// channel.
    fn pick_split(
        &self,
        seen: &str,
        unusual_weight: f64,
        account_id: i64,
        target_count: usize,
    ) -> Vec<PickedItem> {
        let config = serde_json::json!({
            "exploration_fraction": 0.0,
            "seen": seen,
            "unusual_weight": unusual_weight,
        });
        self.pick_for(
            "movies",
            0,
            target_count,
            Some(config),
            Some(account_id),
            &[],
        )
    }
}

fn seen_scorer() -> Scorer {
    let cat = catalog_of(["mov-a", "mov-c", "mov-d", "gp-1", "gp-2"]);
    Scorer::new_sourced(&cat, write_seen_fixture, Some(influence_sources()))
}

/// The acceptance criterion, and the property two movie pools in one block
/// depend on: the two halves are disjoint and together they are the whole.
#[test]
fn seen_only_and_seen_exclude_partition_the_candidates() {
    let scorer = seen_scorer();

    let comfort = scorer.pick_split("only", 0.0, 42, 10);
    let discovery = scorer.pick_split("exclude", 0.0, 42, 10);

    let seen: Vec<&str> = order_of(&comfort);
    let unseen: Vec<&str> = order_of(&discovery);

    assert_eq!(
        seen,
        vec!["mov-a"],
        "account 42 has played exactly one of the candidates"
    );
    assert!(
        !unseen.contains(&"mov-a"),
        "the discovery half must not repeat the comfort half: {unseen:?}"
    );
    assert!(
        unseen.contains(&"mov-c"),
        "account 99's plays of mov-c must not make it look watched to 42"
    );

    let mut whole: Vec<&str> = seen.iter().chain(unseen.iter()).copied().collect();
    whole.sort_unstable();
    assert_eq!(
        whole,
        vec!["mov-a", "mov-c", "mov-d"],
        "the two halves together must be every candidate, each exactly once"
    );
}

/// `seen: any` — the default — is every candidate, so a pool that never sets
/// the key behaves as it always did.
#[test]
fn seen_defaults_to_the_whole_library() {
    let scorer = seen_scorer();
    let picked = scorer.pick_split("any", 0.0, 42, 10);
    let mut all = order_of(&picked);
    all.sort_unstable();
    assert_eq!(all, vec!["mov-a", "mov-c", "mov-d"]);
}

/// The unusual term subtracts the house's own cosine, so a title the house
/// has a habit of watching falls behind one it does not. `mov-c` carries
/// `contact`, which account 99's two plays put heavily into the pooled vector
/// — so raising `unusual_weight` must push `mov-c` down.
#[test]
fn the_unusual_term_pushes_away_from_what_the_house_watches() {
    let scorer = seen_scorer();

    let flat = scorer.pick_split("exclude", 0.0, 42, 10);
    let pushed = scorer.pick_split("exclude", 1.0, 42, 10);

    let before = order_of(&flat)
        .iter()
        .position(|id| *id == "mov-c")
        .unwrap();
    let after = order_of(&pushed)
        .iter()
        .position(|id| *id == "mov-c")
        .unwrap();
    assert!(
        after > before,
        "mov-c carries the house's heaviest keyword and must fall, was {before} now {after}"
    );

    let c = pushed.iter().find(|p| p.id == "mov-c").unwrap();
    assert!(
        detail_f64(c, "house_score") > 0.0,
        "the audit must report the house cosine it was penalised by"
    );
    assert_close(detail_f64(c, "unusual_weight"), 1.0, "the weight");
    assert_close(
        detail_f64(c, "base_score"),
        detail_f64(c, "taste_score") / (1.0 + detail_f64(c, "house_score")),
        "base_score against taste / (1 + weight * house)",
    );
}

/// The house term divides rather than subtracts, so no weight — however
/// extreme — can drive a score to or below zero. A negative score would sort
/// under a title carrying no keywords at all, and would invert the recency
/// damping, which is a multiply by a number in (0, 1].
#[test]
fn the_house_term_never_drives_a_score_to_zero_or_below() {
    let picked = seen_scorer().pick_split("any", 5000.0, 42, 10);
    let scored: Vec<&PickedItem> = picked
        .iter()
        .filter(|p| detail_f64(p, "taste_score") > 0.0)
        .collect();
    assert!(!scored.is_empty(), "the fixture must produce scored titles");
    for p in scored {
        assert!(
            detail_f64(p, "base_score") > 0.0,
            "{} scored {} at unusual_weight 5000",
            p.id,
            detail_f64(p, "base_score")
        );
    }
}

/// The verdict names the half and the push, so an audit line says which of
/// two sibling pools a pick came out of without the reader holding the config.
#[test]
fn the_verdict_names_the_half_of_the_library_a_pick_came_from() {
    let comfort = seen_scorer().pick_split("only", 0.0, 42, 10);
    let a = comfort.iter().find(|p| p.id == "mov-a").unwrap();
    assert_eq!(
        a.metadata.as_ref().unwrap()["audit"][0]["verdict"]
            .as_str()
            .unwrap(),
        "ranked by keyword cosine against pooled taste, \
         from titles this account has already played",
    );

    let discovery = seen_scorer().pick_split("exclude", 1.0, 42, 10);
    let c = discovery.iter().find(|p| p.id == "mov-c").unwrap();
    let v = c.metadata.as_ref().unwrap()["audit"][0]["verdict"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        v.contains("never played") && v.contains("what the house watches"),
        "a discovery pick must name both, got: {v}"
    );
}

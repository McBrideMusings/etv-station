//! Acceptance test for the plex-db-ex reader capability grant (#181): a
//! plugin whose channel config granted a named datastore reaches the
//! `plexdb-reader` crate's six accessors through `ctx.datastore("name")`;
//! one that did not cannot reach it at all, and says so.
//!
//! The version gate itself — a store at the wrong `schema_version` failing
//! load naming both versions — is covered where the openability check lives,
//! `crate::config::validate::tests`. This file is the runtime half: given a
//! grant that already validated, does the script actually get real rows back.

use etv_station::catalog::Catalog;
use etv_station::config::DatastoreGrant;
use etv_station::score::{GrantedCapabilities, ScoreCache, ScoreInputs, pick};

/// A minimal but real plexdb store at the schema version `plexdb-reader`
/// understands: two items each with an enrichment fact, one edge, and one
/// play from a single account — just enough to exercise every accessor's
/// happy path with a value a script can assert on, including grouping
/// `enrichment_for_many` across more than one id and, since the store holds
/// exactly one account's plays, `pooled_taste_vector` against
/// `taste_vector_for` for that same account.
fn write_fixture(path: &std::path::Path) {
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
         );
         INSERT INTO items (item_id, type) VALUES ('imdb:tt1', 'movie'), ('imdb:tt3', 'movie');
         INSERT INTO enrichment (item_id, namespace, key, value, fetched_at) VALUES
             ('imdb:tt1', 'tmdb_keywords', 'keyword', 'heist', '2026-01-01T00:00:00+00:00'),
             ('imdb:tt3', 'tmdb_keywords', 'keyword', 'caper', '2026-01-01T00:00:00+00:00');
         INSERT INTO edges (from_id, to_id, edge_type, rank, fetched_at) VALUES
             ('imdb:tt1', 'imdb:tt2', 'tmdb_similar', 1, '2026-01-01T00:00:00+00:00');
         INSERT INTO plays (history_key, item_id, plex_account_id, viewed_at) VALUES
             ('h1', 'imdb:tt1', 42, 1700000000);",
        version = plexdb_reader::SUPPORTED_SCHEMA_VERSION,
    ))
    .unwrap();
}

/// Calls all six accessors and throws (surfacing as a `pick()` error the
/// test would see) unless every one returned the real row the fixture seeded
/// — proof the data actually crosses the Rhai boundary, not just that the
/// call did not error.
const GRANTED_SCRIPT: &str = r#"
fn hooks() { ["pool_provider"] }
fn capabilities() { [#{ datastore: "taste_db" }] }
fn sources() { #{} }
fn pick(ctx) {
    let ds = ctx.datastore("taste_db");

    let facts = ds.enrichment_for("imdb:tt1", "tmdb_keywords");
    if facts.len != 1 { throw "enrichment_for: expected 1 fact, got " + facts.len; }
    if facts[0].namespace != "tmdb_keywords" { throw "enrichment_for: wrong namespace"; }
    if facts[0].value != "heist" { throw "enrichment_for: wrong value: " + facts[0].value; }

    // One call over the whole candidate set, not one call per id (plex-db-ex#40)
    // — the id with no facts under this namespace is simply absent from the map.
    let grouped = ds.enrichment_for_many(["imdb:tt1", "imdb:tt3", "imdb:tt-missing"], "tmdb_keywords");
    if grouped.len() != 2 { throw "enrichment_for_many: expected 2 ids, got " + grouped.len(); }
    if grouped["imdb:tt1"].len != 1 || grouped["imdb:tt1"][0].value != "heist" {
        throw "enrichment_for_many: wrong facts for imdb:tt1";
    }
    if grouped["imdb:tt3"].len != 1 || grouped["imdb:tt3"][0].value != "caper" {
        throw "enrichment_for_many: wrong facts for imdb:tt3";
    }

    let out = ds.edges_from("imdb:tt1", "tmdb_similar");
    if out.len != 1 || out[0].to_id != "imdb:tt2" { throw "edges_from: wrong result"; }

    let back = ds.edges_to("imdb:tt2", "tmdb_similar");
    if back.len != 1 || back[0].from_id != "imdb:tt1" { throw "edges_to: wrong result"; }

    let vector = ds.taste_vector_for(42);
    if vector.attributes.len != 1 { throw "taste_vector_for: expected 1 attribute"; }
    if vector.attributes[0].value != "heist" { throw "taste_vector_for: wrong attribute"; }

    // The fixture's only plays are account 42's, so the server-wide pool must
    // equal that one account's vector exactly.
    let pooled = ds.pooled_taste_vector();
    if pooled.attributes.len != vector.attributes.len {
        throw "pooled_taste_vector: attribute count differs from taste_vector_for(42)";
    }
    if pooled.attributes[0].value != vector.attributes[0].value
        || pooled.attributes[0].weight != vector.attributes[0].weight {
        throw "pooled_taste_vector: does not equal taste_vector_for(42) for the sole account";
    }

    #{ picks: ["ok"], workspace: () }
}
fn audit(ctx, picks, workspace) { #{} }
"#;

/// The acceptance criterion: a plugin in a channel with the grant can call
/// all six accessors and receives the real rows the fixture store holds.
#[test]
fn a_plugin_with_the_grant_reaches_every_accessor_and_gets_real_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("taste.db");
    write_fixture(&db);
    let script = dir.path().join("scorer.rhai");
    std::fs::write(&script, GRANTED_SCRIPT).unwrap();

    let cat = Catalog::open_in_memory().unwrap();
    let mut cache = ScoreCache::default();
    cache.prepare(&cat, &script, None).unwrap();

    // Mirrors what `pattern::resolve_pool_sources` does at generation time:
    // widen the boolean grants with the opened datastore handles.
    let granted = GrantedCapabilities::from_names(&[])
        .with_datastores(&[DatastoreGrant {
            name: "taste_db".into(),
            path: db.to_str().unwrap().into(),
        }])
        .expect("a store that just validated must still open");

    let picked = pick(
        &cache,
        &script,
        None,
        &ScoreInputs::default(),
        0,
        "test",
        None,
        granted,
    )
    .expect("every accessor must return the fixture's real rows");
    assert_eq!(
        picked.into_iter().map(|p| p.id).collect::<Vec<_>>(),
        vec!["ok"]
    );
}

/// A plugin whose pool granted nothing reaches for the same name and fails
/// the `pick()` call, naming the datastore — the acceptance criterion's other
/// half: no grant, no reach, and a clear reason why.
#[test]
fn a_plugin_without_the_grant_cannot_reach_the_datastore_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("scorer.rhai");
    std::fs::write(
        &script,
        r#"
fn hooks() { ["pool_provider"] }
fn sources() { #{} }
fn pick(ctx) { ctx.datastore("taste_db"); ["must not reach here"] }
"#,
    )
    .unwrap();

    let cat = Catalog::open_in_memory().unwrap();
    let mut cache = ScoreCache::default();
    cache.prepare(&cat, &script, None).unwrap();

    let err = pick(
        &cache,
        &script,
        None,
        &ScoreInputs::default(),
        0,
        "test",
        None,
        GrantedCapabilities::default(),
    )
    .expect_err("no grant at all must fail the reach for ctx.datastore");
    assert!(err.contains("taste_db"), "got {err}");
}

/// A plugin granted one datastore reaching for a *different* name it was
/// never granted fails the same way — the grant is per name, not a single
/// on/off switch.
#[test]
fn a_plugin_granted_one_datastore_cannot_reach_a_different_name() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("taste.db");
    write_fixture(&db);
    let script = dir.path().join("scorer.rhai");
    std::fs::write(
        &script,
        r#"
fn hooks() { ["pool_provider"] }
fn sources() { #{} }
fn pick(ctx) { ctx.datastore("other_db"); ["must not reach here"] }
"#,
    )
    .unwrap();

    let cat = Catalog::open_in_memory().unwrap();
    let mut cache = ScoreCache::default();
    cache.prepare(&cat, &script, None).unwrap();

    let granted = GrantedCapabilities::from_names(&[])
        .with_datastores(&[DatastoreGrant {
            name: "taste_db".into(),
            path: db.to_str().unwrap().into(),
        }])
        .unwrap();

    let err = pick(
        &cache,
        &script,
        None,
        &ScoreInputs::default(),
        0,
        "test",
        None,
        granted,
    )
    .expect_err("a name this pool never granted must fail the reach");
    assert!(err.contains("other_db"), "got {err}");
}

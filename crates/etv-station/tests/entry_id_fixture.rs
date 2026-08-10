//! The shared identity fixture, run against the Rust derivation.
//!
//! `entry_id` is derived by two implementations: this one, and a Python copy in
//! [plex-db-ex](https://github.com/McBrideMusings/plex-db-ex) at
//! `plexdb/identity.py`, where it is called `item_id`. Nothing enforces at
//! runtime that they agree, and if they drift every join between that store and
//! this catalog silently returns nothing — no error on either side.
//!
//! So it is enforced by test. Both repositories keep a byte-identical copy of
//! `tests/fixtures/entry_id.json`, both run every case in it, and both pin its
//! SHA-256.
//!
//! **What that actually guarantees, stated precisely:** each repository hashes
//! its *own* copy against its *own* recorded constant, and nothing compares the
//! two constants to each other. So this catches "the fixture changed here and
//! somebody did not notice" — the suite goes red until the constant is updated
//! deliberately. It does **not** make divergence impossible: edit the fixture
//! *and* its constant here, forget the other repository, and both suites stay
//! green while the copies disagree. The remaining guard at that point is the
//! failure message, which names the other repository, and the discipline to
//! follow it.
//!
//! **The duplication is deliberate.** Do not de-duplicate it. A single copy read
//! across repositories by relative path holds only while both checkouts sit side
//! by side, and a clone of this repository alone would *skip* the check rather
//! than fail it — which is precisely the silence being defended against. The
//! reasoning is recorded in plex-db-ex ADR-0006.

use std::path::PathBuf;

use etv_station::catalog::{ExternalNs, canonical_path, derive_entry_id};
use sha2::{Digest, Sha256};

/// SHA-256 of `tests/fixtures/entry_id.json`. `plex-db-ex` pins the same value
/// in `tests/test_identity.py`. Changing the fixture means changing both repos
/// in the same breath and updating this constant in both.
const FIXTURE_SHA256: &str = "73fd792cb8e84a88fd577f4d7c0966db1c0240b88db7c939f54c9374fdbb4daf";

/// Resolved from `CARGO_MANIFEST_DIR`, so the test reads nothing outside this
/// repository and passes in a checkout with no sibling `plex-db-ex`.
fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("entry_id.json")
}

fn fixture_bytes() -> Vec<u8> {
    std::fs::read(fixture_path()).expect("shared identity fixture is missing")
}

fn fixture_json() -> serde_json::Value {
    serde_json::from_slice(&fixture_bytes()).expect("shared identity fixture is not valid JSON")
}

/// The fixture speaks namespace strings; this crate speaks [`ExternalNs`].
///
/// Parsed through [`ExternalNs`]'s own `FromStr` rather than a local match, so
/// the harness tracks the enum automatically. A hand-written match would map a
/// newly-added fifth namespace to `None` and silently drop it, leaving a fixture
/// case that exercises it passing here without ever testing it — the cross-repo
/// check would quietly stop covering the new namespace.
///
/// An unrecognised namespace has no representation in this crate at all, which
/// is how the "unrecognised namespace is ignored, not ranked" cases pass: the
/// caller drops it before derivation ever sees it, exactly as the ingester does.
fn external_ids(case: &serde_json::Value) -> Vec<(ExternalNs, String)> {
    case["external_ids"]
        .as_array()
        .expect("external_ids is an array")
        .iter()
        .filter_map(|pair| {
            let ns = pair[0].as_str().expect("namespace is a string");
            let value = pair[1].as_str().expect("value is a string");
            ns.parse::<ExternalNs>()
                .ok()
                .map(|ns| (ns, value.to_string()))
        })
        .collect()
}

/// A case's `source_roots`, owned. Kept separate from the borrow below because
/// `canonical_path` takes `&[&str]` and the owned strings have to outlive it.
fn source_roots(case: &serde_json::Value) -> Vec<String> {
    case["source_roots"]
        .as_array()
        .expect("source_roots is an array")
        .iter()
        .map(|v| v.as_str().expect("a source root is a string").to_string())
        .collect()
}

fn root_refs(roots: &[String]) -> Vec<&str> {
    roots.iter().map(String::as_str).collect()
}

fn cases<'a>(fixture: &'a serde_json::Value, key: &str) -> &'a Vec<serde_json::Value> {
    fixture[key]
        .as_array()
        .unwrap_or_else(|| panic!("fixture has no `{key}` array"))
}

#[test]
fn fixture_matches_the_hash_recorded_in_both_repositories() {
    let actual = format!("{:x}", Sha256::digest(fixture_bytes()));
    assert_eq!(
        actual, FIXTURE_SHA256,
        "\n\ntests/fixtures/entry_id.json does not match the recorded SHA-256.\n\
         This file is shared with McBrideMusings/plex-db-ex (tests/fixtures/entry_id.json)\n\
         and both repositories pin its hash. If you changed it here, the two copies have\n\
         diverged and every join between that store and this catalog will silently return\n\
         nothing.\n\n\
         Copy the new file to the other repository and update the recorded SHA-256 in BOTH.\n"
    );
}

#[test]
fn every_entry_id_case_derives_the_expected_id() {
    let fixture = fixture_json();
    for case in cases(&fixture, "entry_id_cases") {
        let name = case["name"].as_str().expect("case has a name");
        let expect = case["expect"].as_str().expect("case has an expectation");
        let got = derive_entry_id(
            &external_ids(case),
            case["canonical_path"].as_str().unwrap(),
        );
        assert_eq!(got, expect, "entry_id case: {name}");
    }
}

#[test]
fn every_canonical_path_case_normalises_as_expected() {
    let fixture = fixture_json();
    for case in cases(&fixture, "canonical_path_cases") {
        let name = case["name"].as_str().expect("case has a name");
        let expect = case["expect"].as_str().expect("case has an expectation");
        let roots = source_roots(case);
        let got = canonical_path(case["raw"].as_str().unwrap(), &root_refs(&roots));
        assert_eq!(got, expect, "canonical_path case: {name}");
    }
}

#[test]
fn every_end_to_end_case_canonicalises_then_derives() {
    let fixture = fixture_json();
    for case in cases(&fixture, "end_to_end_cases") {
        let name = case["name"].as_str().expect("case has a name");
        let expect = case["expect"].as_str().expect("case has an expectation");
        let roots = source_roots(case);
        let canonical = canonical_path(case["raw"].as_str().unwrap(), &root_refs(&roots));
        let got = derive_entry_id(&external_ids(case), &canonical);
        assert_eq!(got, expect, "end_to_end case: {name}");
    }
}

/// The fixture carries the cases; this pins the *reason* they exist, so a future
/// reader who deletes a fixture case still trips something that explains itself.
#[test]
fn an_unusable_guid_value_is_absent_rather_than_an_identity() {
    // Two different films, each carrying a present-but-empty IMDb GUID, must not
    // collapse onto one id. Before #184 both derived the literal `imdb:`.
    let a = derive_entry_id(
        &[(ExternalNs::Imdb, String::new())],
        "movies/first-film.mkv",
    );
    let b = derive_entry_id(
        &[(ExternalNs::Imdb, "   ".to_string())],
        "movies/second-film.mkv",
    );
    assert_ne!(a, b, "two blank-GUID films must not share an identity");
    assert!(a.starts_with("fs:"), "expected the path fallback, got {a}");
    assert!(b.starts_with("fs:"), "expected the path fallback, got {b}");

    // A usable value in a weaker namespace beats an unusable one in a stronger.
    assert_eq!(
        derive_entry_id(
            &[
                (ExternalNs::Imdb, "  ".to_string()),
                (ExternalNs::Tmdb, "27205".to_string())
            ],
            "movies/inception.mkv"
        ),
        "tmdb:27205"
    );

    // Within one namespace, an unusable pair is skipped and a later usable pair
    // still counts — it does not disqualify the namespace.
    assert_eq!(
        derive_entry_id(
            &[
                (ExternalNs::Imdb, String::new()),
                (ExternalNs::Imdb, "tt1375666".to_string())
            ],
            "movies/inception.mkv"
        ),
        "imdb:tt1375666"
    );

    // An otherwise-usable value is used verbatim; whitespace is not trimmed.
    assert_eq!(
        derive_entry_id(
            &[(ExternalNs::Imdb, " tt1375666 ".to_string())],
            "movies/inception.mkv"
        ),
        "imdb: tt1375666 "
    );
}

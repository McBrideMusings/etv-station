//! The shared identity fixture, run against the Rust derivation.
//!
//! `entry_id` is derived by two implementations: this one, and a Python copy in
//! [plex-db-ex](https://github.com/McBrideMusings/plex-db-ex) at
//! `plexdb/identity.py`, where it is called `item_id`. Nothing enforces at
//! runtime that they agree, and if they drift every join between that store and
//! this catalog silently returns nothing — no error on either side.
//!
//! So each side runs the same table of cases against its own implementation.
//! This repository keeps `tests/fixtures/entry_id.json`; plex-db-ex keeps
//! `tests/fixtures/item_id.json`, which is the same table under its own name for
//! the id (its top-level key is `item_id_cases` where ours is `entry_id_cases`).
//!
//! **The two files are no longer byte-identical, and only this side pins a
//! hash.** plex-db-ex removed its pin deliberately and rewrote its ADR-0006 to
//! say why: the guard made every legitimate case addition a red suite plus a
//! constant to update, and the version it had pinned this repository's copy,
//! which made a consumer's state a condition of that store's suite going green.
//! Its fixture is now "the published spec of the derivation rule" — added to,
//! never quietly reinterpreted — and nothing hashes it.
//!
//! **What the pin below still buys, stated precisely:** it catches an
//! accidental edit to *our* copy. The suite goes red until the constant is
//! updated deliberately, so a fixture change is always a thing someone chose.
//! It says nothing whatsoever about the other repository, and it never did —
//! each side only ever hashed its own file. Keeping the two tables in agreement
//! is discipline, not machinery: change a case here and the same case has to be
//! carried across by hand.
//!
//! **The duplication is deliberate.** Do not de-duplicate it. A single copy read
//! across repositories by relative path holds only while both checkouts sit side
//! by side, and a clone of this repository alone would *skip* the check rather
//! than fail it — which is precisely the silence being defended against.

use std::path::PathBuf;

use etv_station::catalog::{ExternalNs, canonical_path, derive_entry_id};
use sha2::{Digest, Sha256};

/// SHA-256 of this repository's own `tests/fixtures/entry_id.json`. Nothing
/// else records it — plex-db-ex removed its pin (its ADR-0006). Update this
/// deliberately when you change a case, and carry the same case across to
/// plex-db-ex's `tests/fixtures/item_id.json` by hand.
const FIXTURE_SHA256: &str = "845182affd5d0f96430b61332681a6a75eab338ff6342775afdb60ec91f54e91";

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
fn the_fixture_matches_its_recorded_hash() {
    let actual = format!("{:x}", Sha256::digest(fixture_bytes()));
    assert_eq!(
        actual, FIXTURE_SHA256,
        "\n\ntests/fixtures/entry_id.json does not match the recorded SHA-256.\n\
         If you changed it on purpose, update FIXTURE_SHA256 in this file to the value\n\
         above — that is all this check asks for.\n\n\
         Then carry the same case across by hand. The other implementation of this rule\n\
         lives in McBrideMusings/plex-db-ex, which keeps its own table at\n\
         tests/fixtures/item_id.json (same cases, `item_id_cases` where ours is\n\
         `entry_id_cases`). It does not pin a hash and nothing compares the two files, so\n\
         if the tables disagree, every join between that store and this catalog silently\n\
         returns nothing and no suite goes red.\n"
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

    // U+001C-U+001F count as blank, because Python's `str.isspace()` does and
    // Rust's `char::is_whitespace` does not. Comparing the two definitions over
    // all of Unicode, those four are the complete disagreement — without this,
    // "\u{1c}" would be `imdb:\u{1c}` here and a path hash in Python.
    for sep in ['\u{1c}', '\u{1d}', '\u{1e}', '\u{1f}'] {
        let got = derive_entry_id(
            &[(ExternalNs::Imdb, sep.to_string())],
            "movies/separator.mkv",
        );
        assert!(
            got.starts_with("fs:"),
            "U+{:04X} must be blank, got {got}",
            sep as u32
        );
    }

    // An otherwise-usable value is used verbatim; whitespace is not trimmed.
    assert_eq!(
        derive_entry_id(
            &[(ExternalNs::Imdb, " tt1375666 ".to_string())],
            "movies/inception.mkv"
        ),
        "imdb: tt1375666 "
    );
}

/// #274's fallback for a show with no usable GUID hashes
/// `/library/metadata/{ratingKey}/children` — the show's own Plex `key`
/// (its episode-listing endpoint, the resource Plex itself names for a
/// container record), not `/library/metadata/{ratingKey}` bare. Pinned
/// against every one of the 10 GUID-less shows in a real `plex-db-ex`
/// schema-7 snapshot (1,617 shows total), spot-checked 2026-08-12 — this
/// exact set of rating keys and `fs:` hashes is what that store's own walk
/// (`plexdb/walk.py`) derived for the same shows, confirming both sides
/// still agree.
#[test]
fn a_guidless_shows_id_matches_plex_db_exs_derivation_for_the_same_shows() {
    let cases: &[(&str, &str)] = &[
        ("25546", "fs:7ea66ae69c76e410"),
        ("26383", "fs:c1a498b24adac30a"),
        ("168843", "fs:c84d9617554479dc"),
        ("30370", "fs:74a1e326ba39aa17"),
        ("31196", "fs:9b848940c9e03baa"),
        ("105549", "fs:6a36d7a75c2c3cfe"),
        ("63873", "fs:dc5307b309689fd3"),
        ("133232", "fs:63340dfa4e5eb586"),
        ("157592", "fs:7345dc875e803a59"),
        ("43876", "fs:6edc996d401664c8"),
    ];
    for (rk, expect) in cases {
        let canonical = canonical_path(&format!("/library/metadata/{rk}/children"), &[]);
        let got = derive_entry_id(&[], &canonical);
        assert_eq!(&got, expect, "rating key {rk}");
    }
}

//! Drift guard for the hand-copied `vendor/plexdb-reader/` snapshot.
//!
//! `vendor/plexdb-reader/src/` is a byte-for-byte copy of `plex-db-ex`'s
//! `crates/plexdb-reader/src/` (see `CLAUDE.md` and `vendor/plexdb-reader/Cargo.toml`'s
//! header comment). Nothing enforces at compile time that anyone actually refreshed it
//! when the source changed — the copy just keeps compiling against whatever schema it
//! last believed in, and drift only surfaces at runtime, in the load-time schema-version
//! gate, on a deployed station.
//!
//! So it is enforced by test, the same way `entry_id_fixture.rs` pins the identity
//! fixture shared with `plex-db-ex`: this hashes the vendored `src/` tree and compares
//! it against a pinned constant. A refresh that copies the files without updating the
//! constant fails `cargo test` here; the failure names both the file to re-copy from and
//! the constant to update, so there's no separate lookup for what to do next.
//!
//! Unlike the identity fixture, there's nothing to compare *against* in this checkout —
//! `plex-db-ex` is a private repository and this test deliberately does not read a path
//! outside this repository, so a clone with no sibling `plex-db-ex` checkout still runs
//! the check rather than skipping it. The pin is the only source of truth here; refreshing
//! it means going to a `plex-db-ex` checkout by hand.

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// SHA-256 over the sorted `(relative path, contents)` pairs of every file under
/// `vendor/plexdb-reader/src/`. Update by running the failing test, copying the actual
/// hash it prints, and pasting it here — in the same commit as the copy that changed it.
const VENDOR_SRC_SHA256: &str = "e919068b2a4c6d07cbadb337bf940ffd6823b3ab8b9de371d9379baf8ccddeb2";

/// Resolved from `CARGO_MANIFEST_DIR`, so the test reads nothing outside this repository.
fn vendored_src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("vendor")
        .join("plexdb-reader")
        .join("src")
}

/// Every file under `dir`. Recurses into subdirectories; `vendor/plexdb-reader/src/`
/// currently has none, but nothing should silently stop covering one if it grows some.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("failed to read vendored src dir {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Hashes the tree deterministically: paths are sorted before hashing, so the result
/// never depends on filesystem enumeration order (which `read_dir` does not guarantee)
/// or on which OS walked the directory.
fn hash_vendored_src() -> String {
    let dir = vendored_src_dir();
    let mut paths = Vec::new();
    collect_files(&dir, &mut paths);
    assert!(
        !paths.is_empty(),
        "vendor/plexdb-reader/src/ is empty or missing at {}",
        dir.display()
    );
    paths.sort();

    let mut hasher = Sha256::new();
    for path in &paths {
        let relative = path.strip_prefix(&dir).expect("file is under the src dir");
        // Forward-slash the path so the hash doesn't change between a checkout built on
        // Windows (backslash separators) and one built on Linux/macOS.
        let normalized = relative.to_string_lossy().replace('\\', "/");
        hasher.update(normalized.as_bytes());
        hasher.update(b"\0");
        let contents =
            fs::read(path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        hasher.update(&contents);
        hasher.update(b"\0");
    }
    format!("{:x}", hasher.finalize())
}

#[test]
fn vendored_src_matches_the_pinned_hash() {
    let actual = hash_vendored_src();
    assert_eq!(
        actual, VENDOR_SRC_SHA256,
        "\n\nvendor/plexdb-reader/src/ does not match the pinned SHA-256.\n\
         Either the copy fell behind plex-db-ex's crates/plexdb-reader/src/, or it was\n\
         just refreshed and the pin wasn't updated in the same commit.\n\n\
         To refresh: from a plex-db-ex checkout, copy crates/plexdb-reader/{{Cargo.toml,src}}\n\
         over vendor/plexdb-reader/ in this repo (re-adapting only the manifest's dependency\n\
         lines to this workspace's [workspace.dependencies], per CLAUDE.md), then replace\n\
         VENDOR_SRC_SHA256 in this file with the hash just printed:\n\n    {actual}\n"
    );
}

//! Every committed config under `examples/` still matches the serde types that
//! read it (#336).
//!
//! `examples/station-test.yaml` sat broken from the day `normalization` became a
//! required field: nothing in the test suite ever deserialized it, so the only
//! signal was `verify-integration.sh` aborting before it bound 8409. This test
//! is the durable half — it walks the committed examples and parses each one, so
//! the next field that becomes required fails here instead of stranding a sample.
//!
//! **Parse only, deliberately.** It calls the raw readers, not `config::load`:
//! no `${VAR}` expansion, no block splicing, no validation, no catalog. That is
//! the point — the failure being guarded is "the file no longer matches the
//! struct", and answering that must not depend on a populated catalog, real
//! media on disk, or a `PLEXDB_SNAPSHOT_PATH` pointing at a live Plex snapshot.
//!
//! `examples/overlays/` is not covered: those parse through the overlay crate's
//! own loader, which reroots script and logo paths as it reads.
//!
//! Block-file coverage is split in two (#360). `examples/samples/blocks/`
//! holds a tracked, generic fixture and is always present, so it is parsed
//! unconditionally — that is the coverage this test ships. `examples/blocks/`
//! is gitignored personal channel content: present with real files on a
//! machine that authored its own channels, absent on a fresh clone or a
//! worktree. It is parsed only when it exists, as an opportunistic local
//! check, and stays silent rather than failing when it is legitimately
//! empty.

use std::path::{Path, PathBuf};

use etv_station::config::{read_channel, read_station, BlockFile};

/// The repo root, from this crate's manifest directory.
fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .canonicalize()
        .expect("examples/ exists at the repo root")
}

/// Every `.yaml`/`.yml`/`.toml` config directly inside `dir`, sorted so a
/// failure names the same file on every run. Not recursive: each directory
/// under `examples/` holds one kind of config and gets its own assertion.
fn configs_in(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| entry.expect("readable dir entry").path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| matches!(e, "yaml" | "yml" | "toml"))
        })
        .collect();
    found.sort();
    assert!(
        !found.is_empty(),
        "no configs found in {} — did they move? An empty directory makes this \
         test pass without checking anything.",
        dir.display()
    );
    found
}

/// Like [`configs_in`], but for a directory that is legitimately absent —
/// gitignored personal content that exists only on the machine that authored
/// it, never on a fresh clone or a git worktree (#360). Missing entirely
/// means "nothing to check": returns empty rather than failing. A directory
/// that *does* exist is still handed to `configs_in`, so a present-but-empty
/// leftover or a malformed file inside it still fails loudly — the local
/// check only stops biting when the directory is gone, not when it's broken.
fn optional_configs_in(dir: &Path) -> Vec<PathBuf> {
    if dir.is_dir() {
        configs_in(dir)
    } else {
        Vec::new()
    }
}

#[test]
fn every_example_station_config_parses() {
    for path in configs_in(&examples_dir()) {
        read_station(&path).unwrap_or_else(|e| {
            panic!(
                "{} no longer deserializes as a StationConfig: {e}",
                path.display()
            )
        });
    }
}

#[test]
fn every_example_channel_config_parses() {
    let root = examples_dir();
    for dir in ["channels", "samples"] {
        for path in configs_in(&root.join(dir)) {
            read_channel(&path).unwrap_or_else(|e| {
                panic!(
                    "{} no longer deserializes as a ChannelConfig: {e}",
                    path.display()
                )
            });
        }
    }
}

/// Every tracked sample under `examples/samples/` sets `display_name` (#343)
/// — a copied sample should air under the name it schedules, not the folder
/// slug it happened to be checked out into. `examples/channels/` is exempt:
/// `.gitignore` keeps everything there but `lavfi-test.yaml` untracked and
/// personal, and this test only covers what a copier would find committed.
#[test]
fn every_sample_channel_config_sets_display_name() {
    for path in configs_in(&examples_dir().join("samples")) {
        let channel = read_channel(&path).unwrap_or_else(|e| {
            panic!(
                "{} no longer deserializes as a ChannelConfig: {e}",
                path.display()
            )
        });
        assert!(
            channel.display_name.is_some(),
            "{} has no display_name, so it would air under its folder identity",
            path.display()
        );
    }
}

/// Parses `path` as a `BlockFile`, panicking with the path on failure.
fn assert_parses_as_block_file(path: &Path) {
    let text = std::fs::read_to_string(path).expect("readable block file");
    serde_norway::from_str::<BlockFile>(&text).unwrap_or_else(|e| {
        panic!(
            "{} no longer deserializes as a BlockFile: {e}",
            path.display()
        )
    });
}

/// Tracked coverage (always present) plus an opportunistic local check
/// (present only on a machine with personal channels) — see the module doc
/// comment and #360.
#[test]
fn every_example_block_file_parses() {
    let root = examples_dir();

    // Shipped coverage: a generic fixture that exists on every clone.
    for path in configs_in(&root.join("samples").join("blocks")) {
        assert_parses_as_block_file(&path);
    }

    // Opportunistic: gitignored personal blocks, parsed only if present.
    for path in optional_configs_in(&root.join("blocks")) {
        assert_parses_as_block_file(&path);
    }
}

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
//! own loader, which reroots script and image paths as it reads.
//!
//! Block-file coverage is tracked only (#360). `examples/samples/blocks/`
//! holds a generic fixture and is parsed. `examples/blocks/` is gitignored
//! personal channel content and is not parsed here, so a broken personal file
//! cannot turn this test red in the one checkout that has it; the dev station
//! fails loading it at startup instead.

use std::path::{Path, PathBuf};

use etv_station::config::{BlockFile, read_channel, read_station};

/// The repo root, from this crate's manifest directory.
fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .canonicalize()
        .expect("examples/ exists at the repo root")
}

/// True for a `.yaml`/`.yml`/`.toml` file.
fn is_config(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e, "yaml" | "yml" | "toml"))
}

/// Panics when `found` is empty, then sorts it so a failure names the same
/// file on every run.
fn nonempty_sorted(mut found: Vec<PathBuf>, dir: &Path) -> Vec<PathBuf> {
    found.sort();
    assert!(
        !found.is_empty(),
        "no configs found in {} — did they move? An empty directory makes this \
         test pass without checking anything.",
        dir.display()
    );
    found
}

/// Every git-tracked file under `dir`, recursively, as absolute paths.
///
// Without `--full-name`, paths come back relative to `dir` itself, so nothing
// here assumes where the git toplevel is. A hook's `GIT_DIR` or `GIT_INDEX_FILE`
// would point the listing at another repo or index.
fn tracked_files_under(dir: &Path) -> Vec<PathBuf> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .args(["ls-files", "-z", "--", "."])
        .output()
        .unwrap_or_else(|e| panic!("run git ls-files in {}: {e}", dir.display()));
    assert!(
        output.status.success(),
        "git ls-files in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    output
        .stdout
        .split(|&b| b == 0)
        .filter(|name| !name.is_empty())
        .map(|name| dir.join(String::from_utf8_lossy(name).as_ref()))
        .collect()
}

/// Every git-tracked config directly inside `dir`. Not recursive: each
/// directory under `examples/` holds one kind of config and gets its own
/// assertion.
///
/// Asks git rather than reading the directory because `examples/channels/`
/// mixes one tracked channel with gitignored personal ones the dev station
/// globs. Reading the directory made a hand-written personal config turn this
/// test red in the one checkout that had it, while every worktree and fresh
/// clone stayed green.
fn configs_in(dir: &Path) -> Vec<PathBuf> {
    let found = tracked_files_under(dir)
        .into_iter()
        .filter(|path| path.parent() == Some(dir) && is_config(path))
        .collect();
    nonempty_sorted(found, dir)
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

/// Tracked block fixtures only — see the module doc comment and #360.
#[test]
fn every_example_block_file_parses() {
    let root = examples_dir();
    for path in configs_in(&root.join("samples").join("blocks")) {
        assert_parses_as_block_file(&path);
    }
}

/// `examples/station.yaml` loads both `channels/*.yaml` and `samples/*.yaml`
/// into one number and name space, and the loader drops every channel that
/// collides. Two channels landing on one number each passed their own checks
/// and left the lineup short only once both were on `main`, so this loads the
/// real station file and fails if any tracked channel is dropped, naming the
/// file and the loader's reason.
///
/// The station file's `channels:`, `chronologies:` and `arcs:` globs are
/// narrowed to tracked files before loading: the primary checkout also holds
/// gitignored personal files under `examples/channels/`, and a personal
/// channel must not turn this red (or mask a tracked collision) in the one
/// checkout that has it.
#[test]
fn the_example_station_drops_no_tracked_channel() {
    let root = examples_dir();
    let tracked = tracked_files_under(&root);

    let station_path = root.join("station.yaml");
    let text = std::fs::read_to_string(&station_path).expect("readable station.yaml");
    let mut doc: serde_norway::Value = serde_norway::from_str(&text).expect("station.yaml is YAML");
    let map = doc.as_mapping_mut().expect("station.yaml is a mapping");

    let literal_separators = glob::MatchOptions {
        require_literal_separator: true,
        ..Default::default()
    };
    for key in ["channels", "chronologies", "arcs"] {
        let Some(patterns) = map.get(key) else {
            continue;
        };
        let patterns: Vec<String> = serde_norway::from_value(patterns.clone())
            .unwrap_or_else(|e| panic!("station.yaml `{key}:` is not a list of strings: {e}"));
        let mut kept = Vec::new();
        for pattern in &patterns {
            let compiled = glob::Pattern::new(&root.join(pattern).to_string_lossy())
                .unwrap_or_else(|e| panic!("station.yaml `{key}:` pattern {pattern:?}: {e}"));
            let mut matched: Vec<String> = tracked
                .iter()
                .filter(|p| compiled.matches_path_with(p, literal_separators))
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            matched.sort();
            kept.extend(matched);
        }
        map.insert(
            serde_norway::Value::from(key),
            serde_norway::to_value(&kept).expect("paths serialise"),
        );
    }

    let scratch = tempfile::tempdir().expect("scratch dir");
    let scratch_station = scratch.path().join("station.yaml");
    std::fs::write(
        &scratch_station,
        serde_norway::to_string(&doc).expect("station serialises"),
    )
    .expect("write scratch station.yaml");

    let station = etv_station::config::load_for_inspection(&scratch_station)
        .unwrap_or_else(|e| panic!("examples/station.yaml no longer loads: {e}"));

    assert!(
        !station.lineup.is_empty(),
        "no tracked channel matched the station's `channels:` patterns"
    );
    let dropped: Vec<String> = station
        .dropped
        .iter()
        // `load_for_inspection` stands in `/nonexistent/unset-env/<VAR>` for a
        // `${VAR}` the host never set (a Plex snapshot path, a media root). A
        // channel that fails on that placeholder is missing host environment,
        // not colliding or malformed, and the check is about the latter.
        .filter(|d| !d.reason.contains("/nonexistent/unset-env/"))
        .map(|d| format!("{}: {}", d.config_path.display(), d.reason))
        .collect();
    assert!(
        dropped.is_empty(),
        "tracked example channels the loader dropped from the lineup:\n{}",
        dropped.join("\n")
    );
}

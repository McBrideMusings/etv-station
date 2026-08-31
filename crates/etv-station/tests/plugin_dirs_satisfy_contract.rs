//! Drift guard for the plugin scripts this repo ships and the ones a deploy
//! ships (#396).
//!
//! `examples/plugins/` is what the repo's own tests load. `deploy/appdata/plugins/`
//! is a separate, gitignored copy, and it is what reaches the host. #389 made
//! `audit(ctx, picks, workspace)` mandatory for every `pool_provider`, updated the
//! first directory and never touched the second — new binary, old scripts, config
//! load refused, entrypoint died, 64 channels dark for 27 minutes across 35
//! restarts. Every test passed, because the whole gate only ever saw `examples/`.
//!
//! So both directories are walked here, script by script, against the same
//! `REQUIRED_HOOK_FNS` list config-load validation enforces. Adding a required
//! contract function without updating the deployed copy now fails `cargo test` on
//! the deploy machine.
//!
//! `deploy/appdata/plugins/` is gitignored, so a worktree, a fresh clone and a CI
//! checkout do not have it. Its absence is a skip, not a failure — the guard is
//! there to catch the drift on the one machine that can see both copies.
//!
//! **This is not the whole of the check, and cannot be.** A host carries scripts
//! with no counterpart in this checkout at all: on 2026-08-31 the station's
//! plugin directory held `taste-engine.rhai`, a `pool_provider` with no `audit()`
//! that exists nowhere in this repo, armed and invisible because its one reference
//! sat inside a YAML comment. Nothing readable from a checkout can see that file.
//! `etv-station --check-plugins <DIR>` walks a directory instead of comparing
//! copies, and `admin plugin-check` runs it against the host — that is the half of
//! the guard that catches an orphan, and it belongs to the deploy, not to a test.

use std::path::{Path, PathBuf};

use etv_station::score::check_plugin_dir;

/// The repository root, from this crate's manifest directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/etv-station has a grandparent")
        .to_path_buf()
}

/// Walk one directory and assert every script in it satisfies its declared
/// hooks' contract, naming every offender at once rather than the first.
fn assert_dir_satisfies_contract(dir: &Path) {
    let checks = check_plugin_dir(dir).unwrap_or_else(|e| panic!("{e}"));
    assert!(
        !checks.is_empty(),
        "{} holds no *.rhai at all — the guard is checking nothing",
        dir.display()
    );

    let failures: Vec<String> = checks
        .iter()
        .filter(|c| !c.is_ok())
        .map(|c| match &c.result {
            Ok((hooks, missing)) => format!(
                "  {}: declares [{}] but implements no {}",
                c.path.display(),
                hooks.join(", "),
                missing
                    .iter()
                    .map(|r| r.signature)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Err(err) => format!("  {}: {err}", c.path.display()),
        })
        .collect();

    assert!(
        failures.is_empty(),
        "{} holds {} script(s) a station would refuse to load:\n{}\n\
         A station started against these fails at config load and takes every \
         channel off the air. See score::REQUIRED_HOOK_FNS.",
        dir.display(),
        failures.len(),
        failures.join("\n")
    );
}

/// The scripts the repo ships and its own integration tests load.
#[test]
fn example_plugins_satisfy_the_hook_contract() {
    assert_dir_satisfies_contract(&repo_root().join("examples/plugins"));
}

/// The scripts a deploy ships. Gitignored, so absent in a worktree or a fresh
/// clone — skipped there rather than failed, present and checked on the machine
/// that deploys.
#[test]
fn deployed_plugins_satisfy_the_hook_contract() {
    let dir = repo_root().join("deploy/appdata/plugins");
    if !dir.is_dir() {
        eprintln!(
            "skipping: {} is absent (gitignored; a worktree or fresh clone has no copy)",
            dir.display()
        );
        return;
    }
    assert_dir_satisfies_contract(&dir);
}

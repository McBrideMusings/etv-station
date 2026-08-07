//! Fails when `skrifa` resolves to more than one version in the workspace lockfile.
//!
//! `etv-overlay` pulls `skrifa` through two independent paths — `parley` for text
//! layout and `vello` for rendering. The two share a single copy only when both
//! happen to require the same `skrifa` major, and the projects release on separate
//! cadences, so most version pairs do not line up. Nothing fails when they diverge:
//! two versions of a library compile happily side by side, so a split produces a
//! green build, a bigger binary, and two font parsers disagreeing about the same
//! typeface — with no signal at all. This test is the signal. See issues #59, #122.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// One `[[package]]` block of a `Cargo.lock`, reduced to the fields this test reads.
#[derive(Default)]
struct LockPackage {
    name: String,
    version: String,
    dependencies: Vec<String>,
}

/// The workspace lockfile, found by walking up from this crate's own manifest
/// directory so the test resolves the same path whether cargo was invoked from the
/// workspace root or from `crates/etv-overlay`.
fn workspace_lockfile() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .map(|dir| dir.join("Cargo.lock"))
        .find(|lock| lock.is_file())
        .expect("no Cargo.lock in any directory above crates/etv-overlay")
}

/// Split a lockfile dependency entry into its name and, when the lockfile had to
/// disambiguate, its version. Entries take the forms `skrifa`, `skrifa 0.43.2`, and
/// `skrifa 0.43.2 (registry+https://...)`.
fn split_dependency(entry: &str) -> (&str, Option<&str>) {
    let mut parts = entry.split_whitespace();
    (parts.next().unwrap_or_default(), parts.next())
}

fn parse_packages(lock: &str) -> Vec<LockPackage> {
    let mut packages: Vec<LockPackage> = Vec::new();
    let mut current: Option<LockPackage> = None;
    let mut in_dependencies = false;

    for raw in lock.lines() {
        let line = raw.trim();

        // Any table header closes the block in progress; only `[[package]]` opens one.
        if line.starts_with('[') {
            if let Some(package) = current.take() {
                packages.push(package);
            }
            in_dependencies = false;
            if line == "[[package]]" {
                current = Some(LockPackage::default());
            }
            continue;
        }

        let Some(package) = current.as_mut() else {
            continue;
        };

        if in_dependencies {
            if line == "]" {
                in_dependencies = false;
            } else {
                package
                    .dependencies
                    .push(line.trim_end_matches(',').trim_matches('"').to_string());
            }
        } else if line == "dependencies = [" {
            in_dependencies = true;
        } else if let Some(value) = line.strip_prefix("name = ") {
            package.name = value.trim_matches('"').to_string();
        } else if let Some(value) = line.strip_prefix("version = ") {
            package.version = value.trim_matches('"').to_string();
        }
    }

    packages.extend(current);
    packages
}

/// `Ok` when exactly one `skrifa` is locked. Otherwise an `Err` naming every locked
/// version and the packages that ask for each, so whoever hits it does not have to
/// rediscover what #59 already worked out.
fn check_skrifa_unified(lock: &str, lock_path: &Path) -> Result<(), String> {
    let packages = parse_packages(lock);
    let mut wanted_by: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for package in packages.iter().filter(|p| p.name == "skrifa") {
        wanted_by.entry(package.version.as_str()).or_default();
    }

    if wanted_by.is_empty() {
        return Err(format!(
            "no `skrifa` package found in {} — either etv-overlay no longer depends on \
             parley/vello, or this test's lockfile parser has stopped working. Both need \
             a look; a silently empty result is how this check would go green while \
             checking nothing.",
            lock_path.display()
        ));
    }
    if wanted_by.len() == 1 {
        return Ok(());
    }

    // A lockfile that carries several versions of a package qualifies every reference
    // to it with a version, so each requester below lands under exactly one heading.
    // Anything left unqualified is reported separately rather than guessed at.
    let mut unqualified: Vec<String> = Vec::new();
    for package in &packages {
        for dependency in &package.dependencies {
            let (name, version) = split_dependency(dependency);
            if name != "skrifa" {
                continue;
            }
            let requester = format!("{} {}", package.name, package.version);
            match version.and_then(|version| wanted_by.get_mut(version)) {
                Some(requesters) => requesters.push(requester),
                None => unqualified.push(requester),
            }
        }
    }

    let mut message = format!(
        "`skrifa` is locked at {} versions in {}, and etv-overlay needs exactly one.\n",
        wanted_by.len(),
        lock_path.display()
    );
    for (version, requesters) in &wanted_by {
        let requesters = if requesters.is_empty() {
            "(nothing in the lockfile requires it directly)".to_string()
        } else {
            requesters.join(", ")
        };
        let _ = writeln!(message, "  skrifa {version}  <-  {requesters}");
    }
    if !unqualified.is_empty() {
        let _ = writeln!(
            message,
            "  (version unstated) <-  {}",
            unqualified.join(", ")
        );
    }
    message.push_str(
        "\nparley (text layout) and vello (rendering) each pull skrifa on their own, and \
         they share one copy only when both require the same skrifa major. Two copies \
         still compile, which is why nothing else complains: the cost is a bigger binary \
         and two font parsers reading the same typeface differently.\n\
         \nissue #122 carries the parley/vello/skrifa/wgpu compatibility table — pick a \
         row where the pair unifies, and note that moving vello also moves a wgpu major. \
         Bumping parley on its own is what re-splits this; wait for a vello release on \
         the skrifa major parley wants. Background: #59.",
    );
    Err(message)
}

#[test]
fn workspace_locks_exactly_one_skrifa() {
    let lock_path = workspace_lockfile();
    let lock = std::fs::read_to_string(&lock_path)
        .unwrap_or_else(|err| panic!("reading {}: {err}", lock_path.display()));
    if let Err(message) = check_skrifa_unified(&lock, &lock_path) {
        panic!("{message}");
    }
}

#[test]
fn a_split_lockfile_names_every_version_and_what_wants_it() {
    let lock = r#"
version = 4

[[package]]
name = "parley"
version = "0.11.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
dependencies = [
 "fontique",
 "skrifa 0.43.2",
]

[[package]]
name = "skrifa"
version = "0.42.1"
source = "registry+https://github.com/rust-lang/crates.io-index"
dependencies = [
 "read-fonts",
]

[[package]]
name = "skrifa"
version = "0.43.2"
source = "registry+https://github.com/rust-lang/crates.io-index"
dependencies = [
 "read-fonts",
]

[[package]]
name = "vello"
version = "0.9.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
dependencies = [
 "peniko",
 "skrifa 0.42.1",
 "wgpu",
]
"#;

    let message = check_skrifa_unified(lock, Path::new("Cargo.lock"))
        .expect_err("two locked skrifa versions must fail the check");

    assert!(
        message.contains("skrifa 0.42.1  <-  vello 0.9.0"),
        "message must name 0.42.1 and the crate wanting it: {message}"
    );
    assert!(
        message.contains("skrifa 0.43.2  <-  parley 0.11.0"),
        "message must name 0.43.2 and the crate wanting it: {message}"
    );
    assert!(
        message.contains("#122"),
        "message must point at the compatibility table: {message}"
    );
}

#[test]
fn a_unified_lockfile_passes() {
    let lock = r#"
version = 4

[[package]]
name = "parley"
version = "0.9.0"
dependencies = [
 "skrifa",
]

[[package]]
name = "skrifa"
version = "0.42.1"

[[package]]
name = "vello"
version = "0.9.0"
dependencies = [
 "skrifa",
]
"#;

    check_skrifa_unified(lock, Path::new("Cargo.lock")).expect("one locked skrifa must pass");
}

#[test]
fn a_lockfile_without_skrifa_fails_rather_than_passing_vacuously() {
    let lock = "version = 4\n\n[[package]]\nname = \"vello\"\nversion = \"0.9.0\"\n";
    let message = check_skrifa_unified(lock, Path::new("Cargo.lock"))
        .expect_err("no skrifa at all must fail, not silently pass");
    assert!(message.contains("no `skrifa` package found"), "{message}");
}

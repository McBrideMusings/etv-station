use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::block::BlockFile;
use super::channel::ChannelConfig;
use super::entry::Entry;
use super::overlay::{self, ChannelOverlays};
use super::source::SourceConfig;
use super::station::{self, StationConfig};
use super::validate;
use crate::errors::ConfigError;

#[derive(Debug)]
pub struct Station {
    pub config_path: PathBuf,
    pub station: StationConfig,
    pub channels: Vec<LoadedChannel>,
}

#[derive(Debug)]
pub struct LoadedChannel {
    /// Resolved channel identity: the config's `name` override, else its file
    /// stem. Drives the log label, overlay handshake, and output folder leaf.
    pub name: String,
    pub config_path: PathBuf,
    /// Derived write target: `{station.output_base}/{name}`, used verbatim
    /// relative to the process CWD.
    pub output_folder: PathBuf,
    pub config: ChannelConfig,
    /// The station → channel → block overlay cascade (#48), already resolved,
    /// loaded, and path-absolute. Held beside the config rather than inside it
    /// because resolving it needs the *station's* declaration and the two
    /// files' directories, none of which a `ChannelConfig` knows on its own.
    pub overlays: ChannelOverlays,
}

/// How config loading resolves an environment variable by name. Production
/// passes [`process_env`]; tests pass a closure over a fixed table.
///
/// This exists so a test never has to write to the process environment to steer
/// a load. `std::env::set_var` is unsound in Rust 2024 once any other thread
/// reads the environment, and `cargo test` runs this module's tests
/// concurrently — so the old "set the variable, then load" shape was a race
/// waiting for the next test that reads env.
type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The process environment, as an [`EnvLookup`]. The single impure boundary
/// config loading has; everything below it takes the lookup as an argument.
fn process_env(var: &str) -> Option<String> {
    std::env::var(var).ok()
}

pub fn load(station_path: &Path) -> Result<Station, ConfigError> {
    load_with_env(station_path, &process_env)
}

/// Load a station config for a read-only query that only cares about structure
/// — today, `--dump-overlay`.
///
/// Unset `${VAR}` references resolve to a placeholder instead of failing the
/// load. The daemon must never do this: an unset media root there would mean
/// scheduling against paths that do not exist. But a caller asking "what does
/// this channel's overlay resolve to" is asking about the cascade, and failing
/// that question because a *pool's* snapshot path is unset would make the
/// overlay preview depend on the deploy host's environment. The placeholder is
/// deliberately not a valid path, so anything that tries to *use* one fails
/// loudly rather than reading the wrong file.
pub fn load_for_inspection(station_path: &Path) -> Result<Station, ConfigError> {
    load_with_env(station_path, &|var| {
        Some(process_env(var).unwrap_or_else(|| format!("/nonexistent/unset-env/{var}")))
    })
}

fn load_with_env(station_path: &Path, env: EnvLookup<'_>) -> Result<Station, ConfigError> {
    let mut station: StationConfig = read_config_file(station_path)?;
    apply_env_overrides(
        &mut station,
        env("ETV_STATION_TZ"),
        env("ETV_STATION_OUTPUT_BASE"),
        env("ETV_STATION_CATALOG"),
        env("ETV_STATION_SOURCE_ROOTS"),
        env("ETV_STATION_IDENTITY_ROOTS"),
        env("ETV_STATION_ARTWORK_CACHE"),
    );
    validate::validate_station(station_path, &station)?;

    let base = station_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let channel_paths = expand_channel_patterns(station_path, &base, &station.channels)?;

    let mut channels = Vec::with_capacity(channel_paths.len());
    for channel_path in channel_paths {
        let mut config: ChannelConfig = read_config_file(&channel_path)?;
        resolve_blocks(&mut config, &channel_path, env)?;
        validate::validate_channel(&channel_path, &config)?;
        let name = resolve_identity(station_path, &channel_path, &config)?;
        apply_station_seed(&mut config, station.seed, &name);
        // After `resolve_blocks`, so a block-file body's own `overlay:` has
        // already been spliced onto the include and takes part in the cascade.
        let channel_dir = channel_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let overlays =
            overlay::resolve_channel(station.overlay.as_ref(), &base, &config, &channel_dir)
                .map_err(|message| ConfigError::Validation {
                    path: channel_path.clone(),
                    message,
                })?;
        // Verbatim relative to CWD (matching how the daemon writes), NOT joined
        // to the station config's directory — see `validate_output_folders`.
        let output_folder = station.output_base.join(&name);
        channels.push(LoadedChannel {
            name,
            config_path: channel_path,
            output_folder,
            config,
            overlays,
        });
    }

    // Cross-channel: two channels sharing an output_folder collide on the
    // `.anchor` and `.durations.json` sidecars. Two channels with the same
    // derived identity land on the same folder and are caught here.
    let folder_specs: Vec<(&str, &Path)> = channels
        .iter()
        .map(|c| (c.name.as_str(), c.output_folder.as_path()))
        .collect();
    validate::validate_output_folders(station_path, &folder_specs)?;

    Ok(Station {
        config_path: station_path.to_path_buf(),
        station,
        channels,
    })
}

/// Apply runtime overrides to the station config — the Docker-friendly knobs
/// that override the file value without editing it. `tz` comes from
/// `ETV_STATION_TZ`, `output_base` from `ETV_STATION_OUTPUT_BASE`,
/// `catalog_path` from `ETV_STATION_CATALOG`, `source_roots` from
/// `ETV_STATION_SOURCE_ROOTS`, `identity_roots` from
/// `ETV_STATION_IDENTITY_ROOTS`, and `artwork_cache_dir` from
/// `ETV_STATION_ARTWORK_CACHE` (all read at the single call site in [`load`]).
/// An absent or blank value leaves the file value untouched. Taking the values
/// as parameters keeps this pure and testable without mutating process-global
/// env in parallel tests.
///
/// `source_roots` and `identity_roots` are each colon-separated, matching `PATH`
/// and the `ETV_FS_ROOTS` convention the query-test harness already uses. They
/// exist so the mount paths a given host happens to use — which differ between
/// a laptop, the deployed container, and CI — stay out of the committed station
/// config entirely. The two are deliberately separate overrides for separate
/// fields (#243): one names directories the daemon may scan, the other names
/// the root to strip when deriving identity, and neither defaults from the
/// other.
fn apply_env_overrides(
    station: &mut StationConfig,
    tz: Option<String>,
    output_base: Option<String>,
    catalog_path: Option<String>,
    source_roots: Option<String>,
    identity_roots: Option<String>,
    artwork_cache_dir: Option<String>,
) {
    if let Some(tz) = tz
        && !tz.trim().is_empty()
    {
        station.tz = tz;
    }
    if let Some(base) = output_base
        && !base.trim().is_empty()
    {
        station.output_base = PathBuf::from(base);
    }
    if let Some(path) = catalog_path
        && !path.trim().is_empty()
    {
        station.catalog_path = Some(path);
    }
    if let Some(roots) = source_roots
        && !roots.trim().is_empty()
    {
        station.source_roots = split_colon_roots(&roots);
    }
    if let Some(roots) = identity_roots
        && !roots.trim().is_empty()
    {
        station.identity_roots = split_colon_roots(&roots);
    }
    if let Some(dir) = artwork_cache_dir
        && !dir.trim().is_empty()
    {
        station.artwork_cache_dir = Some(dir);
    }
}

/// Split a colon-separated root list, dropping empty segments so a stray
/// leading/trailing/doubled colon can't inject `""` as a root — an empty root
/// would strip nothing and prefix-match every path during canonicalisation
/// (identity) or glob every path on the filesystem (scanning).
fn split_colon_roots(roots: &str) -> Vec<String> {
    roots
        .split(':')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Resolve the `channels` list — a mix of literal paths and glob patterns —
/// into a deduplicated, order-preserving list of channel config files.
///
/// Each entry is resolved relative to the station config's directory. An entry
/// containing a glob metacharacter (`*`, `?`, `[`) expands to every matching
/// file (and matching nothing is an error); a literal entry is taken as-is (a
/// missing file surfaces later when it fails to read). Files matched by more
/// than one pattern appear once, in first-seen order.
fn expand_channel_patterns(
    station_path: &Path,
    base: &Path,
    patterns: &[String],
) -> Result<Vec<PathBuf>, ConfigError> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for pattern in patterns {
        let resolved = if Path::new(pattern).is_absolute() {
            PathBuf::from(pattern)
        } else {
            base.join(pattern)
        };

        if pattern.contains(['*', '?', '[']) {
            let glob_str = resolved.to_str().ok_or_else(|| ConfigError::Validation {
                path: station_path.to_path_buf(),
                message: format!("channel pattern {pattern:?} is not valid UTF-8"),
            })?;
            let matches = glob::glob(glob_str).map_err(|e| ConfigError::Validation {
                path: station_path.to_path_buf(),
                message: format!("invalid channel pattern {pattern:?}: {e}"),
            })?;
            let mut found = Vec::new();
            for entry in matches {
                let path = entry.map_err(|e| ConfigError::Validation {
                    path: station_path.to_path_buf(),
                    message: format!("reading channel pattern {pattern:?}: {e}"),
                })?;
                if path.is_file() {
                    found.push(path);
                }
            }
            if found.is_empty() {
                return Err(ConfigError::Validation {
                    path: station_path.to_path_buf(),
                    message: format!("channel pattern {pattern:?} matched no files"),
                });
            }
            // Deterministic order regardless of filesystem enumeration order.
            found.sort();
            for path in found {
                if seen.insert(dedup_key(&path)) {
                    out.push(path);
                }
            }
        } else if seen.insert(dedup_key(&resolved)) {
            out.push(resolved);
        }
    }
    Ok(out)
}

/// Lexical dedup key: drop `.` (current-dir) components so a literal
/// `./channels/a.yaml` and the glob match `channels/a.yaml` compare equal and
/// dedup, rather than slipping through to collide later in
/// [`validate::validate_output_folders`]. Purely textual — no filesystem access,
/// so it works for not-yet-existing literal paths.
fn dedup_key(path: &Path) -> PathBuf {
    path.components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect()
}

/// Derive a channel's identity: the config's `name` override if set, else
/// derived from the config path. Rejects an empty identity or one containing
/// path separators (which would let the derived output folder escape
/// `output_base`).
///
/// The derivation has two forms, because a channel file's own name is only
/// unique under the flat layout (`channels/diehard.yaml` → `diehard`). A
/// per-channel-directory layout (`channels/<name>/channel.yaml`, deploy's own
/// shape — see `docs/file-map.md`'s `deploy/appdata/` row) names every file
/// identically, so a file stem of exactly `channel` falls back to its parent
/// directory's name instead. Every other stem is used as before — a layout
/// with real per-file names never triggers the fallback.
/// Cascade the station `seed` onto one channel that declared none (#324), the
/// same station → channel shape `overlay` already uses: merge it into the
/// `ChannelConfig` here at load time, so `resolve.rs`'s
/// `config.seed.unwrap_or_else(fresh_seed)` keeps reading exactly one field and
/// there is no second place to look for a seed.
///
/// The inherited value is salted with `name` — the channel's folder name, the
/// leaf its output folder is derived from — because the SplitMix64 chain mixes
/// in nothing channel-specific on its own. Without the salt, every channel
/// under one station seed with the same candidate multiset would produce the
/// *same* shuffle. See [`derive_channel_seed`].
///
/// A channel that set its own `seed:` is left alone. With neither set, this
/// logs at INFO naming the channel, so a channel still drawing a wall-clock
/// seed on every generation is visible in the startup log rather than silent.
fn apply_station_seed(config: &mut ChannelConfig, station_seed: Option<u64>, name: &str) {
    if config.seed.is_some() {
        return;
    }
    match station_seed {
        Some(station_seed) => {
            config.seed = Some(station::derive_channel_seed(station_seed, name));
        }
        None => tracing::info!(
            channel = %name,
            "no channel seed and no station seed: this channel reshuffles on every generation"
        ),
    }
}

fn resolve_identity(
    station_path: &Path,
    channel_path: &Path,
    config: &ChannelConfig,
) -> Result<String, ConfigError> {
    let identity = match &config.name {
        Some(name) => name.trim().to_string(),
        None => {
            let stem = channel_path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| ConfigError::Validation {
                    path: channel_path.to_path_buf(),
                    message: "channel config path has no file stem to derive a name from".into(),
                })?;
            if stem == "channel" {
                channel_path
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|s| s.to_str())
                    .map(str::to_string)
                    .ok_or_else(|| ConfigError::Validation {
                        path: channel_path.to_path_buf(),
                        message: "a channel.yaml with no name: needs a named parent directory \
                                  to derive an identity from"
                            .into(),
                    })?
            } else {
                stem.to_string()
            }
        }
    };
    if identity.is_empty() {
        return Err(ConfigError::Validation {
            path: channel_path.to_path_buf(),
            message: "channel name resolved to empty".into(),
        });
    }
    if identity.contains(['/', '\\']) || identity == ".." || identity == "." {
        return Err(ConfigError::Validation {
            path: station_path.to_path_buf(),
            message: format!("channel name {identity:?} may not contain path separators"),
        });
    }
    // A newline (or other control char) in the identity becomes a folder leaf
    // that `--list-folders` prints one-per-line, so a consumer reading that
    // output would split one channel into two and misalign every channel number
    // after it.
    if identity.chars().any(char::is_control) {
        return Err(ConfigError::Validation {
            path: station_path.to_path_buf(),
            message: format!("channel name {identity:?} may not contain control characters"),
        });
    }
    Ok(identity)
}

/// Splice path-referenced block files into their includes and expand `${VAR}`
/// references in every item source, resolving each name through `env`. After
/// this runs, every `[[rule.blocks]]` entry is in normalized inline form.
fn resolve_blocks(
    config: &mut ChannelConfig,
    channel_path: &Path,
    env: EnvLookup<'_>,
) -> Result<(), ConfigError> {
    let dir = channel_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    for (idx, include) in config.rule.blocks.iter_mut().enumerate() {
        // Exactly one of: a `block = "path"` reference, or an inline body.
        // Enforced here, before splicing clears the path reference.
        let has_path = include.block.is_some();
        let has_inline = include.program.is_some()
            || include.duplicates.is_some()
            || !include.entries.is_empty()
            || !include.pools.is_empty()
            || !include.pattern.is_empty();
        match (has_path, has_inline) {
            (true, true) => {
                return Err(ConfigError::Validation {
                    path: channel_path.to_path_buf(),
                    message: format!(
                        "block #{idx} sets both a `block` path and inline fields; use exactly one"
                    ),
                });
            }
            (false, false) => {
                return Err(ConfigError::Validation {
                    path: channel_path.to_path_buf(),
                    message: format!(
                        "block #{idx} has none of: a `block` path, `entries`, or `pools` + `pattern`"
                    ),
                });
            }
            _ => {}
        }

        if let Some(block_rel) = include.block.clone() {
            let block_path = if block_rel.is_absolute() {
                block_rel
            } else {
                dir.join(&block_rel)
            };
            let body: BlockFile = read_config_file(&block_path)?;
            include.apply_body(body);
        }

        for entry in &mut include.entries {
            if let Entry::Item(item) = entry
                && let SourceConfig::Local { path } = &mut item.source
            {
                *path = expand_env(path, channel_path, env)?;
            }
        }

        // A pool's datastore grants (#167) name their location the same way —
        // `${VAR}`, never a literal — expanded here before validation ever
        // tries to open one.
        for pool in &mut include.pools {
            for grant in &mut pool.datastores {
                grant.path = expand_env(&grant.path, channel_path, env)?;
            }
        }
    }

    Ok(())
}

/// Substitute every `${VAR}` in `input` with the value `env` returns for that
/// name. `env` is the seam: production hands this [`process_env`], a test hands
/// it a fixed table, and neither path needs the variable to exist in the
/// running process.
fn expand_env(input: &str, ctx: &Path, env: EnvLookup<'_>) -> Result<String, ConfigError> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').ok_or_else(|| ConfigError::Validation {
            path: ctx.to_path_buf(),
            message: format!("unterminated ${{...}} in {input:?}"),
        })?;
        let var = &after[..end];
        if var.is_empty() {
            return Err(ConfigError::Validation {
                path: ctx.to_path_buf(),
                message: format!("empty ${{}} in {input:?}"),
            });
        }
        let val = env(var).ok_or_else(|| ConfigError::Validation {
            path: ctx.to_path_buf(),
            message: format!("env var `{var}` referenced by {input:?} is not set"),
        })?;
        out.push_str(&val);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Read and deserialize a single channel config file, picking TOML or YAML by
/// extension. Raw parse only — does not splice block references or expand
/// `${VAR}` in item sources (that happens inside [`load`]). Useful for tests and
/// tools that resolve one channel against a catalog directly.
pub fn read_channel(path: &Path) -> Result<ChannelConfig, ConfigError> {
    read_config_file(path)
}

/// Read and deserialize a single station config file, picking TOML or YAML by
/// extension. Raw parse only — does not expand the channel list, apply env
/// overrides, or validate (that happens inside [`load`]). Useful for asking
/// whether a station file still matches [`StationConfig`] without needing the
/// catalog, media, or env the full load demands.
pub fn read_station(path: &Path) -> Result<StationConfig, ConfigError> {
    read_config_file(path)
}

/// Read any config file — station, channel, or block — picking the deserializer
/// by extension: `.yaml`/`.yml` parse as YAML (`serde_norway`), everything else
/// as TOML. The station, channel, and block serde types are format-agnostic, so
/// the same file authored in either format produces identical output.
///
/// A key the types do not know is **reported, not refused**. The config
/// types carried `#[serde(deny_unknown_fields)]` until a rollback proved what
/// that costs: channel configs live in a bind-mounted volume that outlives the
/// container, the parser that reads them ships inside the image, so rolling the
/// image back to before a field was added left a newer config in front of an
/// older parser. It refused every start — 11 crashloops — over `attribution:
/// true`, a key the running image had understood minutes earlier. Any rollback
/// across a config-schema addition failed the same way, which made the rollback
/// path useless at exactly the moment it was reached for.
///
/// The reason for the old strictness is still real: a misspelled key that reads
/// as a default airs the wrong thing and says nothing. So the key is not
/// silently dropped — every unrecognised path is logged by name, against the
/// file it came from, through the one funnel every config file already passes
/// through. `serde_ignored` collects those paths from the deserializer itself,
/// so nothing here keeps a second list of field names that could drift from the
/// structs.
fn read_config_file<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, ConfigError> {
    let is_yaml = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"));
    let contents = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let mut ignored: Vec<String> = Vec::new();
    let parsed = if is_yaml {
        let de = serde_norway::Deserializer::from_str(&contents);
        serde_ignored::deserialize(de, |p| ignored.push(p.to_string())).map_err(|source| {
            ConfigError::ParseYaml {
                path: path.to_path_buf(),
                source,
            }
        })?
    } else {
        let de = toml::Deserializer::new(&contents);
        serde_ignored::deserialize(de, |p| ignored.push(p.to_string())).map_err(|source| {
            ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            }
        })?
    };

    if !ignored.is_empty() {
        tracing::warn!(
            event = "config.unknown_keys",
            path = %path.display(),
            keys = %ignored.join(", "),
            "config keys this build does not know; they were ignored. A typo reads \
             as the field being unset, so check the spelling if the channel does \
             not behave as written"
        );
    }

    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Entry;

    fn examples_station() -> PathBuf {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        manifest_dir.join("../../examples/station.yaml")
    }

    /// An [`EnvLookup`] backed by a fixed table. Tests use this instead of
    /// writing to the process environment, which is unsound in Rust 2024 while
    /// the rest of the module's tests run on other threads.
    fn env_map(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let table: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name| {
            table
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    /// An [`EnvLookup`] where nothing is set, for fixtures with no `${VAR}`.
    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// A minimal but real plexdb store at the schema version `plexdb-reader`
    /// understands — just enough for the sample `foryou.yaml` channel's
    /// `movies` pool to open its granted `taste` datastore at load. Its
    /// contents are never read here; this test only proves the whole example
    /// station still loads end to end, not what the plugin scores.
    fn write_minimal_taste_store() -> PathBuf {
        // `keep()` hands back the directory and gives up deleting it: the
        // fixture has to outlive this call, and a `tempdir()` still owned here
        // would clean itself up on drop.
        let path = tempfile::tempdir().unwrap().keep().join("taste.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE schema_version (version INTEGER NOT NULL);
             INSERT INTO schema_version (version) VALUES ({version});",
            version = plexdb_reader::SUPPORTED_SCHEMA_VERSION,
        ))
        .unwrap();
        path
    }

    fn local_paths(loaded: &Station) -> Vec<&str> {
        loaded
            .channels
            .iter()
            .flat_map(|c| c.config.rule.blocks.iter())
            .flat_map(|b| b.entries())
            .filter_map(|e| match e {
                Entry::Item(item) => match &item.source {
                    SourceConfig::Local { path } => Some(path.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    #[test]
    fn loads_example_fixture() {
        // An example channel references ${ETV_TEST_MEDIA_DIR}, and the
        // `foryou.yaml` sample's movies pool references
        // ${PLEXDB_SNAPSHOT_PATH} (#254) — hand the loader a table holding
        // both rather than exporting either into the running process.
        let taste_store = write_minimal_taste_store();
        // Bound rather than passed inline: `env_map` returns an `impl Fn` that
        // captures its argument's lifetime, so a temporary array would be
        // freed while the returned lookup still borrowed it.
        let vars = [
            ("ETV_TEST_MEDIA_DIR", "/tmp/etv-test-media"),
            ("PLEXDB_SNAPSHOT_PATH", taste_store.to_str().unwrap()),
        ];
        let env = env_map(&vars);
        let path = examples_station();
        let loaded = load_with_env(&path, &env).expect("examples/station.yaml should load");
        let ch = loaded
            .channels
            .iter()
            .find(|c| c.name == "test")
            .expect("test channel present");
        let entries: usize = ch
            .config
            .rule
            .blocks
            .iter()
            .map(|b| b.entries().len())
            .sum();
        assert!(entries > 0, "rule must resolve to at least one entry");

        // Proves the expansion came from the table above and not from whatever
        // ETV_TEST_MEDIA_DIR happens to hold in the shell running the suite.
        let paths = local_paths(&loaded);
        assert!(
            paths.iter().any(|p| p.starts_with("/tmp/etv-test-media")),
            "a local source must expand from the supplied table, got {paths:?}"
        );
    }

    #[test]
    fn resolves_path_referenced_block() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("blocks")).unwrap();
        std::fs::write(
            dir.path().join("blocks/b.toml"),
            "[[entries]]\nkind = \"item\"\nid = \"x\"\n[entries.source]\nkind = \"lavfi\"\nparams = \"testsrc\"\n",
        )
        .unwrap();
        let channel_path = dir.path().join("channel.toml");
        std::fs::write(
            &channel_path,
            "[[rule.blocks]]\nblock = \"blocks/b.toml\"\nmode = \"all\"\norder = \"manual\"\n",
        )
        .unwrap();

        let mut config: ChannelConfig = read_config_file(&channel_path).unwrap();
        resolve_blocks(&mut config, &channel_path, &no_env).unwrap();
        let inc = &config.rule.blocks[0];
        assert!(
            inc.block.is_none(),
            "path ref should be cleared after splice"
        );
        assert_eq!(inc.entries().len(), 1);
        assert!(matches!(inc.entries()[0], Entry::Item(_)));
    }

    #[test]
    fn resolves_yaml_path_referenced_block() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("blocks")).unwrap();
        std::fs::write(
            dir.path().join("blocks/b.yaml"),
            "entries:\n  - kind: item\n    id: x\n    source:\n      kind: lavfi\n      params: testsrc\n",
        )
        .unwrap();
        let channel_path = dir.path().join("channel.toml");
        std::fs::write(
            &channel_path,
            "[[rule.blocks]]\nblock = \"blocks/b.yaml\"\nmode = \"all\"\norder = \"manual\"\n",
        )
        .unwrap();

        let mut config: ChannelConfig = read_config_file(&channel_path).unwrap();
        resolve_blocks(&mut config, &channel_path, &no_env).unwrap();
        let inc = &config.rule.blocks[0];
        assert!(
            inc.block.is_none(),
            "path ref should be cleared after splice"
        );
        assert_eq!(inc.entries().len(), 1);
        assert!(matches!(inc.entries()[0], Entry::Item(_)));
    }

    #[test]
    fn loads_yaml_channel_file() {
        let dir = tempfile::tempdir().unwrap();
        let channel_path = dir.path().join("channel.yaml");
        std::fs::write(
            &channel_path,
            "roll_interval: 60s\nrule:\n  blocks:\n    - mode: all\n      order: manual\n      entries:\n        - kind: item\n          id: x\n          out_point: 30s\n          source:\n            kind: lavfi\n            params: testsrc\n",
        )
        .unwrap();

        let mut config: ChannelConfig = read_config_file(&channel_path).unwrap();
        resolve_blocks(&mut config, &channel_path, &no_env).unwrap();
        let inc = &config.rule.blocks[0];
        assert_eq!(inc.entries().len(), 1);
        assert!(matches!(inc.entries()[0], Entry::Item(_)));
    }

    #[test]
    fn pattern_block_without_program_is_not_empty() {
        // Regression for #149: a pattern block (pools + pattern) with no
        // `program:` metadata was rejected as if it had no content, because
        // the emptiness check only looked at `program`, `duplicates`, and
        // `entries` — never `pools` or `pattern`. Exact shape from the issue.
        let dir = tempfile::tempdir().unwrap();
        let channel_path = dir.path().join("channel.yaml");
        std::fs::write(
            &channel_path,
            "rule:\n  blocks:\n    - mode: \"all\"\n      pools:\n        - name: \"shows\"\n          expr: 'kind == \"episode\"'\n      pattern:\n        - pool: \"shows\"\n          take: 1\n",
        )
        .unwrap();

        let mut config: ChannelConfig = read_config_file(&channel_path).unwrap();
        resolve_blocks(&mut config, &channel_path, &no_env)
            .expect("a block with pools + pattern has content and must load");
        let inc = &config.rule.blocks[0];
        assert_eq!(inc.pools.len(), 1);
        assert_eq!(inc.pattern.len(), 1);
    }

    /// A pool's datastore grant (#167) names its location as `${VAR}`, never
    /// a literal — expanded the same way an item's local `source.path` is,
    /// so validation's open-check sees a real filesystem path.
    #[test]
    fn resolve_blocks_expands_a_pool_datastore_path() {
        let dir = tempfile::tempdir().unwrap();
        let channel_path = dir.path().join("channel.yaml");
        std::fs::write(
            &channel_path,
            "rule:\n  blocks:\n    - mode: \"all\"\n      pools:\n        - name: \"shows\"\n          plugin: \"scorer.rhai\"\n          datastores:\n            - name: \"taste_db\"\n              path: \"${TASTE_DB_PATH}\"\n      pattern:\n        - pool: \"shows\"\n          take: 1\n",
        )
        .unwrap();

        let env = env_map(&[("TASTE_DB_PATH", "/srv/taste.db")]);
        let mut config: ChannelConfig = read_config_file(&channel_path).unwrap();
        resolve_blocks(&mut config, &channel_path, &env).unwrap();
        let inc = &config.rule.blocks[0];
        assert_eq!(inc.pools[0].datastores[0].path, "/srv/taste.db");
    }

    #[test]
    fn expand_env_substitutes_vars() {
        let env = env_map(&[("ETV_LOAD_TEST_DIR", "/tmp/etv-load-test")]);
        let out = expand_env(
            "${ETV_LOAD_TEST_DIR}/movie.mkv",
            Path::new("/dev/null"),
            &env,
        )
        .unwrap();
        assert_eq!(out, "/tmp/etv-load-test/movie.mkv");
    }

    #[test]
    fn expand_env_errors_on_missing_var() {
        let err = expand_env(
            "${ETV_LOAD_TEST_DEFINITELY_UNSET}/x",
            Path::new("/dev/null"),
            &no_env,
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("ETV_LOAD_TEST_DEFINITELY_UNSET"),
            "msg = {msg}"
        );
    }

    #[test]
    fn expand_env_passes_through_literals() {
        let out = expand_env("/no/vars/here.mkv", Path::new("/dev/null"), &no_env).unwrap();
        assert_eq!(out, "/no/vars/here.mkv");
    }

    fn touch_channels(base: &Path, names: &[&str]) {
        std::fs::create_dir_all(base.join("channels")).unwrap();
        for n in names {
            std::fs::write(base.join("channels").join(n), "").unwrap();
        }
    }

    fn file_names(paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn glob_pattern_expands_sorted() {
        let dir = tempfile::tempdir().unwrap();
        touch_channels(dir.path(), &["b.yaml", "a.yaml", "c.yaml"]);
        let station = dir.path().join("station.yaml");
        let got =
            expand_channel_patterns(&station, dir.path(), &["channels/*.yaml".into()]).unwrap();
        assert_eq!(file_names(&got), ["a.yaml", "b.yaml", "c.yaml"]);
    }

    #[test]
    fn literal_and_glob_overlap_dedups_first_seen() {
        let dir = tempfile::tempdir().unwrap();
        touch_channels(dir.path(), &["a.yaml", "b.yaml"]);
        let station = dir.path().join("station.yaml");
        let got = expand_channel_patterns(
            &station,
            dir.path(),
            &["channels/a.yaml".into(), "channels/*.yaml".into()],
        )
        .unwrap();
        // a.yaml from the literal (first), b.yaml from the glob; a not repeated.
        assert_eq!(file_names(&got), ["a.yaml", "b.yaml"]);
    }

    #[test]
    fn dot_prefixed_literal_dedups_against_glob() {
        let dir = tempfile::tempdir().unwrap();
        touch_channels(dir.path(), &["a.yaml", "b.yaml"]);
        let station = dir.path().join("station.yaml");
        // The `./` prefix must not defeat dedup against the glob's match.
        let got = expand_channel_patterns(
            &station,
            dir.path(),
            &["./channels/a.yaml".into(), "channels/*.yaml".into()],
        )
        .unwrap();
        assert_eq!(file_names(&got), ["a.yaml", "b.yaml"]);
    }

    #[test]
    fn glob_matching_nothing_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("channels")).unwrap();
        let station = dir.path().join("station.yaml");
        let err =
            expand_channel_patterns(&station, dir.path(), &["channels/*.yaml".into()]).unwrap_err();
        assert!(format!("{err}").contains("matched no files"));
    }

    fn parse_channel(yaml: &str) -> ChannelConfig {
        serde_norway::from_str(yaml).unwrap()
    }

    const MINIMAL_RULE: &str = "rule:\n  blocks:\n    - mode: all\n      order: manual\n      entries:\n        - kind: item\n          id: x\n          out_point: 30s\n          source:\n            kind: lavfi\n            params: testsrc\n";

    #[test]
    fn identity_defaults_to_file_stem() {
        let cfg = parse_channel(MINIMAL_RULE);
        let id = resolve_identity(
            Path::new("/s/station.yaml"),
            Path::new("/s/channels/diehard.yaml"),
            &cfg,
        )
        .unwrap();
        assert_eq!(id, "diehard");
    }

    #[test]
    fn identity_uses_name_override() {
        let cfg = parse_channel(&format!("name: \"Star Wars Saga\"\n{MINIMAL_RULE}"));
        let id = resolve_identity(
            Path::new("/s/station.yaml"),
            Path::new("/s/channels/starwars.yaml"),
            &cfg,
        )
        .unwrap();
        assert_eq!(id, "Star Wars Saga");
    }

    /// The per-channel-directory layout (`channels/<name>/channel.yaml`) names
    /// every file identically, so the flat-layout default of "the file's own
    /// stem" would resolve every channel in it to the same identity —
    /// `channel` — and collide. A channel.yaml with no `name:` falls back to
    /// its parent directory instead.
    #[test]
    fn a_bare_channel_dot_yaml_falls_back_to_its_parent_directory() {
        let cfg = parse_channel(MINIMAL_RULE);
        let id = resolve_identity(
            Path::new("/s/station.yaml"),
            Path::new("/s/channels/085-hbo/channel.yaml"),
            &cfg,
        )
        .unwrap();
        assert_eq!(id, "085-hbo");
    }

    /// Two different directories under the per-channel layout must resolve to
    /// two different identities — the regression this fallback exists to fix
    /// (both channels landed on the literal identity `channel` and collided on
    /// `output_folder`).
    #[test]
    fn two_channel_dot_yaml_files_in_different_directories_get_different_identities() {
        let cfg = parse_channel(MINIMAL_RULE);
        let a = resolve_identity(
            Path::new("/s/station.yaml"),
            Path::new("/s/channels/001-for-you/channel.yaml"),
            &cfg,
        )
        .unwrap();
        let b = resolve_identity(
            Path::new("/s/station.yaml"),
            Path::new("/s/channels/002-for-pierce/channel.yaml"),
            &cfg,
        )
        .unwrap();
        assert_ne!(a, b);
    }

    /// A `channel.yaml` with an explicit `name:` still uses it — the parent-
    /// directory fallback only applies when nothing else was said.
    #[test]
    fn a_bare_channel_dot_yaml_still_honors_an_explicit_name() {
        let cfg = parse_channel(&format!("name: \"custom-id\"\n{MINIMAL_RULE}"));
        let id = resolve_identity(
            Path::new("/s/station.yaml"),
            Path::new("/s/channels/085-hbo/channel.yaml"),
            &cfg,
        )
        .unwrap();
        assert_eq!(id, "custom-id");
    }

    /// A channel that said nothing inherits the station seed (#324) — the
    /// whole point: 37 production channels had no `seed:` and drew a fresh
    /// wall-clock one on every regeneration.
    #[test]
    fn a_channel_with_no_seed_inherits_the_station_seed() {
        let mut cfg = parse_channel(MINIMAL_RULE);
        apply_station_seed(&mut cfg, Some(42), "001-for-you");
        assert_eq!(
            cfg.seed,
            Some(station::derive_channel_seed(42, "001-for-you"))
        );
    }

    /// The salt is the reason this is not a one-liner. Two channels with the
    /// same pools under one station seed must not receive the same number, or
    /// `seeded_shuffle` hands them the identical permutation.
    #[test]
    fn two_channels_inheriting_one_station_seed_get_different_seeds() {
        let mut a = parse_channel(MINIMAL_RULE);
        let mut b = parse_channel(MINIMAL_RULE);
        apply_station_seed(&mut a, Some(42), "001-for-you");
        apply_station_seed(&mut b, Some(42), "002-for-pierce");
        assert!(a.seed.is_some());
        assert_ne!(a.seed, b.seed);
    }

    /// A channel that pinned its own seed is untouched by the station value.
    #[test]
    fn an_explicit_channel_seed_beats_the_station_seed() {
        let mut cfg = parse_channel(&format!("seed: 7\n{MINIMAL_RULE}"));
        apply_station_seed(&mut cfg, Some(42), "001-for-you");
        assert_eq!(cfg.seed, Some(7));
    }

    /// Neither set leaves the field alone, so `resolve.rs` still falls through
    /// to `fresh_seed()` — behavior unchanged for a station that never adopts
    /// this key.
    #[test]
    fn no_station_seed_leaves_an_unseeded_channel_unseeded() {
        let mut cfg = parse_channel(MINIMAL_RULE);
        apply_station_seed(&mut cfg, None, "001-for-you");
        assert_eq!(cfg.seed, None);
    }

    fn station_config() -> StationConfig {
        StationConfig {
            overlay: None,
            seed: None,
            tz: "UTC".into(),
            output_base: PathBuf::from("out"),
            channels: vec!["channels/a.yaml".into()],
            source_roots: vec![],
            identity_roots: vec![],
            catalog_path: None,
            artwork_cache_dir: None,
            catalog_refresh_secs: 900,
            full_sweep_after_secs: 86_400,
            device_id: None,
            ffmpeg: crate::config::station::default_ffmpeg(),
            normalization: crate::config::station::empty_normalization_for_test(),
        }
    }

    #[test]
    fn env_overrides_apply_when_set() {
        let mut s = station_config();
        apply_env_overrides(
            &mut s,
            Some("America/Chicago".into()),
            Some("/shared/playout".into()),
            Some("/var/lib/etv/catalog.db".into()),
            Some("/mnt/movies:/mnt/tv".into()),
            Some("/mnt/identity".into()),
            Some("/data/artwork".into()),
        );
        assert_eq!(s.tz, "America/Chicago");
        assert_eq!(s.output_base, PathBuf::from("/shared/playout"));
        assert_eq!(s.catalog_path.as_deref(), Some("/var/lib/etv/catalog.db"));
        assert_eq!(s.source_roots, vec!["/mnt/movies", "/mnt/tv"]);
        assert_eq!(s.identity_roots, vec!["/mnt/identity"]);
        assert_eq!(s.artwork_cache_dir.as_deref(), Some("/data/artwork"));
    }

    #[test]
    fn env_overrides_ignore_absent_and_blank() {
        let mut s = station_config();
        apply_env_overrides(
            &mut s,
            None,
            Some("   ".into()),
            Some("  ".into()),
            None,
            None,
            Some("   ".into()),
        );
        assert_eq!(s.tz, "UTC");
        assert_eq!(s.output_base, PathBuf::from("out"));
        assert_eq!(s.catalog_path, None);
        assert!(s.source_roots.is_empty());
        assert!(s.identity_roots.is_empty());
        assert_eq!(s.artwork_cache_dir, None);
    }

    #[test]
    fn source_roots_override_drops_empty_segments() {
        let mut s = station_config();
        apply_env_overrides(
            &mut s,
            None,
            None,
            None,
            Some(":/mnt/a: :/mnt/b:".into()),
            None,
            None,
        );
        assert_eq!(s.source_roots, vec!["/mnt/a", "/mnt/b"]);
    }

    #[test]
    fn source_roots_override_keeps_file_value_when_blank() {
        let mut s = station_config();
        s.source_roots = vec!["/from/file".into()];
        apply_env_overrides(&mut s, None, None, None, Some("   ".into()), None, None);
        assert_eq!(s.source_roots, vec!["/from/file"]);
    }

    #[test]
    fn identity_roots_override_drops_empty_segments() {
        let mut s = station_config();
        apply_env_overrides(
            &mut s,
            None,
            None,
            None,
            None,
            Some(":/mnt/a: :/mnt/b:".into()),
            None,
        );
        assert_eq!(s.identity_roots, vec!["/mnt/a", "/mnt/b"]);
    }

    #[test]
    fn identity_roots_override_keeps_file_value_when_blank() {
        let mut s = station_config();
        s.identity_roots = vec!["/from/file".into()];
        apply_env_overrides(&mut s, None, None, None, None, Some("   ".into()), None);
        assert_eq!(s.identity_roots, vec!["/from/file"]);
    }

    #[test]
    fn artwork_cache_dir_override_keeps_file_value_when_blank() {
        let mut s = station_config();
        s.artwork_cache_dir = Some("/from/file".into());
        apply_env_overrides(&mut s, None, None, None, None, None, Some("  ".into()));
        assert_eq!(s.artwork_cache_dir.as_deref(), Some("/from/file"));
    }

    #[test]
    fn source_roots_and_identity_roots_overrides_are_independent() {
        // Setting one must not touch the other — the whole point of the split
        // (#243) is that the scan roots and the identity roots are two separate
        // decisions, not one defaulted from the other.
        let mut s = station_config();
        apply_env_overrides(
            &mut s,
            None,
            None,
            None,
            Some("/mnt/scan".into()),
            None,
            None,
        );
        assert_eq!(s.source_roots, vec!["/mnt/scan"]);
        assert!(s.identity_roots.is_empty());

        let mut s = station_config();
        apply_env_overrides(
            &mut s,
            None,
            None,
            None,
            None,
            Some("/mnt/identity".into()),
            None,
        );
        assert!(s.source_roots.is_empty());
        assert_eq!(s.identity_roots, vec!["/mnt/identity"]);
    }

    #[test]
    fn identity_rejects_path_separators() {
        let cfg = parse_channel(&format!("name: \"../escape\"\n{MINIMAL_RULE}"));
        let err = resolve_identity(
            Path::new("/s/station.yaml"),
            Path::new("/s/channels/c.yaml"),
            &cfg,
        )
        .unwrap_err();
        assert!(format!("{err}").contains("path separators"));
    }

    #[test]
    fn identity_rejects_control_chars() {
        let cfg = parse_channel(&format!("name: \"foo\\nbar\"\n{MINIMAL_RULE}"));
        let err = resolve_identity(
            Path::new("/s/station.yaml"),
            Path::new("/s/channels/c.yaml"),
            &cfg,
        )
        .unwrap_err();
        assert!(format!("{err}").contains("control characters"));
    }
}

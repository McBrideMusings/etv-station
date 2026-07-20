use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::block::BlockFile;
use super::channel::ChannelConfig;
use super::entry::Entry;
use super::source::SourceConfig;
use super::station::StationConfig;
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
}

pub fn load(station_path: &Path) -> Result<Station, ConfigError> {
    let mut station: StationConfig = read_config_file(station_path)?;
    apply_env_overrides(
        &mut station,
        std::env::var("ETV_STATION_TZ").ok(),
        std::env::var("ETV_STATION_OUTPUT_BASE").ok(),
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
        resolve_blocks(&mut config, &channel_path)?;
        validate::validate_channel(&channel_path, &config)?;
        let name = resolve_identity(station_path, &channel_path, &config)?;
        // Verbatim relative to CWD (matching how the daemon writes), NOT joined
        // to the station config's directory — see `validate_output_folders`.
        let output_folder = station.output_base.join(&name);
        channels.push(LoadedChannel {
            name,
            config_path: channel_path,
            output_folder,
            config,
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
/// `ETV_STATION_TZ`, `output_base` from `ETV_STATION_OUTPUT_BASE` (both read at
/// the single call site in [`load`]). An absent or blank value leaves the file
/// value untouched. Taking the values as parameters keeps this pure and testable
/// without mutating process-global env in parallel tests.
fn apply_env_overrides(
    station: &mut StationConfig,
    tz: Option<String>,
    output_base: Option<String>,
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

/// Derive a channel's identity: the config's `name` override if set, else the
/// config file's stem. Rejects an empty identity or one containing path
/// separators (which would let the derived output folder escape `output_base`).
fn resolve_identity(
    station_path: &Path,
    channel_path: &Path,
    config: &ChannelConfig,
) -> Result<String, ConfigError> {
    let identity = match &config.name {
        Some(name) => name.trim().to_string(),
        None => channel_path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
            .ok_or_else(|| ConfigError::Validation {
                path: channel_path.to_path_buf(),
                message: "channel config path has no file stem to derive a name from".into(),
            })?,
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
    // that `--list-folders` prints one-per-line, so render-etv-next.py would
    // split one channel into two and misalign every channel number after it.
    if identity.chars().any(char::is_control) {
        return Err(ConfigError::Validation {
            path: station_path.to_path_buf(),
            message: format!("channel name {identity:?} may not contain control characters"),
        });
    }
    Ok(identity)
}

/// Splice path-referenced block files into their includes and expand `${VAR}`
/// references in every item source. After this runs, every `[[rule.blocks]]`
/// entry is in normalized inline form.
fn resolve_blocks(config: &mut ChannelConfig, channel_path: &Path) -> Result<(), ConfigError> {
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
            || !include.entries.is_empty();
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
                    message: format!("block #{idx} has neither a `block` path nor inline entries"),
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
                *path = expand_env(path, channel_path)?;
            }
        }
    }

    Ok(())
}

fn expand_env(input: &str, ctx: &Path) -> Result<String, ConfigError> {
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
        let val = std::env::var(var).map_err(|_| ConfigError::Validation {
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

/// Read any config file — station, channel, or block — picking the deserializer
/// by extension: `.yaml`/`.yml` parse as YAML (`serde_norway`), everything else
/// as TOML. The station, channel, and block serde types are format-agnostic, so
/// the same file authored in either format produces identical output.
fn read_config_file<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, ConfigError> {
    let is_yaml = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"));
    let contents = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if is_yaml {
        serde_norway::from_str(&contents).map_err(|source| ConfigError::ParseYaml {
            path: path.to_path_buf(),
            source,
        })
    } else {
        toml::from_str(&contents).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Entry;

    fn examples_station() -> PathBuf {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        manifest_dir.join("../../examples/station.yaml")
    }

    #[test]
    fn loads_example_fixture() {
        // The diehard example references ${ETV_TEST_MEDIA_DIR}; set a placeholder
        // so the env-expansion step in `load` succeeds in test environments.
        // SAFETY: single-threaded test; value is local in scope.
        unsafe {
            std::env::set_var("ETV_TEST_MEDIA_DIR", "/tmp/etv-test-media");
        }
        let path = examples_station();
        let loaded = load(&path).expect("examples/station.yaml should load");
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
        resolve_blocks(&mut config, &channel_path).unwrap();
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
        resolve_blocks(&mut config, &channel_path).unwrap();
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
        resolve_blocks(&mut config, &channel_path).unwrap();
        let inc = &config.rule.blocks[0];
        assert_eq!(inc.entries().len(), 1);
        assert!(matches!(inc.entries()[0], Entry::Item(_)));
    }

    #[test]
    fn expand_env_substitutes_vars() {
        // SAFETY: single-threaded test, value scoped to this test only.
        unsafe {
            std::env::set_var("ETV_LOAD_TEST_DIR", "/tmp/etv-load-test");
        }
        let out = expand_env("${ETV_LOAD_TEST_DIR}/movie.mkv", Path::new("/dev/null")).unwrap();
        assert_eq!(out, "/tmp/etv-load-test/movie.mkv");
    }

    #[test]
    fn expand_env_errors_on_missing_var() {
        let err = expand_env(
            "${ETV_LOAD_TEST_DEFINITELY_UNSET}/x",
            Path::new("/dev/null"),
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
        let out = expand_env("/no/vars/here.mkv", Path::new("/dev/null")).unwrap();
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

    fn station_config() -> StationConfig {
        StationConfig {
            tz: "UTC".into(),
            output_base: PathBuf::from("out"),
            channels: vec!["channels/a.yaml".into()],
        }
    }

    #[test]
    fn env_overrides_apply_when_set() {
        let mut s = station_config();
        apply_env_overrides(
            &mut s,
            Some("America/Chicago".into()),
            Some("/shared/playout".into()),
        );
        assert_eq!(s.tz, "America/Chicago");
        assert_eq!(s.output_base, PathBuf::from("/shared/playout"));
    }

    #[test]
    fn env_overrides_ignore_absent_and_blank() {
        let mut s = station_config();
        apply_env_overrides(&mut s, None, Some("   ".into()));
        assert_eq!(s.tz, "UTC");
        assert_eq!(s.output_base, PathBuf::from("out"));
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

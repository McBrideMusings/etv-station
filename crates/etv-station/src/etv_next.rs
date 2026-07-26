//! Render ETV-next's `lineup.json` + `channel{N}.json` from the station config.
//!
//! This is the single-source-of-truth generator for the ETV-next side of the
//! shared-folder contract. Instead of hand-authoring each channel's
//! `playout.folder` to match what the station writes, we DERIVE it: the station
//! config loader resolves each channel's output folder, and we emit ETV-next
//! config that reads exactly those folders.
//!
//! What the station owns (derived here): the channel roster, channel numbers
//! (station order), and each `playout.folder`.
//!
//! What ETV-next owns (supplied here, NOT from the station config): display
//! names and the `normalization` / `ffmpeg` playback block. Defaults come from
//! `normalization.default.json` in the output directory; per-channel display
//! names and playback overrides come from an optional `presentation.json` keyed
//! by channel identity, each value `{name?, config?}` where `config` is a
//! partial deep-merged onto the default channel body.
//!
//! It lives in the daemon binary rather than a helper script because the
//! runtime image runs both processes and has no interpreter: the container
//! entrypoint renders ETV-next's config from the mounted station config at
//! start, so the two can never disagree.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::config;

/// Where the generated files go and what the lineup server is told to do.
#[derive(Debug, Clone)]
pub struct RenderOptions {
    /// `server.bind_address` in the emitted lineup.
    pub bind_address: String,
    /// `server.port` in the emitted lineup.
    pub port: u16,
    /// `output.folder` in the emitted lineup — ETV-next's HLS working dir.
    pub hls_output: String,
    /// Directory holding `normalization.default.json` (and the optional
    /// `presentation.json`), and receiving the generated files.
    pub out_dir: PathBuf,
}

impl RenderOptions {
    /// Read the knobs from the environment, matching the names `dev-run.sh` and
    /// the container entrypoint already use. Defaults mirror the dev setup.
    pub fn from_env(out_dir: PathBuf) -> Result<Self, RenderError> {
        fn var(name: &str) -> Option<String> {
            match std::env::var(name) {
                Ok(v) if !v.is_empty() => Some(v),
                _ => None,
            }
        }

        let port_raw = var("ETV_PORT").unwrap_or_else(|| "8409".to_string());
        let port = port_raw
            .parse::<u16>()
            .map_err(|_| RenderError::Port(port_raw))?;

        Ok(Self {
            bind_address: var("ETV_BIND_ADDRESS").unwrap_or_else(|| "0.0.0.0".to_string()),
            port,
            hls_output: var("ETV_HLS_OUTPUT").unwrap_or_else(|| "tmp/hls".to_string()),
            out_dir,
        })
    }
}

/// What a successful render produced, for the caller to log.
#[derive(Debug, Clone)]
pub struct Rendered {
    pub lineup_path: PathBuf,
    pub channels: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("failed to load configuration: {0}")]
    Config(String),
    #[error("ETV_PORT must be an integer, got {0:?}")]
    Port(String),
    #[error("no channels resolved from {0}")]
    NoChannels(PathBuf),
    #[error("missing {0}")]
    MissingDefaults(PathBuf),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{0}: expected a JSON object at the top level")]
    NotAnObject(PathBuf),
}

/// Load the station config and render ETV-next's config from its channels.
pub fn render(config_path: &Path, opts: &RenderOptions) -> Result<Rendered, RenderError> {
    let station = config::load(config_path).map_err(|e| RenderError::Config(e.to_string()))?;
    let folders: Vec<PathBuf> = station
        .channels
        .iter()
        .map(|ch| ch.output_folder.clone())
        .collect();
    if folders.is_empty() {
        return Err(RenderError::NoChannels(config_path.to_path_buf()));
    }
    render_folders(&folders, opts)
}

/// Render from an already-resolved list of playout folders, in channel order.
///
/// Split out from [`render`] so the emitted shape can be tested without
/// standing up a whole station config.
pub fn render_folders(folders: &[PathBuf], opts: &RenderOptions) -> Result<Rendered, RenderError> {
    let default_path = opts.out_dir.join("normalization.default.json");
    if !default_path.exists() {
        return Err(RenderError::MissingDefaults(default_path));
    }
    let default_body = read_json_object(&default_path)?;

    let presentation_path = opts.out_dir.join("presentation.json");
    let presentation = if presentation_path.exists() {
        read_json_object(&presentation_path)?
    } else {
        Map::new()
    };

    // Drop any previously generated channel files so a shrunk roster (or the
    // legacy un-numbered channel.json) can't leave orphans behind.
    remove_stale_channel_files(&opts.out_dir)?;

    let mut lineup_channels = Vec::with_capacity(folders.len());
    for (index, folder) in folders.iter().enumerate() {
        let number = index + 1;
        let identity = folder
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        let overrides = presentation.get(&identity).and_then(Value::as_object);
        let display = overrides
            .and_then(|o| o.get("name"))
            .and_then(Value::as_str)
            .unwrap_or(&identity)
            .to_string();

        // Absolute so ETV-next reads exactly where the station writes,
        // regardless of ETV-next's own working directory. A relative folder is
        // resolved against the process CWD — the same base the daemon uses it
        // against — and an absolute one passes through unchanged.
        let playout_folder = std::path::absolute(folder).map_err(|source| RenderError::Io {
            path: folder.clone(),
            source,
        })?;

        let channel_overrides = overrides
            .and_then(|o| o.get("config"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut channel = deep_merge(&default_body, &channel_overrides);

        // The station owns playout.folder — inject it AFTER the merge so a
        // `playout` key in the default body or a presentation override can never
        // clobber the derived folder (it may still carry other playout.* keys).
        let playout = channel
            .entry("playout".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !playout.is_object() {
            *playout = Value::Object(Map::new());
        }
        playout
            .as_object_mut()
            .expect("playout is an object")
            .insert(
                "folder".to_string(),
                Value::String(playout_folder.to_string_lossy().into_owned()),
            );

        let channel_path = opts.out_dir.join(format!("channel{number}.json"));
        write_json(&channel_path, &Value::Object(channel))?;

        lineup_channels.push(serde_json::json!({
            "number": number.to_string(),
            "name": display,
            "config": format!("./channel{number}.json"),
        }));
    }

    let lineup = serde_json::json!({
        "server": {"bind_address": opts.bind_address, "port": opts.port},
        "output": {"folder": opts.hls_output},
        "channels": lineup_channels,
    });
    let lineup_path = opts.out_dir.join("lineup.json");
    write_json(&lineup_path, &lineup)?;

    Ok(Rendered {
        lineup_path,
        channels: folders.len(),
    })
}

/// Recursively merge `override_` onto a copy of `base` (objects only).
fn deep_merge(base: &Map<String, Value>, override_: &Map<String, Value>) -> Map<String, Value> {
    let mut result = base.clone();
    for (key, val) in override_ {
        match (result.get(key), val) {
            (Some(Value::Object(existing)), Value::Object(incoming)) => {
                result.insert(key.clone(), Value::Object(deep_merge(existing, incoming)));
            }
            _ => {
                result.insert(key.clone(), val.clone());
            }
        }
    }
    result
}

fn read_json_object(path: &Path) -> Result<Map<String, Value>, RenderError> {
    let text = fs::read_to_string(path).map_err(|source| RenderError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let value: Value = serde_json::from_str(&text).map_err(|source| RenderError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(RenderError::NotAnObject(path.to_path_buf())),
    }
}

fn write_json(path: &Path, value: &Value) -> Result<(), RenderError> {
    let mut text = serde_json::to_string_pretty(value).map_err(|source| RenderError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    text.push('\n');
    fs::write(path, text).map_err(|source| RenderError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn remove_stale_channel_files(dir: &Path) -> Result<(), RenderError> {
    let entries = fs::read_dir(dir).map_err(|source| RenderError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| RenderError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("channel") && name.ends_with(".json") {
            fs::remove_file(entry.path()).map_err(|source| RenderError::Io {
                path: entry.path(),
                source,
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULTS: &str = r#"{
      "ffmpeg": {"ffmpeg_path": "", "disabled_filters": []},
      "normalization": {"video": {"width": 1280, "height": 720}}
    }"#;

    fn opts(dir: &Path) -> RenderOptions {
        RenderOptions {
            bind_address: "0.0.0.0".to_string(),
            port: 8409,
            hls_output: "tmp/hls".to_string(),
            out_dir: dir.to_path_buf(),
        }
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn emits_a_channel_file_per_folder_in_order() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("normalization.default.json"), DEFAULTS).unwrap();

        let folders = vec![PathBuf::from("out/star-trek"), PathBuf::from("out/diehard")];
        let rendered = render_folders(&folders, &opts(dir.path())).unwrap();
        assert_eq!(rendered.channels, 2);

        let lineup = read(&rendered.lineup_path);
        assert_eq!(lineup["server"]["port"], 8409);
        assert_eq!(lineup["output"]["folder"], "tmp/hls");
        assert_eq!(lineup["channels"][0]["number"], "1");
        assert_eq!(lineup["channels"][0]["name"], "star-trek");
        assert_eq!(lineup["channels"][0]["config"], "./channel1.json");
        assert_eq!(lineup["channels"][1]["name"], "diehard");

        let channel1 = read(&dir.path().join("channel1.json"));
        assert_eq!(channel1["normalization"]["video"]["width"], 1280);
        let folder = channel1["playout"]["folder"].as_str().unwrap();
        assert!(
            Path::new(folder).is_absolute(),
            "playout folder must be absolute, got {folder}"
        );
        assert!(folder.ends_with("out/star-trek"));
    }

    #[test]
    fn presentation_supplies_display_name_and_merges_config() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("normalization.default.json"), DEFAULTS).unwrap();
        fs::write(
            dir.path().join("presentation.json"),
            r#"{"star-trek": {"name": "Star Trek 24/7",
                 "config": {"normalization": {"video": {"width": 1920}}}}}"#,
        )
        .unwrap();

        let folders = vec![PathBuf::from("out/star-trek")];
        let rendered = render_folders(&folders, &opts(dir.path())).unwrap();

        let lineup = read(&rendered.lineup_path);
        assert_eq!(lineup["channels"][0]["name"], "Star Trek 24/7");

        let channel1 = read(&dir.path().join("channel1.json"));
        assert_eq!(channel1["normalization"]["video"]["width"], 1920);
        // Untouched sibling keys survive the merge.
        assert_eq!(channel1["normalization"]["video"]["height"], 720);
        assert_eq!(channel1["ffmpeg"]["ffmpeg_path"], "");
    }

    #[test]
    fn presentation_cannot_clobber_the_derived_playout_folder() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("normalization.default.json"), DEFAULTS).unwrap();
        fs::write(
            dir.path().join("presentation.json"),
            r#"{"star-trek": {"config": {"playout": {"folder": "/wrong", "mode": "shuffle"}}}}"#,
        )
        .unwrap();

        let folders = vec![PathBuf::from("out/star-trek")];
        render_folders(&folders, &opts(dir.path())).unwrap();

        let channel1 = read(&dir.path().join("channel1.json"));
        assert!(
            channel1["playout"]["folder"]
                .as_str()
                .unwrap()
                .ends_with("out/star-trek")
        );
        // Other playout keys from the override still come through.
        assert_eq!(channel1["playout"]["mode"], "shuffle");
    }

    #[test]
    fn a_shrunk_roster_leaves_no_orphan_channel_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("normalization.default.json"), DEFAULTS).unwrap();
        fs::write(dir.path().join("channel7.json"), "{}").unwrap();
        fs::write(dir.path().join("channel.json"), "{}").unwrap();

        let folders = vec![PathBuf::from("out/only")];
        render_folders(&folders, &opts(dir.path())).unwrap();

        assert!(dir.path().join("channel1.json").exists());
        assert!(!dir.path().join("channel7.json").exists());
        assert!(!dir.path().join("channel.json").exists());
    }

    #[test]
    fn missing_defaults_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = render_folders(&[PathBuf::from("out/x")], &opts(dir.path())).unwrap_err();
        assert!(matches!(err, RenderError::MissingDefaults(_)));
    }
}

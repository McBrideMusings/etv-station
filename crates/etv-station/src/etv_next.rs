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

/// Which encoder every channel is built to use, unless `presentation.json`
/// overrides it for one.
///
/// This is deliberately environment-driven rather than a value in the committed
/// `normalization.default.json`: `vaapi_device` names a device node that exists
/// on one machine and not the next, and a checkout without that hardware must
/// not inherit a default that fails. It sits beside `ETV_PORT` and
/// `ETV_HLS_OUTPUT`, which are host facts for the same reason.
#[derive(Debug, Clone, PartialEq)]
pub struct AccelSettings {
    /// ETV-next's `normalization.video.accel` — `vaapi`, `cuda`, `qsv`, and so
    /// on. Never empty; an unset `ETV_ACCEL` produces `None` for the whole
    /// struct rather than an empty string here.
    pub accel: String,
    /// `normalization.video.vaapi_device`. Required by VAAPI, unused otherwise.
    pub vaapi_device: Option<String>,
    /// `normalization.video.vaapi_driver`. Required by VAAPI, unused otherwise.
    pub vaapi_driver: Option<String>,
}

/// The `accel` values ETV-next's channel config will deserialize
/// (`vendor/etv-next/crates/ersatztv-channel/src/config.rs`). Checked here so a
/// typo fails the render with a readable message instead of becoming a channel
/// that quietly encodes in software.
const KNOWN_ACCELS: [&str; 7] = [
    "amf",
    "cuda",
    "qsv",
    "rkmpp",
    "vaapi",
    "videotoolbox",
    "vulkan",
];

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
    /// Station-wide encoder. `None` leaves whatever the defaults file carries,
    /// which ships as `""` — software.
    pub accel: Option<AccelSettings>,
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

        let accel = match var("ETV_ACCEL") {
            None => None,
            Some(accel) => {
                if !KNOWN_ACCELS.contains(&accel.as_str()) {
                    return Err(RenderError::UnknownAccel(accel));
                }
                let vaapi_device = var("ETV_VAAPI_DEVICE");
                let vaapi_driver = var("ETV_VAAPI_DRIVER");
                // ETV-next reverts to software when either is missing, and says
                // so only at DEBUG. Refusing here turns a channel that quietly
                // encodes on the CPU into a container that will not start.
                if accel == "vaapi" && (vaapi_device.is_none() || vaapi_driver.is_none()) {
                    return Err(RenderError::VaapiIncomplete);
                }
                Some(AccelSettings {
                    accel,
                    vaapi_device,
                    vaapi_driver,
                })
            }
        };

        Ok(Self {
            bind_address: var("ETV_BIND_ADDRESS").unwrap_or_else(|| "0.0.0.0".to_string()),
            port,
            hls_output: var("ETV_HLS_OUTPUT").unwrap_or_else(|| "tmp/hls".to_string()),
            out_dir,
            accel,
        })
    }
}

/// What a successful render produced, for the caller to log.
#[derive(Debug, Clone)]
pub struct Rendered {
    pub lineup_path: PathBuf,
    pub channels: usize,
    /// What was published as the tuner identity. Logged on every render: the
    /// whole point of this value is that it never changes between runs, and
    /// when Plex shows a tuner that isn't the expected one, the container log
    /// is the first place anyone looks.
    pub device_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("failed to load configuration: {0}")]
    Config(String),
    #[error("ETV_PORT must be an integer, got {0:?}")]
    Port(String),
    #[error("ETV_ACCEL must be one of {}, got {:?}", KNOWN_ACCELS.join(", "), .0)]
    UnknownAccel(String),
    #[error(
        "ETV_ACCEL=vaapi also needs ETV_VAAPI_DEVICE (e.g. /dev/dri/renderD128) and ETV_VAAPI_DRIVER (e.g. iHD); without both, every channel silently encodes in software"
    )]
    VaapiIncomplete,
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
    // The station config is the only place the tuner identity can come from,
    // and this is the only entry point that has it. The minted id is kept beside
    // that config file — see `resolve_device_id` for why not on the data volume.
    let device_id = resolve_device_id(
        station.station.device_id.as_deref(),
        config_dir(config_path),
    )?;
    render_folders(&folders, opts, &device_id)
}

/// The directory holding the station config, which is where state that must
/// outlive a wipe of the data volume belongs.
fn config_dir(config_path: &Path) -> &Path {
    config_path.parent().unwrap_or(Path::new("."))
}

/// The identity ETV-next reports to Plex, which must be the same string on
/// every run or Plex sees a new tuner and drops the channel mapping.
///
/// Configured value wins outright. Otherwise the id is read from
/// `{station-config-dir}/.device_id`, and minted and stored there on the first
/// run that finds no file.
///
/// It lives beside the station config rather than under `output_base` because
/// this is the one value on the station that cannot be regenerated. In the
/// container `output_base` is `/data/playout`, and everything else on that
/// volume is disposable — playout JSON is rewritten every roll and
/// `deploy/appdata/README.md` calls `catalog.db` "a rebuildable cache, not
/// config" — so clearing `/data` to force a catalog rebuild is a normal thing to
/// do. Doing it with the id stored there would mint a new one, and Plex would
/// silently drop the channel mapping for every channel. The config directory is
/// the mount nobody clears. Legacy ErsatzTV keeps its HDHomeRun UUID as config
/// for the same reason.
pub fn resolve_device_id(
    configured: Option<&str>,
    state_dir: &Path,
) -> Result<String, RenderError> {
    if let Some(id) = configured.map(str::trim).filter(|id| !id.is_empty()) {
        return Ok(id.to_string());
    }

    let path = state_dir.join(".device_id");
    match fs::read_to_string(&path) {
        Ok(stored) if !stored.trim().is_empty() => return Ok(stored.trim().to_string()),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(RenderError::Io {
                path: path.clone(),
                source,
            });
        }
    }

    let minted = uuid::Uuid::new_v4().to_string();
    fs::create_dir_all(state_dir).map_err(|source| RenderError::Io {
        path: state_dir.to_path_buf(),
        source,
    })?;

    // Written through a temp file and a rename so a crash between create and
    // flush cannot leave a zero-length file behind. An empty file reads as "no
    // id" on the next start and mints a different one — which is the same lost
    // channel mapping, arrived at by a slower route.
    let tmp = state_dir.join(".device_id.tmp");
    fs::write(&tmp, &minted).map_err(|source| RenderError::Io {
        path: tmp.clone(),
        source,
    })?;
    fs::rename(&tmp, &path).map_err(|source| RenderError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(minted)
}

/// Render from an already-resolved list of playout folders, in channel order.
///
/// Split out from [`render`] so the emitted shape can be tested without
/// standing up a whole station config.
/// `device_id` is a parameter rather than a field on [`RenderOptions`] so there
/// is no way to render a lineup without having resolved one. As a field it
/// defaulted to empty in [`RenderOptions::from_env`], and any caller pairing
/// that with this function would have emitted a tuner with a blank identity —
/// which nothing here would notice and only Plex would.
pub fn render_folders(
    folders: &[PathBuf],
    opts: &RenderOptions,
    device_id: &str,
) -> Result<Rendered, RenderError> {
    let default_path = opts.out_dir.join("normalization.default.json");
    if !default_path.exists() {
        return Err(RenderError::MissingDefaults(default_path));
    }
    let mut default_body = read_json_object(&default_path)?;

    // Onto the defaults, BEFORE the per-channel merge, so a `presentation.json`
    // override still wins for one channel. This is the station-wide answer, not
    // the last word — the opposite of `playout.folder` below, which the station
    // owns outright and injects afterwards.
    if let Some(accel) = opts.accel.as_ref() {
        apply_accel(&mut default_body, accel);
    }
    let default_body = default_body;

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
        "server": {
            "bind_address": opts.bind_address,
            "port": opts.port,
            "device_id": device_id,
        },
        "output": {"folder": opts.hls_output},
        "channels": lineup_channels,
    });
    let lineup_path = opts.out_dir.join("lineup.json");
    write_json(&lineup_path, &lineup)?;

    Ok(Rendered {
        lineup_path,
        channels: folders.len(),
        device_id: device_id.to_string(),
    })
}

/// Write the station-wide encoder into a channel body's
/// `normalization.video` block.
///
/// The VAAPI keys are removed rather than left behind when the encoder is not
/// VAAPI: a stale `vaapi_device` naming a node that no longer exists is exactly
/// the shape that makes a channel revert to software with nothing above DEBUG
/// to say so.
fn apply_accel(body: &mut Map<String, Value>, accel: &AccelSettings) {
    let video = body
        .entry("normalization".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !video.is_object() {
        *video = Value::Object(Map::new());
    }
    let video = video
        .as_object_mut()
        .expect("normalization is an object")
        .entry("video".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !video.is_object() {
        *video = Value::Object(Map::new());
    }
    let video = video.as_object_mut().expect("video is an object");

    video.insert("accel".to_string(), Value::String(accel.accel.clone()));

    for (key, value) in [
        ("vaapi_device", accel.vaapi_device.as_ref()),
        ("vaapi_driver", accel.vaapi_driver.as_ref()),
    ] {
        match value {
            Some(v) if accel.accel == "vaapi" => {
                video.insert(key.to_string(), Value::String(v.clone()));
            }
            _ => {
                video.remove(key);
            }
        }
    }
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
            accel: None,
        }
    }

    fn opts_with_accel(dir: &Path, accel: AccelSettings) -> RenderOptions {
        RenderOptions {
            accel: Some(accel),
            ..opts(dir)
        }
    }

    fn vaapi() -> AccelSettings {
        AccelSettings {
            accel: "vaapi".to_string(),
            vaapi_device: Some("/dev/dri/renderD128".to_string()),
            vaapi_driver: Some("iHD".to_string()),
        }
    }

    // Plex keys a DVR's whole channel mapping on the device id, so an id that
    // changes between runs costs the user 60 channels of remapping by hand.
    // These three tests are what "stable" means in practice.
    #[test]
    fn a_generated_device_id_is_reused_on_every_later_run() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();

        let first = resolve_device_id(None, base).unwrap();
        let second = resolve_device_id(None, base).unwrap();

        assert_eq!(first, second, "a restart must not mint a new identity");
        assert!(!first.is_empty());
        assert_eq!(
            fs::read_to_string(base.join(".device_id")).unwrap().trim(),
            first,
            "the id must be on the data volume, not just in memory",
        );
    }

    #[test]
    fn a_configured_device_id_wins_and_is_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        // A file is already there from an earlier unconfigured run.
        let generated = resolve_device_id(None, base).unwrap();

        let resolved = resolve_device_id(Some("  authored-by-hand  "), base).unwrap();

        assert_eq!(resolved, "authored-by-hand", "config wins, and is trimmed");
        assert_eq!(
            fs::read_to_string(base.join(".device_id")).unwrap().trim(),
            generated,
            "configuring an id must not overwrite the stored one",
        );
    }

    #[test]
    fn a_blank_configured_device_id_falls_through_to_the_stored_one() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();

        let generated = resolve_device_id(None, base).unwrap();

        // An empty `device_id:` in YAML is a key someone left blank, not a
        // request for a tuner with no name.
        assert_eq!(resolve_device_id(Some("   "), base).unwrap(), generated);
    }

    #[test]
    fn the_rendered_lineup_carries_the_device_id() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("normalization.default.json"), DEFAULTS).unwrap();

        let folders = vec![PathBuf::from("out/star-trek")];
        let rendered = render_folders(&folders, &opts(dir.path()), "test-device-id").unwrap();

        // Read back through ETV-next's own config type, not as loose JSON. The
        // two sides have to agree on this key's name, and a mismatch is exactly
        // the failure nothing here would notice: ETV-next would ignore the
        // unknown key and serve a different id than the station published.
        let text = fs::read_to_string(&rendered.lineup_path).unwrap();
        let parsed: ersatztv::config::LineupConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.server.device_id, "test-device-id");
        assert_eq!(rendered.device_id, "test-device-id");
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn emits_a_channel_file_per_folder_in_order() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("normalization.default.json"), DEFAULTS).unwrap();

        let folders = vec![PathBuf::from("out/star-trek"), PathBuf::from("out/diehard")];
        let rendered = render_folders(&folders, &opts(dir.path()), "test-device-id").unwrap();
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

    /// The whole point of the setting: one value reaches every channel, and
    /// every channel it reaches has all three keys VAAPI needs. Missing any one
    /// of them is what makes ETV-next revert to software with nothing above
    /// DEBUG to say so.
    #[test]
    fn the_station_wide_encoder_reaches_every_channel() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("normalization.default.json"), DEFAULTS).unwrap();

        let folders = vec![PathBuf::from("out/star-trek"), PathBuf::from("out/diehard")];
        render_folders(&folders, &opts_with_accel(dir.path(), vaapi()), "id").unwrap();

        for name in ["channel1.json", "channel2.json"] {
            let video = read(&dir.path().join(name))["normalization"]["video"].clone();
            assert_eq!(video["accel"], "vaapi", "{name}");
            assert_eq!(video["vaapi_device"], "/dev/dri/renderD128", "{name}");
            assert_eq!(video["vaapi_driver"], "iHD", "{name}");
        }
    }

    /// A non-VAAPI encoder must not leave VAAPI keys behind. A `vaapi_device`
    /// naming a node that is not there is the same silent-software failure.
    #[test]
    fn switching_to_cuda_drops_the_vaapi_keys() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("normalization.default.json"),
            r#"{"normalization": {"video": {"accel": "vaapi",
                 "vaapi_device": "/dev/dri/renderD128", "vaapi_driver": "iHD"}}}"#,
        )
        .unwrap();

        let cuda = AccelSettings {
            accel: "cuda".to_string(),
            vaapi_device: None,
            vaapi_driver: None,
        };
        let folders = vec![PathBuf::from("out/star-trek")];
        render_folders(&folders, &opts_with_accel(dir.path(), cuda), "id").unwrap();

        let video = read(&dir.path().join("channel1.json"))["normalization"]["video"].clone();
        assert_eq!(video["accel"], "cuda");
        assert!(video.get("vaapi_device").is_none(), "{video}");
        assert!(video.get("vaapi_driver").is_none(), "{video}");
    }

    /// The station-wide value is a default, not the last word: it goes on
    /// before the per-channel merge so `presentation.json` can still put one
    /// channel on a different card. This is the hook #260's profiles hang off.
    #[test]
    fn a_presentation_override_still_beats_the_station_wide_encoder() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("normalization.default.json"), DEFAULTS).unwrap();
        fs::write(
            dir.path().join("presentation.json"),
            r#"{"diehard": {"config": {"normalization": {"video": {"accel": "cuda"}}}}}"#,
        )
        .unwrap();

        let folders = vec![PathBuf::from("out/star-trek"), PathBuf::from("out/diehard")];
        render_folders(&folders, &opts_with_accel(dir.path(), vaapi()), "id").unwrap();

        assert_eq!(
            read(&dir.path().join("channel1.json"))["normalization"]["video"]["accel"],
            "vaapi",
            "a channel that says nothing takes the station-wide encoder"
        );
        assert_eq!(
            read(&dir.path().join("channel2.json"))["normalization"]["video"]["accel"],
            "cuda",
            "a channel that names its own encoder keeps it"
        );
    }

    /// Leaving `ETV_ACCEL` unset must not touch the defaults file's own value.
    #[test]
    fn no_configured_encoder_leaves_the_defaults_alone() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("normalization.default.json"),
            r#"{"normalization": {"video": {"accel": "", "width": 1280}}}"#,
        )
        .unwrap();

        let folders = vec![PathBuf::from("out/star-trek")];
        render_folders(&folders, &opts(dir.path()), "id").unwrap();

        let video = read(&dir.path().join("channel1.json"))["normalization"]["video"].clone();
        assert_eq!(video["accel"], "");
        assert_eq!(video["width"], 1280);
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
        let rendered = render_folders(&folders, &opts(dir.path()), "test-device-id").unwrap();

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
        render_folders(&folders, &opts(dir.path()), "test-device-id").unwrap();

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
        render_folders(&folders, &opts(dir.path()), "test-device-id").unwrap();

        assert!(dir.path().join("channel1.json").exists());
        assert!(!dir.path().join("channel7.json").exists());
        assert!(!dir.path().join("channel.json").exists());
    }

    #[test]
    fn missing_defaults_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = render_folders(
            &[PathBuf::from("out/x")],
            &opts(dir.path()),
            "test-device-id",
        )
        .unwrap_err();
        assert!(matches!(err, RenderError::MissingDefaults(_)));
    }
}

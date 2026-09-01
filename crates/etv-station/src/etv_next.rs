//! Render ETV-next's `lineup.json` + `channel{N}.json` from the station config.
//!
//! This is the single-source-of-truth generator for the ETV-next side of the
//! shared-folder contract. Instead of hand-authoring each channel's
//! `playout.folder` to match what the station writes, we DERIVE it: the station
//! config loader resolves each channel's output folder, and we emit ETV-next
//! config that reads exactly those folders.
//!
//! What the station owns (derived here): the channel roster and each
//! `playout.folder`. Each channel's number (#263) is not derived at all — it
//! is the config's own declared `number:`, carried through verbatim. It also
//! now owns the
//! `normalization` / `ffmpeg` playback block ETV-next runs with — typed fields
//! on the station config (`StationConfig::ffmpeg`, `StationConfig::normalization`),
//! deserialized through ETV-next's own `ersatztv_channel::config` types so an
//! upstream field rename is a compile error here instead of a station.yaml key
//! ETV-next silently ignores. The lineup display name, by the same logic, IS
//! station config — each channel's own `display_name` (#158), falling back to
//! its identity — never a second file.
//!
//! Until #158, the display name and a per-channel playback override both came
//! from an optional `presentation.json` keyed by channel identity. That file is
//! no longer read at all — no precedence rule, no dual support — because it
//! lived outside the channel config and outside git, so a channel's guide-facing
//! name could only be changed by hand-editing a file nobody would think to look
//! in. The per-channel playback override `presentation.json` also carried has no
//! replacement here; a channel that needs one waits on whatever issue picks that
//! back up.
//!
//! It lives in the daemon binary rather than a helper script because the
//! runtime image runs both processes and has no interpreter: the container
//! entrypoint renders ETV-next's config from the mounted station config at
//! start, so the two can never disagree.

use std::fs;
use std::path::{Path, PathBuf};

use ersatztv_channel::config::{
    FfmpegConfig, HardwareAccel, NormalizationConfig, VaapiDriver, VideoNormalizationConfig,
};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::config;
use ersatztv_playout::playout::OverlaySpec as PlayoutOverlaySpec;

/// Which encoder every channel is built to use — station-wide; there is no
/// remaining per-channel override since `presentation.json` was removed
/// (#158 decision #5).
///
/// This is deliberately environment-driven rather than a value in the station
/// config's `normalization:` block: `vaapi_device` names a device node that
/// exists on one machine and not the next, and a checkout without that
/// hardware must not inherit a default that fails. It sits beside `ETV_PORT`
/// and `ETV_HLS_OUTPUT`, which are host facts for the same reason.
#[derive(Debug, Clone)]
pub struct AccelSettings {
    /// ETV-next's `normalization.video.accel`.
    pub accel: HardwareAccel,
    /// `normalization.video.vaapi_device`. Required by VAAPI, unused otherwise.
    pub vaapi_device: Option<PathBuf>,
    /// `normalization.video.vaapi_driver`. Required by VAAPI, unused otherwise.
    pub vaapi_driver: Option<VaapiDriver>,
}

/// Parse an env var's raw string through ETV-next's own `HardwareAccel` type,
/// the same deserializer path a `normalization.video.accel` key in config goes
/// through — so a typo fails the render with a readable message instead of
/// becoming a channel that quietly encodes in software, with no second,
/// hand-written list of valid values to drift from that type.
fn parse_hardware_accel(raw: &str) -> Result<HardwareAccel, RenderError> {
    HardwareAccel::deserialize(
        serde::de::value::StrDeserializer::<serde::de::value::Error>::new(raw),
    )
    .map_err(|_| RenderError::UnknownAccel(raw.to_string()))
}

/// Parse `ETV_VAAPI_DRIVER` through ETV-next's own `VaapiDriver` type, the same
/// deserializer path `normalization.video.vaapi_driver` goes through.
fn parse_vaapi_driver(raw: &str) -> Result<VaapiDriver, RenderError> {
    VaapiDriver::deserialize(serde::de::value::StrDeserializer::<serde::de::value::Error>::new(raw))
        .map_err(|_| RenderError::UnknownVaapiDriver(raw.to_string()))
}

/// Where the generated files go and what the lineup server is told to do.
#[derive(Debug, Clone)]
pub struct RenderOptions {
    /// `server.bind_address` in the emitted lineup.
    pub bind_address: String,
    /// `server.port` in the emitted lineup.
    pub port: u16,
    /// `output.folder` in the emitted lineup — ETV-next's HLS working dir.
    pub hls_output: String,
    /// Directory receiving the generated files.
    pub out_dir: PathBuf,
    /// Station-wide encoder. `None` leaves whatever the station config's
    /// `normalization.video.accel` carries.
    pub accel: Option<AccelSettings>,
    /// `artwork.folder` in the emitted lineup — the directory the station
    /// caches Plex posters to and ETV-next serves at `/artwork` (#187).
    /// `None` (the default) omits the `artwork` block entirely, which mounts
    /// nothing at `/artwork` and matches the station never publishing an
    /// `<icon>` for anything it hasn't cached. Same env var etv-station's own
    /// ingest reads (`ETV_STATION_ARTWORK_CACHE`) — one directory, one source
    /// of truth, read independently by each process the way `ETV_HLS_OUTPUT`
    /// already is.
    pub artwork_dir: Option<String>,
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
            Some(raw) => {
                let accel = parse_hardware_accel(&raw)?;
                let vaapi_device = var("ETV_VAAPI_DEVICE").map(PathBuf::from);
                let vaapi_driver = var("ETV_VAAPI_DRIVER")
                    .map(|raw| parse_vaapi_driver(&raw))
                    .transpose()?;
                // ETV-next reverts to software when either is missing, and says
                // so only at DEBUG. Refusing here turns a channel that quietly
                // encodes on the CPU into a container that will not start.
                if matches!(accel, HardwareAccel::Vaapi)
                    && (vaapi_device.is_none() || vaapi_driver.is_none())
                {
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
            artwork_dir: var("ETV_STATION_ARTWORK_CACHE"),
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
    #[error(
        "ETV_ACCEL must be a hardware accel ETV-next recognizes (e.g. vaapi, cuda, qsv, amf, rkmpp, videotoolbox, vulkan), got {0:?}"
    )]
    UnknownAccel(String),
    #[error(
        "ETV_VAAPI_DRIVER must be a VAAPI driver ETV-next recognizes (e.g. iHD, i965, RadeonSI), got {0:?}"
    )]
    UnknownVaapiDriver(String),
    #[error(
        "ETV_ACCEL=vaapi also needs ETV_VAAPI_DEVICE (e.g. /dev/dri/renderD128) and ETV_VAAPI_DRIVER (e.g. iHD); without both, every channel silently encodes in software"
    )]
    VaapiIncomplete,
    #[error("no channels resolved from {0}")]
    NoChannels(PathBuf),
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
}

/// One channel's inputs to the ETV-next config render.
///
/// A struct rather than parallel slices because these three travel together and
/// are indexed together — the length assert that guarded two of them was the
/// seam, and a third and fourth array would only widen it.
pub struct ChannelRender {
    /// The channel's declared dial number (#263) — no longer derived from
    /// this channel's position in `channels`. Need not be contiguous with any
    /// other channel's number.
    pub number: i64,
    /// Playout folder the station writes and ETV-next reads.
    pub folder: PathBuf,
    /// The channel's own `display_name` (#158) — never a second file. `None`
    /// falls back to the identity resolved from the folder name.
    pub display_name: Option<String>,
    /// The channel's live overlay, or `None` for a channel that has none. This
    /// is the only record of that decision: it used to be copied onto every
    /// playout item, where an item written before the channel had an overlay
    /// kept "no overlay" forever.
    pub overlay: Option<PlayoutOverlaySpec>,
}

/// Load the station config and render ETV-next's config from its channels.
pub fn render(config_path: &Path, opts: &RenderOptions) -> Result<Rendered, RenderError> {
    let station = config::load(config_path).map_err(|e| RenderError::Config(e.to_string()))?;
    if station.channels.is_empty() {
        return Err(RenderError::NoChannels(config_path.to_path_buf()));
    }
    let channels: Vec<ChannelRender> = station
        .channels
        .iter()
        .map(|ch| ChannelRender {
            number: ch.config.number,
            folder: ch.output_folder.clone(),
            display_name: ch.config.display_name.clone(),
            overlay: crate::daemon::load_overlay_playout_spec(ch),
        })
        .collect();
    // The station config is the only place the tuner identity can come from,
    // and this is the only entry point that has it. The minted id is kept beside
    // that config file — see `resolve_device_id` for why not on the data volume.
    let device_id = resolve_device_id(
        station.station.device_id.as_deref(),
        config_dir(config_path),
    )?;
    render_channels(
        &channels,
        &station.station.ffmpeg,
        &station.station.normalization,
        opts,
        &device_id,
    )
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

/// Render from an already-resolved list of channels, in channel order.
///
/// Split out from [`render`] so the emitted shape can be tested without
/// standing up a whole station config.
/// `device_id` is a parameter rather than a field on [`RenderOptions`] so there
/// is no way to render a lineup without having resolved one. As a field it
/// defaulted to empty in [`RenderOptions::from_env`], and any caller pairing
/// that with this function would have emitted a tuner with a blank identity —
/// which nothing here would notice and only Plex would.
pub fn render_channels(
    channels: &[ChannelRender],
    ffmpeg: &FfmpegConfig,
    normalization: &NormalizationConfig,
    opts: &RenderOptions,
    device_id: &str,
) -> Result<Rendered, RenderError> {
    let mut normalization = normalization.clone();
    if let Some(accel) = opts.accel.as_ref() {
        apply_accel(&mut normalization.video, accel);
    }

    // Serialized once, then cloned per channel below — same shape the old
    // `normalization.default.json` body had, built from the typed config
    // instead of read off disk.
    let default_body = match serde_json::json!({"ffmpeg": ffmpeg, "normalization": normalization}) {
        Value::Object(map) => map,
        _ => unreachable!("json! with an object literal always produces an object"),
    };

    // Drop any previously generated channel files so a shrunk roster (or the
    // legacy un-numbered channel.json) can't leave orphans behind.
    remove_stale_channel_files(&opts.out_dir)?;

    let mut lineup_channels = Vec::with_capacity(channels.len());
    for channel_render in channels {
        let number = channel_render.number;
        let folder = &channel_render.folder;
        let identity = folder
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        // #158: the display name is the channel's own `display_name`, read
        // from its YAML by `render` above — never a second file. Falls back
        // to the identity, same as before this field existed.
        let display = channel_render.display_name.clone().unwrap_or(identity);

        // Absolute so ETV-next reads exactly where the station writes,
        // regardless of ETV-next's own working directory. A relative folder is
        // resolved against the process CWD — the same base the daemon uses it
        // against — and an absolute one passes through unchanged.
        let playout_folder = std::path::absolute(folder).map_err(|source| RenderError::Io {
            path: folder.clone(),
            source,
        })?;

        let mut channel = default_body.clone();

        // The station owns playout.folder — inject it AFTER the merge so a
        // `playout` key in the default body can never clobber the derived
        // folder (it may still carry other playout.* keys).
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

        // The station owns `overlay` for the same reason it owns
        // `playout.folder`: both name paths the station itself creates. Injected
        // after the merge so a stray `overlay` key in the defaults body cannot
        // point a channel at another channel's fifo. A channel with no overlay
        // writes no key, which ETV-next reads as "no overlay on this channel".
        if let Some(spec) = channel_render.overlay.as_ref() {
            channel.insert(
                "overlay".to_string(),
                serde_json::to_value(spec).map_err(|source| RenderError::Json {
                    path: opts.out_dir.join(format!("channel{number}.json")),
                    source,
                })?,
            );
        } else {
            channel.remove("overlay");
        }

        let channel_path = opts.out_dir.join(format!("channel{number}.json"));
        write_json(&channel_path, &Value::Object(channel))?;

        lineup_channels.push(serde_json::json!({
            "number": number.to_string(),
            "name": display,
            "config": format!("./channel{number}.json"),
        }));
    }

    let mut lineup = serde_json::json!({
        "server": {
            "bind_address": opts.bind_address,
            "port": opts.port,
            "device_id": device_id,
        },
        "output": {"folder": opts.hls_output},
        "channels": lineup_channels,
    });
    // Mounted at ETV-next's `/artwork` (#187) only when the station has
    // artwork caching turned on — unset leaves the lineup exactly as it was
    // before this existed, and ETV-next mounts nothing there.
    if let Some(dir) = &opts.artwork_dir {
        lineup["artwork"] = serde_json::json!({"folder": dir});
    }
    let lineup_path = opts.out_dir.join("lineup.json");
    write_json(&lineup_path, &lineup)?;

    Ok(Rendered {
        lineup_path,
        channels: channels.len(),
        device_id: device_id.to_string(),
    })
}

/// Write the station-wide encoder into the normalization body's `video`
/// block.
///
/// The VAAPI fields are cleared rather than left behind when the encoder is
/// not VAAPI: a stale `vaapi_device` naming a node that no longer exists is
/// exactly the shape that makes a channel revert to software with nothing
/// above DEBUG to say so.
fn apply_accel(video: &mut VideoNormalizationConfig, accel: &AccelSettings) {
    video.accel = Some(accel.accel.clone());

    if matches!(accel.accel, HardwareAccel::Vaapi) {
        video.vaapi_device = accel.vaapi_device.clone();
        video.vaapi_driver = accel.vaapi_driver.clone();
    } else {
        video.vaapi_device = None;
        video.vaapi_driver = None;
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

    /// Every field defaults, matching the old `{"ffmpeg_path": "", "disabled_filters": []}`.
    fn ffmpeg() -> FfmpegConfig {
        serde_json::from_value(serde_json::json!({})).unwrap()
    }

    /// Matches the old `{"video": {"width": 1280, "height": 720}}` — no accel
    /// configured, so `video.accel` is `None`.
    fn normalization() -> NormalizationConfig {
        serde_json::from_value(serde_json::json!({
            "audio": {},
            "video": {"width": 1280, "height": 720},
        }))
        .unwrap()
    }

    fn opts(dir: &Path) -> RenderOptions {
        RenderOptions {
            bind_address: "0.0.0.0".to_string(),
            port: 8409,
            hls_output: "tmp/hls".to_string(),
            out_dir: dir.to_path_buf(),
            accel: None,
            artwork_dir: None,
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
            accel: HardwareAccel::Vaapi,
            vaapi_device: Some(PathBuf::from("/dev/dri/renderD128")),
            vaapi_driver: Some(VaapiDriver::Ihd),
        }
    }

    /// Render with the standard `ffmpeg()`/`normalization()` fixtures — the
    /// common case for tests that only care about `opts` and `channels`.
    fn render_with_defaults(
        channels: &[ChannelRender],
        opts: &RenderOptions,
        device_id: &str,
    ) -> Result<Rendered, RenderError> {
        render_channels(channels, &ffmpeg(), &normalization(), opts, device_id)
    }

    /// `ffmpeg.ffmpeg_path` must reach EVERY rendered channel, unaltered.
    ///
    /// In production that path is the instrumented wrapper
    /// (`/usr/local/bin/ffmpeg-probe.sh`, set in `deploy/appdata/station.yaml`),
    /// and it is what records the argv proving a channel encodes on the GPU
    /// instead of silently on the CPU (#258) — plus the `-progress` stream that
    /// makes a freeze diagnosable at all. A render that dropped it, or wrote it
    /// to only the first channel, would take the instrumentation off every
    /// channel with nothing anywhere saying so: the station keeps running and
    /// the logs look identical. That is exactly how it went stale unnoticed for
    /// six days before it was baked into the image, so it gets a test.
    #[test]
    fn ffmpeg_path_reaches_every_channel() {
        let dir = tempfile::tempdir().unwrap();
        let probe = "/usr/local/bin/ffmpeg-probe.sh";
        let ffmpeg: FfmpegConfig =
            serde_json::from_value(serde_json::json!({"ffmpeg_path": probe})).unwrap();

        let folders: Vec<PathBuf> = (1..=3).map(|n| dir.path().join(format!("ch{n}"))).collect();
        render_channels(
            &chans(&folders),
            &ffmpeg,
            &normalization(),
            &opts(dir.path()),
            "device",
        )
        .unwrap();

        for number in 1..=3 {
            let path = dir.path().join(format!("channel{number}.json"));
            let body: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(
                body["ffmpeg"]["ffmpeg_path"], probe,
                "channel{number}.json lost the probe path"
            );
        }
    }

    /// The opposite direction: an empty `ffmpeg_path` must stay absent rather
    /// than becoming the literal string "", which ETV-next would try to exec.
    #[test]
    fn empty_ffmpeg_path_renders_as_null() {
        let dir = tempfile::tempdir().unwrap();
        let ffmpeg: FfmpegConfig =
            serde_json::from_value(serde_json::json!({"ffmpeg_path": ""})).unwrap();

        let folders = vec![dir.path().join("ch1")];
        render_channels(
            &chans(&folders),
            &ffmpeg,
            &normalization(),
            &opts(dir.path()),
            "device",
        )
        .unwrap();

        let body: Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join("channel1.json")).unwrap())
                .unwrap();
        assert!(
            body["ffmpeg"]["ffmpeg_path"].is_null(),
            "empty ffmpeg_path should render as null, got {}",
            body["ffmpeg"]["ffmpeg_path"]
        );
    }

    /// No `display_name:` authored on any channel and no overlay — every one
    /// falls back to its identity, same as before those fields existed.
    fn chans(folders: &[PathBuf]) -> Vec<ChannelRender> {
        folders
            .iter()
            .enumerate()
            .map(|(i, folder)| ChannelRender {
                number: (i + 1) as i64,
                folder: folder.clone(),
                display_name: None,
                overlay: None,
            })
            .collect()
    }

    /// Channels carrying authored `display_name:` values, in the same order.
    fn named(folders: &[PathBuf], names: Vec<Option<String>>) -> Vec<ChannelRender> {
        folders
            .iter()
            .zip(names)
            .enumerate()
            .map(|(i, (folder, display_name))| ChannelRender {
                number: (i + 1) as i64,
                folder: folder.clone(),
                display_name,
                overlay: None,
            })
            .collect()
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

        let folders = vec![PathBuf::from("out/star-trek")];
        let rendered =
            render_with_defaults(&chans(&folders), &opts(dir.path()), "test-device-id").unwrap();

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

        let folders = vec![PathBuf::from("out/star-trek"), PathBuf::from("out/diehard")];
        let rendered =
            render_with_defaults(&chans(&folders), &opts(dir.path()), "test-device-id").unwrap();
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

    // #187: unset by default (artwork caching is off), so the lineup carries
    // no `artwork` key at all and ETV-next mounts nothing at `/artwork`.
    #[test]
    fn a_configured_artwork_dir_is_emitted_and_an_unset_one_is_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let folders = vec![PathBuf::from("out/star-trek")];

        let without = opts(dir.path());
        let rendered = render_with_defaults(&chans(&folders), &without, "test-device-id").unwrap();
        let lineup = read(&rendered.lineup_path);
        assert!(lineup.get("artwork").is_none(), "{lineup}");

        let with = RenderOptions {
            artwork_dir: Some("/data/artwork".to_string()),
            ..opts(dir.path())
        };
        let rendered = render_with_defaults(&chans(&folders), &with, "test-device-id").unwrap();
        let lineup = read(&rendered.lineup_path);
        assert_eq!(lineup["artwork"]["folder"], "/data/artwork");
    }

    /// The whole point of the setting: one value reaches every channel, and
    /// every channel it reaches has all three keys VAAPI needs. Missing any one
    /// of them is what makes ETV-next revert to software with nothing above
    /// DEBUG to say so.
    #[test]
    fn the_station_wide_encoder_reaches_every_channel() {
        let dir = tempfile::tempdir().unwrap();

        let folders = vec![PathBuf::from("out/star-trek"), PathBuf::from("out/diehard")];
        render_with_defaults(
            &chans(&folders),
            &opts_with_accel(dir.path(), vaapi()),
            "id",
        )
        .unwrap();

        for name in ["channel1.json", "channel2.json"] {
            let video = read(&dir.path().join(name))["normalization"]["video"].clone();
            assert_eq!(video["accel"], "vaapi", "{name}");
            assert_eq!(video["vaapi_device"], "/dev/dri/renderD128", "{name}");
            assert_eq!(video["vaapi_driver"], "ihd", "{name}");
        }
    }

    /// A non-VAAPI encoder must not leave VAAPI keys behind. A `vaapi_device`
    /// naming a node that is not there is the same silent-software failure.
    #[test]
    fn switching_to_cuda_drops_the_vaapi_keys() {
        let dir = tempfile::tempdir().unwrap();

        // Station-configured normalization already carries VAAPI fields, as if
        // an earlier run had set them.
        let normalization: NormalizationConfig = serde_json::from_value(serde_json::json!({
            "audio": {},
            "video": {
                "accel": "vaapi",
                "vaapi_device": "/dev/dri/renderD128",
                "vaapi_driver": "ihd",
            },
        }))
        .unwrap();

        let cuda = AccelSettings {
            accel: HardwareAccel::Cuda,
            vaapi_device: None,
            vaapi_driver: None,
        };
        let folders = vec![PathBuf::from("out/star-trek")];
        render_channels(
            &chans(&folders),
            &ffmpeg(),
            &normalization,
            &opts_with_accel(dir.path(), cuda),
            "id",
        )
        .unwrap();

        let video = read(&dir.path().join("channel1.json"))["normalization"]["video"].clone();
        assert_eq!(video["accel"], "cuda");
        assert!(video["vaapi_device"].is_null(), "{video}");
        assert!(video["vaapi_driver"].is_null(), "{video}");
    }

    /// Leaving `ETV_ACCEL` unset must not touch the station-configured value.
    #[test]
    fn no_configured_encoder_leaves_the_defaults_alone() {
        let dir = tempfile::tempdir().unwrap();

        let folders = vec![PathBuf::from("out/star-trek")];
        render_with_defaults(&chans(&folders), &opts(dir.path()), "id").unwrap();

        let video = read(&dir.path().join("channel1.json"))["normalization"]["video"].clone();
        assert!(video["accel"].is_null(), "{video}");
        assert_eq!(video["width"], 1280);
    }

    /// #158: the lineup display name comes from the channel's own
    /// `display_name` (already resolved by `render` before this function
    /// sees it), never a second file.
    #[test]
    fn a_channels_display_name_reaches_the_lineup() {
        let dir = tempfile::tempdir().unwrap();

        let folders = vec![PathBuf::from("out/star-trek"), PathBuf::from("out/diehard")];
        let names = vec![Some("Star Trek 24/7".to_string()), None];
        let rendered =
            render_with_defaults(&named(&folders, names), &opts(dir.path()), "test-device-id")
                .unwrap();

        let lineup = read(&rendered.lineup_path);
        assert_eq!(lineup["channels"][0]["name"], "Star Trek 24/7");
        // A channel with no `display_name:` falls back to its identity.
        assert_eq!(lineup["channels"][1]["name"], "diehard");
    }

    /// #158 decision #5: no dual support. A `presentation.json` left behind
    /// from before this change must not be read at all — not for the name,
    /// not for a config override — even though it still exists on disk.
    #[test]
    fn a_leftover_presentation_json_is_never_read() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("presentation.json"),
            r#"{"star-trek": {"name": "Old Name From A File",
                 "config": {"normalization": {"video": {"width": 1920}}}}}"#,
        )
        .unwrap();

        let folders = vec![PathBuf::from("out/star-trek")];
        let rendered =
            render_with_defaults(&chans(&folders), &opts(dir.path()), "test-device-id").unwrap();

        let lineup = read(&rendered.lineup_path);
        assert_eq!(
            lineup["channels"][0]["name"], "star-trek",
            "the identity, not presentation.json's name"
        );
        let channel1 = read(&dir.path().join("channel1.json"));
        assert_eq!(
            channel1["normalization"]["video"]["width"], 1280,
            "the default body, not presentation.json's config override"
        );
    }

    #[test]
    fn a_shrunk_roster_leaves_no_orphan_channel_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("channel7.json"), "{}").unwrap();
        fs::write(dir.path().join("channel.json"), "{}").unwrap();

        let folders = vec![PathBuf::from("out/only")];
        render_with_defaults(&chans(&folders), &opts(dir.path()), "test-device-id").unwrap();

        assert!(dir.path().join("channel1.json").exists());
        assert!(!dir.path().join("channel7.json").exists());
        assert!(!dir.path().join("channel.json").exists());
    }

    /// The overlay decision is recorded once, on the channel — so a channel
    /// that has one always has one, no matter when its playout items were
    /// generated. This is the whole point of moving it off the item: 1371 items
    /// across 52 deployed channels had been written before their channel had an
    /// overlay, and every one of them stayed watermark-free forever.
    #[test]
    fn a_channels_overlay_lands_in_its_channel_json() {
        let dir = tempfile::tempdir().unwrap();
        let channels = vec![
            ChannelRender {
                number: 1,
                folder: PathBuf::from("out/hbo"),
                display_name: None,
                overlay: Some(PlayoutOverlaySpec {
                    fifo_path: "/data/playout/085-hbo/overlay.fifo".into(),
                    pixel_format: "rgba".into(),
                    width: 1280,
                    height: 720,
                    framerate: 30,
                    x: 0,
                    y: 0,
                }),
            },
            ChannelRender {
                number: 2,
                folder: PathBuf::from("out/plain"),
                display_name: None,
                overlay: None,
            },
        ];
        render_with_defaults(&channels, &opts(dir.path()), "test-device-id").unwrap();

        let with = read(&dir.path().join("channel1.json"));
        assert_eq!(
            with["overlay"]["fifo_path"], "/data/playout/085-hbo/overlay.fifo",
            "a channel with an overlay must publish its fifo to ETV-next"
        );
        assert_eq!(with["overlay"]["width"], 1280);
        assert_eq!(with["overlay"]["framerate"], 30);

        let without = read(&dir.path().join("channel2.json"));
        assert!(
            without.get("overlay").is_none(),
            "a channel with no overlay must write no overlay key, got {:?}",
            without.get("overlay")
        );
    }

    // `a_defaults_overlay_never_leaks_into_a_channel` and `missing_defaults_is_an_error`
    // were deleted here: both tested the untyped-JSON defaults file this change
    // removes. A stray `overlay` in the shared body is no longer expressible —
    // `NormalizationConfig`/`FfmpegConfig` have no such field — so the seam the
    // first test guarded is closed, not merely tested; and there is no longer a
    // defaults file that can be missing, so the second test's premise is gone.
}

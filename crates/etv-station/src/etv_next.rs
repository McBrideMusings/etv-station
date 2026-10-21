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

use crate::config::{self, DroppedChannel};
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

/// `idle_grace_seconds` written when pre-warm is on: a worker that outlives
/// any gap between viewers (ten years), so it never idles out and the first
/// tune finds it running. Overrides a configured `idle_grace_seconds`.
const PREWARM_IDLE_GRACE_SECONDS: u64 = 315_360_000;

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
    /// `idle_grace_seconds` written into every `channelN.json`. Comes from the
    /// station config, not the environment, so [`RenderOptions::from_env`]
    /// leaves it `None` and [`render_with`] fills it in.
    pub idle_grace_seconds: Option<u64>,
    /// `server.prewarm` in the emitted lineup. Comes from the station config,
    /// like `idle_grace_seconds`.
    pub prewarm: bool,
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
            idle_grace_seconds: None,
            prewarm: false,
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
    /// Channel configs the station found and left out of this lineup. The
    /// render still succeeds — one bad channel costs one channel — so a caller
    /// that must refuse on any (`--check-config`) reads it here.
    pub dropped: Vec<DroppedChannel>,
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
    #[error("no channels resolved from {} ({} channel config(s) dropped)", .path.display(), .dropped.len())]
    NoChannels {
        path: PathBuf,
        /// Every channel config that was found and dropped, so a caller can
        /// name each one rather than report an empty lineup with no cause.
        dropped: Vec<DroppedChannel>,
    },
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
    /// falls back to the channel's declared `name`.
    pub display_name: Option<String>,
    /// The channel's live overlay, or `None` for a channel that has none. This
    /// is the only record of that decision: it used to be copied onto every
    /// playout item, where an item written before the channel had an overlay
    /// kept "no overlay" forever.
    pub overlay: Option<PlayoutOverlaySpec>,
    /// The channel's own `subtitle.default_on` (#290). `None` leaves whatever
    /// the station's `normalization.subtitle.default_on` says.
    pub subtitle_default_on: Option<bool>,
    /// List bumper items as their own guide rows (`guide.show_bumpers`),
    /// already resolved: the channel's own value, else the station's.
    pub show_bumpers: bool,
    /// The channel's `logo:` image (#342), already resolved against its
    /// config file's directory. `None` publishes no logo.
    pub logo: Option<PathBuf>,
    /// The channel's own `ffmpeg_profile` (#260), already looked up. Written
    /// over the station's picture in this channel's `normalization.video`.
    /// `None` leaves the station's picture (which `render_with` has already
    /// set from `active_profile`).
    pub profile: Option<crate::config::FfmpegProfile>,
}

/// Where [`render_with`] gets the tuner identity it publishes.
pub enum DeviceIdSource<'a> {
    /// The configured id, else the one stored beside the station config,
    /// minting and storing one when neither exists.
    Resolve,
    /// This value, read and written nowhere. For a render whose output is
    /// thrown away (`--check-config`), which must neither mint a real identity
    /// nor need write access to the config directory.
    Fixed(&'a str),
}

/// Load the station config and render ETV-next's config from its channels.
pub fn render(config_path: &Path, opts: &RenderOptions) -> Result<Rendered, RenderError> {
    render_with(config_path, opts, DeviceIdSource::Resolve)
}

/// [`render`], with the tuner identity's source chosen by the caller.
pub fn render_with(
    config_path: &Path,
    opts: &RenderOptions,
    device_id: DeviceIdSource<'_>,
) -> Result<Rendered, RenderError> {
    let station = config::load(config_path).map_err(|e| RenderError::Config(e.to_string()))?;
    if station.channels.is_empty() {
        return Err(RenderError::NoChannels {
            path: config_path.to_path_buf(),
            dropped: station.dropped,
        });
    }
    let channels: Vec<ChannelRender> = station
        .channels
        .iter()
        .map(|ch| ChannelRender {
            number: ch.config.number,
            folder: ch.output_folder.clone(),
            display_name: ch.config.display_name.clone(),
            overlay: crate::daemon::load_overlay_playout_spec(ch),
            subtitle_default_on: ch.config.subtitle.as_ref().and_then(|s| s.default_on),
            show_bumpers: resolve_show_bumpers(ch.config.guide.as_ref(), &station.station.guide),
            logo: ch
                .config
                .logo
                .as_ref()
                .map(|l| config_dir(&ch.config_path).join(l)),
            // The channel's own profile replaces the station's `active_profile`
            // outright; a field the channel's profile omits keeps the
            // station's `normalization.video`, not the active profile's.
            profile: ch
                .config
                .ffmpeg_profile
                .as_ref()
                .or(station.station.active_profile.as_ref())
                .and_then(|key| station.station.ffmpeg_profiles.get(key))
                .cloned(),
        })
        .collect();
    // The station config is the only place the tuner identity can come from,
    // and this is the only entry point that has it. The minted id is kept beside
    // that config file — see `resolve_device_id` for why not on the data volume.
    let device_id = match device_id {
        DeviceIdSource::Resolve => resolve_device_id(
            station.station.device_id.as_deref(),
            config_dir(config_path),
        )?,
        DeviceIdSource::Fixed(id) => id.to_string(),
    };
    let opts = RenderOptions {
        idle_grace_seconds: station.station.idle_grace_seconds,
        prewarm: station.station.prewarm,
        ..opts.clone()
    };
    let rendered = render_channels(
        &channels,
        &station.station.ffmpeg,
        &station.station.normalization,
        &opts,
        &device_id,
    )?;
    Ok(Rendered {
        dropped: station.dropped,
        ..rendered
    })
}

/// A channel's own `guide.show_bumpers` wins; otherwise the station's.
fn resolve_show_bumpers(
    channel: Option<&crate::config::ChannelGuide>,
    station: &crate::config::StationGuide,
) -> bool {
    channel
        .and_then(|g| g.show_bumpers)
        .unwrap_or(station.show_bumpers)
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
    let mut logo_files = Vec::new();
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

        // A channel's own subtitle setting lands in the same `normalization.
        // subtitle` object the station-wide one came from, after the merge, so
        // the channel wins.
        if let Some(default_on) = channel_render.subtitle_default_on {
            let normalization = channel
                .entry("normalization".to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Some(normalization) = normalization.as_object_mut() {
                let subtitle = normalization
                    .entry("subtitle".to_string())
                    .or_insert_with(|| Value::Object(Map::new()));
                if let Some(subtitle) = subtitle.as_object_mut() {
                    subtitle.insert("default_on".to_string(), Value::Bool(default_on));
                }
            }
        }

        // A channel's own `ffmpeg_profile` (#260) lands in `normalization.video`
        // after the merge, so it beats the station's `active_profile`.
        if let Some(profile) = channel_render.profile.as_ref()
            && let Some(video) = channel
                .get_mut("normalization")
                .and_then(|n| n.get_mut("video"))
        {
            let mut picture: VideoNormalizationConfig = serde_json::from_value(video.clone())
                .map_err(|source| RenderError::Json {
                    path: opts.out_dir.join(format!("channel{number}.json")),
                    source,
                })?;
            profile.apply(&mut picture);
            *video = serde_json::to_value(&picture).map_err(|source| RenderError::Json {
                path: opts.out_dir.join(format!("channel{number}.json")),
                source,
            })?;
        }

        // The XMLTV route reads this to decide whether a bumper gets its own
        // guide row or folds into the programme before it. Written for every
        // channel, `false` included, so the file says what the guide does.
        channel.insert(
            "guide".to_string(),
            serde_json::json!({"show_bumpers": channel_render.show_bumpers}),
        );

        let idle_grace = if opts.prewarm {
            Some(PREWARM_IDLE_GRACE_SECONDS)
        } else {
            opts.idle_grace_seconds
        };
        if let Some(secs) = idle_grace {
            channel.insert("idle_grace_seconds".to_string(), Value::from(secs));
        }

        let channel_path = opts.out_dir.join(format!("channel{number}.json"));
        write_json(&channel_path, &Value::Object(channel))?;

        let mut entry = serde_json::json!({
            "number": number.to_string(),
            "name": display,
            "config": format!("./channel{number}.json"),
        });
        if let Some(src) = &channel_render.logo
            && let Some(file) = publish_logo(src, &opts.out_dir, number)?
        {
            entry["logo"] = Value::String(format!("./{LOGO_DIR}/{file}"));
            logo_files.push(file);
        }
        lineup_channels.push(entry);
    }
    remove_stale_logos(&opts.out_dir, &logo_files)?;

    let mut lineup = serde_json::json!({
        "server": {
            "bind_address": opts.bind_address,
            "port": opts.port,
            "device_id": device_id,
        },
        "output": {"folder": opts.hls_output},
        "channels": lineup_channels,
    });
    // Written only when on, so an unset station leaves the lineup as it was.
    if opts.prewarm {
        lineup["server"]["prewarm"] = serde_json::json!(true);
    }
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
        dropped: Vec::new(),
    })
}

/// The folder beside `lineup.json` that holds channel logos. The lineup names
/// each file relative to itself, and ETV-next serves it at
/// `/logo/{number}.{ext}`, so everything a render produces stays inside the
/// folder it was given.
const LOGO_DIR: &str = "channel-logos";

/// Copy a channel's `logo:` image to `<out_dir>/channel-logos/<number>.<ext>`
/// and return the file name, or `None` when it cannot be published.
///
/// Written to a temporary name and renamed into place, so ETV-next serving the
/// previous image never reads a half-written one. An unreadable image is logged
/// and the channel published without a logo rather than failing the render: a
/// guide tile is not worth taking the lineup down for.
fn publish_logo(src: &Path, out_dir: &Path, number: i64) -> Result<Option<String>, RenderError> {
    let bytes = match fs::read(src) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(
                event = "lineup.logo.skipped",
                channel = number,
                logo = %src.display(),
                reason = %e,
            );
            return Ok(None);
        }
    };
    let ext = src
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_else(|| "png".to_string());
    let file = format!("{number}.{ext}");
    let logos = out_dir.join(LOGO_DIR);
    fs::create_dir_all(&logos).map_err(|source| RenderError::Io {
        path: logos.clone(),
        source,
    })?;
    let dest = logos.join(&file);
    let tmp = logos.join(format!(".{file}.tmp"));
    fs::write(&tmp, &bytes).map_err(|source| RenderError::Io {
        path: tmp.clone(),
        source,
    })?;
    fs::rename(&tmp, &dest).map_err(|source| RenderError::Io {
        path: dest.clone(),
        source,
    })?;
    tracing::info!(
        event = "lineup.logo.published",
        channel = number,
        logo = %src.display(),
        dest = %dest.display(),
        bytes = bytes.len(),
    );
    Ok(Some(file))
}

/// Remove every file in `<out_dir>/channel-logos/` this render did not write,
/// so a dropped `logo:` or a removed channel leaves nothing behind. Only stale
/// files go: a logo still in use is never absent, not even for a moment.
fn remove_stale_logos(out_dir: &Path, keep: &[String]) -> Result<(), RenderError> {
    let logos = out_dir.join(LOGO_DIR);
    let entries = match fs::read_dir(&logos) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(RenderError::Io {
                path: logos,
                source,
            });
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !keep.contains(&name) {
            let path = entry.path();
            fs::remove_file(&path).map_err(|source| RenderError::Io { path, source })?;
        }
    }
    Ok(())
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
            idle_grace_seconds: None,
            prewarm: false,
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
                profile: None,
                subtitle_default_on: None,
                show_bumpers: false,
                logo: None,
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
                profile: None,
                subtitle_default_on: None,
                show_bumpers: false,
                logo: None,
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

    #[test]
    fn prewarm_lands_in_the_lineup_server_block_only_when_on() {
        let dir = tempfile::tempdir().unwrap();
        let folders = vec![dir.path().join("a")];
        let off =
            render_with_defaults(&chans(&folders), &opts(dir.path()), "test-device-id").unwrap();
        assert!(read(&off.lineup_path)["server"].get("prewarm").is_none());
        let on_opts = RenderOptions {
            prewarm: true,
            ..opts(dir.path())
        };
        let on = render_with_defaults(&chans(&folders), &on_opts, "test-device-id").unwrap();
        assert_eq!(read(&on.lineup_path)["server"]["prewarm"], true);
        let body = read(&dir.path().join("channel1.json"));
        assert_eq!(
            body["idle_grace_seconds"], PREWARM_IDLE_GRACE_SECONDS,
            "{body}"
        );
    }

    #[test]
    fn the_idle_grace_reaches_every_channel_and_unset_omits_it() {
        let dir = tempfile::tempdir().unwrap();
        let folders = [dir.path().join("a"), dir.path().join("b")];
        let with = RenderOptions {
            idle_grace_seconds: Some(300),
            ..opts(dir.path())
        };
        render_with_defaults(&chans(&folders), &with, "test-device-id").unwrap();
        for n in 1..=2 {
            let body = read(&dir.path().join(format!("channel{n}.json")));
            assert_eq!(body["idle_grace_seconds"], 300, "{body}");
        }
        let without = opts(dir.path());
        render_with_defaults(&chans(&folders), &without, "test-device-id").unwrap();
        let body = read(&dir.path().join("channel1.json"));
        assert!(body.get("idle_grace_seconds").is_none(), "{body}");
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

    // #342: a channel's `logo:` is copied beside the lineup and named in its
    // entry relative to the lineup; a channel without one gets no `logo` key,
    // and a logo file from an earlier render that no channel names is removed.
    #[test]
    fn a_channel_logo_is_written_beside_the_lineup_and_named_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("mark.PNG");
        fs::write(&src, b"not really a png").unwrap();
        let stale = dir.path().join("channel-logos/9.png");
        fs::create_dir_all(stale.parent().unwrap()).unwrap();
        fs::write(&stale, b"old").unwrap();
        let channel = |number: i64, logo: Option<PathBuf>| ChannelRender {
            profile: None,
            number,
            folder: PathBuf::from(format!("out/ch{number}")),
            display_name: None,
            overlay: None,
            subtitle_default_on: None,
            show_bumpers: false,
            logo,
        };
        let rendered = render_with_defaults(
            &[channel(1, Some(src)), channel(2, None)],
            &opts(dir.path()),
            "id",
        )
        .unwrap();
        let lineup = read(&rendered.lineup_path);

        assert_eq!(lineup["channels"][0]["logo"], "./channel-logos/1.png");
        assert_eq!(
            fs::read(dir.path().join("channel-logos/1.png")).unwrap(),
            b"not really a png"
        );
        assert!(lineup["channels"][1].get("logo").is_none(), "{lineup}");
        assert!(!stale.exists());
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
                profile: None,
                subtitle_default_on: None,
                show_bumpers: false,
                logo: None,
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
                profile: None,
                subtitle_default_on: None,
                show_bumpers: false,
                logo: None,
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

    /// `DEFAULT=YES` is an attribute of a channel's whole subtitle rendition,
    /// so it is a per-channel setting (#290): a channel's own value beats the
    /// station-wide one, and a channel that says nothing keeps the station's.
    #[test]
    fn a_channels_own_default_on_beats_the_station_wide_one() {
        let dir = tempfile::tempdir().unwrap();
        let station_on: NormalizationConfig = serde_json::from_value(serde_json::json!({
            "audio": {},
            "video": {"width": 1280, "height": 720},
            "subtitle": {"mode": "convert", "default_on": true},
        }))
        .unwrap();
        let channel = |number: i64, subtitle_default_on: Option<bool>| ChannelRender {
            profile: None,
            number,
            folder: PathBuf::from(format!("out/ch{number}")),
            display_name: None,
            overlay: None,
            subtitle_default_on,
            show_bumpers: false,
            logo: None,
        };
        let channels = vec![channel(1, None), channel(2, Some(false))];
        render_channels(&channels, &ffmpeg(), &station_on, &opts(dir.path()), "id").unwrap();

        let inherits = read(&dir.path().join("channel1.json"));
        assert_eq!(inherits["normalization"]["subtitle"]["default_on"], true);
        let overrides = read(&dir.path().join("channel2.json"));
        assert_eq!(overrides["normalization"]["subtitle"]["default_on"], false);
        assert_eq!(
            overrides["normalization"]["subtitle"]["mode"], "convert",
            "the override changes one setting and leaves the rest of the block"
        );
    }

    #[test]
    fn a_channels_show_bumpers_beats_the_stations() {
        use crate::config::{ChannelGuide, StationGuide};
        let on = StationGuide { show_bumpers: true };
        let off = StationGuide::default();
        let channel = |v| ChannelGuide {
            show_bumpers: v,
            ..Default::default()
        };
        assert!(!resolve_show_bumpers(None, &off));
        assert!(resolve_show_bumpers(None, &on));
        assert!(resolve_show_bumpers(Some(&channel(None)), &on));
        assert!(!resolve_show_bumpers(Some(&channel(Some(false))), &on));
        assert!(resolve_show_bumpers(Some(&channel(Some(true))), &off));
    }

    #[test]
    fn show_bumpers_lands_in_each_channel_json() {
        let dir = tempfile::tempdir().unwrap();
        let channel = |number: i64, show_bumpers: bool| ChannelRender {
            profile: None,
            number,
            folder: PathBuf::from(format!("out/ch{number}")),
            display_name: None,
            overlay: None,
            subtitle_default_on: None,
            show_bumpers,
            logo: None,
        };
        let channels = vec![channel(1, true), channel(2, false)];
        render_with_defaults(&channels, &opts(dir.path()), "id").unwrap();

        assert_eq!(
            read(&dir.path().join("channel1.json"))["guide"]["show_bumpers"],
            true
        );
        assert_eq!(
            read(&dir.path().join("channel2.json"))["guide"]["show_bumpers"],
            false
        );
    }

    #[test]
    fn default_on_is_off_unless_someone_sets_it() {
        let dir = tempfile::tempdir().unwrap();
        let channels = vec![ChannelRender {
            profile: None,
            number: 1,
            folder: PathBuf::from("out/ch1"),
            display_name: None,
            overlay: None,
            subtitle_default_on: None,
            show_bumpers: false,
            logo: None,
        }];
        render_with_defaults(&channels, &opts(dir.path()), "id").unwrap();
        let channel = read(&dir.path().join("channel1.json"));
        assert_eq!(channel["normalization"]["subtitle"]["default_on"], false);
    }

    // `a_defaults_overlay_never_leaks_into_a_channel` and `missing_defaults_is_an_error`
    // were deleted here: both tested the untyped-JSON defaults file this change
    // removes. A stray `overlay` in the shared body is no longer expressible —
    // `NormalizationConfig`/`FfmpegConfig` have no such field — so the seam the
    // first test guarded is closed, not merely tested; and there is no longer a
    // defaults file that can be missing, so the second test's premise is gone.

    const PROFILE_CHANNEL: &str = "number: {n}\nname: ch{n}\n{extra}roll_interval: 60s\nrule:\n  blocks:\n    - mode: all\n      order: manual\n      entries:\n        - kind: item\n          id: x\n          out_point: 30s\n          source:\n            kind: lavfi\n            params: testsrc\n";

    /// A station dir with channel 1 (no override) and channel 2 (`channel_extra`
    /// in its YAML), and the profile block `profiles` appended to the station.
    fn profile_station(dir: &Path, profiles: &str, channel_extra: &str) -> PathBuf {
        for (n, extra) in [(1, ""), (2, channel_extra)] {
            let body = PROFILE_CHANNEL
                .replace("{n}", &n.to_string())
                .replace("{extra}", extra);
            fs::write(dir.join(format!("ch{n}.yaml")), body).unwrap();
        }
        let station = dir.join("station.yaml");
        fs::write(
            &station,
            format!(
                "output_base: {out}\nchannels: [ch1.yaml, ch2.yaml]\nnormalization:\n  audio: {{}}\n  video: {{}}\n{profiles}",
                out = dir.join("out").display()
            ),
        )
        .unwrap();
        station
    }

    const PROFILES: &str = "ffmpeg_profiles:\n  quality: {resolution: 1920x1080, video_bitrate: 6000, video_codec: h264, bit_depth: 8}\n  light: {resolution: 1280x720, video_bitrate: 4000}\n";

    fn rendered_video(dir: &Path, station: &Path, n: u32) -> Value {
        let out = dir.join("render");
        fs::create_dir_all(&out).unwrap();
        render_with(station, &opts(&out), DeviceIdSource::Fixed("id")).unwrap();
        read(&out.join(format!("channel{n}.json")))["normalization"]["video"].clone()
    }

    /// Flipping `active_profile` from quality to light is the whole edit, and
    /// every channel's rendered picture follows it.
    #[test]
    fn active_profile_sets_every_channels_picture() {
        let dir = tempfile::tempdir().unwrap();
        let profiles = format!("{PROFILES}active_profile: quality\n");
        let station = profile_station(dir.path(), &profiles, "");
        for n in [1, 2] {
            let v = rendered_video(dir.path(), &station, n);
            assert_eq!(
                (v["width"].clone(), v["height"].clone()),
                (1920.into(), 1080.into())
            );
            assert_eq!(v["bitrate_kbps"], 6000);
        }

        let profiles = format!("{PROFILES}active_profile: light\n");
        let station = profile_station(dir.path(), &profiles, "");
        for n in [1, 2] {
            let v = rendered_video(dir.path(), &station, n);
            assert_eq!(
                (v["width"].clone(), v["height"].clone()),
                (1280.into(), 720.into())
            );
            assert_eq!(v["bitrate_kbps"], 4000);
            assert_eq!(v["buffer_kbps"], 8000);
        }
    }

    /// A channel's profile replaces `active_profile` rather than layering on
    /// it: a field the channel's profile omits falls back to the station's
    /// `normalization.video`.
    #[test]
    fn a_channels_profile_does_not_inherit_the_active_profiles_fields() {
        let dir = tempfile::tempdir().unwrap();
        let profiles = "ffmpeg_profiles:\n  hi: {resolution: 1920x1080, video_codec: hevc, bit_depth: 10}\n  lo: {resolution: 1280x720}\nactive_profile: hi\n";
        let station = profile_station(dir.path(), profiles, "ffmpeg_profile: lo\n");
        let hi = rendered_video(dir.path(), &station, 1);
        assert_eq!(
            (hi["format"].clone(), hi["bit_depth"].clone()),
            ("hevc".into(), 10.into())
        );
        let lo = rendered_video(dir.path(), &station, 2);
        assert_eq!(lo["width"], 1280);
        assert_eq!(
            (lo["format"].clone(), lo["bit_depth"].clone()),
            ("h264".into(), 8.into())
        );
    }

    /// A `bit_depth` other than 8 or 10 and a `video_codec` ETV-next lacks fail
    /// the load.
    #[test]
    fn a_bad_bit_depth_or_codec_fails_the_load() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["bit_depth: 12", "video_codec: vp9"] {
            let profiles = format!("ffmpeg_profiles:\n  bad: {{{bad}}}\n");
            let station = profile_station(dir.path(), &profiles, "");
            let err = config::load(&station).unwrap_err().to_string();
            assert!(err.contains("ffmpeg_profiles.bad"), "{bad}: {err}");
        }
    }

    /// A channel's own `ffmpeg_profile` beats `active_profile` for that channel
    /// alone.
    #[test]
    fn a_channels_profile_beats_the_active_one() {
        let dir = tempfile::tempdir().unwrap();
        let profiles = format!("{PROFILES}active_profile: quality\n");
        let station = profile_station(dir.path(), &profiles, "ffmpeg_profile: light\n");
        assert_eq!(rendered_video(dir.path(), &station, 1)["width"], 1920);
        let v = rendered_video(dir.path(), &station, 2);
        assert_eq!(
            (v["width"].clone(), v["bitrate_kbps"].clone()),
            (1280.into(), 4000.into())
        );
    }

    /// An `active_profile` that names nothing fails the station load, naming
    /// the key.
    #[test]
    fn an_unknown_active_profile_fails_the_load() {
        let dir = tempfile::tempdir().unwrap();
        let profiles = format!("{PROFILES}active_profile: ultra\n");
        let station = profile_station(dir.path(), &profiles, "");
        let err = config::load(&station).unwrap_err().to_string();
        assert!(err.contains("\"ultra\""), "{err}");
    }

    /// A channel naming an unknown profile does not load, and the reason names
    /// the key and the channel's file.
    #[test]
    fn a_channel_naming_an_unknown_profile_is_dropped_with_the_key_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let station = profile_station(dir.path(), PROFILES, "ffmpeg_profile: ultra\n");
        let loaded = config::load(&station).unwrap();
        assert_eq!(loaded.channels.len(), 1);
        let dropped = &loaded.dropped[0];
        assert!(dropped.config_path.ends_with("ch2.yaml"));
        assert!(dropped.reason.contains("\"ultra\""), "{}", dropped.reason);
    }

    /// A profile whose resolution is not `WxH` fails the station load.
    #[test]
    fn a_malformed_resolution_fails_the_load() {
        let dir = tempfile::tempdir().unwrap();
        let station = profile_station(
            dir.path(),
            "ffmpeg_profiles:\n  bad: {resolution: big}\n",
            "",
        );
        let err = config::load(&station).unwrap_err().to_string();
        assert!(err.contains("ffmpeg_profiles.bad"), "{err}");
    }
}

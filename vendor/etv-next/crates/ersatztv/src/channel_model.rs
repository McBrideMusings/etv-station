use std::path::{Path, PathBuf};

use ersatztv::config::ChannelConfig;
use ersatztv::error::LineupError;
use serde::Deserialize;

#[derive(Deserialize, Default)]
struct ConfigHint {
    #[serde(default)]
    normalization: NormalizationHint,
}

#[derive(Deserialize, Default)]
struct NormalizationHint {
    #[serde(default)]
    video: BitrateHint,
    #[serde(default)]
    audio: BitrateHint,
    #[serde(default)]
    subtitle: SubtitleHint,
}

#[derive(Deserialize, Default)]
struct BitrateHint {
    bitrate_kbps: Option<u32>,
}

/// The channel's subtitle settings as this crate needs to read them. The
/// authoritative definitions live in `ersatztv-channel`, which this crate does
/// not depend on, so the shape is mirrored here the same way the bitrate hint is.
#[derive(Deserialize, Default)]
struct SubtitleHint {
    #[serde(default)]
    mode: SubtitleModeHint,
    #[serde(default)]
    language: SubtitleLanguageHint,
}

#[derive(Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
enum SubtitleModeHint {
    #[default]
    Burn,
    Convert,
}

#[derive(Deserialize)]
struct SubtitleLanguageHint {
    #[serde(default = "default_language_tag")]
    tag: String,
    #[serde(default = "default_language_name")]
    name: String,
}

fn default_language_tag() -> String {
    String::from("en")
}

fn default_language_name() -> String {
    String::from("English")
}

impl Default for SubtitleLanguageHint {
    fn default() -> Self {
        SubtitleLanguageHint {
            tag: default_language_tag(),
            name: default_language_name(),
        }
    }
}

pub struct ChannelModel {
    number: String,
    name: String,
    config_path: PathBuf,
    overlay_paths: Vec<PathBuf>,
    output_folder: PathBuf,
    tvg_id: String,
    playout_folder: PathBuf,
    logo: Option<String>,
    group: Option<String>,
    bandwidth_bps: u32,
    subtitle: SubtitleHint,
}

#[derive(Deserialize)]
struct ChannelConfigPlayoutOnly {
    playout: PlayoutFolder,
}

#[derive(Deserialize)]
struct PlayoutFolder {
    folder: String,
}

impl ChannelModel {
    pub async fn new(
        config_path: &Path,
        output_folder: &Path,
        channel: ChannelConfig,
    ) -> Result<ChannelModel, LineupError> {
        let parent = config_path
            .parent()
            .ok_or(LineupError::LineupConfigNoParent)?;

        let channel_config = ersatztv::validate_config_path(parent, &channel.config)?;

        let mut overlay_paths: Vec<PathBuf> = Vec::new();
        for overlay in &channel.overlays {
            let overlay_path = ersatztv::validate_config_path(parent, overlay)?;
            overlay_paths.push(overlay_path);
        }

        let mut merged: serde_json::Value = serde_json::Value::Null;
        if let Ok(config_base) = tokio::fs::read_to_string(&channel_config).await
            && let Ok(value) = serde_json::from_str(&config_base)
        {
            merged = value;
        }

        for overlay_path in &overlay_paths {
            if let Ok(overlay_str) = tokio::fs::read_to_string(overlay_path).await
                && let Ok(overlay_value) = serde_json::from_str(&overlay_str)
            {
                ersatztv_core::deep_merge(&mut merged, overlay_value);
            }
        }

        let hint = serde_json::from_value::<ConfigHint>(merged).unwrap_or_default();

        let video = hint.normalization.video.bitrate_kbps.unwrap_or(4000);
        let audio = hint.normalization.audio.bitrate_kbps.unwrap_or(192);
        let bandwidth_bps = (video + audio) * 1100; // kbps => bps + 10% for hls overhead

        let subtitle = hint.normalization.subtitle;

        let playout_folder = read_playout_folder(&channel_config).await?;

        Ok(ChannelModel {
            number: channel.number.clone(),
            name: channel.name.clone(),
            config_path: channel_config,
            overlay_paths,
            output_folder: output_folder.join(&channel.number),
            tvg_id: channel
                .tvg_id
                .unwrap_or_else(|| format!("ersatztv.{}", channel.number)),
            playout_folder,
            logo: channel.logo.clone(),
            group: channel.group.clone(),
            bandwidth_bps,
            subtitle,
        })
    }

    pub fn number(&self) -> &str {
        self.number.as_str()
    }

    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    pub fn config_path(&self) -> &Path {
        self.config_path.as_path()
    }

    pub fn overlay_paths(&self) -> &[PathBuf] {
        self.overlay_paths.as_ref()
    }

    pub fn output_folder(&self) -> &Path {
        self.output_folder.as_path()
    }

    pub fn tvg_id(&self) -> &str {
        self.tvg_id.as_str()
    }

    pub fn playout_folder(&self) -> &Path {
        self.playout_folder.as_path()
    }

    pub fn logo(&self) -> Option<&str> {
        self.logo.as_deref()
    }

    pub fn group(&self) -> Option<&str> {
        self.group.as_deref()
    }

    pub fn bandwidth_bps(&self) -> u32 {
        self.bandwidth_bps
    }

    /// Whether this channel serves subtitles as their own selectable track.
    /// Burned-in subtitles are part of the video picture, so there is nothing to
    /// announce and no subtitle playlist to serve.
    pub fn has_subtitle_track(&self) -> bool {
        self.subtitle.mode == SubtitleModeHint::Convert
    }

    pub fn subtitle_language_tag(&self) -> &str {
        self.subtitle.language.tag.as_str()
    }

    pub fn subtitle_language_name(&self) -> &str {
        self.subtitle.language.name.as_str()
    }
}

/// Resolves `playout.folder` the same way `ersatztv-channel` does: through
/// `ersatztv_core::resolve_relative_paths`, which owns tilde-expansion and
/// relative-path joining for both crates. This function does not reimplement
/// any of that — it only turns the shared resolver's outcome into the two
/// errors below, since `resolve_relative_paths` never fails outright (it
/// falls back to a best-effort path instead of erroring).
async fn read_playout_folder(channel_config_path: &Path) -> Result<PathBuf, LineupError> {
    let body = tokio::fs::read_to_string(channel_config_path)
        .await
        .map_err(|e| LineupError::ChannelConfigRead {
            path: channel_config_path.display().to_string(),
            source: e,
        })?;

    let parsed: ChannelConfigPlayoutOnly =
        serde_json::from_str(&body).map_err(|e| LineupError::ChannelConfigParse {
            path: channel_config_path.display().to_string(),
            source: e,
        })?;

    let parent = channel_config_path
        .parent()
        .ok_or(LineupError::LineupConfigFailure(String::from(
            "failed to find parent of channel config",
        )))?;

    let mut value = serde_json::json!({ "playout": { "folder": parsed.playout.folder } });
    ersatztv_core::resolve_relative_paths(&mut value, parent, &["/playout/folder"]);

    let resolved = value
        .pointer("/playout/folder")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let resolved_path = PathBuf::from(resolved);

    // A resolved folder whose first component is still a bare `~` means the
    // shared resolver's tilde-expansion silently gave up — its only failure
    // mode is that the home directory can't be determined — and fell back to
    // treating that segment as a plain path piece rather than erroring.
    //
    // The component must be *exactly* `~`, matching what expansion actually
    // attempts: `simple_expand_tilde::expand_tilde` tests `Path::starts_with("~")`,
    // which compares whole components. A directory legitimately named
    // `~backup` is therefore never expanded, never fails, and must not be
    // read as a failure just because its name opens with a tilde.
    let raw_first_component = Path::new(&parsed.playout.folder).components().next();
    let tilde_expand_failed = raw_first_component.is_some_and(|first| {
        first.as_os_str() == "~" && resolved_path.components().any(|c| c == first)
    });
    if tilde_expand_failed {
        return Err(LineupError::PlayoutFolderExpand(parsed.playout.folder));
    }

    tokio::fs::canonicalize(&resolved_path)
        .await
        .map_err(|e| LineupError::PlayoutFolderResolve {
            path: resolved_path.display().to_string(),
            source: e,
        })
}

use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use simple_expand_tilde::expand_tilde;

use crate::error::LineupError;

const PATH_FIELDS: &[&str] = &["/output/folder", "/xmltv/folder", "/artwork/folder"];

#[derive(Deserialize, Serialize, Clone, JsonSchema)]
pub struct LineupConfig {
    #[serde(default = "server_config_default")]
    pub server: ServerConfig,
    pub output: OutputConfig,
    pub xmltv: Option<XmltvConfig>,
    /// Directory served at `/artwork` (station-cached Plex posters, #187).
    /// `None` mounts nothing there — a request to `/artwork/...` 404s, which
    /// only matters if something links there, and nothing does unless this is
    /// set: the station only ever writes a `/artwork/...` `<icon src>` when it
    /// has itself cached the file this config point at.
    #[serde(default)]
    pub artwork: Option<ArtworkConfig>,
    pub channels: Vec<ChannelConfig>,
}

#[derive(Deserialize, Serialize, Clone, JsonSchema)]
pub struct ServerConfig {
    #[serde(default = "bind_address_default")]
    pub bind_address: String,
    #[serde(default = "port_default")]
    pub port: u16,
    /// How Plex tells this tuner apart from every other one it has seen. Plex
    /// keys a DVR's whole channel mapping on it, so a value that changes
    /// between restarts reads as a brand-new tuner and the mapping has to be
    /// redone by hand.
    ///
    /// Read, never generated: whatever writes this file owns the value and owns
    /// keeping it stable.
    ///
    /// Deliberately has no serde default. A default would make every way of
    /// getting this key wrong — a rename here, a typo in the generator, a
    /// hand-written lineup that omits it — fail by quietly substituting one
    /// shared constant, so every deployment would report the same DeviceID and
    /// Plex would treat unrelated servers as the same tuner. Missing is an
    /// error instead, which is loud and happens at startup. `scaffold` writes
    /// one, so a generated lineup always has it.
    pub device_id: String,
}

#[derive(Deserialize, Serialize, Clone, JsonSchema)]
pub struct OutputConfig {
    pub folder: String,
}

#[derive(Deserialize, Serialize, Clone, JsonSchema)]
pub struct XmltvConfig {
    pub folder: String,
}

#[derive(Deserialize, Serialize, Clone, JsonSchema)]
pub struct ArtworkConfig {
    pub folder: String,
}

#[derive(Deserialize, Serialize, Clone, JsonSchema)]
pub struct ChannelConfig {
    pub number: String,
    pub name: String,
    /// Base configuration path
    pub config: String,
    /// Optional configuration overlay paths; values will be merged with base config, nulls will remove keys from base config
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overlays: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tvg_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

impl ChannelConfig {
    pub fn scaffold(number: &str) -> Self {
        Self {
            number: number.to_string(),
            name: format!("Channel {number}"),
            config: format!("./channels/{number}/channel.json"),
            overlays: Vec::new(),
            group: None,
            logo: None,
            tvg_id: None,
        }
    }
}

fn server_config_default() -> ServerConfig {
    ServerConfig {
        bind_address: bind_address_default(),
        port: port_default(),
        // Only reached when the lineup omits the whole `server` block. Minted
        // rather than constant, so two servers in that state are still distinct
        // tuners to Plex.
        device_id: uuid::Uuid::new_v4().to_string(),
    }
}

fn bind_address_default() -> String {
    String::from("0.0.0.0")
}
fn port_default() -> u16 {
    std::env::var("ETV_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8409)
}

pub async fn from_file(path: &PathBuf) -> Result<LineupConfig, LineupError> {
    if !path.exists() {
        return Err(LineupError::LineupConfigFailure(format!(
            "file does not exist: {:?}",
            path
        )));
    }

    let config_string = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| LineupError::LineupConfigFailure(e.to_string()))?;
    let mut lineup_value: serde_json::Value = serde_json::from_str(&config_string)
        .map_err(|e| LineupError::LineupConfigFailure(e.to_string()))?;
    let lineup_parent = path.parent().ok_or(LineupError::LineupConfigNoParent)?;
    ersatztv_core::resolve_relative_paths(&mut lineup_value, lineup_parent, PATH_FIELDS)?;
    let lineup_config: LineupConfig = serde_json::from_value(lineup_value)
        .map_err(|e| LineupError::LineupConfigFailure(e.to_string()))?;
    Ok(lineup_config)
}

pub fn resolve_output_folder(lineup_path: &Path, raw: &str) -> PathBuf {
    let raw_path_buf = Path::new(raw).to_path_buf();
    let expanded_path = expand_tilde(raw).unwrap_or(raw_path_buf.clone());
    if expanded_path.is_relative()
        && let Some(parent) = lineup_path.parent()
    {
        parent
            .join(&expanded_path)
            .canonicalize()
            .unwrap_or_else(|_| parent.join(&expanded_path))
    } else {
        expanded_path
    }
}

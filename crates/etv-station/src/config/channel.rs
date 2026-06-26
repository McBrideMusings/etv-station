use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::rule::RuleConfig;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelConfig {
    pub output_folder: PathBuf,

    #[serde(default = "default_window_days")]
    pub window_days: u32,

    #[serde(default = "default_chunk_hours")]
    pub chunk_hours: u32,

    #[serde(default = "default_roll_interval", with = "humantime_serde")]
    pub roll_interval: Duration,

    #[serde(default = "default_retention_days")]
    pub retention_days: u32,

    /// Channel-level random seed. Only meaningful when a block uses
    /// `order = "random"`; unset means a fresh (non-reproducible) shuffle per
    /// generation, set means a pinned shuffle (#46 locked decision). Omit on
    /// channels with no random ordering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,

    pub rule: RuleConfig,

    /// Optional live overlay. When set, the station daemon supervises an
    /// `etv-overlay` subprocess that writes RGBA frames to a fifo per
    /// channel; the emitted playout JSON carries an `overlay` field
    /// pointing etv-next at that fifo.
    #[serde(default)]
    pub overlay: Option<ChannelOverlayConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChannelOverlayConfig {
    /// Path to the etv-overlay TOML config (relative to the channel config
    /// directory or absolute). The config supplies width / height / framerate
    /// and the rendering script.
    pub config: PathBuf,

    /// Path to the fifo the channel + overlay process share. If omitted,
    /// defaults to `{output_folder}/overlay.fifo`.
    #[serde(default)]
    pub fifo_path: Option<PathBuf>,
}

fn default_window_days() -> u32 {
    30
}
fn default_chunk_hours() -> u32 {
    24
}
fn default_roll_interval() -> Duration {
    Duration::from_secs(3600)
}
fn default_retention_days() -> u32 {
    7
}

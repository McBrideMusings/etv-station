use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::rule::RuleConfig;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelConfig {
    /// Optional channel identity override. When unset, the channel's identity
    /// is its config file's stem (e.g. `diehard.yaml` -> `diehard`). The
    /// identity drives the log label, the overlay handshake name, and the
    /// output folder leaf under the station's `output_base`. Must not contain
    /// path separators.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

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

    /// Tuning for pools that draw from a scorer plugin (#74). Absent on every
    /// channel that uses none, which is why it is optional rather than a
    /// defaulted struct — an unused knob in a config file invites tuning that
    /// does nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scoring: Option<ScoringConfig>,

    pub rule: RuleConfig,

    /// Optional live overlay. When set, the station daemon supervises an
    /// `etv-overlay` subprocess that writes RGBA frames to a fifo per
    /// channel; the emitted playout JSON carries an `overlay` field
    /// pointing etv-next at that fifo.
    #[serde(default)]
    pub overlay: Option<ChannelOverlayConfig>,
}

impl ChannelConfig {
    /// Whether any block interleaves pools via a pattern (#72). A pattern
    /// channel's resolved list advances every generation, so it materializes
    /// forward from a `.resume` sidecar instead of looping a fixed list from
    /// the `.anchor` — see [`crate::rule::Sequential`].
    pub fn is_pattern(&self) -> bool {
        self.rule.blocks.iter().any(|b| b.is_pattern())
    }
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

/// What a scorer plugin is told about, per channel.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScoringConfig {
    /// How many recently-aired entries the plugin sees in `ctx.recent`. This is
    /// the window a script suppresses repeats over, so it belongs to the
    /// channel's taste, not to the daemon: a channel with a deep library wants
    /// a long memory, a narrow one would starve on the same setting.
    #[serde(default = "default_recent_depth")]
    pub recent_depth: usize,

    /// Nominal seconds per item, used only to turn a generation's span into the
    /// `ctx.target_count` hint. A channel of half-hour episodes and one of
    /// three-hour films need very different numbers to ask for a sensible
    /// amount.
    #[serde(default = "default_nominal_item_secs")]
    pub nominal_item_secs: u32,
}

impl Default for ScoringConfig {
    fn default() -> Self {
        Self {
            recent_depth: default_recent_depth(),
            nominal_item_secs: default_nominal_item_secs(),
        }
    }
}

fn default_recent_depth() -> usize {
    200
}

fn default_nominal_item_secs() -> u32 {
    1800
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

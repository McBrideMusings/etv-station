use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::rule::RuleConfig;

#[derive(Debug, Deserialize, Serialize)]
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

    pub rule: RuleConfig,
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

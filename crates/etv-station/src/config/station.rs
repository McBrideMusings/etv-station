use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct StationConfig {
    #[serde(default = "default_tz")]
    pub tz: String,

    pub channels: Vec<ChannelEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ChannelEntry {
    pub name: String,
    pub path: PathBuf,
}

fn default_tz() -> String {
    "UTC".to_string()
}

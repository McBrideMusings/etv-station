use std::path::{Path, PathBuf};

use super::channel::ChannelConfig;
use super::station::StationConfig;
use super::validate;
use crate::errors::ConfigError;

#[derive(Debug)]
pub struct Station {
    pub config_path: PathBuf,
    pub station: StationConfig,
    pub channels: Vec<LoadedChannel>,
}

#[derive(Debug)]
pub struct LoadedChannel {
    pub name: String,
    pub config_path: PathBuf,
    pub config: ChannelConfig,
}

pub fn load(station_path: &Path) -> Result<Station, ConfigError> {
    let station: StationConfig = read_toml(station_path)?;
    validate::validate_station(station_path, &station)?;

    let base = station_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let mut channels = Vec::with_capacity(station.channels.len());
    for entry in &station.channels {
        let channel_path = if entry.path.is_absolute() {
            entry.path.clone()
        } else {
            base.join(&entry.path)
        };
        let config: ChannelConfig = read_toml(&channel_path)?;
        validate::validate_channel(&channel_path, &config)?;
        channels.push(LoadedChannel {
            name: entry.name.clone(),
            config_path: channel_path,
            config,
        });
    }

    Ok(Station {
        config_path: station_path.to_path_buf(),
        station,
        channels,
    })
}

fn read_toml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, ConfigError> {
    let contents = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&contents).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn examples_station() -> PathBuf {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        manifest_dir.join("../../examples/station.toml")
    }

    #[test]
    fn loads_example_fixture() {
        let path = examples_station();
        let loaded = load(&path).expect("examples/station.toml should load");
        assert_eq!(loaded.channels.len(), 1);
        let ch = &loaded.channels[0];
        assert_eq!(ch.name, "test");
        let items = ch.config.rule.items();
        assert!(!items.is_empty(), "rule must have at least one item");
    }
}

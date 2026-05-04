use std::path::{Path, PathBuf};

use super::channel::ChannelConfig;
use super::item::SourceConfig;
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
        let mut config: ChannelConfig = read_toml(&channel_path)?;
        validate::validate_channel(&channel_path, &config)?;
        for item in config.rule.items_mut() {
            if let SourceConfig::Local { path } = &mut item.source {
                *path = expand_env(path, &channel_path)?;
            }
        }
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

fn expand_env(input: &str, ctx: &Path) -> Result<String, ConfigError> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').ok_or_else(|| ConfigError::Validation {
            path: ctx.to_path_buf(),
            message: format!("unterminated ${{...}} in {input:?}"),
        })?;
        let var = &after[..end];
        if var.is_empty() {
            return Err(ConfigError::Validation {
                path: ctx.to_path_buf(),
                message: format!("empty ${{}} in {input:?}"),
            });
        }
        let val = std::env::var(var).map_err(|_| ConfigError::Validation {
            path: ctx.to_path_buf(),
            message: format!("env var `{var}` referenced by {input:?} is not set"),
        })?;
        out.push_str(&val);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
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
        // The diehard example references ${ETV_TEST_MEDIA_DIR}; set a placeholder
        // so the env-expansion step in `load` succeeds in test environments.
        // SAFETY: single-threaded test; value is local in scope.
        unsafe {
            std::env::set_var("ETV_TEST_MEDIA_DIR", "/tmp/etv-test-media");
        }
        let path = examples_station();
        let loaded = load(&path).expect("examples/station.toml should load");
        let ch = loaded
            .channels
            .iter()
            .find(|c| c.name == "test")
            .expect("test channel present");
        let items = ch.config.rule.items();
        assert!(!items.is_empty(), "rule must have at least one item");
    }

    #[test]
    fn expand_env_substitutes_vars() {
        // SAFETY: single-threaded test, value scoped to this test only.
        unsafe {
            std::env::set_var("ETV_LOAD_TEST_DIR", "/tmp/etv-load-test");
        }
        let out = expand_env("${ETV_LOAD_TEST_DIR}/movie.mkv", Path::new("/dev/null")).unwrap();
        assert_eq!(out, "/tmp/etv-load-test/movie.mkv");
    }

    #[test]
    fn expand_env_errors_on_missing_var() {
        let err = expand_env(
            "${ETV_LOAD_TEST_DEFINITELY_UNSET}/x",
            Path::new("/dev/null"),
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("ETV_LOAD_TEST_DEFINITELY_UNSET"),
            "msg = {msg}"
        );
    }

    #[test]
    fn expand_env_passes_through_literals() {
        let out = expand_env("/no/vars/here.mkv", Path::new("/dev/null")).unwrap();
        assert_eq!(out, "/no/vars/here.mkv");
    }
}

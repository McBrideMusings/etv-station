use std::collections::HashSet;
use std::path::Path;

use super::channel::ChannelConfig;
use super::rule::RuleConfig;
use super::station::StationConfig;
use crate::errors::ConfigError;

pub(super) fn validate_station(path: &Path, station: &StationConfig) -> Result<(), ConfigError> {
    if station.channels.is_empty() {
        return Err(ConfigError::Validation {
            path: path.to_path_buf(),
            message: "station.toml must declare at least one channel".into(),
        });
    }

    let mut seen = HashSet::new();
    for entry in &station.channels {
        if entry.name.trim().is_empty() {
            return Err(ConfigError::Validation {
                path: path.to_path_buf(),
                message: "channel entry has empty name".into(),
            });
        }
        if !seen.insert(entry.name.clone()) {
            return Err(ConfigError::Validation {
                path: path.to_path_buf(),
                message: format!("duplicate channel name: {}", entry.name),
            });
        }
    }

    if station.tz.trim().is_empty() {
        return Err(ConfigError::Validation {
            path: path.to_path_buf(),
            message: "tz cannot be empty".into(),
        });
    }

    Ok(())
}

pub(super) fn validate_channel(path: &Path, channel: &ChannelConfig) -> Result<(), ConfigError> {
    if channel.window_days == 0 {
        return Err(ConfigError::Validation {
            path: path.to_path_buf(),
            message: "window_days must be > 0".into(),
        });
    }
    if channel.chunk_hours == 0 {
        return Err(ConfigError::Validation {
            path: path.to_path_buf(),
            message: "chunk_hours must be > 0".into(),
        });
    }
    if channel.roll_interval.is_zero() {
        return Err(ConfigError::Validation {
            path: path.to_path_buf(),
            message: "roll_interval must be > 0".into(),
        });
    }

    match &channel.rule {
        RuleConfig::LoopForever { items } => {
            if items.is_empty() {
                return Err(ConfigError::Validation {
                    path: path.to_path_buf(),
                    message: "loop_forever rule requires at least one item".into(),
                });
            }
            let mut ids = HashSet::new();
            for item in items {
                if item.id.trim().is_empty() {
                    return Err(ConfigError::Validation {
                        path: path.to_path_buf(),
                        message: "item id cannot be empty".into(),
                    });
                }
                if !ids.insert(item.id.clone()) {
                    return Err(ConfigError::Validation {
                        path: path.to_path_buf(),
                        message: format!("duplicate item id: {}", item.id),
                    });
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::item::{ItemConfig, SourceConfig};
    use crate::config::station::ChannelEntry;
    use std::path::PathBuf;
    use std::time::Duration;

    fn dummy_path() -> PathBuf {
        PathBuf::from("/tmp/test.toml")
    }

    fn item(id: &str) -> ItemConfig {
        ItemConfig {
            id: id.into(),
            source: SourceConfig::Lavfi {
                params: "testsrc".into(),
            },
            in_point: None,
            out_point: None,
            program: None,
        }
    }

    fn channel_with(items: Vec<ItemConfig>) -> ChannelConfig {
        ChannelConfig {
            output_folder: PathBuf::from("/out"),
            window_days: 1,
            chunk_hours: 24,
            roll_interval: Duration::from_secs(3600),
            retention_days: 1,
            rule: RuleConfig::LoopForever { items },
        }
    }

    #[test]
    fn rejects_empty_channels() {
        let s = StationConfig {
            tz: "UTC".into(),
            channels: vec![],
        };
        let err = validate_station(&dummy_path(), &s).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("at least one channel"), "msg = {msg}");
    }

    #[test]
    fn rejects_duplicate_channel_names() {
        let s = StationConfig {
            tz: "UTC".into(),
            channels: vec![
                ChannelEntry {
                    name: "a".into(),
                    path: "x".into(),
                },
                ChannelEntry {
                    name: "a".into(),
                    path: "y".into(),
                },
            ],
        };
        assert!(validate_station(&dummy_path(), &s).is_err());
    }

    #[test]
    fn rejects_loop_forever_with_no_items() {
        let ch = channel_with(vec![]);
        assert!(validate_channel(&dummy_path(), &ch).is_err());
    }

    #[test]
    fn rejects_duplicate_item_ids() {
        let ch = channel_with(vec![item("a"), item("a")]);
        assert!(validate_channel(&dummy_path(), &ch).is_err());
    }

    #[test]
    fn accepts_valid_channel() {
        let ch = channel_with(vec![item("a"), item("b")]);
        validate_channel(&dummy_path(), &ch).unwrap();
    }
}

use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::block::Duplicates;
use super::channel::ChannelConfig;
use super::entry::Entry;
use super::station::StationConfig;
use crate::errors::ConfigError;

pub(super) fn validate_station(path: &Path, station: &StationConfig) -> Result<(), ConfigError> {
    if station.channels.is_empty() {
        return Err(ConfigError::Validation {
            path: path.to_path_buf(),
            message: "station config must declare at least one channel".into(),
        });
    }

    for entry in &station.channels {
        if entry.trim().is_empty() {
            return Err(ConfigError::Validation {
                path: path.to_path_buf(),
                message: "channel entry is empty".into(),
            });
        }
    }

    if station.output_base.as_os_str().is_empty() {
        return Err(ConfigError::Validation {
            path: path.to_path_buf(),
            message: "output_base cannot be empty".into(),
        });
    }

    if station.tz.trim().is_empty() {
        return Err(ConfigError::Validation {
            path: path.to_path_buf(),
            message: "tz cannot be empty".into(),
        });
    }

    Ok(())
}

/// Reject two channels that write to the same `output_folder`. A shared folder
/// silently misbehaves: both channels fight over the `.anchor` sidecar and each
/// startup prunes the other's `.durations.json` cache, forcing re-probes on
/// every restart.
///
/// Folders are compared exactly as the daemon uses them — verbatim, relative to
/// the single process CWD (see `daemon::channel_loop`, which uses
/// `LoadedChannel::output_folder` as-is), NOT resolved against each channel's
/// own config directory. Two channels whose derived identities land on the same
/// `{output_base}/{identity}` therefore collide, because at runtime both write
/// the same path — that shared runtime target is the collision we must reject.
///
/// `channels` is `(identity, output_folder)` per channel.
pub(super) fn validate_output_folders(
    station_path: &Path,
    channels: &[(&str, &Path)],
) -> Result<(), ConfigError> {
    let mut seen: HashMap<&Path, &str> = HashMap::new();
    for (name, output_folder) in channels {
        if let Some(prev) = seen.insert(output_folder, name) {
            return Err(ConfigError::Validation {
                path: station_path.to_path_buf(),
                message: format!(
                    "channels {:?} and {:?} both write to output_folder {}",
                    prev,
                    name,
                    output_folder.display()
                ),
            });
        }
    }
    Ok(())
}

/// Validate a channel after [`super::load`] has resolved every block-include
/// into normalized inline form (path refs spliced, env vars expanded). The
/// structural "exactly one of path/inline" check happens during load; this is
/// the semantic pass over the resolved shape.
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

    if channel.rule.blocks.is_empty() {
        return Err(ConfigError::Validation {
            path: path.to_path_buf(),
            message: "channel rule requires at least one block".into(),
        });
    }

    for (idx, include) in channel.rule.blocks.iter().enumerate() {
        let entries = include.entries();
        if entries.is_empty() {
            return Err(ConfigError::Validation {
                path: path.to_path_buf(),
                message: format!("block #{idx} has no entries"),
            });
        }

        // Item ids must be non-empty, and unique within a block unless the
        // block opted into `duplicates = "keep"`.
        let mut ids = HashSet::new();
        for entry in entries {
            if let Entry::Item(item) = entry {
                if item.id.trim().is_empty() {
                    return Err(ConfigError::Validation {
                        path: path.to_path_buf(),
                        message: format!("block #{idx} has an item with an empty id"),
                    });
                }
                if include.duplicates() == Duplicates::Keep {
                    continue;
                }
                if !ids.insert(item.id.clone()) {
                    return Err(ConfigError::Validation {
                        path: path.to_path_buf(),
                        message: format!(
                            "block #{idx} has duplicate item id {:?} (set duplicates = \"keep\" to allow)",
                            item.id
                        ),
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
    use crate::config::{
        BlockInclude, ChannelConfig, Entry, ItemEntry, Mode, Order, RuleConfig, SourceConfig,
        StationConfig,
    };
    use std::path::PathBuf;
    use std::time::Duration;

    fn dummy_path() -> PathBuf {
        PathBuf::from("/tmp/test.toml")
    }

    fn item_entry(id: &str) -> Entry {
        Entry::Item(ItemEntry {
            id: id.into(),
            source: SourceConfig::Lavfi {
                params: "testsrc".into(),
            },
            in_point: None,
            out_point: Some(Duration::from_secs(30)),
            program: None,
        })
    }

    fn inline_block(entries: Vec<Entry>) -> BlockInclude {
        BlockInclude {
            block: None,
            program: None,
            duplicates: None,
            entries,
            mode: Mode::All,
            order: Order::Manual,
            filter: None,
        }
    }

    fn channel_with(blocks: Vec<BlockInclude>) -> ChannelConfig {
        ChannelConfig {
            name: None,
            window_days: 1,
            chunk_hours: 24,
            roll_interval: Duration::from_secs(3600),
            retention_days: 1,
            seed: None,
            rule: RuleConfig { blocks },
            overlay: None,
        }
    }

    #[test]
    fn rejects_empty_channels() {
        let s = StationConfig {
            tz: "UTC".into(),
            output_base: PathBuf::from("out"),
            channels: vec![],
        };
        let err = validate_station(&dummy_path(), &s).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("at least one channel"), "msg = {msg}");
    }

    #[test]
    fn rejects_empty_output_base() {
        let s = StationConfig {
            tz: "UTC".into(),
            output_base: PathBuf::new(),
            channels: vec!["channels/a.yaml".into()],
        };
        let err = validate_station(&dummy_path(), &s).unwrap_err();
        assert!(format!("{err}").contains("output_base"));
    }

    #[test]
    fn rejects_empty_channel_entry() {
        let s = StationConfig {
            tz: "UTC".into(),
            output_base: PathBuf::from("out"),
            channels: vec!["  ".into()],
        };
        assert!(validate_station(&dummy_path(), &s).is_err());
    }

    #[test]
    fn rejects_channel_with_no_blocks() {
        let ch = channel_with(vec![]);
        let err = validate_channel(&dummy_path(), &ch).unwrap_err();
        assert!(format!("{err}").contains("at least one block"));
    }

    #[test]
    fn rejects_block_with_no_entries() {
        let ch = channel_with(vec![inline_block(vec![])]);
        let err = validate_channel(&dummy_path(), &ch).unwrap_err();
        assert!(format!("{err}").contains("no entries"));
    }

    #[test]
    fn rejects_duplicate_item_ids_by_default() {
        let ch = channel_with(vec![inline_block(vec![item_entry("a"), item_entry("a")])]);
        let err = validate_channel(&dummy_path(), &ch).unwrap_err();
        assert!(format!("{err}").contains("duplicate item id"));
    }

    #[test]
    fn allows_duplicate_item_ids_with_keep() {
        let mut block = inline_block(vec![item_entry("a"), item_entry("a")]);
        block.duplicates = Some(Duplicates::Keep);
        let ch = channel_with(vec![block]);
        validate_channel(&dummy_path(), &ch).unwrap();
    }

    #[test]
    fn accepts_valid_channel() {
        let ch = channel_with(vec![inline_block(vec![item_entry("a"), item_entry("b")])]);
        validate_channel(&dummy_path(), &ch).unwrap();
    }

    #[test]
    fn rejects_shared_absolute_output_folder() {
        let out = Path::new("/srv/out");
        let err = validate_output_folders(&dummy_path(), &[("a", out), ("b", out)]).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("both write to output_folder"), "msg = {msg}");
        assert!(
            msg.contains("\"a\"") && msg.contains("\"b\""),
            "msg = {msg}"
        );
    }

    #[test]
    fn rejects_identical_relative_output_folder() {
        // Both channels write the same relative folder → at runtime both land on
        // `<cwd>/out`, so this is a real collision the daemon can't tolerate.
        let out = Path::new("out");
        assert!(validate_output_folders(&dummy_path(), &[("a", out), ("b", out)]).is_err());
    }

    #[test]
    fn accepts_distinct_output_folders() {
        validate_output_folders(
            &dummy_path(),
            &[("a", Path::new("/srv/a")), ("b", Path::new("/srv/b"))],
        )
        .unwrap();
    }
}

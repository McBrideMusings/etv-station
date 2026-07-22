use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::block::Duplicates;
use super::channel::ChannelConfig;
use super::order::Order;
use super::rule::BlockInclude;
use super::station::StationConfig;
use crate::errors::ConfigError;
use crate::pattern::MAX_CYCLES;

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

    // Pool names key the `.resume` sidecar, so they must be unique across the
    // whole channel — that is what lets the sidecar survive blocks being
    // reordered without a block index in the key.
    let mut pool_names: HashSet<&str> = HashSet::new();

    for (idx, include) in channel.rule.blocks.iter().enumerate() {
        let bad = |message: String| ConfigError::Validation {
            path: path.to_path_buf(),
            message: format!("block #{idx}: {message}"),
        };

        if include.is_pattern() {
            validate_pattern_block(include, &mut pool_names, &bad)?;
            continue;
        }

        if include.entries().is_empty() {
            return Err(ConfigError::Validation {
                path: path.to_path_buf(),
                message: format!("block #{idx} has no entries"),
            });
        }
        // Item identity is derived from the source at resolution time, not
        // authored — so there is no id to validate here. Within-block duplicates
        // (two entries resolving to the same file) collapse in `resolve`, they
        // are not a config error. `duplicates = "keep"` opts out of the collapse.
    }

    Ok(())
}

/// Semantic checks for a pools + pattern block (#72).
///
/// The refusals here all guard the same thing: a pattern block whose *other*
/// fields quietly contradict the pattern. A block-level `order` would re-sort
/// the interleave the pattern just built, and `duplicates = "collapse"` would
/// delete every repeat a looping pool deliberately produces. Both are rejected
/// at load with the conflict named, rather than accepted and ignored — a config
/// that says `order: random` and doesn't shuffle is a lie the author can't see
/// from the file.
fn validate_pattern_block<'a>(
    include: &'a BlockInclude,
    pool_names: &mut HashSet<&'a str>,
    bad: &impl Fn(String) -> ConfigError,
) -> Result<(), ConfigError> {
    if !include.entries().is_empty() {
        return Err(bad(
            "a block is either an `entries` block or a `pools` + `pattern` block, not both".into(),
        ));
    }
    if include.pools.is_empty() {
        return Err(bad("`pattern` needs at least one `pools` entry".into()));
    }
    if include.pattern.is_empty() {
        return Err(bad("`pools` needs a `pattern` to draw from them".into()));
    }
    if include.order != Order::Manual {
        return Err(bad(format!(
            "order {:?} conflicts with `pattern` — the pattern IS the ordering; \
             sort inside a pool with its own `order` instead",
            include.order
        )));
    }
    if include.duplicates == Some(Duplicates::Collapse) {
        return Err(bad(
            "duplicates = \"collapse\" conflicts with `pattern` — collapse would delete \
             the repeats a looping pool produces; a pattern block is always \"keep\""
                .into(),
        ));
    }
    if let Some(n) = include.cycles {
        if n == 0 {
            return Err(bad("cycles must be > 0".into()));
        }
        if n > MAX_CYCLES {
            return Err(bad(format!(
                "cycles = {n} exceeds the maximum of {MAX_CYCLES}"
            )));
        }
    }

    let mut local: HashSet<&str> = HashSet::new();
    for pool in &include.pools {
        if pool.name.trim().is_empty() {
            return Err(bad("a pool has an empty name".into()));
        }
        if pool.expr.trim().is_empty() {
            return Err(bad(format!("pool {:?} has an empty expr", pool.name)));
        }
        if !pool_names.insert(pool.name.as_str()) {
            return Err(bad(format!(
                "pool name {:?} is already used by another block in this channel; \
                 pool names key the .resume sidecar and must be unique per channel",
                pool.name
            )));
        }
        local.insert(pool.name.as_str());
    }

    for (step_idx, step) in include.pattern.iter().enumerate() {
        if !local.contains(step.pool.as_str()) {
            return Err(bad(format!(
                "pattern step #{step_idx} names pool {:?}, which this block does not declare",
                step.pool
            )));
        }
        if step.take == 0 {
            return Err(bad(format!("pattern step #{step_idx} has take = 0")));
        }
        if !(0.0..=1.0).contains(&step.chance) {
            return Err(bad(format!(
                "pattern step #{step_idx} has chance = {}, outside 0.0–1.0",
                step.chance
            )));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        Advance, BlockInclude, ChannelConfig, Entry, ItemEntry, Mode, OnShort, Order, PatternStep,
        Pool, Rotate, RuleConfig, Select, SourceConfig, StationConfig, Wrap,
    };
    use std::path::PathBuf;
    use std::time::Duration;

    fn dummy_path() -> PathBuf {
        PathBuf::from("/tmp/test.toml")
    }

    fn item_entry(id: &str) -> Entry {
        Entry::Item(ItemEntry {
            source: SourceConfig::Lavfi {
                params: format!("src={id}"),
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
            pools: Vec::new(),
            pattern: Vec::new(),
            cycles: None,
            mode: Mode::All,
            order: Order::Manual,
            filter: None,
        }
    }

    fn pool(name: &str) -> Pool {
        Pool {
            name: name.into(),
            expr: format!("item.type == \"{name}\""),
            order: None,
            select: Select::default(),
            rotate: Rotate::default(),
            advance: Advance::default(),
            wrap: Wrap::default(),
            on_short: OnShort::default(),
        }
    }

    fn step(pool: &str, take: usize) -> PatternStep {
        PatternStep {
            pool: pool.into(),
            take,
            chance: 1.0,
        }
    }

    fn pattern_block(pools: Vec<Pool>, pattern: Vec<PatternStep>) -> BlockInclude {
        let mut b = inline_block(vec![]);
        b.pools = pools;
        b.pattern = pattern;
        b
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
            source_roots: vec![],
            catalog_path: None,
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
            source_roots: vec![],
            catalog_path: None,
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
            source_roots: vec![],
            catalog_path: None,
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
    fn accepts_valid_channel() {
        let ch = channel_with(vec![inline_block(vec![item_entry("a"), item_entry("b")])]);
        validate_channel(&dummy_path(), &ch).unwrap();
    }

    // ---- pattern blocks (#72) ---------------------------------------------

    #[test]
    fn accepts_a_pattern_block() {
        let b = pattern_block(
            vec![pool("movies"), pool("shows")],
            vec![step("movies", 1), step("shows", 3)],
        );
        validate_channel(&dummy_path(), &channel_with(vec![b])).unwrap();
    }

    #[test]
    fn rejects_a_block_that_is_both_entries_and_pattern() {
        let mut b = pattern_block(vec![pool("movies")], vec![step("movies", 1)]);
        b.entries = vec![item_entry("a")];
        let err = validate_channel(&dummy_path(), &channel_with(vec![b])).unwrap_err();
        assert!(format!("{err}").contains("not both"), "err = {err}");
    }

    #[test]
    fn rejects_pools_without_a_pattern() {
        let b = pattern_block(vec![pool("movies")], vec![]);
        let err = validate_channel(&dummy_path(), &channel_with(vec![b])).unwrap_err();
        assert!(format!("{err}").contains("pattern"), "err = {err}");
    }

    #[test]
    fn rejects_a_pattern_without_pools() {
        let b = pattern_block(vec![], vec![step("movies", 1)]);
        let err = validate_channel(&dummy_path(), &channel_with(vec![b])).unwrap_err();
        assert!(format!("{err}").contains("pools"), "err = {err}");
    }

    #[test]
    fn rejects_a_step_naming_an_undeclared_pool() {
        let b = pattern_block(vec![pool("movies")], vec![step("shows", 3)]);
        let err = validate_channel(&dummy_path(), &channel_with(vec![b])).unwrap_err();
        assert!(format!("{err}").contains("does not declare"), "err = {err}");
    }

    #[test]
    fn rejects_block_order_on_a_pattern_block() {
        // The pattern IS the ordering — a block-level sort would silently
        // un-pattern the block.
        let mut b = pattern_block(vec![pool("movies")], vec![step("movies", 1)]);
        b.order = Order::Random;
        let err = validate_channel(&dummy_path(), &channel_with(vec![b])).unwrap_err();
        assert!(
            format!("{err}").contains("conflicts with `pattern`"),
            "err = {err}"
        );
    }

    #[test]
    fn rejects_explicit_collapse_on_a_pattern_block() {
        let mut b = pattern_block(vec![pool("movies")], vec![step("movies", 1)]);
        b.duplicates = Some(Duplicates::Collapse);
        let err = validate_channel(&dummy_path(), &channel_with(vec![b])).unwrap_err();
        assert!(format!("{err}").contains("collapse"), "err = {err}");
    }

    #[test]
    fn a_pattern_block_reports_keep_regardless_of_the_unset_default() {
        let b = pattern_block(vec![pool("movies")], vec![step("movies", 1)]);
        assert_eq!(b.duplicates(), Duplicates::Keep);
        // An entries block still defaults to collapse.
        assert_eq!(
            inline_block(vec![item_entry("a")]).duplicates(),
            Duplicates::Collapse
        );
    }

    #[test]
    fn rejects_take_zero_and_out_of_range_chance() {
        let b = pattern_block(vec![pool("movies")], vec![step("movies", 0)]);
        let err = validate_channel(&dummy_path(), &channel_with(vec![b])).unwrap_err();
        assert!(format!("{err}").contains("take = 0"), "err = {err}");

        let mut b = pattern_block(vec![pool("movies")], vec![step("movies", 1)]);
        b.pattern[0].chance = 1.5;
        let err = validate_channel(&dummy_path(), &channel_with(vec![b])).unwrap_err();
        assert!(format!("{err}").contains("chance"), "err = {err}");
    }

    #[test]
    fn rejects_a_duplicate_pool_name_across_blocks() {
        // Pool names key the .resume sidecar, so a channel-wide collision would
        // make two pools share one cursor.
        let a = pattern_block(vec![pool("shows")], vec![step("shows", 1)]);
        let b = pattern_block(vec![pool("shows")], vec![step("shows", 1)]);
        let err = validate_channel(&dummy_path(), &channel_with(vec![a, b])).unwrap_err();
        assert!(format!("{err}").contains("already used"), "err = {err}");
    }

    #[test]
    fn rejects_cycles_out_of_range() {
        let mut b = pattern_block(vec![pool("movies")], vec![step("movies", 1)]);
        b.cycles = Some(0);
        let err = validate_channel(&dummy_path(), &channel_with(vec![b])).unwrap_err();
        assert!(format!("{err}").contains("cycles"), "err = {err}");

        let mut b = pattern_block(vec![pool("movies")], vec![step("movies", 1)]);
        b.cycles = Some(MAX_CYCLES + 1);
        let err = validate_channel(&dummy_path(), &channel_with(vec![b])).unwrap_err();
        assert!(format!("{err}").contains("maximum"), "err = {err}");
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

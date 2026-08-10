use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::block::Duplicates;
use super::channel::{ChannelConfig, TasteScope};
use super::constraints::{Constraints, NoRepeatWithin};
use super::order::Order;
use super::pool::{Rotate, Take, TakeFrom};
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
/// silently misbehaves: both channels fight over the `.resume` sidecar and
/// each startup prunes the other's `.durations.json` cache, forcing re-probes
/// on every restart. Play history no longer lives in this folder (#111) —
/// it is keyed by channel name in the shared `history.db`, so two channels
/// only collide on it if their *names* also collide, which is rejected
/// elsewhere.
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

    validate_taste_scope(path, channel)?;

    // Pool names key the `.resume` sidecar, so they must be unique across the
    // whole channel — that is what lets the sidecar survive blocks being
    // reordered without a block index in the key.
    let mut pool_names: HashSet<&str> = HashSet::new();

    for (idx, include) in channel.rule.blocks.iter().enumerate() {
        let bad = |message: String| ConfigError::Validation {
            path: path.to_path_buf(),
            message: format!("block #{idx}: {message}"),
        };

        // Checked before the pattern/entries split: `[constraints]` applies to
        // either kind of block, so this must not sit on one branch only. On a
        // pattern block it is the default its pools inherit (#115) rather than a
        // rule over the block's own list, but it has to be a legal table either
        // way — an inherited nonsense value is still nonsense.
        validate_constraints(&include.constraints(), &bad)?;

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

/// `taste_scope` and `user` have to agree (#112).
///
/// Both halves are rejected rather than papered over, because the failure they
/// prevent is silent: a `single_user` channel with no `user` would fall back to
/// the server-wide pool and rank a personal channel against everyone's viewing,
/// looking exactly like a working channel. And a `user` under `all_users` reads
/// as a personal channel to whoever wrote it while behaving as the house one.
///
/// A `user` that names nobody on the Plex server is *not* caught here — this is
/// a config pass with no network. Tautulli answers an unknown user with an empty
/// history, which surfaces at runtime as `rows=0` on the `tautulli.history`
/// line for that scope.
fn validate_taste_scope(path: &Path, channel: &ChannelConfig) -> Result<(), ConfigError> {
    let Some(scoring) = channel.scoring.as_ref() else {
        return Ok(());
    };

    let bad = |message: &str| {
        Err(ConfigError::Validation {
            path: path.to_path_buf(),
            message: format!("scoring: {message}"),
        })
    };

    match (scoring.taste_scope, scoring.user.as_deref()) {
        (TasteScope::SingleUser, None) => bad(
            "taste_scope: single_user requires `user` — the Tautulli username or \
             user id whose watch history this channel ranks against",
        ),
        (TasteScope::SingleUser, Some(u)) if u.trim().is_empty() => {
            bad("`user` cannot be empty under taste_scope: single_user")
        }
        (TasteScope::AllUsers, Some(_)) => {
            bad("`user` is only meaningful with taste_scope: single_user; \
             the default all_users pools every user's history")
        }
        _ => Ok(()),
    }
}

/// Semantic checks for one `[constraints]` table (#73) — a block's, for either
/// block kind, or a pool's own (#115).
///
/// Every refusal here is the same shape: a constraint the config states but that
/// would not actually constrain anything. Silently doing nothing is the failure
/// mode worth preventing — the author reads the file, sees a rule, and believes
/// it holds.
fn validate_constraints(
    c: &Constraints,
    bad: &impl Fn(String) -> ConfigError,
) -> Result<(), ConfigError> {
    // An explicit `0` — positions or duration — reads as "on" but spaces
    // nothing apart.
    let zero = match c.no_repeat_within {
        Some(NoRepeatWithin::Positions(0)) => true,
        Some(NoRepeatWithin::Duration(d)) => d.is_zero(),
        _ => false,
    };
    if zero {
        return Err(bad(
            "no_repeat_within must be > 0 (omit it to leave the block unconstrained)".into(),
        ));
    }
    if c.separate_min_gap == Some(0) {
        return Err(bad(
            "separate_min_gap must be > 0 (omit separate_by to leave the block unconstrained)"
                .into(),
        ));
    }

    // A gap with no field to separate on. Accepting it would silently ignore a
    // line the author clearly meant to do something.
    if c.separate_min_gap.is_some() && c.separate_by.is_none() {
        return Err(bad(
            "separate_min_gap needs separate_by to say which field to separate on".into(),
        ));
    }

    // A field that isn't multi-valued has nothing to intersect, so separating on
    // it would quietly never fire.
    if let Some(field) = &c.separate_by {
        crate::catalog::TagNs::for_separate_by(field).map_err(bad)?;
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
    if let Some(order) = &include.order
        && *order != Order::Manual
    {
        return Err(bad(format!(
            "order {order:?} conflicts with `pattern` — the pattern IS the ordering; \
             sort inside a pool with its own `order` instead",
        )));
    }
    if include.duplicates == Some(Duplicates::Collapse) {
        return Err(bad(
            "duplicates = \"collapse\" conflicts with `pattern` — collapse would delete \
             the repeats a looping pool produces; a pattern block is always \"keep\""
                .into(),
        ));
    }
    if include.fallback.is_some() {
        return Err(bad(
            "fallback conflicts with `pattern` — a pattern block's pools already have their \
             own empty-pool policy (`on_short`); block-level `fallback` only applies to an \
             entries block"
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
        // A pool names exactly one source of items. Both is ambiguous about
        // which one wins; neither leaves the pool with nothing to resolve.
        match (&pool.expr, &pool.plugin) {
            (Some(expr), None) => {
                if expr.trim().is_empty() {
                    return Err(bad(format!("pool {:?} has an empty expr", pool.name)));
                }
            }
            (None, Some(plugin)) => {
                if plugin.as_os_str().is_empty() {
                    return Err(bad(format!(
                        "pool {:?} has an empty plugin path",
                        pool.name
                    )));
                }
                // The plugin returns its set already ranked; a re-sort would
                // discard the ranking it exists to produce.
                if pool.order.is_some() {
                    return Err(bad(format!(
                        "pool {:?} sets both `plugin` and `order` — a plugin returns its \
                         set already ranked, so ordering it again would discard the \
                         ranking; drop `order`",
                        pool.name
                    )));
                }
                // Same reasoning one level up: the series sequence a plugin
                // pool gets is the ranking's, and re-sequencing it throws the
                // ranking away just as surely as re-sorting the items would.
                if pool.bucket_order.is_some() {
                    return Err(bad(format!(
                        "pool {:?} sets both `plugin` and `bucket_order` — the order its \
                         series come up in is the plugin's ranking, so re-sequencing them \
                         would discard it; drop `bucket_order`",
                        pool.name
                    )));
                }
            }
            (Some(_), Some(_)) => {
                return Err(bad(format!(
                    "pool {:?} sets both `expr` and `plugin` — a pool draws its items \
                     from one or the other",
                    pool.name
                )));
            }
            (None, None) => {
                return Err(bad(format!(
                    "pool {:?} sets neither `expr` nor `plugin` — it has no items to draw",
                    pool.name
                )));
            }
        }
        // A pool's own `[constraints]` replaces the block's wholesale, so it has
        // to stand on its own — and the error names the pool, since that is the
        // line the author has to go and fix (#115).
        if let Some(c) = &pool.constraints {
            let pool_bad = |m: String| bad(format!("pool {:?}: {m}", pool.name));
            validate_constraints(c, &pool_bad)?;
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
        if step.take == Take::Count(0) {
            return Err(bad(format!("pattern step #{step_idx} has take = 0")));
        }
        // `all` empties the bucket the visit picked, and `rotate = "slot"` picks
        // a new bucket for every single item — so there is no one bucket for
        // `all` to mean. Refuse the pair rather than pick a reading.
        if step.take == Take::All
            && let Some(pool) = include.pools.iter().find(|p| p.name == step.pool)
            && pool.rotate == Rotate::Slot
        {
            return Err(bad(format!(
                "pattern step #{step_idx} has take = \"all\" but pool {:?} sets \
                 rotate = \"slot\", which changes series every item — \"all\" has \
                 no single series to empty. Use rotate = \"visit\", or an explicit count",
                step.pool
            )));
        }
        // `all` empties the series whatever the cut position, so a `from` beside
        // it is a line the author wrote that does nothing. Refused on the same
        // grounds as a constraint that would not constrain.
        if step.take == Take::All && step.from != TakeFrom::Start {
            return Err(bad(format!(
                "pattern step #{step_idx} has take = \"all\" and from = {:?} — \
                 \"all\" empties the series, so there is no slice to move. Drop \
                 one of the two",
                step.from
            )));
        }
        if !(0.0..=1.0).contains(&step.chance) {
            return Err(bad(format!(
                "pattern step #{step_idx} has chance = {}, outside 0.0–1.0",
                step.chance
            )));
        }
    }

    // A take-all step and adjacency constraints cannot share a block.
    //
    // `RuleConfig::adjacency_reach` converts "N draws of this pool" into "N
    // positions of aired schedule" so the generation seam can be checked, and
    // it does that arithmetic from the pattern's `take` numbers while the file
    // is being loaded — before anything has resolved a pool against the
    // catalog. `all` has no number there: how many items it emits depends on
    // which bucket the visit picks. Sizing the seam from the counted steps
    // alone would report a tail shorter than the schedule actually lays, and a
    // repeat would cross a generation boundary with nothing logged. Refusing
    // the pair keeps the seam honest; sizing one across a take-all step is its
    // own piece of work.
    if include.pattern.iter().any(|s| s.take == Take::All) {
        let constrained = include
            .constraints
            .is_some()
            .then(|| "the block".to_string())
            .or_else(|| {
                include
                    .pools
                    .iter()
                    .find(|p| p.constraints.is_some())
                    .map(|p| format!("pool {:?}", p.name))
            });
        if let Some(who) = constrained {
            return Err(bad(format!(
                "this block has a pattern step with take = \"all\" and {who} declares \
                 `constraints` — how much airtime a take-all step lays is not known \
                 until its pool is resolved, so the generation seam cannot be sized \
                 for it. Drop the constraints, or give every step an explicit count",
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
        Pool, Rotate, RuleConfig, Select, SourceConfig, StationConfig,
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
            constraints: None,
            entries,
            fallback: None,
            pools: Vec::new(),
            pattern: Vec::new(),
            cycles: None,
            mode: Mode::All,
            order: Some(Order::Manual),
            filter: None,
        }
    }

    fn pool(name: &str) -> Pool {
        Pool {
            name: name.into(),
            expr: Some(format!("item.type == \"{name}\"")),
            plugin: None,
            order: None,
            bucket_order: None,
            group_by: Default::default(),
            select: Select::default(),
            rotate: Rotate::default(),
            advance: Advance::default(),
            on_short: OnShort::default(),
            constraints: None,
            config: None,
        }
    }

    fn step(pool: &str, take: usize) -> PatternStep {
        PatternStep {
            pool: pool.into(),
            take: Take::Count(take),
            from: TakeFrom::Start,
            chance: 1.0,
        }
    }

    fn step_all(pool: &str) -> PatternStep {
        PatternStep {
            pool: pool.into(),
            take: Take::All,
            from: TakeFrom::Start,
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
            scoring: None,
            name: None,
            window_days: 1,
            chunk_hours: 24,
            roll_interval: Duration::from_secs(3600),
            retention_days: 1,
            seed: None,
            anchor: None,
            rule: RuleConfig { blocks },
            overlay: None,
        }
    }

    /// A channel with a `scoring:` block scoped to one named person.
    fn channel_scoped_to(taste_scope: TasteScope, user: Option<&str>) -> ChannelConfig {
        let mut c = channel_with(vec![inline_block(vec![item_entry("a")])]);
        c.scoring = Some(super::super::channel::ScoringConfig {
            taste_scope,
            user: user.map(str::to_string),
            ..Default::default()
        });
        c
    }

    /// The silent failure this rule exists to stop: `single_user` with nobody
    /// named falls back to the pooled history, so a channel built to follow one
    /// person would rank against all twenty accounts and still look healthy.
    #[test]
    fn rejects_single_user_with_no_user_named() {
        let c = channel_scoped_to(TasteScope::SingleUser, None);
        let err = validate_channel(&dummy_path(), &c).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("requires `user`"), "msg = {msg}");
    }

    /// A whitespace-only `user` reaches Tautulli as a filter matching nobody,
    /// which returns an empty history — indistinguishable from a quiet week.
    #[test]
    fn rejects_a_blank_user() {
        let c = channel_scoped_to(TasteScope::SingleUser, Some("   "));
        let err = validate_channel(&dummy_path(), &c).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("cannot be empty"), "msg = {msg}");
    }

    /// The other direction: naming a user while leaving the scope pooled reads
    /// as a personal channel and behaves as the house one.
    #[test]
    fn rejects_a_user_under_the_pooled_scope() {
        let c = channel_scoped_to(TasteScope::AllUsers, Some("carol"));
        let err = validate_channel(&dummy_path(), &c).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("only meaningful"), "msg = {msg}");
    }

    #[test]
    fn accepts_a_single_user_channel_that_names_someone() {
        let c = channel_scoped_to(TasteScope::SingleUser, Some("carol"));
        assert!(validate_channel(&dummy_path(), &c).is_ok());
    }

    /// #112 must not change what an already-written config means: every channel
    /// in this repo predates the field and must still validate and still rank
    /// against the pooled history.
    #[test]
    fn a_channel_that_says_nothing_stays_server_wide() {
        let c = channel_with(vec![inline_block(vec![item_entry("a")])]);
        assert!(validate_channel(&dummy_path(), &c).is_ok());
        assert_eq!(c.history_scope(), crate::tautulli::HistoryScope::AllUsers);

        let pooled = channel_scoped_to(TasteScope::AllUsers, None);
        assert_eq!(
            pooled.history_scope(),
            crate::tautulli::HistoryScope::AllUsers,
        );
    }

    #[test]
    fn a_single_user_channel_resolves_to_that_persons_scope() {
        let c = channel_scoped_to(TasteScope::SingleUser, Some("carol"));
        assert_eq!(
            c.history_scope(),
            crate::tautulli::HistoryScope::User("carol".into()),
        );
    }

    /// Padding survives validation — only an entirely blank `user` is rejected —
    /// so it has to be stripped on the way to the scope. Left in, `" carol "`
    /// reaches Tautulli as `user=+carol+`, matches nobody, and the channel
    /// ranks against an empty history while looking perfectly healthy.
    #[test]
    fn a_padded_user_is_trimmed_before_it_becomes_a_scope() {
        let c = channel_scoped_to(TasteScope::SingleUser, Some("  carol  "));
        assert!(
            validate_channel(&dummy_path(), &c).is_ok(),
            "padding is not itself a config error, which is why trimming matters",
        );
        assert_eq!(
            c.history_scope(),
            crate::tautulli::HistoryScope::User("carol".into()),
        );
    }

    #[test]
    fn rejects_empty_channels() {
        let s = StationConfig {
            tz: "UTC".into(),
            output_base: PathBuf::from("out"),
            channels: vec![],
            source_roots: vec![],
            catalog_path: None,
            catalog_refresh_secs: 900,
            full_sweep_after_secs: 86_400,
            device_id: None,
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
            catalog_refresh_secs: 900,
            full_sweep_after_secs: 86_400,
            device_id: None,
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
            catalog_refresh_secs: 900,
            full_sweep_after_secs: 86_400,
            device_id: None,
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
    fn rejects_zero_no_repeat_within() {
        let mut block = inline_block(vec![item_entry("a")]);
        block.constraints = Some(crate::config::Constraints {
            no_repeat_within: Some(NoRepeatWithin::Positions(0)),
            separate_by: None,
            separate_min_gap: None,
        });
        let err = validate_channel(&dummy_path(), &channel_with(vec![block])).unwrap_err();
        assert!(format!("{err}").contains("no_repeat_within must be > 0"));
    }

    fn with_constraints(c: crate::config::Constraints) -> ChannelConfig {
        let mut block = inline_block(vec![item_entry("a"), item_entry("b")]);
        block.constraints = Some(c);
        channel_with(vec![block])
    }

    #[test]
    fn rejects_a_zero_separate_min_gap() {
        let err = validate_channel(
            &dummy_path(),
            &with_constraints(crate::config::Constraints {
                no_repeat_within: None,
                separate_by: Some("cast".into()),
                separate_min_gap: Some(0),
            }),
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("separate_min_gap must be > 0"),
            "msg = {err}"
        );
    }

    #[test]
    fn rejects_a_separate_min_gap_with_no_field() {
        // A gap with nothing to separate on would be silently ignored — which
        // is exactly the failure the author could not see from the file.
        let err = validate_channel(
            &dummy_path(),
            &with_constraints(crate::config::Constraints {
                no_repeat_within: None,
                separate_by: None,
                separate_min_gap: Some(2),
            }),
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("needs separate_by"),
            "msg = {err}"
        );
    }

    #[test]
    fn rejects_separating_on_a_field_that_is_not_multi_valued() {
        // `title` is a single string, so intersecting it would never fire.
        let err = validate_channel(
            &dummy_path(),
            &with_constraints(crate::config::Constraints {
                no_repeat_within: None,
                separate_by: Some("title".into()),
                separate_min_gap: None,
            }),
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("not a multi-valued field"),
            "msg = {err}"
        );
    }

    #[test]
    fn accepts_separate_by_on_a_multi_valued_field() {
        validate_channel(
            &dummy_path(),
            &with_constraints(crate::config::Constraints {
                no_repeat_within: Some(NoRepeatWithin::Positions(1)),
                separate_by: Some("cast".into()),
                separate_min_gap: Some(3),
            }),
        )
        .unwrap();
    }

    #[test]
    fn adjacency_reach_is_the_widest_gap_across_blocks() {
        let mut a = inline_block(vec![item_entry("a")]);
        a.constraints = Some(crate::config::Constraints {
            no_repeat_within: Some(NoRepeatWithin::Positions(2)),
            separate_by: None,
            separate_min_gap: None,
        });
        let mut b = inline_block(vec![item_entry("b")]);
        b.constraints = Some(crate::config::Constraints {
            no_repeat_within: None,
            separate_by: Some("cast".into()),
            separate_min_gap: Some(5),
        });
        assert_eq!(channel_with(vec![a, b]).adjacency_reach(), 5);
    }

    /// A pool's gap counts *its own* draws, so the aired history it needs is
    /// longer than the number authored: `movies` is drawn twice per five-item
    /// cycle, so reaching back 5 draws is three cycles — fifteen aired items.
    /// Sizing the tail at 5 would let the seam check silently see too little.
    #[test]
    fn adjacency_reach_converts_a_pool_gap_into_channel_positions() {
        let mut movies = pool("movies");
        movies.constraints = Some(crate::config::Constraints {
            no_repeat_within: Some(NoRepeatWithin::Positions(5)),
            separate_by: None,
            separate_min_gap: None,
        });
        let block = pattern_block(
            vec![movies, pool("shows")],
            vec![step("movies", 2), step("shows", 3)],
        );
        assert_eq!(channel_with(vec![block]).adjacency_reach(), 15);
    }

    /// A pattern block's own `[constraints]` is the default its pools inherit,
    /// so it reaches the seam through them — and is converted the same way.
    #[test]
    fn adjacency_reach_counts_a_block_constraint_a_pattern_block_lends_its_pools() {
        let mut block = pattern_block(
            vec![pool("movies"), pool("shows")],
            vec![step("movies", 2), step("shows", 3)],
        );
        block.constraints = Some(crate::config::Constraints {
            no_repeat_within: Some(NoRepeatWithin::Positions(2)),
            separate_by: None,
            separate_min_gap: None,
        });
        // `shows` draws 3 per cycle, so 2 of its draws fit inside one cycle;
        // `movies` draws 2, so 2 draws is also one cycle. One cycle is 5 items.
        assert_eq!(channel_with(vec![block]).adjacency_reach(), 5);
    }

    /// A pool naming a constraint that constrains nothing is refused exactly as
    /// a block naming it is — and the message says which pool, since that is the
    /// line the author has to go and fix.
    #[test]
    fn rejects_a_pool_constraint_that_constrains_nothing() {
        let mut movies = pool("movies");
        movies.constraints = Some(crate::config::Constraints {
            no_repeat_within: Some(NoRepeatWithin::Positions(0)),
            separate_by: None,
            separate_min_gap: None,
        });
        let block = pattern_block(vec![movies], vec![step("movies", 1)]);
        let err = validate_channel(&dummy_path(), &channel_with(vec![block])).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("no_repeat_within must be > 0"), "msg = {msg}");
        assert!(msg.contains("pool \"movies\""), "msg = {msg}");
    }

    /// The block-level table stays legal on a pattern block — it is what pools
    /// declaring none inherit, so validating it is still the only chance to
    /// catch a nonsense value before every pool takes it.
    #[test]
    fn rejects_a_nonsense_block_constraint_on_a_pattern_block() {
        let mut block = pattern_block(vec![pool("movies")], vec![step("movies", 1)]);
        block.constraints = Some(crate::config::Constraints {
            no_repeat_within: None,
            separate_by: Some("title".into()),
            separate_min_gap: None,
        });
        let err = validate_channel(&dummy_path(), &channel_with(vec![block])).unwrap_err();
        assert!(
            format!("{err}").contains("not a multi-valued field"),
            "msg = {err}"
        );
    }

    #[test]
    fn accepts_a_positive_no_repeat_within() {
        let mut block = inline_block(vec![item_entry("a"), item_entry("b")]);
        block.constraints = Some(crate::config::Constraints {
            no_repeat_within: Some(NoRepeatWithin::Positions(1)),
            separate_by: None,
            separate_min_gap: None,
        });
        validate_channel(&dummy_path(), &channel_with(vec![block])).unwrap();
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
        b.order = Some(Order::Random);
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

    /// A pattern block's pools already have their own empty-pool policy
    /// (`on_short`) — block-level `fallback` is entries-block only.
    #[test]
    fn rejects_fallback_on_a_pattern_block() {
        let mut b = pattern_block(vec![pool("movies")], vec![step("movies", 1)]);
        b.fallback = Some(crate::config::Fallback::Item(Box::new(
            crate::config::ItemEntry {
                source: crate::config::SourceConfig::Lavfi {
                    params: "standby".into(),
                },
                in_point: None,
                out_point: Some(std::time::Duration::from_secs(30)),
                program: None,
            },
        )));
        let err = validate_channel(&dummy_path(), &channel_with(vec![b])).unwrap_err();
        assert!(
            format!("{err}").contains("conflicts with `pattern`"),
            "err = {err}"
        );
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

    /// `all` empties the series a visit picked; `rotate = "slot"` picks a new
    /// series for every item. There is no reading of the pair, so it is refused
    /// at load rather than resolved to whichever one the walk happens to do.
    #[test]
    fn rejects_take_all_on_a_slot_rotating_pool() {
        let mut p = pool("shows");
        p.rotate = Rotate::Slot;
        let b = pattern_block(vec![p], vec![step_all("shows")]);
        let err = validate_channel(&dummy_path(), &channel_with(vec![b])).unwrap_err();
        assert!(
            format!("{err}").contains("rotate = \"slot\""),
            "err = {err}"
        );

        // The same pool with an explicit count is fine.
        let mut p = pool("shows");
        p.rotate = Rotate::Slot;
        let b = pattern_block(vec![p], vec![step("shows", 3)]);
        assert!(validate_channel(&dummy_path(), &channel_with(vec![b])).is_ok());
    }

    /// A take-all step lays an amount of schedule nothing can size until the
    /// pool is resolved, so the seam arithmetic in `RuleConfig::adjacency_reach`
    /// cannot cover it. Refused whichever level declares the constraints.
    #[test]
    fn rejects_take_all_beside_constraints() {
        let gap = Constraints {
            no_repeat_within: Some(NoRepeatWithin::Positions(3)),
            separate_by: None,
            separate_min_gap: None,
        };

        let mut p = pool("shows");
        p.constraints = Some(gap.clone());
        let b = pattern_block(vec![p], vec![step_all("shows")]);
        let err = validate_channel(&dummy_path(), &channel_with(vec![b])).unwrap_err();
        assert!(format!("{err}").contains("pool \"shows\""), "err = {err}");
        assert!(format!("{err}").contains("take = \"all\""), "err = {err}");

        let mut b = pattern_block(vec![pool("shows")], vec![step_all("shows")]);
        b.constraints = Some(gap);
        let err = validate_channel(&dummy_path(), &channel_with(vec![b])).unwrap_err();
        assert!(format!("{err}").contains("the block"), "err = {err}");

        // Unconstrained, the same take-all block loads.
        let b = pattern_block(vec![pool("shows")], vec![step_all("shows")]);
        assert!(validate_channel(&dummy_path(), &channel_with(vec![b])).is_ok());
    }

    /// `all` empties the series, so a cut position beside it is a line that
    /// does nothing — refused rather than silently ignored.
    #[test]
    fn rejects_a_cut_position_beside_take_all() {
        let mut b = pattern_block(vec![pool("shows")], vec![step_all("shows")]);
        b.pattern[0].from = TakeFrom::End;
        let err = validate_channel(&dummy_path(), &channel_with(vec![b])).unwrap_err();
        assert!(
            format!("{err}").contains("empties the series"),
            "err = {err}"
        );
    }

    /// The series sequence a plugin pool gets is its ranking, so re-sequencing
    /// it discards the ranking exactly as re-sorting the items would — the same
    /// refusal `order` already carries, one level up.
    #[test]
    fn rejects_bucket_order_on_a_plugin_pool() {
        let mut p = pool("shows");
        p.expr = None;
        p.plugin = Some(PathBuf::from("../plugins/taste-engine.rhai"));
        p.bucket_order = Some(Order::Random);
        let b = pattern_block(vec![p], vec![step("shows", 3)]);
        let err = validate_channel(&dummy_path(), &channel_with(vec![b])).unwrap_err();
        assert!(format!("{err}").contains("bucket_order"), "err = {err}");
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

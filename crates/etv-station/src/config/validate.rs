use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, SystemTime};

use super::block::Duplicates;
use super::channel::{ChannelConfig, TasteScope};
use super::constraints::{Constraints, NoRepeatWithin};
use super::order::Order;
use super::pool::{DatastoreGrant, Pool, Rotate, ShowGroup, Take, TakeFrom};
use super::rule::BlockInclude;
use super::station::StationConfig;
use crate::errors::ConfigError;
use crate::pattern::{MAX_CYCLES, MAX_TAKE};

/// A datastore that stops being republished stays openable — the file is
/// still there, still at a schema version this build understands — so
/// `Reader::open` succeeding proves nothing about whether the sweep that
/// writes it is still running. Two missed daily publishes is the line
/// between "ran a little late" and "the pipeline is down and nobody's
/// looking" (#256).
const DATASTORE_STALE_AFTER: Duration = Duration::from_secs(48 * 60 * 60);

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
    validate_show_groups(path, &channel.groups)?;

    // A `plugin:` path means what it means relative to the channel config
    // file (see `score::ScoreEnv::resolve_path`, which every generation-time
    // resolution goes through) — matched here so a load-time hook refusal
    // names the same script a running channel would actually load.
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));

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
            validate_pattern_block(include, &mut pool_names, &channel.groups, base_dir, &bad)?;
            continue;
        }

        if include.is_sequencer() {
            validate_sequencer_block(include, &mut pool_names, &channel.groups, base_dir, &bad)?;
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

/// Structural checks for a channel's `groups:` declarations (#165) — name
/// hygiene only. Whether a declared member show actually exists is a catalog
/// question, checked at generation time in `resolve::validate_groups_against_catalog`
/// rather than here, since this pass runs with no catalog at all.
fn validate_show_groups(path: &Path, groups: &[ShowGroup]) -> Result<(), ConfigError> {
    let bad = |message: String| ConfigError::Validation {
        path: path.to_path_buf(),
        message,
    };
    let mut names: HashSet<&str> = HashSet::new();
    for group in groups {
        if group.name.trim().is_empty() {
            return Err(bad("a show group has an empty name".into()));
        }
        if !names.insert(group.name.as_str()) {
            return Err(bad(format!(
                "group name {:?} is declared more than once",
                group.name
            )));
        }
        if group.shows.is_empty() {
            return Err(bad(format!("group {:?} names no shows", group.name)));
        }
        for show in &group.shows {
            if show.trim().is_empty() {
                return Err(bad(format!(
                    "group {:?} has an empty show name",
                    group.name
                )));
            }
        }
    }
    Ok(())
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
    channel_groups: &[ShowGroup],
    base_dir: &Path,
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
    if include.filter.as_ref().is_some_and(|f| !f.is_empty()) {
        return Err(bad(
            "filter conflicts with `pattern` — the resolve pipeline applies a block's \
             `filter` to a flat `entries` list, never to a pattern's interleaved pools; \
             narrow a pool's own draw instead"
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

    let local = validate_block_pools(&include.pools, pool_names, channel_groups, base_dir, &bad)?;

    for (step_idx, step) in include.pattern.iter().enumerate() {
        if !local.contains(step.pool.as_str()) {
            return Err(bad(format!(
                "pattern step #{step_idx} names pool {:?}, which this block does not declare",
                step.pool
            )));
        }
        if let Take::Count(n) = step.take {
            if n == 0 {
                return Err(bad(format!("pattern step #{step_idx} has take = 0")));
            }
            if n > MAX_TAKE {
                return Err(bad(format!(
                    "pattern step #{step_idx} has take = {n}, which exceeds the maximum of \
                     {MAX_TAKE}"
                )));
            }
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

/// Semantic checks for a `pools` + `sequencer` block (#169): a block naming a
/// plugin script that draws its own timeline instead of walking `pattern`.
///
/// The refusals mirror [`validate_pattern_block`]'s exactly, because they
/// guard the same thing under a different name: a block field that would
/// silently contradict who actually owns the arrangement. A block-level
/// `order` would re-sort what the script decided; `duplicates = "collapse"`
/// could delete a repeat the script deliberately returned; `fallback` is
/// meaningless because a short sequencer timeline already has its own
/// handling (falls through to the channel's normal short-generation
/// behavior, never dead air); `cycles` has no pattern to loop.
fn validate_sequencer_block<'a>(
    include: &'a BlockInclude,
    pool_names: &mut HashSet<&'a str>,
    channel_groups: &[ShowGroup],
    base_dir: &Path,
    bad: &impl Fn(String) -> ConfigError,
) -> Result<(), ConfigError> {
    if !include.entries().is_empty() {
        return Err(bad(
            "a block is either an `entries` block or a `pools` + `sequencer` block, not both"
                .into(),
        ));
    }
    if !include.pattern.is_empty() {
        return Err(bad(
            "a block declares either `pattern` or `sequencer`, not both (issue #169 decision 3)"
                .into(),
        ));
    }
    if include.pools.is_empty() {
        return Err(bad("`sequencer` needs at least one `pools` entry".into()));
    }
    let sequencer = include
        .sequencer
        .as_ref()
        .expect("validate_sequencer_block is only reached when `sequencer` is set");
    if sequencer.as_os_str().is_empty() {
        return Err(bad("block has an empty `sequencer` path".into()));
    }
    if let Some(order) = &include.order
        && *order != Order::Manual
    {
        return Err(bad(format!(
            "order {order:?} conflicts with `sequencer` — the script decides the arrangement; \
             sort inside a pool with its own `order` instead",
        )));
    }
    if include.duplicates == Some(Duplicates::Collapse) {
        return Err(bad(
            "duplicates = \"collapse\" conflicts with `sequencer` — collapse could delete an \
             item the script deliberately repeated; a sequencer block is always \"keep\""
                .into(),
        ));
    }
    if include.fallback.is_some() {
        return Err(bad(
            "fallback conflicts with `sequencer` — a short sequencer timeline falls through to \
             the channel's normal short-generation handling; block-level `fallback` only \
             applies to an entries block"
                .into(),
        ));
    }
    if include.filter.as_ref().is_some_and(|f| !f.is_empty()) {
        return Err(bad(
            "filter conflicts with `sequencer` — the resolve pipeline applies a block's \
             `filter` to a flat `entries` list, never to a script's own timeline; narrow a \
             pool's own draw instead"
                .into(),
        ));
    }
    if let Some(n) = include.cycles {
        return Err(bad(format!(
            "cycles = {n} is meaningless on a `sequencer` block — there is no pattern to loop"
        )));
    }

    validate_plugin_declares_sequencer(sequencer, base_dir, bad)?;
    validate_block_pools(&include.pools, pool_names, channel_groups, base_dir, bad)?;

    Ok(())
}

/// Structural + hook checks for a block's `pools` (#72's own per-pool rules,
/// #159's `pool_provider` hook, #165's groups) — shared by a `pattern` block
/// and a `sequencer` block (#169), since a pool means the same thing to
/// either arrangement hook: exactly one source, its own legal `constraints`,
/// and a name unique across the whole channel. Returns the pool names this
/// block declares, for a pattern block's own step-name validation.
fn validate_block_pools<'a>(
    pools: &'a [Pool],
    pool_names: &mut HashSet<&'a str>,
    channel_groups: &[ShowGroup],
    base_dir: &Path,
    bad: &impl Fn(String) -> ConfigError,
) -> Result<HashSet<&'a str>, ConfigError> {
    let mut local: HashSet<&str> = HashSet::new();
    for pool in pools {
        if pool.name.trim().is_empty() {
            return Err(bad("a pool has an empty name".into()));
        }
        // A pool names exactly one source of items. Two is ambiguous about
        // which one wins; none leaves the pool with nothing to resolve.
        let source_count = [
            pool.expr.is_some(),
            pool.plugin.is_some(),
            !pool.groups.is_empty(),
        ]
        .into_iter()
        .filter(|&set| set)
        .count();
        if source_count == 0 {
            return Err(bad(format!(
                "pool {:?} sets none of `expr`, `plugin`, or `groups` — it has no items to draw",
                pool.name
            )));
        }
        if source_count > 1 {
            return Err(bad(format!(
                "pool {:?} sets more than one of `expr`, `plugin`, `groups` — a pool draws \
                 its items from exactly one",
                pool.name
            )));
        }
        if let Some(expr) = &pool.expr
            && expr.trim().is_empty()
        {
            return Err(bad(format!("pool {:?} has an empty expr", pool.name)));
        }
        if let Some(plugin) = &pool.plugin {
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
            // The pool's own candidate queries (#210), when it wrote any. They
            // replace the script's `sources()` rather than adding to it, so an
            // empty table is not "the script's, plus nothing" — it is a pool
            // with no candidates at all, and saying so here beats a plugin
            // failing on an empty `ctx.sets` mid-generation.
            if let Some(sources) = &pool.sources {
                if sources.is_empty() {
                    return Err(bad(format!(
                        "pool {:?} has an empty `sources` table — it replaces the plugin's \
                         own `sources()` wholesale, so an empty one leaves the plugin \
                         nothing to rank; drop the key to use the script's, or name at \
                         least one set",
                        pool.name
                    )));
                }
                for (name, cel) in sources {
                    if name.trim().is_empty() {
                        return Err(bad(format!(
                            "pool {:?} has a `sources` entry with an empty name — the name \
                             is what the plugin reads the set back as (`ctx.sets.<name>`)",
                            pool.name
                        )));
                    }
                    if cel.trim().is_empty() {
                        return Err(bad(format!(
                            "pool {:?} has an empty `sources` expression for {name:?}",
                            pool.name
                        )));
                    }
                }
            }
            // Checked last among this pool's plugin checks — it is the one
            // that reads the script off disk, so the cheaper structural checks
            // above get to report first when more than one thing is wrong.
            validate_plugin_declares_pool_provider(pool, plugin, base_dir, bad)?;
            validate_plugin_capabilities(pool, plugin, base_dir, bad)?;
        }
        // Refused rather than ignored: an `expr` or `groups` pool has no script
        // to hand sets to, so these expressions would never run, and a table of
        // CEL that silently does nothing looks exactly like one that works.
        if pool.plugin.is_none() && pool.sources.is_some() {
            return Err(bad(format!(
                "pool {:?} sets `sources` without a `plugin` — `sources` names the \
                 candidate sets a scorer plugin ranks, and a pool drawing from `expr` \
                 or `groups` has no script to hand them to; use `expr` to narrow this \
                 pool instead",
                pool.name
            )));
        }
        if !pool.groups.is_empty() {
            validate_pool_groups(pool, channel_groups, bad)?;
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
    Ok(local)
}

/// A pool naming a script for `plugin:` must be able to say the script does
/// that job (#159). The declaration lives in the script (`hooks()`), read
/// without running `sources()` or `pick()`, so this never touches the
/// catalog — a refusal here does not require a full generation.
///
/// Every refusal `declared_hooks` makes on its own — no hooks at all, an
/// unrecognized name — surfaces here too, named against the pool that reached
/// the script.
fn validate_plugin_declares_pool_provider(
    pool: &Pool,
    plugin: &Path,
    base_dir: &Path,
    bad: &impl Fn(String) -> ConfigError,
) -> Result<(), ConfigError> {
    let script_path = crate::score::resolve_plugin_path(base_dir, plugin);
    let hooks = crate::score::declared_hooks(&script_path)
        .map_err(|e| bad(format!("pool {:?}: {e}", pool.name)))?;
    if !hooks.iter().any(|h| h == "pool_provider") {
        return Err(bad(format!(
            "pool {:?} names plugin {} via `plugin:`, which requires the plugin to \
             declare the `pool_provider` hook, but it only declares: {}",
            pool.name,
            script_path.display(),
            hooks.join(", ")
        )));
    }
    Ok(())
}

/// A pool naming a script for `plugin:` grants it exactly the capabilities
/// (#167) the script's `capabilities()` declares — no more, no less.
///
/// Symmetry is enforced both ways: a capability the script declares that this
/// pool does not grant fails the load naming the plugin and the capability
/// (the config is missing a grant); a capability this pool grants that the
/// script never declared also fails, the other way round (the grant is
/// unasked-for — a typo or a stale config, either of which silently doing
/// nothing would hide). A named datastore grant that survives both checks is
/// then proven openable here, before any plugin ever runs — a station whose
/// channels grant none never reaches this code at all, and so opens no
/// connection.
fn validate_plugin_capabilities(
    pool: &Pool,
    plugin: &Path,
    base_dir: &Path,
    bad: &impl Fn(String) -> ConfigError,
) -> Result<(), ConfigError> {
    let script_path = crate::score::resolve_plugin_path(base_dir, plugin);
    let capabilities = crate::score::declared_capabilities(&script_path)
        .map_err(|e| bad(format!("pool {:?}: {e}", pool.name)))?;

    for cap in &pool.capabilities {
        if !crate::score::KNOWN_CAPABILITIES.contains(&cap.as_str()) {
            return Err(bad(format!(
                "pool {:?} grants unknown capability {cap:?} — known capabilities are: {}",
                pool.name,
                crate::score::KNOWN_CAPABILITIES.join(", ")
            )));
        }
    }

    // Simple capabilities and named datastores are spelled out of one
    // namespace on both sides, so the two-way symmetry check is one set
    // difference each way.
    let declared: HashSet<&str> = capabilities
        .iter()
        .map(crate::score::Capability::name)
        .collect();
    let granted: HashSet<&str> = pool
        .capabilities
        .iter()
        .map(String::as_str)
        .chain(pool.datastores.iter().map(|d| d.name.as_str()))
        .collect();

    if let Some(cap) = declared.difference(&granted).next() {
        return Err(bad(format!(
            "pool {:?} names plugin {}, which declares capability {cap:?} that this \
             pool's `capabilities`/`datastores` does not grant",
            pool.name,
            script_path.display()
        )));
    }
    if let Some(cap) = granted.difference(&declared).next() {
        return Err(bad(format!(
            "pool {:?} grants capability {cap:?} that plugin {} never declares — \
             a grant nobody asked for is a typo or a stale config",
            pool.name,
            script_path.display()
        )));
    }

    // Both sets agree now, so every grant here is also declared — prove each
    // named datastore is actually openable, and at the schema version the
    // vendored `plexdb-reader` crate understands, before any plugin ever
    // runs (#181). `Reader::open` names both the found and the supported
    // version when they disagree, and names the path when the file is
    // simply missing.
    for grant in &pool.datastores {
        plexdb_reader::Reader::open(&grant.path).map_err(|e| {
            bad(format!(
                "pool {:?}: datastore {:?} at {:?} could not be opened: {e}",
                pool.name, grant.name, grant.path
            ))
        })?;
        log_datastore_age(&pool.name, grant);
    }

    Ok(())
}

/// Logs a granted datastore's age at load — how long since its file was last
/// written, read from the file's mtime rather than anything inside it. Never
/// fails the load: an absent file already fails loudly a few lines up in
/// [`validate_plugin_capabilities`], via `Reader::open`, and a stale-but-open
/// file is the case this exists for — a stale ranking beats a dark channel
/// (#256).
///
/// `plexdb publish` renames the file into place atomically (plex-db-ex#15),
/// so mtime marks the publish moment rather than a partial write landing
/// mid-copy.
fn log_datastore_age(pool_name: &str, grant: &DatastoreGrant) {
    let modified = match std::fs::metadata(&grant.path).and_then(|m| m.modified()) {
        Ok(modified) => modified,
        Err(e) => {
            tracing::warn!(
                event = "datastore.age_unknown",
                pool = %pool_name,
                datastore = %grant.name,
                path = %grant.path,
                error = %e,
                "datastore opened, but its modification time could not be read; its age is unknown"
            );
            return;
        }
    };
    // A clock behind the file's mtime (skew, or a filesystem that reports a
    // time in the future) is treated as zero age rather than failing —
    // logging "unknown age" here would be a worse answer than logging "just
    // published" for a case this check is not trying to detect.
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::ZERO);

    tracing::info!(
        event = "datastore.age",
        pool = %pool_name,
        datastore = %grant.name,
        path = %grant.path,
        age_secs = age.as_secs(),
        "datastore {:?} age at load: {}",
        grant.name,
        humantime::format_duration(age),
    );

    if age > DATASTORE_STALE_AFTER {
        tracing::warn!(
            event = "datastore.stale",
            pool = %pool_name,
            datastore = %grant.name,
            path = %grant.path,
            age_secs = age.as_secs(),
            threshold_secs = DATASTORE_STALE_AFTER.as_secs(),
            "datastore {:?} at {:?} has not been updated in {} — past the {} staleness \
             threshold; still loading it, since a stale ranking beats a dark channel",
            grant.name,
            grant.path,
            humantime::format_duration(age),
            humantime::format_duration(DATASTORE_STALE_AFTER),
        );
    }
}

/// A block naming a script for `sequencer:` must be able to say the script
/// does that job (#159, #169) — the same check [`validate_plugin_declares_pool_provider`]
/// runs for a pool's `plugin:`, against the `sequencer` hook instead.
fn validate_plugin_declares_sequencer(
    sequencer: &Path,
    base_dir: &Path,
    bad: &impl Fn(String) -> ConfigError,
) -> Result<(), ConfigError> {
    let script_path = crate::score::resolve_plugin_path(base_dir, sequencer);
    let hooks = crate::score::declared_hooks(&script_path).map_err(bad)?;
    if !hooks.iter().any(|h| h == "sequencer") {
        return Err(bad(format!(
            "block names plugin {} via `sequencer:`, which requires the plugin to \
             declare the `sequencer` hook, but it only declares: {}",
            script_path.display(),
            hooks.join(", ")
        )));
    }
    Ok(())
}

/// A pool's `groups` (#165) must each name a channel-declared group, and the
/// groups a single pool combines must be disjoint — a show belonging to two
/// of them would have no single rotation domain to land in. (A show
/// belonging to two groups is fine on its own; only combining both in one
/// pool is rejected, and only here, since that is where the ambiguity would
/// actually bite.)
fn validate_pool_groups(
    pool: &Pool,
    channel_groups: &[ShowGroup],
    bad: &impl Fn(String) -> ConfigError,
) -> Result<(), ConfigError> {
    let mut seen_names: HashSet<&str> = HashSet::new();
    let mut owner: HashMap<&str, &str> = HashMap::new();
    for gname in &pool.groups {
        if !seen_names.insert(gname.as_str()) {
            return Err(bad(format!(
                "pool {:?} references group {:?} more than once",
                pool.name, gname
            )));
        }
        let group = channel_groups
            .iter()
            .find(|g| g.name == *gname)
            .ok_or_else(|| {
                bad(format!(
                    "pool {:?} references group {:?}, which this channel does not declare",
                    pool.name, gname
                ))
            })?;
        for show in &group.shows {
            if let Some(&prev) = owner.get(show.as_str()) {
                return Err(bad(format!(
                    "pool {:?} references groups {:?} and {:?}, which both list show {:?} — \
                     a pool's groups must not overlap",
                    pool.name, prev, gname, show
                )));
            }
            owner.insert(show.as_str(), gname.as_str());
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
        Entry::Item(Box::new(ItemEntry {
            source: SourceConfig::Lavfi {
                params: format!("src={id}"),
            },
            in_point: None,
            out_point: Some(Duration::from_secs(30)),
            program: None,
        }))
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
            sequencer: None,
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
            sources: None,
            groups: Vec::new(),
            order: None,
            bucket_order: None,
            group_by: Default::default(),
            select: Select::default(),
            rotate: Rotate::default(),
            advance: Advance::default(),
            on_short: OnShort::default(),
            constraints: None,
            config: None,
            capabilities: Vec::new(),
            datastores: Vec::new(),
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

    fn sequencer_block(pools: Vec<Pool>) -> BlockInclude {
        let mut b = inline_block(vec![]);
        b.pools = pools;
        b.sequencer = Some(PathBuf::from("script.rhai"));
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
            groups: Vec::new(),
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
            identity_roots: vec![],
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
            identity_roots: vec![],
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
            identity_roots: vec![],
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

    // ---- pool `sources` (#210) --------------------------------------------

    /// A pool with a `sources` table but no script to hand the sets to. The
    /// expressions would never run, and a table of CEL that silently does
    /// nothing looks exactly like one that works — so it is refused, and the
    /// message points at `expr`, which is how an expression pool narrows itself.
    #[test]
    fn rejects_sources_without_a_plugin() {
        let mut movies = pool("movies");
        movies.sources = Some(
            [("movies".to_string(), "item.type == \"movie\"".to_string())]
                .into_iter()
                .collect(),
        );
        let block = pattern_block(vec![movies], vec![step("movies", 1)]);
        let err = validate_channel(&dummy_path(), &channel_with(vec![block])).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("pool \"movies\""), "msg = {msg}");
        assert!(msg.contains("without a `plugin`"), "msg = {msg}");
        assert!(msg.contains("`expr`"), "msg = {msg}");
    }

    /// An empty table is not "the script's sources, plus nothing" — it replaces
    /// them, so it leaves the scorer with nothing to rank. Caught at load
    /// rather than as an empty `ctx.sets` mid-generation. This is refused
    /// before the script is read off disk, which is why the path need not
    /// exist.
    #[test]
    fn rejects_an_empty_sources_table() {
        let mut movies = pool("movies");
        movies.expr = None;
        movies.plugin = Some("taste.rhai".into());
        movies.sources = Some(std::collections::BTreeMap::new());
        let block = pattern_block(vec![movies], vec![step("movies", 1)]);
        let err = validate_channel(&dummy_path(), &channel_with(vec![block])).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("empty `sources` table"), "msg = {msg}");
        assert!(msg.contains("nothing to rank"), "msg = {msg}");
    }

    #[test]
    fn rejects_an_empty_sources_expression() {
        let mut movies = pool("movies");
        movies.expr = None;
        movies.plugin = Some("taste.rhai".into());
        movies.sources = Some(
            [("movies".to_string(), "   ".to_string())]
                .into_iter()
                .collect(),
        );
        let block = pattern_block(vec![movies], vec![step("movies", 1)]);
        let err = validate_channel(&dummy_path(), &channel_with(vec![block])).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("empty `sources` expression"), "msg = {msg}");
        assert!(msg.contains("\"movies\""), "msg = {msg}");
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

    /// #197: the resolve pipeline only ever runs `filter` over a flat
    /// `entries` list — a pattern block's list is an interleaved pool draw,
    /// so a `filter` here would otherwise sit unapplied (see resolve.rs).
    #[test]
    fn rejects_filter_on_a_pattern_block() {
        let mut b = pattern_block(vec![pool("movies")], vec![step("movies", 1)]);
        b.filter = Some(crate::config::Filter {
            seasons: Some(vec![1]),
            episode_ids: None,
        });
        let err = validate_channel(&dummy_path(), &channel_with(vec![b])).unwrap_err();
        assert!(
            format!("{err}").contains("conflicts with `pattern`"),
            "err = {err}"
        );
    }

    /// Same guard, same reason, on a `sequencer` block (#197).
    #[test]
    fn rejects_filter_on_a_sequencer_block() {
        let mut b = sequencer_block(vec![pool("movies")]);
        b.filter = Some(crate::config::Filter {
            seasons: Some(vec![1]),
            episode_ids: None,
        });
        let err = validate_channel(&dummy_path(), &channel_with(vec![b])).unwrap_err();
        assert!(
            format!("{err}").contains("conflicts with `sequencer`"),
            "err = {err}"
        );
    }

    /// An empty `[filter]` table is treated as absent — same rule the
    /// resolver applies — so it must not trip the pattern/sequencer guard.
    #[test]
    fn an_empty_filter_table_does_not_conflict_with_a_pattern_block() {
        let mut b = pattern_block(vec![pool("movies")], vec![step("movies", 1)]);
        b.filter = Some(crate::config::Filter {
            seasons: None,
            episode_ids: None,
        });
        validate_channel(&dummy_path(), &channel_with(vec![b])).unwrap();
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

    /// A `take` past `MAX_TAKE` would otherwise reach `Vec::with_capacity(take)`
    /// in the pattern draw — an allocation Rust aborts the whole daemon over
    /// rather than returning an error (#202).
    #[test]
    fn rejects_take_past_the_maximum() {
        let b = pattern_block(vec![pool("movies")], vec![step("movies", MAX_TAKE + 1)]);
        let err = validate_channel(&dummy_path(), &channel_with(vec![b])).unwrap_err();
        assert!(format!("{err}").contains("maximum"), "err = {err}");
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

    // ---- plugin hook declaration (#159) ------------------------------------

    fn plugin_pool(name: &str, plugin: PathBuf) -> Pool {
        let mut p = pool(name);
        p.expr = None;
        p.plugin = Some(plugin);
        p
    }

    fn plugin_channel(plugin: PathBuf) -> ChannelConfig {
        let block = pattern_block(vec![plugin_pool("shows", plugin)], vec![step("shows", 1)]);
        channel_with(vec![block])
    }

    /// Validate a channel whose one pool draws from a script with this body,
    /// written next to the channel config in a fresh temp directory.
    fn validate_plugin_script(body: &str) -> Result<(), ConfigError> {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("scorer.rhai");
        std::fs::write(&script, body).unwrap();
        validate_channel(&dir.path().join("channel.yaml"), &plugin_channel(script))
    }

    #[test]
    fn a_plugin_declaring_pool_provider_validates() {
        validate_plugin_script(r#"fn hooks() { ["pool_provider"] }"#).unwrap();
    }

    /// A relative `plugin:` path is checked against the channel config's own
    /// directory, exactly like generation-time resolution
    /// (`score::ScoreEnv::resolve_path`) — not the process CWD.
    #[test]
    fn a_relative_plugin_path_is_checked_against_the_channel_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("plugins")).unwrap();
        std::fs::write(
            dir.path().join("plugins/scorer.rhai"),
            r#"fn hooks() { ["pool_provider"] }"#,
        )
        .unwrap();
        let channel = plugin_channel(PathBuf::from("plugins/scorer.rhai"));
        validate_channel(&dir.path().join("channel.yaml"), &channel).unwrap();
    }

    /// Error case 1 from the issue: a pool names a script for a hook the
    /// script does not declare — refused at load, naming both the script and
    /// the hook that was wanted.
    #[test]
    fn a_pool_naming_a_script_that_does_not_declare_pool_provider_is_rejected() {
        let err = validate_plugin_script(r#"fn hooks() { ["sequencer"] }"#).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("pool_provider"), "msg = {msg}");
        assert!(msg.contains("scorer.rhai"), "msg = {msg}");
        assert!(msg.contains("sequencer"), "msg = {msg}");
    }

    /// Error case 2: a script declaring a hook name that does not exist fails
    /// at load, and the error lists the hook names that do.
    #[test]
    fn a_script_declaring_an_unknown_hook_is_rejected_and_lists_known_hooks() {
        let err = validate_plugin_script(r#"fn hooks() { ["scoreboard"] }"#).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("scoreboard"), "msg = {msg}");
        assert!(msg.contains("pool_provider"), "msg = {msg}");
        assert!(msg.contains("sequencer"), "msg = {msg}");
    }

    /// Error case 3: a script declaring no hooks at all fails at load, saying
    /// at least one is required.
    #[test]
    fn a_script_declaring_no_hooks_is_rejected() {
        let err = validate_plugin_script("fn hooks() { [] }").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("at least one"), "msg = {msg}");
    }

    /// The declaration is read without running the script's item loop — a
    /// `sources()`/`pick()` that would blow up (reads the catalog, throws)
    /// still validates cleanly, because load-time validation never calls
    /// either.
    #[test]
    fn hook_validation_never_runs_the_scoring_path() {
        validate_plugin_script(
            r#"
fn hooks() { ["pool_provider"] }
fn sources() { throw "sources must not run at load time"; }
fn pick(ctx) { throw "pick must not run at load time"; }
"#,
        )
        .unwrap();
    }

    // ---- plugin capability declaration (#167) ------------------------------

    /// A channel whose one pool grants exactly what the plugin declares.
    fn validate_plugin_script_with_capabilities(
        body: &str,
        capabilities: Vec<&str>,
        datastores: Vec<(&str, &str)>,
    ) -> Result<(), ConfigError> {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("scorer.rhai");
        std::fs::write(&script, body).unwrap();
        let mut pool = plugin_pool("shows", script);
        pool.capabilities = capabilities.into_iter().map(String::from).collect();
        pool.datastores = datastores
            .into_iter()
            .map(|(name, path)| crate::config::DatastoreGrant {
                name: name.into(),
                path: path.into(),
            })
            .collect();
        let block = pattern_block(vec![pool], vec![step("shows", 1)]);
        validate_channel(&dir.path().join("channel.yaml"), &channel_with(vec![block]))
    }

    #[test]
    fn a_plugin_declaring_no_capabilities_needs_no_grant() {
        validate_plugin_script_with_capabilities(
            r#"fn hooks() { ["pool_provider"] }"#,
            vec![],
            vec![],
        )
        .unwrap();
    }

    #[test]
    fn a_plugin_declaring_capabilities_the_pool_grants_validates() {
        validate_plugin_script_with_capabilities(
            r#"
fn hooks() { ["pool_provider"] }
fn capabilities() { ["catalog_read", "watch_history"] }
"#,
            vec!["catalog_read", "watch_history"],
            vec![],
        )
        .unwrap();
    }

    /// Error case: a plugin declares a capability the config does not grant —
    /// fails at load, naming both the plugin and the capability.
    #[test]
    fn a_declared_capability_the_pool_does_not_grant_is_rejected() {
        let err = validate_plugin_script_with_capabilities(
            r#"
fn hooks() { ["pool_provider"] }
fn capabilities() { ["catalog_read", "watch_history"] }
"#,
            vec!["catalog_read"],
            vec![],
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("watch_history"), "msg = {msg}");
        assert!(msg.contains("scorer.rhai"), "msg = {msg}");
    }

    /// Error case, the other way: the pool grants a capability the plugin
    /// never declared — a typo or stale config, refused rather than ignored.
    #[test]
    fn a_granted_capability_the_plugin_never_declared_is_rejected() {
        let err = validate_plugin_script_with_capabilities(
            r#"
fn hooks() { ["pool_provider"] }
fn capabilities() { ["catalog_read"] }
"#,
            vec!["catalog_read", "watch_history"],
            vec![],
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("watch_history"), "msg = {msg}");
        assert!(msg.contains("never declares"), "msg = {msg}");
    }

    #[test]
    fn a_pool_granting_an_unknown_capability_name_is_rejected() {
        let err = validate_plugin_script_with_capabilities(
            r#"
fn hooks() { ["pool_provider"] }
fn capabilities() { [] }
"#,
            vec!["telepathy"],
            vec![],
        )
        .unwrap_err();
        assert!(format!("{err}").contains("telepathy"));
    }

    /// A minimal but real plexdb store: `Reader::open` reads `schema_version`
    /// and nothing else, so that one table is the whole of what this check
    /// needs. The accessors — and the tables they read — are exercised in
    /// `tests/datastore_capability.rs`, not here.
    fn write_plexdb_fixture(path: &std::path::Path, version: i64) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE schema_version (version INTEGER NOT NULL);
             INSERT INTO schema_version (version) VALUES ({version});"
        ))
        .unwrap();
    }

    /// A declared-and-granted datastore whose path opens cleanly — a real
    /// plexdb store at the schema version `plexdb-reader` understands —
    /// validates, and the acceptance bar for #167: a station with no
    /// datastore grant anywhere never even reaches the open call.
    #[test]
    fn a_datastore_grant_that_opens_validates() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("taste.db");
        write_plexdb_fixture(&db, plexdb_reader::SUPPORTED_SCHEMA_VERSION);
        validate_plugin_script_with_capabilities(
            r#"
fn hooks() { ["pool_provider"] }
fn capabilities() { [#{ datastore: "taste_db" }] }
"#,
            vec![],
            vec![("taste_db", db.to_str().unwrap())],
        )
        .unwrap();
    }

    /// Error case: a grant naming a datastore that cannot be opened fails at
    /// load, naming the datastore and the underlying error.
    #[test]
    fn a_datastore_grant_that_cannot_be_opened_is_rejected() {
        let err = validate_plugin_script_with_capabilities(
            r#"
fn hooks() { ["pool_provider"] }
fn capabilities() { [#{ datastore: "taste_db" }] }
"#,
            vec![],
            vec![("taste_db", "/nonexistent/path/does/not/exist.db")],
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("taste_db"), "msg = {msg}");
        assert!(msg.contains("could not be opened"), "msg = {msg}");
    }

    /// Error case (#181's acceptance bar): a grant pointed at a store whose
    /// `schema_version` does not match what the vendored `plexdb-reader`
    /// crate was built against fails at load, naming both the version found
    /// in the store and the version this build understands — never a panic,
    /// and never an empty pool that looks like a scheduling result.
    #[test]
    fn a_datastore_grant_at_the_wrong_schema_version_is_rejected_naming_both_versions() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("taste.db");
        let wrong_version = plexdb_reader::SUPPORTED_SCHEMA_VERSION + 1;
        write_plexdb_fixture(&db, wrong_version);
        let err = validate_plugin_script_with_capabilities(
            r#"
fn hooks() { ["pool_provider"] }
fn capabilities() { [#{ datastore: "taste_db" }] }
"#,
            vec![],
            vec![("taste_db", db.to_str().unwrap())],
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("taste_db"), "msg = {msg}");
        assert!(msg.contains(&wrong_version.to_string()), "msg = {msg}");
        assert!(
            msg.contains(&plexdb_reader::SUPPORTED_SCHEMA_VERSION.to_string()),
            "msg = {msg}"
        );
    }

    // ---- datastore age at load (#256) --------------------------------------

    /// One captured `datastore.*` tracing event, with the structured fields
    /// the tests below check against — shared by all the age tests since
    /// each is asking a different question of the same event stream ("did
    /// this fire", "did it not fire", "what age did it carry").
    #[derive(Default, Clone, Debug)]
    struct DatastoreAgeEvent {
        event: String,
        age_secs: Option<u64>,
        path: Option<String>,
        datastore: Option<String>,
    }

    /// Runs `f` under a subscriber that records every `datastore.*` event
    /// emitted, in order, so a test can assert either that one fired with
    /// specific fields or that none fired at all.
    fn capture_datastore_age_events<T>(f: impl FnOnce() -> T) -> (T, Vec<DatastoreAgeEvent>) {
        use std::sync::{Arc, Mutex};

        use tracing::field::{Field, Visit};
        use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

        #[derive(Default)]
        struct FieldVisitor(DatastoreAgeEvent);
        impl Visit for FieldVisitor {
            fn record_u64(&mut self, field: &Field, value: u64) {
                match field.name() {
                    "age_secs" => self.0.age_secs = Some(value),
                    _ => {}
                }
            }
            fn record_str(&mut self, field: &Field, value: &str) {
                if field.name() == "event" {
                    self.0.event = value.to_string();
                }
            }
            // `%grant.path` and `%grant.name` (both `String`, via `Display`)
            // record via `record_debug`, not `record_str` — same reasoning
            // as the `collection` field captured in
            // `catalog/ingest/plex.rs`'s tracing tests.
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                match field.name() {
                    "path" => self.0.path = Some(format!("{value:?}")),
                    "datastore" => self.0.datastore = Some(format!("{value:?}")),
                    _ => {}
                }
            }
        }

        struct CaptureDatastoreEvents(Arc<Mutex<Vec<DatastoreAgeEvent>>>);
        impl<S: tracing::Subscriber> Layer<S> for CaptureDatastoreEvents {
            fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
                let mut visitor = FieldVisitor::default();
                event.record(&mut visitor);
                if visitor.0.event.starts_with("datastore.") {
                    self.0.lock().unwrap().push(visitor.0);
                }
            }
        }

        let seen = Arc::new(Mutex::new(Vec::new()));
        let subscriber =
            tracing_subscriber::registry().with(CaptureDatastoreEvents(Arc::clone(&seen)));
        let result = tracing::subscriber::with_default(subscriber, f);
        let events = seen.lock().unwrap().clone();
        (result, events)
    }

    /// Sets a file's mtime directly, bypassing the write that would normally
    /// bump it — the only way to make a fixture read as N hours old without
    /// actually waiting N hours.
    fn set_mtime(path: &std::path::Path, when: SystemTime) {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    /// A granted datastore logs its age at load, whatever that age is —
    /// first acceptance bar for #256. A freshly written fixture's age is
    /// seconds, not hours.
    #[test]
    fn a_datastore_grant_logs_its_age_at_load() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("taste.db");
        write_plexdb_fixture(&db, plexdb_reader::SUPPORTED_SCHEMA_VERSION);

        let (result, events) = capture_datastore_age_events(|| {
            validate_plugin_script_with_capabilities(
                r#"
fn hooks() { ["pool_provider"] }
fn capabilities() { [#{ datastore: "taste_db" }] }
"#,
                vec![],
                vec![("taste_db", db.to_str().unwrap())],
            )
        });
        result.unwrap();

        let age_event = events
            .iter()
            .find(|e| e.event == "datastore.age")
            .unwrap_or_else(|| panic!("no datastore.age event among {events:?}"));
        assert_eq!(age_event.datastore.as_deref(), Some("taste_db"));
        // A generous bound, not a precise one: this is proving the age came
        // from a fresh mtime rather than a cached/stale value, not timing the
        // test itself, so it should never flake on a loaded machine.
        assert!(age_event.age_secs.unwrap() < 60, "events = {events:?}");
        assert!(!events.iter().any(|e| e.event == "datastore.stale"));
    }

    /// A snapshot older than 48 hours logs a warning naming both the age and
    /// the path — second acceptance bar for #256.
    #[test]
    fn a_datastore_older_than_48_hours_logs_a_warning_naming_the_age_and_path() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("taste.db");
        write_plexdb_fixture(&db, plexdb_reader::SUPPORTED_SCHEMA_VERSION);
        let fifty_hours_ago = SystemTime::now() - Duration::from_secs(50 * 60 * 60);
        set_mtime(&db, fifty_hours_ago);

        let (result, events) = capture_datastore_age_events(|| {
            validate_plugin_script_with_capabilities(
                r#"
fn hooks() { ["pool_provider"] }
fn capabilities() { [#{ datastore: "taste_db" }] }
"#,
                vec![],
                vec![("taste_db", db.to_str().unwrap())],
            )
        });
        result.unwrap();

        let stale_event = events
            .iter()
            .find(|e| e.event == "datastore.stale")
            .unwrap_or_else(|| panic!("no datastore.stale event among {events:?}"));
        assert_eq!(stale_event.datastore.as_deref(), Some("taste_db"));
        assert_eq!(stale_event.path.as_deref(), Some(db.to_str().unwrap()));
        assert!(
            stale_event.age_secs.unwrap() >= 50 * 60 * 60,
            "events = {events:?}"
        );
    }

    /// No snapshot age ever refuses the load — verified at an age well past
    /// the 48-hour threshold. A stale ranking is better than a dark channel;
    /// the absent-file case already fails loudly via `Reader::open` (#181),
    /// unchanged by this ticket.
    #[test]
    fn no_snapshot_age_prevents_a_load_even_well_past_the_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("taste.db");
        write_plexdb_fixture(&db, plexdb_reader::SUPPORTED_SCHEMA_VERSION);
        let thirty_days_ago = SystemTime::now() - Duration::from_secs(30 * 24 * 60 * 60);
        set_mtime(&db, thirty_days_ago);

        validate_plugin_script_with_capabilities(
            r#"
fn hooks() { ["pool_provider"] }
fn capabilities() { [#{ datastore: "taste_db" }] }
"#,
            vec![],
            vec![("taste_db", db.to_str().unwrap())],
        )
        .unwrap();
    }

    /// A station granting no datastore anywhere reads no mtime and logs
    /// nothing about a snapshot — the same "never even reaches the check"
    /// bar #167 set for the open call itself.
    #[test]
    fn a_station_granting_no_datastore_reads_no_mtime_and_logs_nothing_about_a_snapshot() {
        let (result, events) = capture_datastore_age_events(|| {
            validate_plugin_script_with_capabilities(
                r#"fn hooks() { ["pool_provider"] }"#,
                vec![],
                vec![],
            )
        });
        result.unwrap();
        assert!(events.is_empty(), "events = {events:?}");
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

    // ---- named show groups (#165) ------------------------------------------

    fn grouped_pool(name: &str, groups: Vec<&str>) -> Pool {
        let mut p = pool(name);
        p.expr = None;
        p.groups = groups.into_iter().map(String::from).collect();
        p
    }

    fn channel_with_groups(groups: Vec<ShowGroup>, blocks: Vec<BlockInclude>) -> ChannelConfig {
        let mut c = channel_with(blocks);
        c.groups = groups;
        c
    }

    fn show_group(name: &str, shows: Vec<&str>) -> ShowGroup {
        ShowGroup {
            name: name.into(),
            shows: shows.into_iter().map(String::from).collect(),
        }
    }

    #[test]
    fn a_pool_sourced_from_a_declared_group_validates() {
        let block = pattern_block(
            vec![grouped_pool("rupaul", vec!["rupaul"])],
            vec![step_all("rupaul")],
        );
        let cfg = channel_with_groups(
            vec![show_group("rupaul", vec!["Drag Race", "All Stars"])],
            vec![block],
        );
        assert!(validate_channel(&dummy_path(), &cfg).is_ok());
    }

    #[test]
    fn a_pool_referencing_an_undeclared_group_is_rejected() {
        let block = pattern_block(
            vec![grouped_pool("rupaul", vec!["nonesuch"])],
            vec![step_all("rupaul")],
        );
        let cfg = channel_with_groups(vec![], vec![block]);
        let err = validate_channel(&dummy_path(), &cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("nonesuch"), "msg = {msg}");
        assert!(msg.contains("does not declare"), "msg = {msg}");
    }

    #[test]
    fn a_pool_combining_two_groups_that_share_a_show_is_rejected() {
        let block = pattern_block(
            vec![grouped_pool("bravo", vec!["rupaul", "below_deck"])],
            vec![step_all("bravo")],
        );
        let cfg = channel_with_groups(
            vec![
                show_group("rupaul", vec!["Drag Race", "Shared Show"]),
                show_group("below_deck", vec!["Below Deck", "Shared Show"]),
            ],
            vec![block],
        );
        let err = validate_channel(&dummy_path(), &cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Shared Show"), "msg = {msg}");
        assert!(
            msg.contains("rupaul") && msg.contains("below_deck"),
            "msg = {msg}"
        );
    }

    /// A show naming a franchise in two *different* pools (or unreferenced
    /// groups) is fine — only combining two overlapping groups in *one* pool
    /// is rejected, per the issue's own error case.
    #[test]
    fn two_groups_sharing_a_show_are_fine_until_one_pool_combines_them() {
        let a = grouped_pool("rupaul", vec!["rupaul"]);
        let b = grouped_pool("below_deck", vec!["below_deck"]);
        let block = pattern_block(vec![a, b], vec![step_all("rupaul"), step_all("below_deck")]);
        let cfg = channel_with_groups(
            vec![
                show_group("rupaul", vec!["Drag Race", "Shared Show"]),
                show_group("below_deck", vec!["Below Deck", "Shared Show"]),
            ],
            vec![block],
        );
        assert!(validate_channel(&dummy_path(), &cfg).is_ok());
    }

    #[test]
    fn a_pool_setting_both_expr_and_groups_is_rejected() {
        let mut p = grouped_pool("rupaul", vec!["rupaul"]);
        p.expr = Some("item.type == \"episode\"".into());
        let block = pattern_block(vec![p], vec![step_all("rupaul")]);
        let cfg = channel_with_groups(vec![show_group("rupaul", vec!["Drag Race"])], vec![block]);
        let err = validate_channel(&dummy_path(), &cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("more than one"), "msg = {msg}");
    }

    #[test]
    fn a_pool_setting_none_of_expr_plugin_or_groups_is_rejected() {
        let mut p = pool("rupaul");
        p.expr = None;
        let block = pattern_block(vec![p], vec![step_all("rupaul")]);
        let cfg = channel_with_groups(vec![], vec![block]);
        let err = validate_channel(&dummy_path(), &cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("none of"), "msg = {msg}");
    }

    #[test]
    fn a_group_declared_twice_is_rejected() {
        let cfg = channel_with_groups(
            vec![
                show_group("rupaul", vec!["Drag Race"]),
                show_group("rupaul", vec!["All Stars"]),
            ],
            vec![pattern_block(
                vec![pool("movies")],
                vec![step_all("movies")],
            )],
        );
        let err = validate_channel(&dummy_path(), &cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("more than once"), "msg = {msg}");
    }

    #[test]
    fn a_group_with_no_shows_is_rejected() {
        let cfg = channel_with_groups(
            vec![show_group("empty", vec![])],
            vec![pattern_block(
                vec![pool("movies")],
                vec![step_all("movies")],
            )],
        );
        let err = validate_channel(&dummy_path(), &cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("empty"), "msg = {msg}");
    }
}

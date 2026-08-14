//! Resolve a channel's block composition into a flat, ordered item list.
//!
//! The Phase C resolve pipeline (#71): for each `[[rule.blocks]]` it
//! **resolves entries → applies `filter` → applies `duplicates` → applies
//! `order` → (mode)**, producing the flat [`ResolvedItem`] list the sequencer
//! ([`crate::rule::Sequential`]) lays across the chunk window. Collapse runs
//! *before* order, so which duplicate survives is deterministic regardless of a
//! `random` shuffle. The blocks concatenate, then the adjacency constraint pass
//! ([`crate::constrain`], #73) runs once over the whole list, reaching back
//! across the generation seam via the play-history ledger.
//!
//! A **pattern** block sits out that final pass. Its constraints run pool by
//! pool inside [`crate::pattern`], before the interleave (#115) — reordering the
//! finished list would swap items between the pattern's slots and lose the shape
//! the pattern was written to build. Its `[constraints]` table is therefore the
//! default its pools inherit, not a rule over the block's own output.
//!
//! `query` entries resolve against the [`Catalog`] (#68 CEL→SQL) and each
//! resolved `entry_id` becomes a `ResolvedItem`; `order` is applied by the
//! order engine (#69). `collection` entries also resolve against the catalog
//! but arrive *already ordered* — their sequence is the collection's stored
//! `position`, so the order step leaves them alone (#107). A channel with no
//! catalog-backed entries and `manual` order needs no catalog, so `catalog` is
//! optional.
//!
//! Still rejected with a clear `unsupported` error (later issue #69):
//! `include` entries. The catalog is not yet opened by the daemon — until
//! that lands, query entries / non-`manual` order / `filter.seasons` only
//! resolve when a catalog is supplied (tests), and error at runtime.
//!
//! A block's `filter` (#197) narrows the resolved list right after entries
//! (and any `fallback` substitution) settle, before `duplicates`/`order` see
//! it: `seasons` keeps items whose catalog season is in the list (needs the
//! catalog), `episode_ids` keeps items whose id is in the list (no catalog
//! needed — it matches the same `entry_id`/derived id every other step keys
//! on). Setting both narrows further, never wider — an item survives only
//! when it satisfies every field the author set. Entries-block only, on the
//! same terms as `fallback` below: a pattern/sequencer block's list is an
//! interleaved pool draw, not a flat `entries` resolution this step could
//! run over, so `filter` is rejected there at validation.
//!
//! A block's optional `fallback` (#97) resolves **instead of** `entries` when
//! `entries` resolves to nothing eligible — the empty-set case a 24/7 channel
//! must not dead-air or error on. It runs through the exact same machinery as
//! a primary entry (`resolve_query` / `resolve_item`), never a parallel path,
//! and then through the block's own duplicates/order/mode exactly like a
//! primary entry's result would. Entries-block only — a pattern block's pools
//! already have their own empty-pool policy (`on_short`), so `fallback` is
//! rejected there at validation.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::time::Duration;

use ersatztv_playout::playout::{PlayoutItemSource, ProgramMetadata};
use time::OffsetDateTime;

use crate::catalog::{Catalog, TagNs, canonical_path, derive_entry_id};
use crate::config::{
    BlockInclude, ChannelConfig, CollectionEntry, Constraints, Duplicates, Entry, Fallback, Filter,
    ItemEntry, Mode, NoRepeatWithin, Order, QueryEntry, ShowGroup, SourceConfig,
};
use crate::constrain::{ItemKeys, Limits, RepeatGap};
use crate::errors::ConfigError;
use crate::guide::{GuideConfig, GuideFields};
use crate::resume::{GenerationState, ResumeMap};

/// A concrete, ordered item ready for duration probing and sequencing. Produced
/// by [`resolve_channel`] — the post-resolution counterpart to the on-disk
/// [`ItemEntry`]. Not `Clone` because `ProgramMetadata` (an ETV-next type) is
/// not `Clone`.
#[derive(Debug)]
pub struct ResolvedItem {
    pub id: String,
    pub source: SourceConfig,
    pub in_point: Option<Duration>,
    pub out_point: Option<Duration>,
    pub program: Option<ProgramMetadata>,
    /// How long the catalog says this item runs, when it came from one.
    ///
    /// Not used for scheduling in the happy path — a local file's length is
    /// read from the file itself, which is authoritative. It exists for the
    /// unhappy path: when the file cannot be opened or probed, this is the only
    /// remaining record of how long its slot was meant to be, and without it an
    /// unreadable file has no slot to put an error card in. `None` for inline
    /// items, which carry their length in `in_point`/`out_point` instead.
    pub catalog_duration: Option<Duration>,
    /// Set when this item's file could not be read and its slot was given over
    /// to an on-screen error card. Carried so the play-history ledger can record
    /// that the slot aired without claiming the film was watched.
    pub error_card: bool,
    /// Opaque per-airing data a `plugin:` pool attached to this pick (#166),
    /// carried untouched to `PlayoutItem::metadata` by
    /// `crate::rule::build_playout_item`. `None` for every item with no pool
    /// or plugin behind it, and for a plugin pick that attached nothing.
    pub metadata: Option<serde_json::Value>,
    /// The effective `guide:` config for this item (#158) — already cascaded
    /// item → block → channel, most specific field wins. `None` means no
    /// level said anything, which is not "no guide text": the daemon still
    /// applies the series-title convention and genre-tag categories with no
    /// template involved. `Some` means at least one field has a template to
    /// render once the schedule (start/finish, neighbours) and watch history
    /// are known — which `ResolvedItem` does not have yet, so rendering
    /// happens later, in `crate::daemon`.
    pub guide: Option<crate::guide::GuideConfig>,
    /// Raw catalog attributes the `guide:` template surface can address
    /// beyond `program` (studio, edition, genres, cast, …). Empty for an
    /// inline item, which has no catalog row behind it.
    pub guide_fields: crate::guide::GuideFields,
}

impl ResolvedItem {
    pub fn to_playout_source(&self) -> PlayoutItemSource {
        self.source.to_playout_source(self.in_point, self.out_point)
    }
}

/// Flatten a channel's blocks into an ordered item list. `path` is the channel
/// config path, used only for error messages. `catalog` resolves `query`
/// entries and non-`manual` order; it may be `None` for a channel that is
/// entirely inline items in `manual` order.
///
/// This is the stateless entry point: pattern pools declaring
/// `advance = "resume"` start from the top, a flat `entries` channel starts at
/// its first item, and neither is cut to a window. Use
/// [`resolve_channel_with_resume`] to continue a channel across a window seam.
pub fn resolve_channel(
    config: &ChannelConfig,
    path: &Path,
    identity_roots: &[String],
    path_index: Option<&HashMap<String, String>>,
    catalog: Option<&Catalog>,
) -> Result<Vec<ResolvedItem>, ConfigError> {
    let (items, _) = resolve_channel_with_resume(
        config,
        path,
        identity_roots,
        path_index,
        catalog,
        &GenerationState::empty(),
        &crate::score::ScoreInputs::default(),
        None,
        // A sequencer block (#169) reads this as the window's absolute
        // start; the stateless entry point has no generation seam to anchor
        // to, so "now" is the least surprising answer — the same tolerance
        // this entry point already gives an unpinned `seed`.
        OffsetDateTime::now_utc(),
    )?;
    Ok(items)
}

/// [`resolve_channel`], plus the resume map that carries a pattern channel's
/// progression across a window seam (#72).
///
/// Generation is a pure function of `(catalog, config, resume_in)`: the same
/// three inputs always produce the same items and the same `resume_out`. There
/// is no live cursor anywhere — a pool that wants to continue rather than
/// replay reads where it left off from `resume_in` and reports where it got to
/// in the returned map, which the daemon persists to the `.resume` sidecar.
///
/// A channel with no pattern block has no pools, so it returns an empty pool
/// map — but it does report the list position it reached (#118), which is what
/// lets the next generation continue rather than replay.
///
/// `fill` is how much airtime the caller still needs covered, and it bounds one
/// generation whichever shape the channel is. A pattern block with no authored
/// `cycles` stops once it has laid that much down instead of running until its
/// largest pool drains (#140); a flat `entries` channel cuts its list at the
/// same span and resumes there next time (#118). `None` means "however long the
/// channel naturally runs" — the stateless callers and the tests.
///
/// `window_start` is the absolute wall-clock instant this generation begins
/// airing at — the daemon's own `from`, not the tick's "now". A sequencer
/// block (#169, [`crate::sequence`]) reads it as `ctx.window.from`, which is
/// what lets a daypart script ask "what hour does this generation start at"
/// rather than "what hour is it while the daemon happens to be computing
/// this". Nothing else in the resolve pipeline reads a live clock (see this
/// module's own doc), so a `pattern` or `entries` block ignores it entirely.
#[allow(clippy::too_many_arguments)]
pub fn resolve_channel_with_resume(
    config: &ChannelConfig,
    path: &Path,
    identity_roots: &[String],
    path_index: Option<&HashMap<String, String>>,
    catalog: Option<&Catalog>,
    state: &GenerationState,
    scoring: &crate::score::ScoreInputs,
    fill: Option<Duration>,
    window_start: OffsetDateTime,
) -> Result<(Vec<ResolvedItem>, ResumeMap), ConfigError> {
    // One seed per generation: a pinned `seed` reproduces the shuffle; an unset
    // one draws fresh entropy so an unseeded `random` block reshuffles each
    // generation (#46 "unset = fresh per generation").
    let seed = config.seed.unwrap_or_else(fresh_seed);

    // Named show groups (#165) are declared once on the channel and can sit
    // unreferenced by any pool, so this checks every declared member against
    // the catalog up front rather than waiting for whichever pool first draws
    // from it — one place a bad show name is caught, naming the group, rather
    // than a different error depending on which pool happened to use it.
    // Structural checks (name uniqueness, an unknown group referenced by a
    // pool) already ran with no catalog in `config::validate`; this is the
    // one check that needed one.
    if let Some(cat) = catalog {
        validate_groups_against_catalog(path, cat, &config.groups)?;
    }

    // `path_index` is the catalog's canonical-path → entry_id map, built once by
    // the caller (the catalog is immutable after ingest). A manual `local` item
    // whose path is in it inherits that entry_id, so it collapses against a
    // `query` result for the same physical file (manual∩query dedup).
    let mut out = Vec::new();
    let mut resume_out = ResumeMap::new();
    // Each item carries its own block's adjacency limits, so the constraint
    // pass runs once over the concatenated list and still covers block joins —
    // which a per-block pass would leave open.
    let mut limits: Vec<Limits> = Vec::new();
    // The field each block separates on, if any. Kept per block because two
    // blocks may separate on different fields.
    let mut separate_fields: Vec<Option<String>> = Vec::new();
    // Each block's span in `out` (start, end, is_pattern) — what the window
    // cut below needs to decide per block rather than once for the channel
    // (#146): a pattern block already bounds itself inside `resolve_block`
    // (#140), so only an entries block's own span still needs cutting here.
    let mut block_spans: Vec<(usize, usize, bool)> = Vec::new();
    for (idx, include) in config.rule.blocks.iter().enumerate() {
        let block_items = resolve_block(
            include,
            idx,
            path,
            identity_roots,
            path_index,
            catalog,
            &config.groups,
            seed,
            state,
            scoring,
            fill,
            window_start,
            &mut resume_out,
            config.guide.as_ref(),
        )?;
        // A pattern block is constrained pool by pool, inside the interleave
        // (#115) — so it contributes no limits here. Constraining its finished
        // list would reorder the pattern's slots and destroy the shape the
        // pattern was written to build; its `[constraints]` table is the default
        // its pools inherit, not a rule over the block's output. A sequencer
        // block (#169) is the same story one level up: its pools are
        // constrained inside `crate::sequence::build`, never over the
        // finished timeline the script returned.
        let block_is_pattern = include.is_pattern() || include.is_sequencer();
        let c = if block_is_pattern {
            Constraints::default()
        } else {
            include.constraints()
        };
        let no_repeat = match c.no_repeat_gap() {
            NoRepeatWithin::Positions(n) => RepeatGap::Positions(n),
            NoRepeatWithin::Duration(d) => RepeatGap::Duration(d),
        };
        let span_start = out.len();
        limits.resize(
            limits.len() + block_items.len(),
            Limits {
                no_repeat,
                separate: c.separate_gap(),
            },
        );
        separate_fields.resize(
            separate_fields.len() + block_items.len(),
            c.separate_by.clone(),
        );
        out.extend(block_items);
        block_spans.push((span_start, out.len(), block_is_pattern));
    }

    // An entries block is a flat authored list, played in a loop. Before the
    // blocks are constrained, seat that list where the last generation left
    // off and cut it to the airtime still wanted, so one generation covers
    // the window instead of the whole list (#118) — a 950-item channel laid a
    // month of playout in one pass and then sat idle for 29 days, during
    // which an edit to its config changed nothing on air. A pattern block
    // needs none of this — it already bounded itself to `fill` inside
    // `resolve_block` (#140) — so this only ever touches an entries block's
    // own span, decided per block rather than once for the whole channel
    // (#146): a channel with one pattern block and one entries block used to
    // skip this cut entirely on its entries half, because `is_pattern()` was
    // answered once for the channel and any pattern block made it answer
    // `true`.
    //
    // Cutting here rather than after emission is what keeps the cut lossless:
    // the adjacency pass below, and the ledger the daemon writes, both see
    // exactly the items that are about to air. Trimming a finished, permuted
    // list instead would drop items the ledger had already recorded as played.
    //
    // Continuing by position is exact for the list that motivated it — an
    // authored `manual` one, where item 37 is the same item next tick. An
    // unseeded `order = "random"` channel reshuffles per generation by design,
    // so its position lands in a different arrangement and an item may come up
    // sooner or later than it otherwise would. That is what an unseeded shuffle
    // already promises; what it gains is that a month of random schedule is no
    // longer decided a month in advance.
    let entries_spans: Vec<(usize, usize)> = block_spans
        .iter()
        .filter(|(_, _, is_pattern)| !is_pattern)
        .map(|(start, end, _)| (*start, *end))
        .collect();
    let has_pattern_block = block_spans.iter().any(|(_, _, is_pattern)| *is_pattern);
    if !entries_spans.is_empty() {
        if !has_pattern_block {
            // No pattern block in the channel: every item in `out` belongs to
            // an entries block, so this is exactly #118's original whole-list
            // cut — unchanged.
            let whole = 0..out.len();
            resume_out.position = cut_entries_window(
                &mut out,
                &mut limits,
                &mut separate_fields,
                whole,
                state.resume.position,
                fill,
                catalog,
                nominal_item_runtime(config),
            );
        } else if entries_spans.len() == 1 {
            // Exactly one entries block shares the channel with pattern
            // block(s): cut that block's own span in place, leaving the
            // pattern blocks' already-bounded output untouched.
            let (start, end) = entries_spans[0];
            resume_out.position = cut_entries_window(
                &mut out,
                &mut limits,
                &mut separate_fields,
                start..end,
                state.resume.position,
                fill,
                catalog,
                nominal_item_runtime(config),
            );
        } else {
            // Two or more entries blocks sharing a channel with a pattern
            // block: #118 deliberately made `.resume.position` one cursor for
            // the channel's whole entries list, not per block, because the
            // adjacency pass below permutes the concatenated list and a
            // per-block cursor would not survive it. That reasoning covers a
            // channel with only entries blocks (they fuse into the one span
            // above) — it does not say what a *fused* cursor spanning several
            // non-contiguous entries spans should splice back to once a
            // pattern block's span sits between them, and picking an answer
            // here would be deciding new resume-position semantics rather
            // than applying the two that already exist. Refuse rather than
            // silently mis-schedule; see #190.
            return Err(ConfigError::Unsupported {
                path: path.to_path_buf(),
                message: format!(
                    "channel mixes a pattern block with {} entries blocks: cutting several \
                     entries blocks to the window is only supported today when no pattern \
                     block shares the channel (see #190 on resume-position semantics for a \
                     fused multi-block cursor)",
                    entries_spans.len()
                ),
            });
        }
    }

    if out.is_empty() {
        // Nothing resolved. A channel can no longer reach this by *playing* its
        // way through its content — every series loops — so an empty list means
        // the resolved set itself is empty: an expression that matches nothing,
        // or a catalog that holds nothing. That is a broken config, always, and
        // it is reported as one.
        return Err(ConfigError::Validation {
            path: path.to_path_buf(),
            message: "channel resolved to zero items".into(),
        });
    }

    // 5. Adjacency constraints — runs last, after every block has ordered its
    //    own list, so it reorders a settled sequence rather than fighting the
    //    order engine.
    if crate::constrain::any_constrained(&limits) {
        // A temporal `no_repeat_within` (#185) needs to know how long each
        // item runs to measure distance in time; a purely positional pass
        // never reads it, so the catalog is only asked when something here is
        // actually spelled as a duration.
        let need_durations = limits
            .iter()
            .any(|l| matches!(l.no_repeat, RepeatGap::Duration(_)));
        let out_durations: Vec<Duration> = if need_durations {
            estimated_runtimes(&out, catalog, nominal_item_runtime(config))
        } else {
            Vec::new()
        };
        let keys = adjacency_keys(
            &out.iter().map(|i| i.id.clone()).collect::<Vec<_>>(),
            &separate_fields,
            &out_durations,
            catalog,
            path,
        )?;
        // The aired tail carries the same field values, looked up the same way,
        // so a seam comparison means what a within-list one means. The
        // previous generation's own blocks are gone, so every tail item is read
        // under this channel's first separating field.
        let tail_field = separate_fields.iter().flatten().next().cloned();
        let tail_durations: Vec<Duration> = if need_durations {
            let estimated =
                estimated_durations_for_ids(&state.tail, catalog, nominal_item_runtime(config));
            state
                .tail
                .iter()
                .map(|id| estimated.get(id).copied().unwrap_or_default())
                .collect()
        } else {
            Vec::new()
        };
        let preceding = adjacency_keys(
            &state.tail,
            &vec![tail_field; state.tail.len()],
            &tail_durations,
            catalog,
            path,
        )?;

        let result = crate::constrain::order_constrained(&keys, &limits, &preceding);
        if result.unresolved > 0 {
            // The set cannot satisfy what the config asks — an all-one-title
            // pool, or a cast too interlinked to separate. Generation completes
            // either way; say so, or a channel quietly failing its constraint
            // looks exactly like one honouring it.
            tracing::warn!(
                event = "constraints.unsatisfied",
                channel = %path.display(),
                violations = result.unresolved,
                items = out.len(),
                "adjacency constraints could not be fully satisfied; airing the closest arrangement found",
            );
        }
        out = permute(out, &result.order);
    }

    Ok((out, resume_out))
}

/// Every declared show group's member shows must have at least one episode on
/// record (#165). Checked once, for every declared group, rather than once
/// per pool that happens to reference one — the same bad title fails the same
/// way whether or not something currently draws from it.
///
/// Structural checks — an unknown group name a pool references, two of a
/// pool's own groups sharing a show — need no catalog and already ran in
/// `config::validate`; this is the one check that does.
fn validate_groups_against_catalog(
    path: &Path,
    catalog: &Catalog,
    groups: &[ShowGroup],
) -> Result<(), ConfigError> {
    if groups.is_empty() {
        return Ok(());
    }
    let all_shows: Vec<String> = groups
        .iter()
        .flat_map(|g| g.shows.iter().cloned())
        .collect();
    let found = catalog
        .episode_ids_for_shows(&all_shows)
        .map_err(|e| ConfigError::Validation {
            path: path.to_path_buf(),
            message: format!("checking show groups against the catalog: {e}"),
        })?;
    for group in groups {
        for show in &group.shows {
            if found.get(show).is_none_or(Vec::is_empty) {
                return Err(ConfigError::Validation {
                    path: path.to_path_buf(),
                    message: format!(
                        "group {:?} names show {:?}, which is not in the catalog",
                        group.name, show
                    ),
                });
            }
        }
    }
    Ok(())
}

/// Build the per-item keys the adjacency pass compares: the `entry_id`, plus
/// Build the per-item keys the adjacency pass compares: the `entry_id`, the
/// values of whatever field that item's block separates on, and — when
/// `durations` has an entry for its position — its estimated runtime (#185).
///
/// The field values come from the catalog's tags, read with the same vocabulary
/// an expression uses — `separate_by: "cast"` reads exactly what `item.cast`
/// reads. An item with no values for the field simply never triggers the
/// separation, which is why a catalog-free channel can still use
/// `no_repeat_within`. `durations` is empty whenever nothing here is measured
/// in time, so a purely positional pass never pays for it and every item's
/// duration is the zero it does not need.
fn adjacency_keys(
    ids: &[String],
    separate_fields: &[Option<String>],
    durations: &[Duration],
    catalog: Option<&Catalog>,
    path: &Path,
) -> Result<Vec<ItemKeys>, ConfigError> {
    ids.iter()
        .zip(separate_fields.iter())
        .enumerate()
        .map(|(i, (id, field))| {
            let duration = durations.get(i).copied().unwrap_or_default();
            let Some(field) = field else {
                return Ok(ItemKeys {
                    id: id.clone(),
                    group: Vec::new(),
                    duration,
                });
            };
            let ns = TagNs::for_separate_by(field).map_err(|message| ConfigError::Validation {
                path: path.to_path_buf(),
                message,
            })?;
            let Some(cat) = catalog else {
                return Err(ConfigError::Unsupported {
                    path: path.to_path_buf(),
                    message: format!(
                        "separate_by = {field:?} needs the catalog, which is not available"
                    ),
                });
            };
            Ok(ItemKeys {
                id: id.clone(),
                group: cat.tags_for(id, ns).map_err(|e| ConfigError::Validation {
                    path: path.to_path_buf(),
                    message: format!("reading {field:?} for {id}: {e}"),
                })?,
                duration,
            })
        })
        .collect()
}

/// The stand-in length for an item nothing has measured yet. Shared with the
/// `ctx.target_count` hint, which is the same quantity asked a different way:
/// how long an item on this channel typically runs.
fn nominal_item_runtime(config: &ChannelConfig) -> Duration {
    let secs = config
        .scoring
        .as_ref()
        .map(|s| s.nominal_item_secs)
        .unwrap_or_else(|| crate::config::ScoringConfig::default().nominal_item_secs)
        .max(1);
    Duration::from_secs(u64::from(secs))
}

/// Each item's runtime, as well as it can be known before anything is probed —
/// which is what the window bound above has to work with, since durations are
/// read off the files themselves and that happens after this returns.
///
/// Three sources, most authoritative first: an item that declares its own
/// in/out points, a catalog row the item already carries, and one bulk catalog
/// query for the rest. An authored `local` item carries no length of its own,
/// but when its path matched a catalog row it inherited that row's `entry_id`,
/// so the catalog has usually measured it.
///
/// Whatever is still unmeasured counts at the mean of what is — the same
/// stand-in [`crate::pattern`] uses for an item the catalog never measured —
/// and, when nothing at all is known, at the channel's nominal item length.
fn estimated_runtimes(
    items: &[ResolvedItem],
    catalog: Option<&Catalog>,
    nominal: Duration,
) -> Vec<Duration> {
    let mut known: Vec<Option<Duration>> = items
        .iter()
        .map(|item| {
            let in_p = item.in_point.unwrap_or_default();
            match item.out_point {
                Some(out_p) if out_p > in_p => Some(out_p - in_p),
                _ => item.catalog_duration,
            }
        })
        .collect();

    if let Some(cat) = catalog {
        let unmeasured: Vec<String> = items
            .iter()
            .zip(&known)
            .filter(|(_, k)| k.is_none())
            .map(|(item, _)| item.id.clone())
            .collect();
        // A catalog read that fails leaves every hole to the mean below rather
        // than failing the channel: this only sizes a generation, and a
        // mis-sized one is corrected by the next tick.
        if let Ok(raw) = cat.durations_for(&unmeasured) {
            for (item, slot) in items.iter().zip(known.iter_mut()) {
                if slot.is_none() {
                    *slot = raw
                        .get(&item.id)
                        .filter(|ms| **ms > 0 && **ms <= MAX_CATALOG_DURATION_MS)
                        .map(|ms| Duration::from_millis(*ms as u64));
                }
            }
        }
    }

    let measured: Vec<Duration> = known.iter().flatten().copied().collect();
    let stand_in = if measured.is_empty() {
        nominal
    } else {
        measured.iter().sum::<Duration>() / measured.len() as u32
    };
    known.into_iter().map(|d| d.unwrap_or(stand_in)).collect()
}

/// [`estimated_runtimes`]'s fallback ladder, minus the top rung: the
/// play-history tail (#185's seam) is bare `entry_id`s, not [`ResolvedItem`]s,
/// so there is no in/out point or carried `catalog_duration` to read first —
/// only the catalog's own record, the mean of what it has for these ids, and
/// the channel's nominal length when it has none of them.
fn estimated_durations_for_ids(
    ids: &[String],
    catalog: Option<&Catalog>,
    nominal: Duration,
) -> HashMap<String, Duration> {
    let mut known: HashMap<String, Duration> = HashMap::new();
    if let Some(cat) = catalog
        && let Ok(raw) = cat.durations_for(ids)
    {
        for (id, ms) in raw {
            if ms > 0 && ms <= MAX_CATALOG_DURATION_MS {
                known.insert(id, Duration::from_millis(ms as u64));
            }
        }
    }
    let stand_in = if known.is_empty() {
        nominal
    } else {
        known.values().sum::<Duration>() / known.len() as u32
    };
    ids.iter()
        .map(|id| (id.clone(), known.get(id).copied().unwrap_or(stand_in)))
        .collect()
}

/// How many items from the top it takes to cover `fill`.
///
/// The item that crosses the boundary is included, so the window is covered
/// rather than left a few minutes short of it — the same bargain the pattern
/// walk strikes when it lets the last cycle finish. Always at least one: a
/// generation that laid nothing would never advance the clock.
fn items_covering(runtimes: &[Duration], fill: Duration) -> usize {
    let mut laid = Duration::ZERO;
    for (i, d) in runtimes.iter().enumerate() {
        laid += *d;
        if laid >= fill {
            return i + 1;
        }
    }
    runtimes.len().max(1)
}

/// Rotate-and-cut one contiguous span of `items` (and its parallel `limits` /
/// `separate_fields`) to seat it where the last generation left off and trim
/// it to the airtime still wanted (#118) — used both for an entries-only
/// channel's whole list and, per block, for a single entries block sharing a
/// channel with pattern block(s) (#146). `range` must index all three slices
/// consistently; on return the three have shrunk (or stayed the same size, if
/// `fill` is `None`) by the same amount, at the same position.
///
/// Returns the position the *next* generation should resume from. A `range`
/// that names an empty span is a no-op, returning `position` unchanged — the
/// division below would otherwise panic on an empty entries block.
#[allow(clippy::too_many_arguments)]
fn cut_entries_window(
    items: &mut Vec<ResolvedItem>,
    limits: &mut Vec<Limits>,
    separate_fields: &mut Vec<Option<String>>,
    range: std::ops::Range<usize>,
    position: usize,
    fill: Option<Duration>,
    catalog: Option<&Catalog>,
    nominal: Duration,
) -> usize {
    let total = range.len();
    if total == 0 {
        return position;
    }
    let insert_at = range.start;
    let mut span_items: Vec<ResolvedItem> = items.drain(range.clone()).collect();
    let mut span_limits: Vec<Limits> = limits.drain(range.clone()).collect();
    let mut span_fields: Vec<Option<String>> = separate_fields.drain(range).collect();

    let start = position % total;
    span_items.rotate_left(start);
    span_limits.rotate_left(start);
    span_fields.rotate_left(start);

    let laid = match fill {
        Some(fill) => {
            let runtimes = estimated_runtimes(&span_items, catalog, nominal);
            items_covering(&runtimes, fill)
        }
        // The stateless entry point wants the list whole.
        None => total,
    };
    span_items.truncate(laid);
    span_limits.truncate(laid);
    span_fields.truncate(laid);

    items.splice(insert_at..insert_at, span_items);
    limits.splice(insert_at..insert_at, span_limits);
    separate_fields.splice(insert_at..insert_at, span_fields);

    (start + laid) % total
}

/// Reorder `items` by `perm` (a permutation of `0..items.len()`).
/// [`ResolvedItem`] is not `Clone`, so items are moved out of slots rather than
/// copied.
fn permute(items: Vec<ResolvedItem>, perm: &[usize]) -> Vec<ResolvedItem> {
    let mut slots: Vec<Option<ResolvedItem>> = items.into_iter().map(Some).collect();
    perm.iter()
        .map(|&i| {
            slots[i]
                .take()
                .expect("a permutation visits each index exactly once")
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn resolve_block(
    include: &BlockInclude,
    idx: usize,
    path: &Path,
    identity_roots: &[String],
    path_index: Option<&HashMap<String, String>>,
    catalog: Option<&Catalog>,
    groups: &[ShowGroup],
    seed: u64,
    state: &GenerationState,
    scoring: &crate::score::ScoreInputs,
    fill: Option<Duration>,
    window_start: OffsetDateTime,
    resume_out: &mut ResumeMap,
    channel_guide: Option<&GuideConfig>,
) -> Result<Vec<ResolvedItem>, ConfigError> {
    let unsupported = |message: String| ConfigError::Unsupported {
        path: path.to_path_buf(),
        message,
    };

    let defaults = include.program();
    // Block ∘ channel, pre-merged once here — same shape as `defaults`
    // above: every catalog-resolved item on this block sees the identical
    // 2-level cascade, and an inline item's own `guide:` merges on top of
    // this result as the third (most specific) level in `resolve_item`.
    let guide_defaults = GuideConfig::cascade(None, include.guide(), channel_guide);
    let guide_defaults = guide_defaults.as_ref();

    // A pattern block builds its list by interleaving pools instead of playing
    // a flat entries list, so it takes its own path: the pattern IS the
    // ordering and the repeats are deliberate, which is why validation rejects
    // a block-level `order` or an explicit `collapse` here rather than letting
    // either quietly undo the interleave.
    if include.is_pattern() {
        let cat = catalog.ok_or_else(|| {
            unsupported(format!(
                "block #{idx}: a pattern block needs the catalog, which is not available"
            ))
        })?;
        // A `plugin:` path means what it means relative to the channel config
        // file, exactly like a `block:` include — not relative to wherever the
        // daemon was launched.
        let score_env = crate::score::ScoreEnv {
            inputs: scoring,
            base_dir: path.parent().unwrap_or_else(|| Path::new(".")),
        };
        let (ids, pools, metadata, pool_guides) = crate::pattern::build(
            cat,
            &include.pools,
            groups,
            &include.pattern,
            include.cycles,
            include.constraints.as_ref(),
            state,
            seed,
            score_env,
            fill,
        )
        .map_err(|m| unsupported(format!("block #{idx}: {m}")))?;
        resume_out.pools.extend(pools);

        return resolve_pool_block_items(
            cat,
            &ids,
            defaults,
            guide_defaults,
            &pool_guides,
            &metadata,
            include.mode,
            idx,
            path,
        );
    }

    // A sequencer block resolves its pools exactly as a pattern block does —
    // same source resolution, same `order`/`bucket_order`/`constraints` — and
    // hands them to the named script instead of walking a pattern (#169).
    if include.is_sequencer() {
        let cat = catalog.ok_or_else(|| {
            unsupported(format!(
                "block #{idx}: a sequencer block needs the catalog, which is not available"
            ))
        })?;
        let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
        let score_env = crate::score::ScoreEnv {
            inputs: scoring,
            base_dir,
        };
        let sequencer = include
            .sequencer
            .as_ref()
            .expect("resolve_block reaches this branch only when `sequencer` is set");
        let script_path = crate::score::resolve_plugin_path(base_dir, sequencer);
        let (ids, pools, metadata, pool_guides) = crate::sequence::build(
            cat,
            &include.pools,
            groups,
            include.constraints.as_ref(),
            &script_path,
            state,
            seed,
            score_env,
            crate::sequence::Window {
                from: window_start.unix_timestamp(),
                fill,
            },
        )
        .map_err(|m| unsupported(format!("block #{idx}: {m}")))?;
        resume_out.pools.extend(pools);

        return resolve_pool_block_items(
            cat,
            &ids,
            defaults,
            guide_defaults,
            &pool_guides,
            &metadata,
            include.mode,
            idx,
            path,
        );
    }

    // 1. Resolve entries to a flat item list (authored order).
    let mut items: Vec<ResolvedItem> = Vec::new();
    for entry in include.entries() {
        match entry {
            Entry::Item(item) => items.push(resolve_item(
                item,
                defaults,
                guide_defaults,
                identity_roots,
                path_index,
            )),
            Entry::Query(query) => {
                let cat = catalog.ok_or_else(|| {
                    unsupported(format!(
                        "block #{idx}: a query entry needs the catalog, which is not available"
                    ))
                })?;
                let resolved = resolve_query(cat, query, defaults, guide_defaults, seed)
                    .map_err(|m| unsupported(format!("block #{idx}: {m}")))?;
                items.extend(resolved);
            }
            Entry::Collection(collection) => {
                let cat = catalog.ok_or_else(|| {
                    unsupported(format!(
                        "block #{idx}: a collection entry needs the catalog, which is not available"
                    ))
                })?;
                let resolved = resolve_collection(cat, collection, defaults, guide_defaults)
                    .map_err(|m| unsupported(format!("block #{idx}: {m}")))?;
                items.extend(resolved);
            }
            Entry::Include(_) => {
                return Err(unsupported(format!(
                    "block #{idx}: include entries are not implemented yet (#69)"
                )));
            }
        }
    }

    // 1.5. Fallback (#97) — a 24/7 channel must not dead-air or error just
    //    because this generation's `entries` resolved to nothing (an empty
    //    `query` match is how a Plex collection being momentarily empty
    //    actually surfaces here). Substitutes for `entries`' output through
    //    the same machinery a primary entry uses, then falls straight into
    //    the same duplicates/order/mode steps below — never a parallel path.
    //    Opt-in only: a block with no `fallback` still resolves to empty
    //    exactly as before this existed, and a fallback that also resolves to
    //    nothing leaves the block empty exactly the same way.
    if items.is_empty()
        && let Some(fallback) = &include.fallback
    {
        items = resolve_fallback(
            catalog,
            fallback,
            defaults,
            guide_defaults,
            seed,
            identity_roots,
            path_index,
        )
        .map_err(|m| unsupported(format!("block #{idx}: fallback: {m}")))?;
    }

    // 1.6. Filter (#197) — narrow the resolved list before duplicates/order
    //    see it, so a `random` order shuffles only the survivors and
    //    `duplicates` collapses only within them.
    if let Some(filter) = &include.filter
        && !filter.is_empty()
    {
        items = apply_filter(catalog, items, filter)
            .map_err(|m| unsupported(format!("block #{idx}: {m}")))?;
    }

    // 2. Duplicates — collapse (default) runs BEFORE order so which occurrence
    //    survives is deterministic even under a `random` shuffle.
    if matches!(include.duplicates(), Duplicates::Collapse) {
        collapse_duplicates(&mut items);
    }

    // 3. Order the block's resolved list. An authored order (`manual`
    //    included) is used as written and needs no catalog. Unset takes the
    //    episode default when every resolved item is catalog type "episode"
    //    (#95) — a set the catalog cannot vouch for (no catalog, or any
    //    non-episode / uningested item) keeps today's manual behavior.
    let effective_order = if let Some(order) = &include.order {
        order.clone()
    } else if let Some(cat) = catalog {
        let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
        let episode_typed = cat
            .all_episode_type(&ids)
            .map_err(|e| unsupported(format!("block #{idx}: {e}")))?;
        if episode_typed {
            Order::episode_default()
        } else {
            Order::Manual
        }
    } else {
        Order::Manual
    };
    if effective_order != Order::Manual {
        let cat = catalog.ok_or_else(|| {
            unsupported(format!(
                "block #{idx}: order {effective_order:?} needs the catalog, which is not available",
            ))
        })?;
        items = apply_order(cat, items, &effective_order, seed)
            .map_err(|m| unsupported(format!("block #{idx}: {m}")))?;
    }

    // 4. Mode — `count` truncates after ordering.
    if let Mode::Count(n) = include.mode {
        items.truncate(n);
    }

    Ok(items)
}

/// Turn a pool block's drawn `entry_id` list into its [`ResolvedItem`]s:
/// catalog lookup, the plugin-pool `metadata` attach (#166, #201), and the
/// `mode: count` shortfall truncate/warn. A `pattern:` block and a
/// `sequencer:` block do all three identically once each has its own ids in
/// hand, so any future per-drawn-item field added here reaches both at once.
#[allow(clippy::too_many_arguments)]
fn resolve_pool_block_items(
    cat: &Catalog,
    ids: &[String],
    defaults: Option<&ProgramMetadata>,
    guide_defaults: Option<&GuideConfig>,
    pool_guides: &HashMap<String, GuideConfig>,
    metadata: &HashMap<String, serde_json::Value>,
    mode: Mode,
    idx: usize,
    path: &Path,
) -> Result<Vec<ResolvedItem>, ConfigError> {
    let unsupported = |message: String| ConfigError::Unsupported {
        path: path.to_path_buf(),
        message,
    };
    let mut items: Vec<ResolvedItem> = ids
        .iter()
        .map(|id| {
            // The pool rung (#289) sits between the block and the item: a
            // drawn id whose pool authored its own `guide:` sees that ahead
            // of the pre-merged block ∘ channel default, exactly like an
            // inline item's own `guide:` would if a pool draw had one to
            // author.
            let effective_guide = GuideConfig::cascade(None, pool_guides.get(id), guide_defaults);
            catalog_item(cat, id, defaults, effective_guide.as_ref())
        })
        .collect::<Result<Vec<_>, String>>()
        .map_err(|m: String| unsupported(format!("block #{idx}: {m}")))?
        .into_iter()
        .flatten()
        .collect();
    // A plugin pool's metadata blob (#166), attached after the catalog
    // lookup above so it lands on the airing even though `catalog_item`
    // itself has no pool/plugin context to draw one from. Empty for a block
    // whose plugin(s) only ever returned bare ids.
    for item in &mut items {
        item.metadata = metadata.get(&item.id).cloned();
    }
    // Skipping happens before the truncate, so an unplayable row normally
    // costs nothing — the next playable item slides up into its place. The
    // one case it cannot cover is the draw handing back exactly `n` ids and
    // one of them being unplayable: there is no spare to slide up, and
    // asking for more would mean advancing cursors past items this block
    // never airs. Say so rather than quietly under-filling.
    if let Mode::Count(n) = mode {
        if items.len() < n {
            tracing::warn!(
                event = "block.count_short",
                block = idx,
                asked = n,
                got = items.len(),
                "block asked for more items than the catalog could play; see the item-level warnings above",
            );
        }
        items.truncate(n);
    }
    Ok(items)
}

/// Resolve a `query` entry against the catalog: run the CEL query, apply the
/// entry's own optional `order` (#46 per-entry order), then turn each resolved
/// `entry_id` into a [`ResolvedItem`].
fn resolve_query(
    catalog: &Catalog,
    query: &QueryEntry,
    defaults: Option<&ProgramMetadata>,
    guide_defaults: Option<&GuideConfig>,
    seed: u64,
) -> Result<Vec<ResolvedItem>, String> {
    let mut ids = catalog
        .resolve_query(&query.query)
        .map_err(|e| e.to_string())?;
    if let Some(order) = &query.order {
        ids = catalog
            .resolve_order(&ids, order, seed)
            .map_err(|e| e.to_string())?;
    }
    Ok(ids
        .iter()
        .map(|id| catalog_item(catalog, id, defaults, guide_defaults))
        .collect::<Result<Vec<_>, String>>()?
        .into_iter()
        .flatten()
        .collect())
}

/// Resolve a block's `fallback` (#97) — through the exact same machinery as a
/// primary entry, never a parallel path: a `query` fallback runs
/// [`resolve_query`], exactly like a primary `kind: query` entry; a static
/// item fallback runs [`resolve_item`], exactly like a primary `kind: item`
/// entry (always exactly one item, so it can never itself resolve to empty).
fn resolve_fallback(
    catalog: Option<&Catalog>,
    fallback: &Fallback,
    defaults: Option<&ProgramMetadata>,
    guide_defaults: Option<&GuideConfig>,
    seed: u64,
    identity_roots: &[String],
    path_index: Option<&HashMap<String, String>>,
) -> Result<Vec<ResolvedItem>, String> {
    match fallback {
        Fallback::Query(query) => {
            let cat = catalog.ok_or_else(|| {
                "a query fallback needs the catalog, which is not available".to_string()
            })?;
            resolve_query(cat, query, defaults, guide_defaults, seed)
        }
        Fallback::Item(item) => Ok(vec![resolve_item(
            item,
            defaults,
            guide_defaults,
            identity_roots,
            path_index,
        )]),
    }
}

/// Resolve a `collection` entry: look the collection up by name and emit its
/// members in stored `collection_items.position` order.
///
/// No ordering step is involved — the run arrives ordered out of the catalog,
/// and the block's default `manual` order preserves it. That is the whole point
/// of collection being an entry kind rather than an `order` value (#107): the
/// authored sequence never has to survive a round-trip through a flat id set.
fn resolve_collection(
    catalog: &Catalog,
    entry: &CollectionEntry,
    defaults: Option<&ProgramMetadata>,
    guide_defaults: Option<&GuideConfig>,
) -> Result<Vec<ResolvedItem>, String> {
    let mut ids = catalog
        .collection_ids_by_name(&entry.name)
        .map_err(|e| e.to_string())?;
    let collection_id = match ids.len() {
        1 => ids.remove(0),
        0 => {
            return Err(format!(
                "no collection named {:?} in the catalog",
                entry.name
            ));
        }
        n => {
            // Names are not unique, and the catalog stores no finer qualifier
            // than `source` (which every collection shares today, since only
            // Plex ingest writes them). So name the offending ids rather than
            // pretend a filter could pick between them.
            return Err(format!(
                "{n} collections are named {:?} — a collection entry must name exactly one \
                 (conflicting ids: {}); rename one in the source and re-ingest",
                entry.name,
                ids.join(", ")
            ));
        }
    };
    let members = catalog
        .collection_members(&collection_id)
        .map_err(|e| e.to_string())?;
    // Naming a collection asserts it has content — unlike a query, which is a
    // filter and may legitimately match nothing. An empty one would otherwise
    // vanish from the channel silently.
    if members.is_empty() {
        return Err(format!(
            "collection {:?} ({collection_id}) has no members",
            entry.name
        ));
    }
    Ok(members
        .iter()
        .map(|id| catalog_item(catalog, id, defaults, guide_defaults))
        .collect::<Result<Vec<_>, String>>()?
        .into_iter()
        .flatten()
        .collect())
}

/// Narrow a block's resolved list to `filter`'s restrictions (#197): `seasons`
/// keeps only items whose catalog season is in the list, `episode_ids` keeps
/// only items whose id is in the list, and with both set an item must satisfy
/// both — two "restrict to" fields combine as a narrower set, not a union.
///
/// `episode_ids` matches the same id every other resolve step keys on (a
/// catalog `entry_id`, or an inline item's derived id), so it needs no catalog
/// round trip. `seasons` does, since season is catalog metadata a resolved
/// item carries no copy of.
fn apply_filter(
    catalog: Option<&Catalog>,
    items: Vec<ResolvedItem>,
    filter: &Filter,
) -> Result<Vec<ResolvedItem>, String> {
    // The wanted seasons and the id -> season map they are checked against
    // both exist only when `filter.seasons` is set, so they travel as one
    // value instead of a set plus a map that means nothing without it.
    let season_filter: Option<(HashSet<i64>, HashMap<String, i64>)> = match &filter.seasons {
        Some(wanted) => {
            let cat = catalog.ok_or_else(|| {
                "filter.seasons needs the catalog, which is not available".to_string()
            })?;
            let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
            let by_id = cat.seasons_for(&ids).map_err(|e| e.to_string())?;
            Some((wanted.iter().map(|&n| i64::from(n)).collect(), by_id))
        }
        None => None,
    };
    let episode_ids: Option<HashSet<&str>> = filter
        .episode_ids
        .as_ref()
        .map(|list| list.iter().map(String::as_str).collect());

    Ok(items
        .into_iter()
        .filter(|item| {
            let season_ok = season_filter.as_ref().is_none_or(|(wanted, by_id)| {
                by_id
                    .get(&item.id)
                    .is_some_and(|season| wanted.contains(season))
            });
            let episode_ok = episode_ids
                .as_ref()
                .is_none_or(|wanted| wanted.contains(item.id.as_str()));
            season_ok && episode_ok
        })
        .collect())
}

/// Order a resolved item list via the #69 engine and reorder the items to match.
fn apply_order(
    catalog: &Catalog,
    items: Vec<ResolvedItem>,
    order: &Order,
    seed: u64,
) -> Result<Vec<ResolvedItem>, String> {
    let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
    let ordered = catalog
        .resolve_order(&ids, order, seed)
        .map_err(|e| e.to_string())?;
    Ok(reorder_to(items, &ordered))
}

/// A fresh, non-reproducible seed for an unseeded `random` order — derived from
/// the wall clock so each generation shuffles differently (#46).
fn fresh_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Reorder `items` to follow `ordered_ids`, then append any items the ordering
/// didn't rank — in authored order — so nothing is lost. The order engine only
/// ranks catalog-backed entries (a field/collection sort is a `SELECT` over
/// `entries`), so an inline item or a `keep` duplicate that the SQL round-trip
/// omits is emitted after the ranked set rather than dropped. Duplicate ids are
/// matched by position via per-id index queues, preserving their relative order.
fn reorder_to(items: Vec<ResolvedItem>, ordered_ids: &[String]) -> Vec<ResolvedItem> {
    let mut indices_by_id: HashMap<&str, VecDeque<usize>> = HashMap::new();
    for (i, item) in items.iter().enumerate() {
        indices_by_id
            .entry(item.id.as_str())
            .or_default()
            .push_back(i);
    }

    let mut order: Vec<usize> = Vec::with_capacity(items.len());
    let mut taken = vec![false; items.len()];
    for id in ordered_ids {
        if let Some(queue) = indices_by_id.get_mut(id.as_str())
            && let Some(i) = queue.pop_front()
        {
            order.push(i);
            taken[i] = true;
        }
    }
    // Append everything the ordering didn't consume, in authored order.
    for (i, is_taken) in taken.iter().enumerate() {
        if !is_taken {
            order.push(i);
        }
    }

    // Rebuild in `order`. `ResolvedItem` isn't `Clone`, so move each out of an
    // `Option` slot exactly once.
    let mut slots: Vec<Option<ResolvedItem>> = items.into_iter().map(Some).collect();
    order
        .into_iter()
        .map(|i| slots[i].take().expect("each index visited once"))
        .collect()
}

/// Build a [`ResolvedItem`] from a catalog `entry_id`: its playback source (the
/// preferred `entry_sources` row) plus program metadata from the entry columns,
/// cascaded under the block `[program]` defaults.
/// Longest slot a catalog-reported length is allowed to size. Beyond a day the
/// number is not a runtime, it is bad metadata.
pub(crate) const MAX_CATALOG_DURATION_MS: i64 = 24 * 60 * 60 * 1000;

/// `Ok(None)` means the catalog knows this item but nothing can play it — the
/// row carries no playback source at all. That is not a channel misconfiguration
/// and must not be raised as one: a single hollow row would otherwise take down
/// every channel whose query happened to match it. It is skipped and logged, and
/// the rest of the list plays. See #137 for how such a row gets written.
fn catalog_item(
    catalog: &Catalog,
    entry_id: &str,
    defaults: Option<&ProgramMetadata>,
    guide_defaults: Option<&GuideConfig>,
) -> Result<Option<ResolvedItem>, String> {
    let entry = catalog
        .entry(entry_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("resolved entry {entry_id} vanished from the catalog"))?;
    let sources = catalog.sources_for(entry_id).map_err(|e| e.to_string())?;
    // Prefer a local-filesystem source (a real path the player can open);
    // fall back to the first provenance row. Source-specific playback (e.g. a
    // Plex streaming URL) is deferred to the ingester that defines it.
    let Some(source) = sources
        .iter()
        .find(|s| s.source == crate::catalog::Source::LocalFs)
        .or_else(|| sources.first())
    else {
        tracing::warn!(
            event = "item.no_playback_source",
            item = %entry_id,
            title = %entry.title,
            "catalog row has no file behind it; skipping this item",
        );
        return Ok(None);
    };
    // Catalog columns are i64; ProgramMetadata uses u32. Out-of-range values
    // (negative / overflow) drop to None rather than wrap.
    let as_u32 = |v: Option<i64>| v.and_then(|n| u32::try_from(n).ok());

    // #158 decision #1: an episode's `<title>` carries the series name and
    // `<sub-title>` its own episode name, per XMLTV convention. Not
    // configurable — there is no toggle for which name goes in which
    // element, unconditional on `entry.kind`. A `guide.title`/`guide.sub_title`
    // template, if authored, still overrides this default like any other
    // field — see `render_program` in `crate::daemon`.
    let is_episode = entry.kind == "episode";
    let (title, sub_title) = if is_episode {
        (
            entry.show.clone().unwrap_or_else(|| entry.title.clone()),
            Some(entry.title.clone()),
        )
    } else {
        (entry.title.clone(), None)
    };

    // #158 decision #3: genre tags become `<category>` automatically, no
    // config required. A channel can still override via `guide.categories`.
    let genres = catalog
        .tags_for(entry_id, TagNs::Genre)
        .map_err(|e| e.to_string())?;
    let categories = if genres.is_empty() {
        None
    } else {
        Some(genres.clone())
    };

    let program = ProgramMetadata {
        title: Some(title),
        sub_title,
        // #186 default: the catalog's summary column, when the source
        // filled one, with no `guide:` config needed. A `guide.description`
        // template, if authored at any cascade level, overrides this in the
        // render pass (`crate::daemon::render_guide_and_attribution`) —
        // same as every other field here.
        description: entry.summary.clone(),
        season: as_u32(entry.season),
        episode: as_u32(entry.episode),
        categories,
        content_rating: entry.content_rating.clone(),
        // A path relative to ETV-next's own HTTP root, never a Plex URL
        // (#187) — a Plex artwork URL carries `X-Plex-Token` as a working
        // credential, and `xmltv.xml` is served over plain HTTP to every
        // guide reader. ETV-next's xmltv writer resolves this against the
        // request's own host (see `xmltv::write_metadata` in vendor/etv-next),
        // so the station never has to know its own externally-reachable
        // address. `None` when no artwork was cached — no `<icon>`, not a
        // broken link.
        artwork_url: entry
            .artwork_cache_path
            .as_deref()
            .map(|filename| format!("/artwork/{filename}")),
        year: as_u32(entry.year),
        // Not sourced from the catalog yet — populating these from the
        // `cast`/`writer`/`director`/`country` tag namespaces is separate
        // producer-side work, tracked as a follow-up. (The same tags ARE
        // exposed to the `guide:` template surface below, as `{cast}` etc —
        // this is specifically about the structured `<credits>`/`<country>`
        // XMLTV elements, split to etv-next-station#35 per #158's "the guide
        // stops at what ETV-next already emits".)
        credits: None,
        country: None,
        star_rating: None,
    };

    let guide_fields = GuideFields {
        show: entry.show.clone(),
        absolute_episode: entry.absolute_episode,
        release_date: entry.release_date.clone(),
        studio: entry.studio.clone(),
        edition: entry.edition.clone(),
        library: entry.library.clone(),
        duration: entry
            .duration_ms
            .filter(|ms| *ms > 0)
            .map(|ms| Duration::from_millis(ms as u64)),
        cast: catalog
            .tags_for(entry_id, TagNs::Cast)
            .map_err(|e| e.to_string())?,
        directors: catalog
            .tags_for(entry_id, TagNs::Director)
            .map_err(|e| e.to_string())?,
        writers: catalog
            .tags_for(entry_id, TagNs::Writer)
            .map_err(|e| e.to_string())?,
        countries: catalog
            .tags_for(entry_id, TagNs::Country)
            .map_err(|e| e.to_string())?,
        genres,
        summary: entry.summary.clone(),
    };

    Ok(Some(ResolvedItem {
        id: entry.entry_id.clone(),
        source: SourceConfig::Local {
            path: source.playback_path.clone(),
        },
        in_point: None,
        out_point: None,
        program: merge_program(Some(&program), defaults),
        // Bounded on both ends, because this value SIZES A SLOT. A negative or
        // zero length is meaningless; a wildly large one (a stray factor of
        // 1000 in a metadata field) would schedule a single item for years,
        // which emits a chunk file per `chunk_hours` across that whole span and
        // can overflow the timeline arithmetic outright. Anything past a day is
        // treated as no length at all — the item is then dropped rather than
        // given an absurd slot.
        catalog_duration: entry
            .duration_ms
            .filter(|ms| *ms > 0 && *ms <= MAX_CATALOG_DURATION_MS)
            .map(|ms| Duration::from_millis(ms as u64)),
        error_card: false,
        // Set afterward, by `resolve_pool_block_items`'s post-pass (#166,
        // #201) — shared by a pattern block and a sequencer block alike —
        // for an id a plugin pool attached a blob to. Every other caller of
        // this function has no pool/plugin context at all, so `None` here is
        // the whole answer for them.
        metadata: None,
        // No item-level `guide:` for a catalog-resolved entry — there is no
        // per-entry config surface for a query/collection result, only the
        // block ∘ channel cascade `guide_defaults` already carries.
        guide: guide_defaults.cloned(),
        guide_fields,
    }))
}

fn resolve_item(
    item: &ItemEntry,
    defaults: Option<&ProgramMetadata>,
    guide_defaults: Option<&GuideConfig>,
    identity_roots: &[String],
    path_index: Option<&HashMap<String, String>>,
) -> ResolvedItem {
    ResolvedItem {
        id: derive_item_id(&item.source, identity_roots, path_index),
        source: item.source.clone(),
        in_point: item.in_point,
        out_point: item.out_point,
        program: merge_program(item.program.as_ref(), defaults),
        // An inline item states its own length via in_point/out_point; there is
        // no catalog row behind it to fall back to.
        catalog_duration: None,
        error_card: false,
        // No pool, no plugin — nothing could have attached a blob.
        metadata: None,
        // Item's own `guide:` wins, then the pre-merged block ∘ channel
        // cascade — the third and most specific cascade level.
        guide: GuideConfig::cascade(item.guide.as_ref(), guide_defaults, None),
        // An inline item has no catalog row, so nothing to fill these from —
        // its `program` above is the whole answer.
        guide_fields: GuideFields::default(),
    }
}

/// Derive a stable, namespaced identity for an inline item from its source —
/// items never carry an authored id. A local file canonicalises its path
/// (root-stripped so the same file under two mount roots is one identity) and,
/// when a catalog `path_index` is present, **inherits the catalog's `entry_id`
/// for that file** — so a manual item and a `query` result for the same physical
/// file share an identity and collapse. With no catalog it falls back to the
/// same `fs:` path hash a filesystem ingester would mint. A generated or remote
/// source keys on its defining field. The result feeds within-block duplicate
/// collapse and the regeneration anchor, so it must be deterministic.
fn derive_item_id(
    source: &SourceConfig,
    identity_roots: &[String],
    path_index: Option<&HashMap<String, String>>,
) -> String {
    match source {
        SourceConfig::Local { path } => {
            let roots: Vec<&str> = identity_roots.iter().map(String::as_str).collect();
            let canonical = canonical_path(path, &roots);
            path_index
                .and_then(|idx| idx.get(&canonical))
                .cloned()
                .unwrap_or_else(|| derive_entry_id(&[], &canonical))
        }
        SourceConfig::Lavfi { params } => format!("lavfi:{params}"),
        SourceConfig::Http { uri, .. } => format!("http:{uri}"),
    }
}

/// Field-level cascade: an item's own program metadata wins field by field,
/// falling back to the block-level `[program]` defaults. Built field-wise
/// because `ProgramMetadata` (an ETV-next type) is not `Clone`.
fn merge_program(
    item: Option<&ProgramMetadata>,
    defaults: Option<&ProgramMetadata>,
) -> Option<ProgramMetadata> {
    if item.is_none() && defaults.is_none() {
        return None;
    }
    // For each field, prefer the item's value, else the block default.
    macro_rules! pick {
        ($field:ident) => {
            item.and_then(|p| p.$field.clone())
                .or_else(|| defaults.and_then(|d| d.$field.clone()))
        };
    }
    Some(ProgramMetadata {
        title: pick!(title),
        sub_title: pick!(sub_title),
        description: pick!(description),
        season: pick!(season),
        episode: pick!(episode),
        categories: pick!(categories),
        content_rating: pick!(content_rating),
        artwork_url: pick!(artwork_url),
        year: pick!(year),
        credits: pick!(credits),
        country: pick!(country),
        star_rating: pick!(star_rating),
    })
}

/// First-occurrence-wins dedup by item id, in place.
fn collapse_duplicates(items: &mut Vec<ResolvedItem>) {
    let mut seen = HashSet::new();
    items.retain(|item| seen.insert(item.id.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChannelConfig, RuleConfig};

    /// A lavfi test item. Its derived id is `lavfi:{id}` (see `derive_item_id`),
    /// so distinct `id`s stay distinct and equal ones collapse.
    fn item_entry(id: &str) -> ItemEntry {
        ItemEntry {
            source: SourceConfig::Lavfi { params: id.into() },
            in_point: None,
            out_point: Some(Duration::from_secs(30)),
            program: None,
            guide: None,
        }
    }

    /// A lavfi test item with an explicit runtime, for exercising a temporal
    /// `no_repeat_within` (#185) without a catalog: `estimated_runtimes` reads
    /// `out_point - in_point` as its most authoritative source.
    fn item_entry_secs(id: &str, secs: u64) -> ItemEntry {
        ItemEntry {
            source: SourceConfig::Lavfi { params: id.into() },
            in_point: None,
            out_point: Some(Duration::from_secs(secs)),
            program: None,
            guide: None,
        }
    }

    /// A local-file test item (no authored id — identity derives from the path).
    fn local_entry(path: &str) -> ItemEntry {
        ItemEntry {
            source: SourceConfig::Local { path: path.into() },
            in_point: None,
            out_point: Some(Duration::from_secs(30)),
            program: None,
            guide: None,
        }
    }

    fn include_with(entries: Vec<Entry>) -> BlockInclude {
        BlockInclude {
            block: None,
            program: None,
            guide: None,
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

    fn channel(blocks: Vec<BlockInclude>) -> ChannelConfig {
        ChannelConfig {
            scoring: None,
            name: None,
            display_name: None,
            guide: None,
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

    fn path() -> &'static Path {
        Path::new("/tmp/channel.toml")
    }

    /// A fixed window-start for tests that don't exercise a sequencer block's
    /// `ctx.window.from` (#169) — any instant does, since nothing else in the
    /// resolve pipeline reads a live clock.
    fn t0() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    #[test]
    fn flattens_items_in_order() {
        let inc = include_with(vec![
            Entry::Item(Box::new(item_entry("a"))),
            Entry::Item(Box::new(item_entry("b"))),
        ]);
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, None).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["lavfi:a", "lavfi:b"]);
    }

    #[test]
    fn concatenates_blocks() {
        let a = include_with(vec![Entry::Item(Box::new(item_entry("a")))]);
        let b = include_with(vec![Entry::Item(Box::new(item_entry("b")))]);
        let items = resolve_channel(&channel(vec![a, b]), path(), &[], None, None).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["lavfi:a", "lavfi:b"]);
    }

    /// `no_repeat_within` only has repeats to work on when they survive to the
    /// pass, so these use `duplicates = "keep"` or cross-block repeats — the two
    /// ways an id legitimately appears twice in a resolved channel.
    fn constrained(mut inc: BlockInclude, n: usize) -> BlockInclude {
        inc.constraints = Some(crate::config::Constraints {
            no_repeat_within: Some(NoRepeatWithin::Positions(n)),
            separate_by: None,
            separate_min_gap: None,
        });
        inc
    }

    /// The temporal spelling of [`constrained`] (#185).
    fn constrained_within(mut inc: BlockInclude, within: Duration) -> BlockInclude {
        inc.constraints = Some(crate::config::Constraints {
            no_repeat_within: Some(NoRepeatWithin::Duration(within)),
            separate_by: None,
            separate_min_gap: None,
        });
        inc
    }

    fn resolved_ids(blocks: Vec<BlockInclude>) -> Vec<String> {
        resolve_channel(&channel(blocks), path(), &[], None, None)
            .unwrap()
            .iter()
            .map(|i| i.id.clone())
            .collect()
    }

    #[test]
    fn no_repeat_within_separates_back_to_back_repeats() {
        let mut inc = include_with(vec![
            Entry::Item(Box::new(item_entry("a"))),
            Entry::Item(Box::new(item_entry("a"))),
            Entry::Item(Box::new(item_entry("b"))),
            Entry::Item(Box::new(item_entry("c"))),
        ]);
        inc.duplicates = Some(Duplicates::Keep);
        let ids = resolved_ids(vec![constrained(inc, 1)]);
        assert_eq!(ids.len(), 4);
        for i in 0..ids.len() {
            assert_ne!(ids[i], ids[(i + 1) % ids.len()], "{ids:?}");
        }
    }

    #[test]
    fn no_repeat_within_holds_across_a_block_join() {
        // `collapse` is block-scoped, so the same title in two blocks survives
        // into the concatenated list — and the channel-level pass is what keeps
        // the join from playing it twice in a row.
        let a = include_with(vec![
            Entry::Item(Box::new(item_entry("x"))),
            Entry::Item(Box::new(item_entry("a"))),
        ]);
        let b = include_with(vec![
            Entry::Item(Box::new(item_entry("a"))),
            Entry::Item(Box::new(item_entry("y"))),
        ]);
        let ids = resolved_ids(vec![constrained(a, 1), constrained(b, 1)]);
        assert_eq!(ids.len(), 4);
        for i in 0..ids.len() {
            assert_ne!(ids[i], ids[(i + 1) % ids.len()], "{ids:?}");
        }
    }

    /// The seam is the *generation* boundary, not the list's own ends:
    /// `Sequential` plays this list once and lays the next one after it, so the
    /// head is constrained against what already aired.
    #[test]
    fn no_repeat_within_holds_across_the_generation_seam() {
        let mut inc = include_with(vec![
            Entry::Item(Box::new(item_entry("a"))),
            Entry::Item(Box::new(item_entry("b"))),
            Entry::Item(Box::new(item_entry("c"))),
        ]);
        inc.duplicates = Some(Duplicates::Keep);
        let state = crate::resume::GenerationState {
            tail: vec!["lavfi:a".to_string()],
            ..Default::default()
        };
        let (items, _) = resolve_channel_with_resume(
            &channel(vec![constrained(inc, 1)]),
            path(),
            &[],
            None,
            None,
            &state,
            &Default::default(),
            None,
            t0(),
        )
        .unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_ne!(
            ids[0], "lavfi:a",
            "repeated the previously-aired item across the seam: {ids:?}"
        );
    }

    // ---- no_repeat_within: temporal spelling (#185) -------------------------

    /// Two 30s items sandwiching another 30s item stays within a 40s window —
    /// a single item never covers it — so, like the positional gap = 1 case,
    /// the pass must still separate the back-to-back repeat.
    #[test]
    fn no_repeat_within_temporal_separates_short_items() {
        let mut inc = include_with(vec![
            Entry::Item(Box::new(item_entry_secs("a", 30))),
            Entry::Item(Box::new(item_entry_secs("a", 30))),
            Entry::Item(Box::new(item_entry_secs("b", 30))),
            Entry::Item(Box::new(item_entry_secs("c", 30))),
        ]);
        inc.duplicates = Some(Duplicates::Keep);
        let ids = resolved_ids(vec![constrained_within(inc, Duration::from_secs(40))]);
        assert_eq!(ids.len(), 4);
        for i in 1..ids.len() {
            assert_ne!(ids[i - 1], ids[i], "{ids:?}");
        }
    }

    /// Each item alone already runs longer than the configured window, so an
    /// adjacent repeat is legal and the list must come back untouched — the
    /// exact case a positional `no_repeat_within = 1` cannot express, since it
    /// would force separation regardless of how long each item runs.
    #[test]
    fn no_repeat_within_temporal_allows_adjacency_when_items_are_long_enough() {
        let mut inc = include_with(vec![
            Entry::Item(Box::new(item_entry_secs("a", 600))),
            Entry::Item(Box::new(item_entry_secs("a", 600))),
            Entry::Item(Box::new(item_entry_secs("b", 600))),
        ]);
        inc.duplicates = Some(Duplicates::Keep);
        let ids = resolved_ids(vec![constrained_within(inc, Duration::from_secs(300))]);
        assert_eq!(
            ids,
            vec!["lavfi:a", "lavfi:a", "lavfi:b"],
            "each item alone already covers the 300s window; a legal list was reordered"
        );
    }

    /// The seam holds in wall-clock time too: with no catalog behind it, the
    /// previously-aired tail item is estimated at the channel's nominal item
    /// length (1800s, the untouched default), which a 2h window still reaches
    /// past — so the new list must not open on the same id.
    #[test]
    fn no_repeat_within_temporal_holds_across_the_generation_seam() {
        let inc = include_with(vec![
            Entry::Item(Box::new(item_entry_secs("a", 600))),
            Entry::Item(Box::new(item_entry_secs("b", 600))),
        ]);
        let state = crate::resume::GenerationState {
            tail: vec!["lavfi:a".to_string()],
            ..Default::default()
        };
        let (items, _) = resolve_channel_with_resume(
            &channel(vec![constrained_within(inc, Duration::from_secs(7200))]),
            path(),
            &[],
            None,
            None,
            &state,
            &Default::default(),
            None,
            t0(),
        )
        .unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_ne!(
            ids[0], "lavfi:a",
            "repeated the previously-aired item within the temporal window: {ids:?}"
        );
    }

    /// The list's own head and tail are NOT adjacent — nothing replays it end
    /// to end — so an already-legal list must come back untouched.
    #[test]
    fn the_lists_own_ends_are_left_alone() {
        let mut inc = include_with(vec![
            Entry::Item(Box::new(item_entry("a"))),
            Entry::Item(Box::new(item_entry("b"))),
            Entry::Item(Box::new(item_entry("c"))),
            Entry::Item(Box::new(item_entry("a"))),
        ]);
        inc.duplicates = Some(Duplicates::Keep);
        let ids = resolved_ids(vec![constrained(inc, 1)]);
        assert_eq!(
            ids,
            vec!["lavfi:a", "lavfi:b", "lavfi:c", "lavfi:a"],
            "a legal list was reordered"
        );
    }

    #[test]
    fn unsatisfiable_constraint_completes_rather_than_hanging() {
        // One title, "no two in a row": impossible. Generation must finish with
        // every item intact and accept the violation.
        let mut inc = include_with(vec![
            Entry::Item(Box::new(item_entry("a"))),
            Entry::Item(Box::new(item_entry("a"))),
            Entry::Item(Box::new(item_entry("a"))),
        ]);
        inc.duplicates = Some(Duplicates::Keep);
        let ids = resolved_ids(vec![constrained(inc, 1)]);
        assert_eq!(ids, vec!["lavfi:a"; 3]);
    }

    #[test]
    fn unconstrained_channel_keeps_its_resolved_order() {
        let mut inc = include_with(vec![
            Entry::Item(Box::new(item_entry("a"))),
            Entry::Item(Box::new(item_entry("a"))),
            Entry::Item(Box::new(item_entry("b"))),
        ]);
        inc.duplicates = Some(Duplicates::Keep);
        assert_eq!(
            resolved_ids(vec![inc]),
            vec!["lavfi:a", "lavfi:a", "lavfi:b"]
        );
    }

    #[test]
    fn collapse_dedups_by_id() {
        let inc = include_with(vec![
            Entry::Item(Box::new(item_entry("a"))),
            Entry::Item(Box::new(item_entry("a"))),
            Entry::Item(Box::new(item_entry("b"))),
        ]);
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, None).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["lavfi:a", "lavfi:b"]);
    }

    #[test]
    fn manual_items_with_same_path_collapse_by_derived_id() {
        // No authored id: two entries pointing at the same file derive the same
        // `fs:` identity and collapse under the default `collapse` policy; a
        // different file keeps its own identity.
        let inc = include_with(vec![
            Entry::Item(Box::new(local_entry("/media/friends/s01e01.mkv"))),
            Entry::Item(Box::new(local_entry("/media/friends/s01e01.mkv"))),
            Entry::Item(Box::new(local_entry("/media/friends/s01e02.mkv"))),
        ]);
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, None).unwrap();
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|i| i.id.starts_with("fs:")));
        assert_ne!(items[0].id, items[1].id);
    }

    #[test]
    fn identity_roots_canonicalise_local_identity_across_mounts() {
        // The same file reached under two configured mount roots derives one
        // identity, so the cross-mount duplicate collapses.
        let roots = vec!["/mnt/media".to_string(), "/Volumes/media".to_string()];
        let inc = include_with(vec![
            Entry::Item(Box::new(local_entry("/mnt/media/friends/s01e01.mkv"))),
            Entry::Item(Box::new(local_entry("/Volumes/media/friends/s01e01.mkv"))),
        ]);
        let items = resolve_channel(&channel(vec![inc]), path(), &roots, None, None).unwrap();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn lavfi_and_http_ids_derive_from_their_defining_field() {
        let lavfi = ItemEntry {
            source: SourceConfig::Lavfi {
                params: "testsrc".into(),
            },
            in_point: None,
            out_point: Some(Duration::from_secs(5)),
            program: None,
            guide: None,
        };
        let http = ItemEntry {
            source: SourceConfig::Http {
                uri: "https://ex/y.mkv".into(),
                headers: None,
                user_agent: None,
            },
            in_point: None,
            out_point: Some(Duration::from_secs(5)),
            program: None,
            guide: None,
        };
        let inc = include_with(vec![
            Entry::Item(Box::new(lavfi)),
            Entry::Item(Box::new(http)),
        ]);
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, None).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["lavfi:testsrc", "http:https://ex/y.mkv"]);
    }

    #[test]
    fn keep_preserves_duplicates() {
        let mut inc = include_with(vec![
            Entry::Item(Box::new(item_entry("a"))),
            Entry::Item(Box::new(item_entry("a"))),
        ]);
        inc.duplicates = Some(Duplicates::Keep);
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, None).unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn count_mode_truncates_after_dedup() {
        let mut inc = include_with(vec![
            Entry::Item(Box::new(item_entry("a"))),
            Entry::Item(Box::new(item_entry("b"))),
            Entry::Item(Box::new(item_entry("c"))),
        ]);
        inc.mode = Mode::Count(2);
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, None).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["lavfi:a", "lavfi:b"]);
    }

    #[test]
    fn block_program_defaults_cascade() {
        let mut inc = include_with(vec![Entry::Item(Box::new(item_entry("a")))]);
        inc.program = Some(ProgramMetadata {
            title: Some("Default Title".into()),
            sub_title: None,
            description: None,
            season: None,
            episode: None,
            categories: Some(vec!["Movie".into()]),
            content_rating: None,
            artwork_url: None,
            year: None,
            credits: None,
            country: None,
            star_rating: None,
        });
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, None).unwrap();
        let p = items[0].program.as_ref().unwrap();
        assert_eq!(p.title.as_deref(), Some("Default Title"));
        assert_eq!(p.categories.as_ref().unwrap(), &vec!["Movie".to_string()]);
    }

    #[test]
    fn item_program_overrides_block_default_field() {
        let mut item = item_entry("a");
        item.program = Some(ProgramMetadata {
            title: Some("Specific".into()),
            sub_title: None,
            description: None,
            season: None,
            episode: None,
            categories: None,
            content_rating: None,
            artwork_url: None,
            year: None,
            credits: None,
            country: None,
            star_rating: None,
        });
        let mut inc = include_with(vec![Entry::Item(Box::new(item))]);
        inc.program = Some(ProgramMetadata {
            title: Some("Default".into()),
            sub_title: None,
            description: None,
            season: None,
            episode: None,
            categories: Some(vec!["Movie".into()]),
            content_rating: None,
            artwork_url: None,
            year: None,
            credits: None,
            country: None,
            star_rating: None,
        });
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, None).unwrap();
        let p = items[0].program.as_ref().unwrap();
        // item title wins; block category fills the gap.
        assert_eq!(p.title.as_deref(), Some("Specific"));
        assert_eq!(p.categories.as_ref().unwrap(), &vec!["Movie".to_string()]);
    }

    #[test]
    fn query_entry_without_catalog_errors() {
        use crate::config::QueryEntry;
        let inc = include_with(vec![Entry::Query(QueryEntry {
            query: "item.type == \"movie\"".into(),
            order: None,
        })]);
        let err = resolve_channel(&channel(vec![inc]), path(), &[], None, None).unwrap_err();
        assert!(format!("{err}").contains("catalog"), "err = {err}");
    }

    #[test]
    fn non_manual_order_without_catalog_errors() {
        let mut inc = include_with(vec![Entry::Item(Box::new(item_entry("a")))]);
        inc.order = Some(Order::Random);
        let err = resolve_channel(&channel(vec![inc]), path(), &[], None, None).unwrap_err();
        assert!(format!("{err}").contains("catalog"), "err = {err}");
    }

    #[test]
    fn rejects_empty_channel() {
        let err = resolve_channel(&channel(vec![]), path(), &[], None, None).unwrap_err();
        assert!(format!("{err}").contains("zero items"), "err = {err}");
    }

    // ---- catalog-backed pipeline (#71) ------------------------------------

    use crate::catalog::ingest::canonical_index;
    use crate::catalog::{
        Catalog, Collection as CatCollection, Entry as CatEntry, EntrySource, Source,
    };
    use crate::config::QueryEntry;

    fn seeded_catalog() -> Catalog {
        let c = Catalog::open_in_memory().unwrap();
        for (id, title, year) in [
            ("imdb:tt0120737", "The Fellowship of the Ring", 2001),
            ("imdb:tt0167261", "The Two Towers", 2002),
            ("imdb:tt0167260", "The Return of the King", 2003),
        ] {
            let mut e = CatEntry::new(id, "movie", title, Source::Plex);
            e.year = Some(year);
            e.release_date = Some(format!("{year}-12-15"));
            c.upsert_entry(&e).unwrap();
            c.add_source(&EntrySource {
                source: Source::LocalFs,
                source_id: format!("fs-{id}"),
                entry_id: id.to_string(),
                playback_path: format!("/media/lotr/{id}.mkv"),
                last_seen: None,
            })
            .unwrap();
        }
        c
    }

    fn query_block(query: &str, order: Order) -> BlockInclude {
        let mut inc = include_with(vec![Entry::Query(QueryEntry {
            query: query.into(),
            order: None,
        })]);
        inc.order = Some(order);
        inc
    }

    #[test]
    fn query_resolves_and_orders_by_release_date() {
        let cat = seeded_catalog();
        let inc = query_block(
            "item.title.contains(\"Ring\") || item.title.contains(\"Tower\") || item.title.contains(\"King\")",
            Order::parse("release_date:asc").unwrap(),
        );
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, Some(&cat)).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["imdb:tt0120737", "imdb:tt0167261", "imdb:tt0167260"]
        );
        // Program metadata + playback path came from the catalog.
        assert_eq!(items[0].program.as_ref().unwrap().year, Some(2001));
        match &items[0].source {
            SourceConfig::Local { path } => assert!(path.ends_with("tt0120737.mkv")),
            other => panic!("expected local source, got {other:?}"),
        }
    }

    /// #186: `entries.summary` reaches `ProgramMetadata.description`, so the
    /// XMLTV guide carries a synopsis with no channel config. An entry with
    /// no summary must emit `description: None` rather than an empty string,
    /// so the vendored xmltv writer's `if let Some` skip stays correct.
    #[test]
    fn catalog_summary_populates_program_description() {
        let cat = seeded_catalog();
        cat.upsert_entry(&{
            let mut e = cat.entry("imdb:tt0120737").unwrap().unwrap();
            e.summary = Some("A hobbit sets out to destroy a ring.".into());
            e
        })
        .unwrap();
        let inc = query_block(
            "item.title.contains(\"Ring\") || item.title.contains(\"Tower\") || item.title.contains(\"King\")",
            Order::parse("release_date:asc").unwrap(),
        );
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, Some(&cat)).unwrap();
        let with_summary = items.iter().find(|i| i.id == "imdb:tt0120737").unwrap();
        assert_eq!(
            with_summary
                .program
                .as_ref()
                .unwrap()
                .description
                .as_deref(),
            Some("A hobbit sets out to destroy a ring.")
        );
        let without_summary = items.iter().find(|i| i.id == "imdb:tt0167261").unwrap();
        assert_eq!(without_summary.program.as_ref().unwrap().description, None);
    }

    // ---- block fallback (#97) ----------------------------------------------

    fn empty_query(order: Option<Order>) -> Entry {
        Entry::Query(QueryEntry {
            query: "item.title == \"Nonesuch\"".into(),
            order,
        })
    }

    #[test]
    fn item_fallback_resolves_when_primary_entries_match_nothing() {
        let cat = seeded_catalog();
        let mut inc = include_with(vec![empty_query(None)]);
        inc.fallback = Some(Fallback::Item(Box::new(item_entry("standby"))));
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, Some(&cat)).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["lavfi:standby"]);
    }

    #[test]
    fn query_fallback_resolves_and_keeps_its_own_order() {
        let cat = seeded_catalog();
        let mut inc = include_with(vec![empty_query(None)]);
        inc.fallback = Some(Fallback::Query(QueryEntry {
            query: "item.title.contains(\"Ring\") || item.title.contains(\"Tower\") \
                     || item.title.contains(\"King\")"
                .into(),
            order: Some(Order::parse("release_date:asc").unwrap()),
        }));
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, Some(&cat)).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["imdb:tt0120737", "imdb:tt0167261", "imdb:tt0167260"]
        );
    }

    #[test]
    fn fallback_is_ignored_when_primary_entries_resolve_to_something() {
        let cat = seeded_catalog();
        let mut inc = include_with(vec![Entry::Query(QueryEntry {
            query: "item.year >= 2001".into(),
            order: None,
        })]);
        inc.fallback = Some(Fallback::Item(Box::new(item_entry("standby"))));
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, Some(&cat)).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(items.len(), 3);
        assert!(
            !ids.contains(&"lavfi:standby"),
            "fallback aired despite non-empty primary entries: {ids:?}"
        );
    }

    /// #97's opt-in guarantee: a block that names no `fallback` must behave
    /// exactly as it did before this field existed, including still failing
    /// the channel when its entries resolve to nothing.
    #[test]
    fn a_block_with_no_fallback_still_resolves_to_empty() {
        let cat = seeded_catalog();
        let inc = include_with(vec![empty_query(None)]);
        let err = resolve_channel(&channel(vec![inc]), path(), &[], None, Some(&cat)).unwrap_err();
        assert!(format!("{err}").contains("zero items"), "err = {err}");
    }

    /// The fallback runs through the block's own duplicates/order/mode exactly
    /// like a primary entry's result would — `mode = { count: 2 }` still
    /// truncates a fallback that matched three items.
    #[test]
    fn fallback_result_still_goes_through_the_blocks_mode_and_order() {
        let cat = seeded_catalog();
        let mut inc = include_with(vec![empty_query(None)]);
        inc.mode = Mode::Count(2);
        inc.order = Some(Order::parse("release_date:asc").unwrap());
        inc.fallback = Some(Fallback::Query(QueryEntry {
            query: "item.year >= 2001".into(),
            order: None,
        }));
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, Some(&cat)).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["imdb:tt0120737", "imdb:tt0167261"]);
    }

    // ---- episode default order (#95) --------------------------------------

    /// Three episodes seeded so `entry_id` order (alphabetical), season/episode
    /// order, and insertion order all disagree — a passing assertion for the
    /// season/episode default can only come from reading `season`/`episode`,
    /// not from the id or the seed order.
    fn out_of_order_episodes_catalog() -> Catalog {
        let cat = Catalog::open_in_memory().unwrap();
        for (id, season, episode) in [("id-c", 1, 2), ("id-a", 1, 1), ("id-b", 2, 1)] {
            let mut e = CatEntry::new(id, "episode", id, Source::Plex);
            e.season = Some(season);
            e.episode = Some(episode);
            cat.upsert_entry(&e).unwrap();
            cat.add_source(&EntrySource {
                source: Source::LocalFs,
                source_id: format!("fs-{id}"),
                entry_id: id.to_string(),
                playback_path: format!("/media/{id}.mkv"),
                last_seen: None,
            })
            .unwrap();
        }
        cat
    }

    #[test]
    fn absent_block_order_over_an_all_episode_set_injects_the_season_episode_default() {
        let cat = out_of_order_episodes_catalog();
        let mut inc = include_with(vec![Entry::Query(QueryEntry {
            query: "item.type == \"episode\"".into(),
            order: None,
        })]);
        inc.order = None; // the author wrote nothing at all
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, Some(&cat)).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["id-a", "id-c", "id-b"],
            "season:asc,episode:asc, not entry_id order ({ids:?})"
        );
    }

    #[test]
    fn explicit_manual_order_over_an_all_episode_set_keeps_authored_order() {
        let cat = out_of_order_episodes_catalog();
        // `include_with`'s default order is `Some(Order::Manual)` — the author
        // explicitly wrote `manual`, which must win even though every item is
        // episode-typed.
        let inc = include_with(vec![Entry::Query(QueryEntry {
            query: "item.type == \"episode\"".into(),
            order: None,
        })]);
        assert_eq!(inc.order, Some(Order::Manual));
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, Some(&cat)).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["id-a", "id-b", "id-c"],
            "entry_id order from the query, unmoved by the episode default ({ids:?})"
        );
    }

    #[test]
    fn absent_block_order_over_a_non_episode_set_keeps_todays_manual_behavior() {
        let cat = seeded_catalog(); // all "movie" type
        let mut inc = include_with(vec![Entry::Query(QueryEntry {
            query: "item.year >= 2001".into(),
            order: None,
        })]);
        inc.order = None; // the author wrote nothing at all
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, Some(&cat)).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["imdb:tt0120737", "imdb:tt0167260", "imdb:tt0167261"],
            "a non-episode-typed set stays in entry_id order, unaffected by #95 ({ids:?})"
        );
    }

    #[test]
    fn field_order_keeps_non_catalog_items_after_the_sorted_set() {
        let cat = seeded_catalog();
        // A block mixing an inline lavfi item (not in the catalog) with a query,
        // sorted by release_date. The inline item can't be ranked — it must
        // survive, appended after the ranked query results, never dropped.
        let mut inc = include_with(vec![
            Entry::Item(Box::new(item_entry("bumper"))),
            Entry::Query(QueryEntry {
                query: "item.year >= 2001".into(),
                order: None,
            }),
        ]);
        inc.order = Some(Order::parse("release_date:asc").unwrap());
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, Some(&cat)).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "imdb:tt0120737",
                "imdb:tt0167261",
                "imdb:tt0167260",
                "lavfi:bumper"
            ]
        );
    }

    // ---- block filter (#197) -----------------------------------------------

    #[test]
    fn filter_seasons_keeps_only_the_matching_season() {
        let cat = out_of_order_episodes_catalog(); // id-c/id-a season 1, id-b season 2
        let mut inc = include_with(vec![Entry::Query(QueryEntry {
            query: "item.type == \"episode\"".into(),
            order: None,
        })]);
        inc.order = None; // let the season/episode default (#95) order the survivors
        inc.filter = Some(Filter {
            seasons: Some(vec![1]),
            episode_ids: None,
        });
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, Some(&cat)).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["id-a", "id-c"],
            "season 2's id-b must be filtered out before the season/episode default \
             orders the rest ({ids:?})"
        );
    }

    #[test]
    fn filter_episode_ids_keeps_only_the_named_ids_with_no_catalog() {
        // episode_ids matches the same derived id every other step keys on, so
        // it needs no catalog at all — exercised entirely with inline items.
        let mut inc = include_with(vec![
            Entry::Item(Box::new(item_entry("a"))),
            Entry::Item(Box::new(item_entry("b"))),
            Entry::Item(Box::new(item_entry("c"))),
        ]);
        inc.filter = Some(Filter {
            seasons: None,
            episode_ids: Some(vec!["lavfi:b".into()]),
        });
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, None).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["lavfi:b"]);
    }

    #[test]
    fn filter_seasons_without_a_catalog_errors() {
        let mut inc = include_with(vec![Entry::Item(Box::new(item_entry("a")))]);
        inc.filter = Some(Filter {
            seasons: Some(vec![1]),
            episode_ids: None,
        });
        let err = resolve_channel(&channel(vec![inc]), path(), &[], None, None).unwrap_err();
        assert!(
            format!("{err}").contains("filter.seasons needs the catalog"),
            "err = {err}"
        );
    }

    /// Both filter fields set narrows to their intersection, not their union —
    /// naming an id from a season the `seasons` field excludes matches nothing.
    #[test]
    fn filter_seasons_and_episode_ids_combine_as_an_intersection() {
        let cat = out_of_order_episodes_catalog();
        let mut inc = include_with(vec![Entry::Query(QueryEntry {
            query: "item.type == \"episode\"".into(),
            order: None,
        })]);
        inc.filter = Some(Filter {
            seasons: Some(vec![1]),                 // id-a, id-c
            episode_ids: Some(vec!["id-b".into()]), // season 2 — disjoint from the above
        });
        let err = resolve_channel(&channel(vec![inc]), path(), &[], None, Some(&cat)).unwrap_err();
        assert!(format!("{err}").contains("zero items"), "err = {err}");
    }

    #[test]
    fn empty_filter_table_is_not_applied() {
        let cat = out_of_order_episodes_catalog();
        let mut inc = include_with(vec![Entry::Query(QueryEntry {
            query: "item.type == \"episode\"".into(),
            order: None,
        })]);
        inc.order = None;
        inc.filter = Some(Filter {
            seasons: None,
            episode_ids: None,
        });
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, Some(&cat)).unwrap();
        assert_eq!(items.len(), 3, "an empty [filter] table restricts nothing");
    }

    // ---- collection entries (#107) ----------------------------------------

    /// The seeded catalog plus a "Halloween Marathon" collection whose authored
    /// positions deliberately contradict both release order and `entry_id`
    /// order, so a passing test can only be reading `position`.
    fn catalog_with_marathon() -> Catalog {
        let c = seeded_catalog();
        c.upsert_collection(&CatCollection {
            collection_id: "plex:coll:1".into(),
            name: "Halloween Marathon".into(),
            source: Source::Plex,
        })
        .unwrap();
        c.add_collection_item("plex:coll:1", "imdb:tt0167260", 0)
            .unwrap(); // Return of the King first
        c.add_collection_item("plex:coll:1", "imdb:tt0120737", 1)
            .unwrap(); // then Fellowship
        c.add_collection_item("plex:coll:1", "imdb:tt0167261", 2)
            .unwrap(); // then Two Towers
        c
    }

    fn collection_block(name: &str) -> BlockInclude {
        include_with(vec![Entry::Collection(CollectionEntry {
            name: name.into(),
        })])
    }

    #[test]
    fn collection_entry_plays_members_in_authored_position_order() {
        let cat = catalog_with_marathon();
        let inc = collection_block("Halloween Marathon");
        // Block order is left at its `manual` default — the run is already
        // ordered, and nothing re-sorts it.
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, Some(&cat)).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["imdb:tt0167260", "imdb:tt0120737", "imdb:tt0167261"]
        );
        // Catalog-backed like a query result: metadata and playback path resolved.
        assert_eq!(items[0].program.as_ref().unwrap().year, Some(2003));
        match &items[0].source {
            SourceConfig::Local { path } => assert!(path.ends_with("tt0167260.mkv")),
            other => panic!("expected local source, got {other:?}"),
        }
    }

    #[test]
    fn collection_entry_composes_with_other_entries_in_authored_order() {
        // A bumper, then the marathon. The block stays `manual`, so the bumper
        // leads and the collection's internal order survives intact.
        let cat = catalog_with_marathon();
        let inc = include_with(vec![
            Entry::Item(Box::new(item_entry("bumper"))),
            Entry::Collection(CollectionEntry {
                name: "Halloween Marathon".into(),
            }),
        ]);
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, Some(&cat)).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "lavfi:bumper",
                "imdb:tt0167260",
                "imdb:tt0120737",
                "imdb:tt0167261"
            ]
        );
    }

    #[test]
    fn unknown_collection_name_errors() {
        let cat = catalog_with_marathon();
        let inc = collection_block("Nonesuch");
        let err = resolve_channel(&channel(vec![inc]), path(), &[], None, Some(&cat)).unwrap_err();
        assert!(
            format!("{err}").contains("no collection named"),
            "err = {err}"
        );
    }

    #[test]
    fn ambiguous_collection_name_errors() {
        // Two sources each define a collection of the same name — the entry
        // names one collection, so this is a config error, not a merge.
        let cat = catalog_with_marathon();
        cat.upsert_collection(&CatCollection {
            collection_id: "plex:coll:2".into(),
            name: "Halloween Marathon".into(),
            source: Source::Plex,
        })
        .unwrap();
        let inc = collection_block("Halloween Marathon");
        let err = resolve_channel(&channel(vec![inc]), path(), &[], None, Some(&cat)).unwrap_err();
        assert!(
            format!("{err}").contains("must name exactly one"),
            "err = {err}"
        );
    }

    #[test]
    fn empty_collection_errors_rather_than_vanishing() {
        let cat = catalog_with_marathon();
        cat.upsert_collection(&CatCollection {
            collection_id: "plex:coll:empty".into(),
            name: "Empty Shelf".into(),
            source: Source::Plex,
        })
        .unwrap();
        let inc = collection_block("Empty Shelf");
        let err = resolve_channel(&channel(vec![inc]), path(), &[], None, Some(&cat)).unwrap_err();
        assert!(format!("{err}").contains("has no members"), "err = {err}");
    }

    #[test]
    fn collection_entry_without_catalog_errors() {
        let inc = collection_block("Halloween Marathon");
        let err = resolve_channel(&channel(vec![inc]), path(), &[], None, None).unwrap_err();
        assert!(format!("{err}").contains("catalog"), "err = {err}");
    }

    #[test]
    fn collapse_runs_before_order_deterministic_under_random() {
        // Two blocks would collapse cross-block; here one block with a dup id.
        let mut inc = include_with(vec![
            Entry::Item(Box::new(item_entry("a"))),
            Entry::Item(Box::new(item_entry("a"))),
            Entry::Item(Box::new(item_entry("b"))),
        ]);
        inc.order = Some(Order::Random);
        let cat = seeded_catalog();
        let mut cfg = channel(vec![inc]);
        cfg.seed = Some(7);
        let first = resolve_channel(&cfg, path(), &[], None, Some(&cat)).unwrap();
        let second = resolve_channel(&cfg, path(), &[], None, Some(&cat)).unwrap();
        let ids1: Vec<&str> = first.iter().map(|i| i.id.as_str()).collect();
        let ids2: Vec<&str> = second.iter().map(|i| i.id.as_str()).collect();
        // Collapsed to unique ids, and the seeded shuffle is reproducible.
        assert_eq!(ids1.len(), 2);
        assert_eq!(ids1, ids2);
    }

    #[test]
    fn keep_with_manual_preserves_duplicate_items() {
        let mut inc = include_with(vec![
            Entry::Item(Box::new(item_entry("a"))),
            Entry::Item(Box::new(item_entry("a"))),
        ]);
        inc.duplicates = Some(Duplicates::Keep);
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, None).unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn manual_local_item_collapses_with_a_query_for_the_same_file() {
        // The payoff of catalog-aware identity: a block holds a manual `local`
        // item pointing at a library file AND a query that returns that same
        // file. The manual item inherits the catalog entry_id, so the two
        // collapse to one under the default policy — three films, not four.
        let cat = seeded_catalog();
        let inc = include_with(vec![
            Entry::Item(Box::new(local_entry("/media/lotr/imdb:tt0120737.mkv"))),
            Entry::Query(QueryEntry {
                query: "item.year >= 2001".into(),
                order: None,
            }),
        ]);
        let index = canonical_index(&cat, &[]).unwrap();
        let items =
            resolve_channel(&channel(vec![inc]), path(), &[], Some(&index), Some(&cat)).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(items.len(), 3);
        assert!(ids.contains(&"imdb:tt0120737"));
    }

    // ---- pattern blocks (#72) ---------------------------------------------

    /// A catalog with two shows of different lengths and two movies — enough to
    /// prove the interleave and the independent progression end to end.
    fn interleave_catalog() -> Catalog {
        let c = Catalog::open_in_memory().unwrap();
        let add = |id: &str, kind: &str, show: Option<(&str, i64)>| {
            let mut e = CatEntry::new(id, kind, format!("Title {id}"), Source::Plex);
            if let Some((show_id, episode)) = show {
                e.show_id = Some(show_id.into());
                e.show = Some(show_id.trim_start_matches("show:").to_string());
                e.season = Some(1);
                e.episode = Some(episode);
            }
            c.upsert_entry(&e).unwrap();
            c.add_source(&EntrySource {
                source: Source::LocalFs,
                source_id: format!("fs-{id}"),
                entry_id: id.to_string(),
                playback_path: format!("/media/{id}.mkv"),
                last_seen: None,
            })
            .unwrap();
        };
        add("mov-1", "movie", None);
        add("mov-2", "movie", None);
        for n in 1..=4 {
            add(&format!("got-e{n}"), "episode", Some(("show:got", n)));
        }
        for n in 1..=2 {
            add(&format!("inv-e{n}"), "episode", Some(("show:inv", n)));
        }
        c
    }

    fn interleave_block(advance: crate::config::Advance) -> BlockInclude {
        use crate::config::{OnShort, PatternStep, Pool, Rotate, Select};
        let mut inc = include_with(vec![]);
        inc.pools = vec![
            Pool {
                name: "movies".into(),
                expr: Some("item.type == \"movie\"".into()),
                plugin: None,
                sources: None,
                groups: Vec::new(),
                order: Some(Order::parse("title:asc").unwrap()),
                bucket_order: None,
                group_by: Default::default(),
                select: Select::RoundRobin,
                rotate: Rotate::Visit,
                advance,
                on_short: OnShort::Next,
                constraints: None,
                config: None,
                capabilities: Vec::new(),
                datastores: Vec::new(),
                guide: None,
            },
            Pool {
                name: "shows".into(),
                expr: Some("item.type == \"episode\"".into()),
                plugin: None,
                sources: None,
                groups: Vec::new(),
                order: Some(Order::parse("season:asc,episode:asc").unwrap()),
                bucket_order: None,
                group_by: Default::default(),
                select: Select::RoundRobin,
                rotate: Rotate::Visit,
                advance,
                on_short: OnShort::Next,
                constraints: None,
                config: None,
                capabilities: Vec::new(),
                datastores: Vec::new(),
                guide: None,
            },
        ];
        inc.pattern = vec![
            PatternStep {
                pool: "movies".into(),
                take: crate::config::Take::Count(1),
                from: crate::config::TakeFrom::Start,
                chance: 1.0,
            },
            PatternStep {
                pool: "shows".into(),
                take: crate::config::Take::Count(2),
                from: crate::config::TakeFrom::Start,
                chance: 1.0,
            },
        ];
        inc.cycles = Some(2);
        inc
    }

    /// Project the state a following window would be handed, exactly as the
    /// daemon does: the pools' rotation from this resolve, and the per-series
    /// cursor read back out of the play-history store the airings were
    /// recorded in (#70, sqlite-backed since #111).
    fn advance_state(
        cat: &Catalog,
        prev: &crate::resume::GenerationState,
        resume: ResumeMap,
        items: &[ResolvedItem],
    ) -> crate::resume::GenerationState {
        use crate::history::{HistoryDb, PlayRecord};
        use time::OffsetDateTime;

        const CHANNEL: &str = "test";

        let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
        let show_ids = cat.show_ids_for(&ids).unwrap();
        let db = HistoryDb::open_in_memory().unwrap();
        // Seed with whatever the previous windows had already recorded, so the
        // projection sees the channel's whole history and not just this window.
        let seed: Vec<PlayRecord> = prev
            .cursor
            .iter()
            .map(|(key, entry_id)| PlayRecord {
                entry_id: entry_id.clone(),
                show_id: Some(key.clone()),
                start: OffsetDateTime::UNIX_EPOCH,
                played_at: OffsetDateTime::UNIX_EPOCH,
                error_card: false,
            })
            .collect();
        db.record(CHANNEL, &seed).unwrap();
        let airings: Vec<PlayRecord> = ids
            .iter()
            .map(|id| PlayRecord {
                entry_id: id.clone(),
                show_id: show_ids.get(id).cloned(),
                start: OffsetDateTime::UNIX_EPOCH,
                played_at: OffsetDateTime::UNIX_EPOCH,
                error_card: false,
            })
            .collect();
        db.record(CHANNEL, &airings).unwrap();
        crate::resume::GenerationState {
            resume,
            cursor: db.series_cursor(CHANNEL).unwrap(),
            tail: db
                .tail(CHANNEL, crate::constrain::DEFAULT_SEAM_TAIL)
                .unwrap(),
        }
    }

    /// The whole pipeline through the public entry point: a pattern block
    /// resolves to the interleaved list, with catalog metadata and playback
    /// paths attached exactly as a query entry gets them.
    #[test]
    fn pattern_block_resolves_through_the_channel() {
        let cat = interleave_catalog();
        let cfg = channel(vec![interleave_block(crate::config::Advance::Restart)]);
        let (items, resume) = resolve_channel_with_resume(
            &cfg,
            path(),
            &[],
            None,
            Some(&cat),
            &GenerationState::empty(),
            &Default::default(),
            None,
            t0(),
        )
        .unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["mov-1", "got-e1", "got-e2", "mov-2", "inv-e1", "inv-e2"]
        );
        // Catalog-backed like any other resolved item.
        assert_eq!(items[1].program.as_ref().unwrap().episode, Some(1));
        match &items[0].source {
            SourceConfig::Local { path } => assert!(path.ends_with("mov-1.mkv")),
            other => panic!("expected local source, got {other:?}"),
        }
        // Both pools reported their rotation, keyed by pool name; where each
        // series stopped lives in the ledger, not here.
        assert!(resume.pool("movies").is_some());
        assert!(resume.pool("shows").is_some());
        let next = advance_state(&cat, &GenerationState::empty(), resume, &items);
        assert_eq!(next.cursor.get("show:got").unwrap(), "got-e2");
    }

    /// Window continuation with no live cursor: window 2 is generated from
    /// window 1's `resume_out` and each show picks up where it left off.
    #[test]
    fn resume_carries_progression_across_a_window_seam() {
        let cat = interleave_catalog();
        let cfg = channel(vec![interleave_block(crate::config::Advance::Resume)]);

        let (first, next) = resolve_channel_with_resume(
            &cfg,
            path(),
            &[],
            None,
            Some(&cat),
            &GenerationState::empty(),
            &Default::default(),
            None,
            t0(),
        )
        .unwrap();
        let first_ids: Vec<&str> = first.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            first_ids,
            vec!["mov-1", "got-e1", "got-e2", "mov-2", "inv-e1", "inv-e2"]
        );

        let next = advance_state(&cat, &GenerationState::empty(), next, &first);
        let (second, _) = resolve_channel_with_resume(
            &cfg,
            path(),
            &[],
            None,
            Some(&cat),
            &next,
            &Default::default(),
            None,
            t0(),
        )
        .unwrap();
        let second_ids: Vec<&str> = second.iter().map(|i| i.id.as_str()).collect();
        // got continues at e3 (it never restarts because inv is shorter), inv
        // wraps, and the movies pool continues its own rotation.
        assert_eq!(
            second_ids,
            vec!["mov-1", "got-e3", "got-e4", "mov-2", "inv-e1", "inv-e2"]
        );
    }

    /// The same three inputs always produce the same two outputs — the property
    /// the whole no-live-cursor model rests on.
    #[test]
    fn generation_is_a_pure_function_of_catalog_config_and_resume() {
        let cat = interleave_catalog();
        let cfg = channel(vec![interleave_block(crate::config::Advance::Resume)]);
        let (first, next) = resolve_channel_with_resume(
            &cfg,
            path(),
            &[],
            None,
            Some(&cat),
            &GenerationState::empty(),
            &Default::default(),
            None,
            t0(),
        )
        .unwrap();
        let state = advance_state(&cat, &GenerationState::empty(), next, &first);

        let (a, ra) = resolve_channel_with_resume(
            &cfg,
            path(),
            &[],
            None,
            Some(&cat),
            &state,
            &Default::default(),
            None,
            t0(),
        )
        .unwrap();
        let (b, rb) = resolve_channel_with_resume(
            &cfg,
            path(),
            &[],
            None,
            Some(&cat),
            &state,
            &Default::default(),
            None,
            t0(),
        )
        .unwrap();
        let ids_a: Vec<&str> = a.iter().map(|i| i.id.as_str()).collect();
        let ids_b: Vec<&str> = b.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids_a, ids_b);
        assert_eq!(ra, rb);
    }

    /// #70 acceptance: a show that leaves the resolved set and comes back
    /// resumes from its stored position, not from its first episode.
    ///
    /// This is exactly what a churning "Trending" list does. The ledger is
    /// keyed by `show_id` and is never pruned to the current set, so a show's
    /// position outlives its absence — which is why the cursor could not be a
    /// per-generation index.
    #[test]
    fn a_show_that_leaves_and_returns_resumes_where_it_stopped() {
        let cat = interleave_catalog();
        let cfg = channel(vec![interleave_block(crate::config::Advance::Resume)]);

        // Window 1 airs both shows; GoT reaches e2.
        let (first, next) = resolve_channel_with_resume(
            &cfg,
            path(),
            &[],
            None,
            Some(&cat),
            &GenerationState::empty(),
            &Default::default(),
            None,
            t0(),
        )
        .unwrap();
        let state = advance_state(&cat, &GenerationState::empty(), next, &first);
        assert_eq!(state.cursor.get("show:got").unwrap(), "got-e2");

        // GoT drops out of the resolved set entirely for a while — the pool's
        // expr no longer matches it. Its ledger entries stay.
        let mut narrowed = interleave_block(crate::config::Advance::Resume);
        for pool in &mut narrowed.pools {
            if pool.name == "shows" {
                pool.expr = Some("item.show == \"inv\"".into());
            }
        }
        let narrowed_cfg = channel(vec![narrowed]);
        let (away, next_away) = resolve_channel_with_resume(
            &narrowed_cfg,
            path(),
            &[],
            None,
            Some(&cat),
            &state,
            &Default::default(),
            None,
            t0(),
        )
        .unwrap();
        assert!(
            !away.iter().any(|i| i.id.starts_with("got-")),
            "GoT is out of the set for this window"
        );
        let state = advance_state(&cat, &state, next_away, &away);

        // It comes back. It must continue at e3, not restart at e1.
        let (back, _) = resolve_channel_with_resume(
            &cfg,
            path(),
            &[],
            None,
            Some(&cat),
            &state,
            &Default::default(),
            None,
            t0(),
        )
        .unwrap();
        let first_got = back
            .iter()
            .map(|i| i.id.as_str())
            .find(|id| id.starts_with("got-"))
            .expect("GoT returns to the set");
        assert_eq!(
            first_got, "got-e3",
            "a returning show resumes from its stored position, not S1E1"
        );
    }

    /// #70 acceptance: one play-history row per scheduled airing — no more, no
    /// fewer. The row count is what makes the cursor's projection correct, so
    /// a duplicate or a dropped row is a scheduling bug, not a bookkeeping
    /// one.
    #[test]
    fn every_scheduled_airing_records_exactly_one_row() {
        use crate::history::{HistoryDb, PlayRecord};
        use time::OffsetDateTime;

        const CHANNEL: &str = "test";

        let cat = interleave_catalog();
        let cfg = channel(vec![interleave_block(crate::config::Advance::Restart)]);
        let (items, _) = resolve_channel_with_resume(
            &cfg,
            path(),
            &[],
            None,
            Some(&cat),
            &GenerationState::empty(),
            &Default::default(),
            None,
            t0(),
        )
        .unwrap();

        let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
        let show_ids = cat.show_ids_for(&ids).unwrap();
        let db = HistoryDb::open_in_memory().unwrap();
        let records: Vec<PlayRecord> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| PlayRecord {
                entry_id: id.clone(),
                show_id: show_ids.get(id).cloned(),
                start: OffsetDateTime::UNIX_EPOCH + time::Duration::minutes(i as i64),
                played_at: OffsetDateTime::UNIX_EPOCH,
                error_card: false,
            })
            .collect();
        db.record(CHANNEL, &records).unwrap();

        assert_eq!(
            db.count(CHANNEL).unwrap(),
            items.len(),
            "one row per airing — the generation aired {} items",
            items.len()
        );
        // A repeat under `wrap = "loop"` is a genuine second airing and gets
        // its own row; the cursor still resolves to the latest one.
        let cursor = db.series_cursor(CHANNEL).unwrap();
        assert_eq!(cursor.get("show:got").unwrap(), "got-e2");
    }

    /// The stateless entry point stays stateless: `resolve_channel` never
    /// consults a resume map, so a `resume` pool replays from the top.
    #[test]
    fn resolve_channel_ignores_resume_state() {
        let cat = interleave_catalog();
        let cfg = channel(vec![interleave_block(crate::config::Advance::Resume)]);
        let first = resolve_channel(&cfg, path(), &[], None, Some(&cat)).unwrap();
        let second = resolve_channel(&cfg, path(), &[], None, Some(&cat)).unwrap();
        let ids1: Vec<&str> = first.iter().map(|i| i.id.as_str()).collect();
        let ids2: Vec<&str> = second.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids1, ids2);
    }

    /// A channel with no pattern block never grows a resume map, so the sidecar
    /// only ever appears for channels that need it.
    #[test]
    fn an_entries_channel_records_its_list_position_and_no_pools() {
        let inc = include_with(vec![Entry::Item(Box::new(item_entry("a")))]);
        let (_, next) = resolve_channel_with_resume(
            &channel(vec![inc]),
            path(),
            &[],
            None,
            None,
            &GenerationState::empty(),
            &Default::default(),
            None,
            t0(),
        )
        .unwrap();
        assert!(next.pools.is_empty(), "a flat channel has no pools");
        // One item, laid whole, wrapping straight back to the top.
        assert_eq!(next.position, 0);
    }

    // ---- bounding a flat entries generation by the window (#118) ------------

    /// One generation of a flat `entries` channel: `at` is where the last one
    /// left off, `fill` the airtime still wanted. Every item here runs 30s.
    fn windowed_entries(
        entries: usize,
        at: usize,
        fill: Option<time::Duration>,
    ) -> (Vec<String>, usize) {
        let inc = include_with(
            (0..entries)
                .map(|i| Entry::Item(Box::new(item_entry(&i.to_string()))))
                .collect(),
        );
        let mut state = GenerationState::empty();
        state.resume.position = at;
        let (items, next) = resolve_channel_with_resume(
            &channel(vec![inc]),
            path(),
            &[],
            None,
            None,
            &state,
            &Default::default(),
            fill.map(|d| d.unsigned_abs()),
            t0(),
        )
        .unwrap();
        (items.into_iter().map(|i| i.id).collect(), next.position)
    }

    /// The bug: a list far longer than the window was laid end to end in one
    /// generation. Ten 30s items is five minutes of playout; a 90s window must
    /// take three of them, not all ten.
    #[test]
    fn a_windowed_entries_generation_stops_at_the_window() {
        let (ids, next) = windowed_entries(10, 0, Some(time::Duration::seconds(90)));
        assert_eq!(ids, vec!["lavfi:0", "lavfi:1", "lavfi:2"]);
        assert_eq!(next, 3, "the next generation continues at item 3");
    }

    /// The acceptance bar: two generations back to back play the authored list
    /// straight through — nothing skipped, nothing aired twice.
    #[test]
    fn two_windowed_generations_continue_with_no_gap_and_no_repeat() {
        let window = Some(time::Duration::seconds(90));
        let (first, after_first) = windowed_entries(10, 0, window);
        let (second, _) = windowed_entries(10, after_first, window);

        let played: Vec<String> = first.iter().chain(second.iter()).cloned().collect();
        assert_eq!(
            played,
            (0..6).map(|i| format!("lavfi:{i}")).collect::<Vec<_>>(),
            "the two generations must concatenate to the authored order"
        );
    }

    /// The list is a loop, so a generation that reaches the end starts the next
    /// one at the top rather than running off it.
    #[test]
    fn an_entries_channel_wraps_to_the_top_at_the_end_of_its_list() {
        let (ids, next) = windowed_entries(4, 3, Some(time::Duration::seconds(30)));
        assert_eq!(ids, vec!["lavfi:3"]);
        assert_eq!(next, 0);
    }

    /// Deleting the `.resume` sidecar leaves position 0, which is the top of the
    /// list — a restart, never an error.
    #[test]
    fn a_missing_resume_starts_an_entries_channel_at_the_top() {
        let (ids, _) = windowed_entries(4, 0, Some(time::Duration::seconds(30)));
        assert_eq!(ids, vec!["lavfi:0"]);
    }

    /// A window shorter than a single item still airs one: a generation that
    /// laid nothing would never move the clock forward.
    #[test]
    fn a_window_shorter_than_one_item_still_lays_one() {
        let (ids, next) = windowed_entries(4, 0, Some(time::Duration::seconds(1)));
        assert_eq!(ids, vec!["lavfi:0"]);
        assert_eq!(next, 1);
    }

    /// The stateless entry point asks for no window and still gets the list
    /// whole — the tests and the one-shot resolve depend on it.
    #[test]
    fn an_unbounded_entries_resolve_still_lays_the_whole_list() {
        let (ids, next) = windowed_entries(10, 0, None);
        assert_eq!(ids.len(), 10);
        assert_eq!(next, 0);
    }

    /// A position left over from a longer list cannot point off the end of a
    /// shorter one — an edit that deletes entries must not error or skip.
    #[test]
    fn a_position_past_the_end_of_an_edited_list_folds_back_into_it() {
        let (ids, next) = windowed_entries(3, 100, Some(time::Duration::seconds(30)));
        assert_eq!(ids, vec!["lavfi:1"], "100 % 3 is item 1");
        assert_eq!(next, 2);
    }

    /// A pattern channel that has played all the way through its content keeps
    /// broadcasting: the next window resolves a full list, not an empty one.
    /// There is no exhausted state to fall into.
    #[test]
    fn a_pattern_channel_keeps_resolving_after_playing_everything() {
        let cat = interleave_catalog();
        let mut inc = interleave_block(crate::config::Advance::Resume);
        inc.cycles = Some(20); // long enough to run past every series' end
        let cfg = channel(vec![inc]);

        let (played, next) = resolve_channel_with_resume(
            &cfg,
            path(),
            &[],
            None,
            Some(&cat),
            &GenerationState::empty(),
            &Default::default(),
            None,
            t0(),
        )
        .unwrap();
        assert!(!played.is_empty());

        // Second window, after everything has aired at least once: still full.
        let state = advance_state(&cat, &GenerationState::empty(), next, &played);
        let (items, _) = resolve_channel_with_resume(
            &cfg,
            path(),
            &[],
            None,
            Some(&cat),
            &state,
            &Default::default(),
            None,
            t0(),
        )
        .unwrap();
        assert!(
            !items.is_empty(),
            "a channel that played everything must keep going, not run dry"
        );
    }

    #[test]
    fn a_pattern_channel_that_never_played_still_errors_on_zero_items() {
        let cat = interleave_catalog();
        let mut inc = interleave_block(crate::config::Advance::Resume);
        for pool in &mut inc.pools {
            pool.expr = Some("item.type == \"nonesuch\"".into());
        }
        let err = resolve_channel(&channel(vec![inc]), path(), &[], None, Some(&cat)).unwrap_err();
        assert!(format!("{err}").contains("zero items"), "err = {err}");
    }

    #[test]
    fn pattern_block_without_catalog_errors() {
        let cfg = channel(vec![interleave_block(crate::config::Advance::Restart)]);
        let err = resolve_channel(&cfg, path(), &[], None, None).unwrap_err();
        assert!(format!("{err}").contains("catalog"), "err = {err}");
    }

    /// `mode = "count"` still truncates a pattern block's interleaved list.
    #[test]
    fn count_mode_truncates_a_pattern_block() {
        let cat = interleave_catalog();
        let mut inc = interleave_block(crate::config::Advance::Restart);
        inc.mode = Mode::Count(3);
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, Some(&cat)).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["mov-1", "got-e1", "got-e2"]);
    }

    // ---- a channel mixing a pattern block with an entries block (#146) ----
    //
    // No channel in `examples/` mixes block kinds, which is why neither #118's
    // nor #140's acceptance run caught this: `config.is_pattern()` was
    // answered once for the whole channel, so a channel with any pattern
    // block skipped the entries-window cut entirely on its entries half.

    /// A ten-item, 30s-each entries block alongside the two-pool interleave
    /// used by the pattern-block tests above. `interleave_block` authors an
    /// explicit `cycles`, so its output ignores `fill` and is deterministic —
    /// exactly the list `pattern_block_resolves_through_the_channel` asserts.
    fn mixed_channel() -> ChannelConfig {
        let pattern_inc = interleave_block(crate::config::Advance::Restart);
        let entries_inc = include_with(
            (0..10)
                .map(|i| Entry::Item(Box::new(item_entry(&i.to_string()))))
                .collect(),
        );
        channel(vec![pattern_inc, entries_inc])
    }

    const MIXED_PATTERN_IDS: [&str; 6] = ["mov-1", "got-e1", "got-e2", "mov-2", "inv-e1", "inv-e2"];

    /// The acceptance bar's first line: a 90s window over ten 30s entries
    /// items must take three of them, not all ten — even though the same
    /// channel also carries a pattern block, which used to make the whole
    /// channel skip this cut.
    #[test]
    fn a_mixed_channels_entries_block_is_cut_to_the_window_while_its_pattern_block_is_untouched() {
        let cat = interleave_catalog();
        let (items, resume) = resolve_channel_with_resume(
            &mixed_channel(),
            path(),
            &[],
            None,
            Some(&cat),
            &GenerationState::empty(),
            &Default::default(),
            Some(Duration::from_secs(90)),
            t0(),
        )
        .unwrap();

        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        let mut expected: Vec<&str> = MIXED_PATTERN_IDS.to_vec();
        expected.extend(["lavfi:0", "lavfi:1", "lavfi:2"]);
        assert_eq!(
            ids, expected,
            "the pattern block's full interleave, then only as much of the \
             entries block as a 90s window covers — not its whole 10-item list"
        );
        assert_eq!(
            resume.position, 3,
            "the entries cursor advances by what it laid, same as a lone entries channel"
        );
        // The pattern block's own resume mechanism (#140) is untouched by the
        // entries cut living alongside it.
        assert!(resume.pool("movies").is_some());
        assert!(resume.pool("shows").is_some());
    }

    /// The acceptance bar's second line: across two consecutive passes, the
    /// entries half of a mixed channel skips no item and repeats none — and
    /// the pattern half, unaffected by the entries cut, resolves the same
    /// interleaved list each time (`Advance::Restart`'s own, separately
    /// tested, contract — not something this fix touches).
    #[test]
    fn two_generations_of_a_mixed_channel_continue_its_entries_block_with_no_gap_or_repeat() {
        let cat = interleave_catalog();
        let generation = |at: usize| {
            let mut state = GenerationState::empty();
            state.resume.position = at;
            resolve_channel_with_resume(
                &mixed_channel(),
                path(),
                &[],
                None,
                Some(&cat),
                &state,
                &Default::default(),
                Some(Duration::from_secs(90)),
                t0(),
            )
            .unwrap()
        };

        let (first, next) = generation(0);
        let (second, _) = generation(next.position);

        for (items, expected_entries_tail) in [
            (&first, ["lavfi:0", "lavfi:1", "lavfi:2"]),
            (&second, ["lavfi:3", "lavfi:4", "lavfi:5"]),
        ] {
            let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
            assert_eq!(&ids[..MIXED_PATTERN_IDS.len()], &MIXED_PATTERN_IDS[..]);
            assert_eq!(&ids[MIXED_PATTERN_IDS.len()..], &expected_entries_tail[..]);
        }
    }

    /// The shape #146 refuses rather than guesses at: two entries blocks
    /// sharing a channel with a pattern block. #118 made `.resume.position`
    /// one cursor for the channel's whole entries list on purpose, and there
    /// is no non-arbitrary answer for what a fused cursor spanning two
    /// non-contiguous entries spans should splice back to once a pattern
    /// block's span sits between them — so this errors instead of silently
    /// mis-scheduling either span.
    #[test]
    fn two_entries_blocks_alongside_a_pattern_block_errors_rather_than_guessing() {
        let cat = interleave_catalog();
        let pattern_inc = interleave_block(crate::config::Advance::Restart);
        let entries_a = include_with(vec![Entry::Item(Box::new(item_entry("a")))]);
        let entries_b = include_with(vec![Entry::Item(Box::new(item_entry("b")))]);
        let cfg = channel(vec![entries_a, pattern_inc, entries_b]);

        let err = resolve_channel_with_resume(
            &cfg,
            path(),
            &[],
            None,
            Some(&cat),
            &GenerationState::empty(),
            &Default::default(),
            Some(Duration::from_secs(90)),
            t0(),
        )
        .unwrap_err();
        assert!(format!("{err}").contains("entries blocks"), "err = {err}");
    }

    #[test]
    fn per_entry_query_order_is_applied() {
        let cat = seeded_catalog();
        // Block is manual; the query entry carries its own descending order.
        let inc = include_with(vec![Entry::Query(QueryEntry {
            query: "item.year >= 2001".into(),
            order: Some(Order::parse("release_date:desc").unwrap()),
        })]);
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, Some(&cat)).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["imdb:tt0167260", "imdb:tt0167261", "imdb:tt0120737"]
        );
    }

    // ---- named show groups (#165) -----------------------------------------

    /// A `groups`-sourced pool draws every member show's episodes and resolves
    /// through the whole pipeline exactly like an `expr` pool would, proving
    /// the group is a source swap and nothing downstream had to change.
    #[test]
    fn a_groups_sourced_pool_resolves_through_the_whole_pipeline() {
        use crate::config::{
            Advance, GroupBy, OnShort, PatternStep, Pool, Rotate, Select, Take, TakeFrom,
        };

        let cat = interleave_catalog();
        let mut inc = include_with(vec![]);
        inc.pools = vec![Pool {
            name: "shows".into(),
            expr: None,
            plugin: None,
            sources: None,
            groups: vec!["franchise".into()],
            order: None,
            bucket_order: None,
            group_by: GroupBy::Show,
            select: Select::RoundRobin,
            rotate: Rotate::Visit,
            advance: Advance::Restart,
            on_short: OnShort::Next,
            constraints: None,
            config: None,
            capabilities: Vec::new(),
            datastores: Vec::new(),
            guide: None,
        }];
        inc.pattern = vec![PatternStep {
            pool: "shows".into(),
            take: Take::All,
            from: TakeFrom::Start,
            chance: 1.0,
        }];

        let mut cfg = channel(vec![inc]);
        cfg.groups = vec![ShowGroup {
            name: "franchise".into(),
            shows: vec!["got".into(), "inv".into()],
        }];

        let items = resolve_channel(&cfg, path(), &[], None, Some(&cat)).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["got-e1", "got-e2", "got-e3", "got-e4", "inv-e1", "inv-e2"],
            "a `groups` pool must draw every member show's episodes"
        );
    }

    #[test]
    fn validate_groups_against_catalog_names_the_show_and_the_group() {
        let cat = interleave_catalog();
        let groups = vec![ShowGroup {
            name: "franchise".into(),
            shows: vec!["got".into(), "Nonexistent Show".into()],
        }];
        let err = validate_groups_against_catalog(path(), &cat, &groups).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("franchise"), "err = {msg}");
        assert!(msg.contains("Nonexistent Show"), "err = {msg}");
    }

    #[test]
    fn validate_groups_against_catalog_passes_when_every_member_has_episodes() {
        let cat = interleave_catalog();
        let groups = vec![ShowGroup {
            name: "franchise".into(),
            shows: vec!["got".into(), "inv".into()],
        }];
        assert!(validate_groups_against_catalog(path(), &cat, &groups).is_ok());
    }
}

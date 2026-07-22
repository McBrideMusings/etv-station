//! Resolve a channel's block composition into a flat, ordered item list.
//!
//! The Phase C resolve pipeline (#71): for each `[[rule.blocks]]` it
//! **resolves entries → applies `duplicates` → applies `order` → (mode)**,
//! producing the flat [`ResolvedItem`] list the sequencer
//! ([`crate::rule::Sequential`]) lays across the chunk window. Collapse runs
//! *before* order, so which duplicate survives is deterministic regardless of a
//! `random` shuffle. The blocks concatenate, then the adjacency constraint pass
//! ([`crate::constrain`], #73) runs once over the whole list, reaching back
//! across the generation seam via the play-history ledger.
//!
//! `query` entries resolve against the [`Catalog`] (#68 CEL→SQL) and each
//! resolved `entry_id` becomes a `ResolvedItem`; `order` is applied by the
//! order engine (#69). `collection` entries also resolve against the catalog
//! but arrive *already ordered* — their sequence is the collection's stored
//! `position`, so the order step leaves them alone (#107). A channel with no
//! catalog-backed entries and `manual` order needs no catalog, so `catalog` is
//! optional.
//!
//! Still rejected with a clear `unsupported` error (later issues): `include`
//! entries, a non-empty block `filter`, and a block `fallback` (its schema is a
//! follow-up). The catalog is not yet opened by the daemon — until that lands,
//! query entries / non-`manual` order only resolve when a catalog is supplied
//! (tests), and error at runtime.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::time::Duration;

use ersatztv_playout::playout::{PlayoutItemSource, ProgramMetadata};

use crate::catalog::{Catalog, TagNs, canonical_path, derive_entry_id};
use crate::config::{
    BlockInclude, ChannelConfig, CollectionEntry, Duplicates, Entry, ItemEntry, Mode, Order,
    QueryEntry, SourceConfig,
};
use crate::constrain::{ItemKeys, Limits};
use crate::errors::ConfigError;
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
/// `advance = "resume"` start from the top. Use [`resolve_channel_with_resume`]
/// to continue a channel across a window seam.
pub fn resolve_channel(
    config: &ChannelConfig,
    path: &Path,
    source_roots: &[String],
    path_index: Option<&HashMap<String, String>>,
    catalog: Option<&Catalog>,
) -> Result<Vec<ResolvedItem>, ConfigError> {
    let (items, _) = resolve_channel_with_resume(
        config,
        path,
        source_roots,
        path_index,
        catalog,
        &GenerationState::empty(),
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
/// A channel with no pattern block ignores `resume_in` and returns an empty
/// map, so the resume sidecar only ever appears for channels that need it.
pub fn resolve_channel_with_resume(
    config: &ChannelConfig,
    path: &Path,
    source_roots: &[String],
    path_index: Option<&HashMap<String, String>>,
    catalog: Option<&Catalog>,
    state: &GenerationState,
) -> Result<(Vec<ResolvedItem>, ResumeMap), ConfigError> {
    // One seed per generation: a pinned `seed` reproduces the shuffle; an unset
    // one draws fresh entropy so an unseeded `random` block reshuffles each
    // generation (#46 "unset = fresh per generation").
    let seed = config.seed.unwrap_or_else(fresh_seed);

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
    for (idx, include) in config.rule.blocks.iter().enumerate() {
        let block_items = resolve_block(
            include,
            idx,
            path,
            source_roots,
            path_index,
            catalog,
            seed,
            state,
            &mut resume_out,
        )?;
        let c = include.constraints();
        limits.resize(
            limits.len() + block_items.len(),
            Limits {
                no_repeat: c.no_repeat_gap(),
                separate: c.separate_gap(),
            },
        );
        separate_fields.resize(
            separate_fields.len() + block_items.len(),
            c.separate_by.clone(),
        );
        out.extend(block_items);
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
        let keys = adjacency_keys(
            &out.iter().map(|i| i.id.clone()).collect::<Vec<_>>(),
            &separate_fields,
            catalog,
            path,
        )?;
        // The aired tail carries the same field values, looked up the same way,
        // so a seam comparison means what a within-list one means. The
        // previous generation's own blocks are gone, so every tail item is read
        // under this channel's first separating field.
        let tail_field = separate_fields.iter().flatten().next().cloned();
        let preceding = adjacency_keys(
            &state.tail,
            &vec![tail_field; state.tail.len()],
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

/// Build the per-item keys the adjacency pass compares: the `entry_id`, plus
/// the values of whatever field that item's block separates on.
///
/// The field values come from the catalog's tags, read with the same vocabulary
/// an expression uses — `separate_by: "cast"` reads exactly what `item.cast`
/// reads. An item with no values for the field simply never triggers the
/// separation, which is why a catalog-free channel can still use
/// `no_repeat_within`.
fn adjacency_keys(
    ids: &[String],
    separate_fields: &[Option<String>],
    catalog: Option<&Catalog>,
    path: &Path,
) -> Result<Vec<ItemKeys>, ConfigError> {
    ids.iter()
        .zip(separate_fields.iter())
        .map(|(id, field)| {
            let Some(field) = field else {
                return Ok(ItemKeys::new(id.clone()));
            };
            let ns = TagNs::from_query_field(field).ok_or_else(|| ConfigError::Validation {
                path: path.to_path_buf(),
                message: format!(
                    "separate_by = {field:?} is not a multi-valued field (expected one of: {})",
                    TagNs::QUERY_FIELDS.join(", ")
                ),
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
            })
        })
        .collect()
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
    source_roots: &[String],
    path_index: Option<&HashMap<String, String>>,
    catalog: Option<&Catalog>,
    seed: u64,
    state: &GenerationState,
    resume_out: &mut ResumeMap,
) -> Result<Vec<ResolvedItem>, ConfigError> {
    let unsupported = |message: String| ConfigError::Unsupported {
        path: path.to_path_buf(),
        message,
    };

    // Honesty gates for features whose runtime lives in later Phase C issues.
    if include.filter.as_ref().is_some_and(|f| !f.is_empty()) {
        return Err(unsupported(format!(
            "block #{idx}: [filter] resolution is not implemented yet (#69)"
        )));
    }

    let defaults = include.program();

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
        let (ids, pools) = crate::pattern::build(
            cat,
            &include.pools,
            &include.pattern,
            include.cycles,
            state,
            seed,
        )
        .map_err(|m| unsupported(format!("block #{idx}: {m}")))?;
        resume_out.pools.extend(pools);

        let mut items: Vec<ResolvedItem> = ids
            .iter()
            .map(|id| catalog_item(cat, id, defaults))
            .collect::<Result<_, _>>()
            .map_err(|m: String| unsupported(format!("block #{idx}: {m}")))?;
        if let Mode::Count(n) = include.mode {
            items.truncate(n);
        }
        return Ok(items);
    }

    // 1. Resolve entries to a flat item list (authored order).
    let mut items: Vec<ResolvedItem> = Vec::new();
    for entry in include.entries() {
        match entry {
            Entry::Item(item) => items.push(resolve_item(item, defaults, source_roots, path_index)),
            Entry::Query(query) => {
                let cat = catalog.ok_or_else(|| {
                    unsupported(format!(
                        "block #{idx}: a query entry needs the catalog, which is not available"
                    ))
                })?;
                let resolved = resolve_query(cat, query, defaults, seed)
                    .map_err(|m| unsupported(format!("block #{idx}: {m}")))?;
                items.extend(resolved);
            }
            Entry::Collection(collection) => {
                let cat = catalog.ok_or_else(|| {
                    unsupported(format!(
                        "block #{idx}: a collection entry needs the catalog, which is not available"
                    ))
                })?;
                let resolved = resolve_collection(cat, collection, defaults)
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

    // 2. Duplicates — collapse (default) runs BEFORE order so which occurrence
    //    survives is deterministic even under a `random` shuffle.
    if matches!(include.duplicates(), Duplicates::Collapse) {
        collapse_duplicates(&mut items);
    }

    // 3. Order the block's resolved list. `manual` keeps authored order and
    //    needs no catalog; every other order goes through the #69 engine.
    if include.order != Order::Manual {
        let cat = catalog.ok_or_else(|| {
            unsupported(format!(
                "block #{idx}: order {:?} needs the catalog, which is not available",
                include.order
            ))
        })?;
        items = apply_order(cat, items, &include.order, seed)
            .map_err(|m| unsupported(format!("block #{idx}: {m}")))?;
    }

    // 4. Mode — `count` truncates after ordering.
    if let Mode::Count(n) = include.mode {
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
    ids.iter()
        .map(|id| catalog_item(catalog, id, defaults))
        .collect()
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
    members
        .iter()
        .map(|id| catalog_item(catalog, id, defaults))
        .collect()
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
fn catalog_item(
    catalog: &Catalog,
    entry_id: &str,
    defaults: Option<&ProgramMetadata>,
) -> Result<ResolvedItem, String> {
    let entry = catalog
        .entry(entry_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("resolved entry {entry_id} vanished from the catalog"))?;
    let sources = catalog.sources_for(entry_id).map_err(|e| e.to_string())?;
    // Prefer a local-filesystem source (a real path the player can open);
    // fall back to the first provenance row. Source-specific playback (e.g. a
    // Plex streaming URL) is deferred to the ingester that defines it.
    let source = sources
        .iter()
        .find(|s| s.source == crate::catalog::Source::LocalFs)
        .or_else(|| sources.first())
        .ok_or_else(|| format!("entry {entry_id} has no playback source"))?;
    // Catalog columns are i64; ProgramMetadata uses u32. Out-of-range values
    // (negative / overflow) drop to None rather than wrap.
    let as_u32 = |v: Option<i64>| v.and_then(|n| u32::try_from(n).ok());
    let program = ProgramMetadata {
        title: Some(entry.title.clone()),
        sub_title: None,
        description: None,
        season: as_u32(entry.season),
        episode: as_u32(entry.episode),
        categories: None,
        content_rating: entry.content_rating.clone(),
        artwork_url: None,
        year: as_u32(entry.year),
    };
    Ok(ResolvedItem {
        id: entry.entry_id.clone(),
        source: SourceConfig::Local {
            path: source.playback_path.clone(),
        },
        in_point: None,
        out_point: None,
        program: merge_program(Some(&program), defaults),
    })
}

fn resolve_item(
    item: &ItemEntry,
    defaults: Option<&ProgramMetadata>,
    source_roots: &[String],
    path_index: Option<&HashMap<String, String>>,
) -> ResolvedItem {
    ResolvedItem {
        id: derive_item_id(&item.source, source_roots, path_index),
        source: item.source.clone(),
        in_point: item.in_point,
        out_point: item.out_point,
        program: merge_program(item.program.as_ref(), defaults),
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
    source_roots: &[String],
    path_index: Option<&HashMap<String, String>>,
) -> String {
    match source {
        SourceConfig::Local { path } => {
            let roots: Vec<&str> = source_roots.iter().map(String::as_str).collect();
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
        }
    }

    /// A local-file test item (no authored id — identity derives from the path).
    fn local_entry(path: &str) -> ItemEntry {
        ItemEntry {
            source: SourceConfig::Local { path: path.into() },
            in_point: None,
            out_point: Some(Duration::from_secs(30)),
            program: None,
        }
    }

    fn include_with(entries: Vec<Entry>) -> BlockInclude {
        BlockInclude {
            block: None,
            program: None,
            duplicates: None,
            constraints: None,
            entries,
            pools: Vec::new(),
            pattern: Vec::new(),
            cycles: None,
            mode: Mode::All,
            order: Order::Manual,
            filter: None,
        }
    }

    fn channel(blocks: Vec<BlockInclude>) -> ChannelConfig {
        ChannelConfig {
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

    fn path() -> &'static Path {
        Path::new("/tmp/channel.toml")
    }

    #[test]
    fn flattens_items_in_order() {
        let inc = include_with(vec![
            Entry::Item(item_entry("a")),
            Entry::Item(item_entry("b")),
        ]);
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, None).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["lavfi:a", "lavfi:b"]);
    }

    #[test]
    fn concatenates_blocks() {
        let a = include_with(vec![Entry::Item(item_entry("a"))]);
        let b = include_with(vec![Entry::Item(item_entry("b"))]);
        let items = resolve_channel(&channel(vec![a, b]), path(), &[], None, None).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["lavfi:a", "lavfi:b"]);
    }

    /// `no_repeat_within` only has repeats to work on when they survive to the
    /// pass, so these use `duplicates = "keep"` or cross-block repeats — the two
    /// ways an id legitimately appears twice in a resolved channel.
    fn constrained(mut inc: BlockInclude, n: usize) -> BlockInclude {
        inc.constraints = Some(crate::config::Constraints {
            no_repeat_within: Some(n),
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
            Entry::Item(item_entry("a")),
            Entry::Item(item_entry("a")),
            Entry::Item(item_entry("b")),
            Entry::Item(item_entry("c")),
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
            Entry::Item(item_entry("x")),
            Entry::Item(item_entry("a")),
        ]);
        let b = include_with(vec![
            Entry::Item(item_entry("a")),
            Entry::Item(item_entry("y")),
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
            Entry::Item(item_entry("a")),
            Entry::Item(item_entry("b")),
            Entry::Item(item_entry("c")),
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
        )
        .unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_ne!(
            ids[0], "lavfi:a",
            "repeated the previously-aired item across the seam: {ids:?}"
        );
    }

    /// The list's own head and tail are NOT adjacent — nothing replays it end
    /// to end — so an already-legal list must come back untouched.
    #[test]
    fn the_lists_own_ends_are_left_alone() {
        let mut inc = include_with(vec![
            Entry::Item(item_entry("a")),
            Entry::Item(item_entry("b")),
            Entry::Item(item_entry("c")),
            Entry::Item(item_entry("a")),
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
            Entry::Item(item_entry("a")),
            Entry::Item(item_entry("a")),
            Entry::Item(item_entry("a")),
        ]);
        inc.duplicates = Some(Duplicates::Keep);
        let ids = resolved_ids(vec![constrained(inc, 1)]);
        assert_eq!(ids, vec!["lavfi:a"; 3]);
    }

    #[test]
    fn unconstrained_channel_keeps_its_resolved_order() {
        let mut inc = include_with(vec![
            Entry::Item(item_entry("a")),
            Entry::Item(item_entry("a")),
            Entry::Item(item_entry("b")),
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
            Entry::Item(item_entry("a")),
            Entry::Item(item_entry("a")),
            Entry::Item(item_entry("b")),
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
            Entry::Item(local_entry("/media/friends/s01e01.mkv")),
            Entry::Item(local_entry("/media/friends/s01e01.mkv")),
            Entry::Item(local_entry("/media/friends/s01e02.mkv")),
        ]);
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, None).unwrap();
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|i| i.id.starts_with("fs:")));
        assert_ne!(items[0].id, items[1].id);
    }

    #[test]
    fn source_roots_canonicalise_local_identity_across_mounts() {
        // The same file reached under two configured mount roots derives one
        // identity, so the cross-mount duplicate collapses.
        let roots = vec!["/mnt/media".to_string(), "/Volumes/media".to_string()];
        let inc = include_with(vec![
            Entry::Item(local_entry("/mnt/media/friends/s01e01.mkv")),
            Entry::Item(local_entry("/Volumes/media/friends/s01e01.mkv")),
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
        };
        let inc = include_with(vec![Entry::Item(lavfi), Entry::Item(http)]);
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, None).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["lavfi:testsrc", "http:https://ex/y.mkv"]);
    }

    #[test]
    fn keep_preserves_duplicates() {
        let mut inc = include_with(vec![
            Entry::Item(item_entry("a")),
            Entry::Item(item_entry("a")),
        ]);
        inc.duplicates = Some(Duplicates::Keep);
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, None).unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn count_mode_truncates_after_dedup() {
        let mut inc = include_with(vec![
            Entry::Item(item_entry("a")),
            Entry::Item(item_entry("b")),
            Entry::Item(item_entry("c")),
        ]);
        inc.mode = Mode::Count(2);
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None, None).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["lavfi:a", "lavfi:b"]);
    }

    #[test]
    fn block_program_defaults_cascade() {
        let mut inc = include_with(vec![Entry::Item(item_entry("a"))]);
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
        });
        let mut inc = include_with(vec![Entry::Item(item)]);
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
        let mut inc = include_with(vec![Entry::Item(item_entry("a"))]);
        inc.order = Order::Random;
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
        inc.order = order;
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

    #[test]
    fn field_order_keeps_non_catalog_items_after_the_sorted_set() {
        let cat = seeded_catalog();
        // A block mixing an inline lavfi item (not in the catalog) with a query,
        // sorted by release_date. The inline item can't be ranked — it must
        // survive, appended after the ranked query results, never dropped.
        let mut inc = include_with(vec![
            Entry::Item(item_entry("bumper")),
            Entry::Query(QueryEntry {
                query: "item.year >= 2001".into(),
                order: None,
            }),
        ]);
        inc.order = Order::parse("release_date:asc").unwrap();
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
            Entry::Item(item_entry("bumper")),
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
            Entry::Item(item_entry("a")),
            Entry::Item(item_entry("a")),
            Entry::Item(item_entry("b")),
        ]);
        inc.order = Order::Random;
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
            Entry::Item(item_entry("a")),
            Entry::Item(item_entry("a")),
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
            Entry::Item(local_entry("/media/lotr/imdb:tt0120737.mkv")),
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
                expr: "item.type == \"movie\"".into(),
                order: Some(Order::parse("title:asc").unwrap()),
                select: Select::RoundRobin,
                rotate: Rotate::Visit,
                advance,
                on_short: OnShort::Next,
            },
            Pool {
                name: "shows".into(),
                expr: "item.type == \"episode\"".into(),
                order: Some(Order::parse("season:asc,episode:asc").unwrap()),
                select: Select::RoundRobin,
                rotate: Rotate::Visit,
                advance,
                on_short: OnShort::Next,
            },
        ];
        inc.pattern = vec![
            PatternStep {
                pool: "movies".into(),
                take: 1,
                chance: 1.0,
            },
            PatternStep {
                pool: "shows".into(),
                take: 2,
                chance: 1.0,
            },
        ];
        inc.cycles = Some(2);
        inc
    }

    /// Project the state a following window would be handed, exactly as the
    /// daemon does: the pools' rotation from this resolve, and the per-series
    /// cursor read back out of the play-history ledger the airings were
    /// recorded in (#70).
    fn advance_state(
        cat: &Catalog,
        prev: &crate::resume::GenerationState,
        resume: ResumeMap,
        items: &[ResolvedItem],
    ) -> crate::resume::GenerationState {
        use crate::history::{Ledger, PlayRecord};
        use time::OffsetDateTime;

        let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
        let show_ids = cat.show_ids_for(&ids).unwrap();
        let mut ledger = Ledger::new();
        // Seed with whatever the previous windows had already recorded, so the
        // projection sees the channel's whole history and not just this window.
        ledger.extend(prev.cursor.iter().map(|(key, entry_id)| PlayRecord {
            entry_id: entry_id.clone(),
            show_id: Some(key.clone()),
            start: OffsetDateTime::UNIX_EPOCH,
            played_at: OffsetDateTime::UNIX_EPOCH,
        }));
        ledger.extend(ids.iter().map(|id| PlayRecord {
            entry_id: id.clone(),
            show_id: show_ids.get(id).cloned(),
            start: OffsetDateTime::UNIX_EPOCH,
            played_at: OffsetDateTime::UNIX_EPOCH,
        }));
        crate::resume::GenerationState {
            resume,
            cursor: ledger.series_cursor(),
            tail: ledger.tail(crate::constrain::DEFAULT_SEAM_TAIL),
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
        )
        .unwrap();
        let first_ids: Vec<&str> = first.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            first_ids,
            vec!["mov-1", "got-e1", "got-e2", "mov-2", "inv-e1", "inv-e2"]
        );

        let next = advance_state(&cat, &GenerationState::empty(), next, &first);
        let (second, _) =
            resolve_channel_with_resume(&cfg, path(), &[], None, Some(&cat), &next).unwrap();
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
        )
        .unwrap();
        let state = advance_state(&cat, &GenerationState::empty(), next, &first);

        let (a, ra) =
            resolve_channel_with_resume(&cfg, path(), &[], None, Some(&cat), &state).unwrap();
        let (b, rb) =
            resolve_channel_with_resume(&cfg, path(), &[], None, Some(&cat), &state).unwrap();
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
        )
        .unwrap();
        let state = advance_state(&cat, &GenerationState::empty(), next, &first);
        assert_eq!(state.cursor.get("show:got").unwrap(), "got-e2");

        // GoT drops out of the resolved set entirely for a while — the pool's
        // expr no longer matches it. Its ledger entries stay.
        let mut narrowed = interleave_block(crate::config::Advance::Resume);
        for pool in &mut narrowed.pools {
            if pool.name == "shows" {
                pool.expr = "item.show == \"inv\"".into();
            }
        }
        let narrowed_cfg = channel(vec![narrowed]);
        let (away, next_away) =
            resolve_channel_with_resume(&narrowed_cfg, path(), &[], None, Some(&cat), &state)
                .unwrap();
        assert!(
            !away.iter().any(|i| i.id.starts_with("got-")),
            "GoT is out of the set for this window"
        );
        let state = advance_state(&cat, &state, next_away, &away);

        // It comes back. It must continue at e3, not restart at e1.
        let (back, _) =
            resolve_channel_with_resume(&cfg, path(), &[], None, Some(&cat), &state).unwrap();
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

    /// #70 acceptance: one ledger row per scheduled airing — no more, no fewer.
    /// The row count is what makes the cursor's projection correct, so a
    /// duplicate or a dropped row is a scheduling bug, not a bookkeeping one.
    #[test]
    fn every_scheduled_airing_records_exactly_one_row() {
        use crate::history::{Ledger, PlayRecord};
        use time::OffsetDateTime;

        let cat = interleave_catalog();
        let cfg = channel(vec![interleave_block(crate::config::Advance::Restart)]);
        let (items, _) = resolve_channel_with_resume(
            &cfg,
            path(),
            &[],
            None,
            Some(&cat),
            &GenerationState::empty(),
        )
        .unwrap();

        let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
        let show_ids = cat.show_ids_for(&ids).unwrap();
        let mut ledger = Ledger::new();
        ledger.extend(ids.iter().enumerate().map(|(i, id)| PlayRecord {
            entry_id: id.clone(),
            show_id: show_ids.get(id).cloned(),
            start: OffsetDateTime::UNIX_EPOCH + time::Duration::minutes(i as i64),
            played_at: OffsetDateTime::UNIX_EPOCH,
        }));

        assert_eq!(
            ledger.len(),
            items.len(),
            "one row per airing — the generation aired {} items",
            items.len()
        );
        // A repeat under `wrap = "loop"` is a genuine second airing and gets
        // its own row; the cursor still resolves to the latest one.
        let cursor = ledger.series_cursor();
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
    fn an_entries_channel_produces_an_empty_resume_map() {
        let inc = include_with(vec![Entry::Item(item_entry("a"))]);
        let (_, next) = resolve_channel_with_resume(
            &channel(vec![inc]),
            path(),
            &[],
            None,
            None,
            &GenerationState::empty(),
        )
        .unwrap();
        assert!(next.is_empty());
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
        )
        .unwrap();
        assert!(!played.is_empty());

        // Second window, after everything has aired at least once: still full.
        let state = advance_state(&cat, &GenerationState::empty(), next, &played);
        let (items, _) =
            resolve_channel_with_resume(&cfg, path(), &[], None, Some(&cat), &state).unwrap();
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
            pool.expr = "item.type == \"nonesuch\"".into();
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
}

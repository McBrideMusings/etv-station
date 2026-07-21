//! Resolve a channel's block composition into a flat, ordered item list.
//!
//! The Phase C resolve pipeline (#71): for each `[[rule.blocks]]` it
//! **resolves entries → applies `duplicates` → applies `order` → (mode)**,
//! producing the flat [`ResolvedItem`] list the window-filling sequencer
//! ([`crate::rule::LoopForever`]) loops across the chunk window. Collapse runs
//! *before* order, so which duplicate survives is deterministic regardless of a
//! `random` shuffle.
//!
//! `query` entries resolve against the [`Catalog`] (#68 CEL→SQL) and each
//! resolved `entry_id` becomes a `ResolvedItem`; `order` is applied by the
//! order engine (#69). A channel with no query entries and `manual` order needs
//! no catalog, so `catalog` is optional.
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

use crate::catalog::ingest::canonical_index;
use crate::catalog::{Catalog, canonical_path, derive_entry_id};
use crate::config::{
    BlockInclude, ChannelConfig, Duplicates, Entry, ItemEntry, Mode, Order, QueryEntry,
    SourceConfig,
};
use crate::errors::ConfigError;

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
pub fn resolve_channel(
    config: &ChannelConfig,
    path: &Path,
    source_roots: &[String],
    catalog: Option<&Catalog>,
) -> Result<Vec<ResolvedItem>, ConfigError> {
    // One seed per generation: a pinned `seed` reproduces the shuffle; an unset
    // one draws fresh entropy so an unseeded `random` block reshuffles each
    // generation (#46 "unset = fresh per generation").
    let seed = config.seed.unwrap_or_else(fresh_seed);

    // Canonical-path → entry_id over the catalog, built once. A manual `local`
    // item whose path is in the catalog inherits that entry_id, so it collapses
    // against a `query` result for the same physical file (manual∩query dedup).
    let roots: Vec<&str> = source_roots.iter().map(String::as_str).collect();
    let path_index = catalog
        .map(|cat| canonical_index(cat, &roots))
        .transpose()
        .map_err(|e| ConfigError::Validation {
            path: path.to_path_buf(),
            message: format!("building the catalog path index failed: {e}"),
        })?;

    let mut out = Vec::new();
    for (idx, include) in config.rule.blocks.iter().enumerate() {
        let block_items =
            resolve_block(include, idx, path, source_roots, path_index.as_ref(), catalog, seed)?;
        out.extend(block_items);
    }

    if out.is_empty() {
        return Err(ConfigError::Validation {
            path: path.to_path_buf(),
            message: "channel resolved to zero items".into(),
        });
    }
    Ok(out)
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
    // `collection` order needs the block to know which collection its set came
    // from — that context isn't wired into resolution yet (#71 follow-up), so a
    // block-level collection sort would otherwise fail deep in the order engine.
    if include.order == Order::Collection {
        return Err(unsupported(format!(
            "block #{idx}: order = \"collection\" is not wired yet (needs block collection context)"
        )));
    }

    let defaults = include.program();

    // 1. Resolve entries to a flat item list (authored order).
    let mut items: Vec<ResolvedItem> = Vec::new();
    for entry in include.entries() {
        match entry {
            Entry::Item(item) => {
                items.push(resolve_item(item, defaults, source_roots, path_index))
            }
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
            .resolve_order(&ids, order, seed, None)
            .map_err(|e| e.to_string())?;
    }
    ids.iter()
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
        .resolve_order(&ids, order, seed, None)
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
            entries,
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
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["lavfi:a", "lavfi:b"]);
    }

    #[test]
    fn concatenates_blocks() {
        let a = include_with(vec![Entry::Item(item_entry("a"))]);
        let b = include_with(vec![Entry::Item(item_entry("b"))]);
        let items = resolve_channel(&channel(vec![a, b]), path(), &[], None).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["lavfi:a", "lavfi:b"]);
    }

    #[test]
    fn collapse_dedups_by_id() {
        let inc = include_with(vec![
            Entry::Item(item_entry("a")),
            Entry::Item(item_entry("a")),
            Entry::Item(item_entry("b")),
        ]);
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None).unwrap();
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
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None).unwrap();
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
        let items = resolve_channel(&channel(vec![inc]), path(), &roots, None).unwrap();
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
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None).unwrap();
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
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None).unwrap();
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
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None).unwrap();
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
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None).unwrap();
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
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None).unwrap();
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
        let err = resolve_channel(&channel(vec![inc]), path(), &[], None).unwrap_err();
        assert!(format!("{err}").contains("catalog"), "err = {err}");
    }

    #[test]
    fn non_manual_order_without_catalog_errors() {
        let mut inc = include_with(vec![Entry::Item(item_entry("a"))]);
        inc.order = Order::Random;
        let err = resolve_channel(&channel(vec![inc]), path(), &[], None).unwrap_err();
        assert!(format!("{err}").contains("catalog"), "err = {err}");
    }

    #[test]
    fn rejects_empty_channel() {
        let err = resolve_channel(&channel(vec![]), path(), &[], None).unwrap_err();
        assert!(format!("{err}").contains("zero items"), "err = {err}");
    }

    // ---- catalog-backed pipeline (#71) ------------------------------------

    use crate::catalog::{Catalog, Entry as CatEntry, EntrySource, Source};
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
        let items = resolve_channel(&channel(vec![inc]), path(), &[], Some(&cat)).unwrap();
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
        let items = resolve_channel(&channel(vec![inc]), path(), &[], Some(&cat)).unwrap();
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

    #[test]
    fn block_collection_order_errors_clearly() {
        let mut inc = include_with(vec![Entry::Item(item_entry("a"))]);
        inc.order = Order::Collection;
        let err = resolve_channel(&channel(vec![inc]), path(), &[], None).unwrap_err();
        assert!(format!("{err}").contains("collection"), "err = {err}");
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
        let first = resolve_channel(&cfg, path(), &[], Some(&cat)).unwrap();
        let second = resolve_channel(&cfg, path(), &[], Some(&cat)).unwrap();
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
        let items = resolve_channel(&channel(vec![inc]), path(), &[], None).unwrap();
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
        let items = resolve_channel(&channel(vec![inc]), path(), &[], Some(&cat)).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(items.len(), 3);
        assert!(ids.contains(&"imdb:tt0120737"));
    }

    #[test]
    fn per_entry_query_order_is_applied() {
        let cat = seeded_catalog();
        // Block is manual; the query entry carries its own descending order.
        let inc = include_with(vec![Entry::Query(QueryEntry {
            query: "item.year >= 2001".into(),
            order: Some(Order::parse("release_date:desc").unwrap()),
        })]);
        let items = resolve_channel(&channel(vec![inc]), path(), &[], Some(&cat)).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["imdb:tt0167260", "imdb:tt0167261", "imdb:tt0120737"]
        );
    }
}

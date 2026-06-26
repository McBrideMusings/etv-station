//! Resolve a channel's block composition into a flat, ordered item list.
//!
//! This is the **item-only** resolver for Phase C (#46): it flattens
//! `[[rule.blocks]]` into concrete [`ResolvedItem`]s, applies each block's
//! `duplicates` policy and `mode`, and merges block-level `[program]` defaults
//! into items. Everything that needs the catalog or sequencing engines is
//! rejected with a clear `unsupported` error rather than silently ignored:
//!
//! - `query` entries → query field set + CEL→SQL resolution (#68)
//! - `include` entries and any non-`manual` order → resolution engine (#69)
//! - a non-empty `filter` → resolution engine (#69)
//!
//! The flat list it returns feeds the existing window-filling sequencer
//! ([`crate::rule::LoopForever`]), which loops it across the chunk window.

use std::path::Path;
use std::time::Duration;

use ersatztv_playout::playout::{PlayoutItemSource, ProgramMetadata};

use crate::config::{
    BlockInclude, ChannelConfig, Duplicates, Entry, ItemEntry, Mode, Order, SourceConfig,
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
/// config path, used only for error messages.
pub fn resolve_channel(
    config: &ChannelConfig,
    path: &Path,
) -> Result<Vec<ResolvedItem>, ConfigError> {
    let mut out = Vec::new();
    for (idx, include) in config.rule.blocks.iter().enumerate() {
        let block_items = resolve_block(include, idx, path)?;
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

fn resolve_block(
    include: &BlockInclude,
    idx: usize,
    path: &Path,
) -> Result<Vec<ResolvedItem>, ConfigError> {
    let unsupported = |message: String| ConfigError::Unsupported {
        path: path.to_path_buf(),
        message,
    };

    // Honesty gates: features whose runtime lives in later Phase C issues.
    if include.order != Order::Manual {
        return Err(unsupported(format!(
            "block #{idx}: order other than \"manual\" is not implemented yet (#69)"
        )));
    }
    if include.filter.as_ref().is_some_and(|f| !f.is_empty()) {
        return Err(unsupported(format!(
            "block #{idx}: [filter] resolution is not implemented yet (#69)"
        )));
    }

    let defaults = include.program();

    let mut items: Vec<ResolvedItem> = Vec::new();
    for entry in include.entries() {
        match entry {
            Entry::Item(item) => items.push(resolve_item(item, defaults)),
            Entry::Query(_) => {
                return Err(unsupported(format!(
                    "block #{idx}: query entries are not implemented yet (#68)"
                )));
            }
            Entry::Include(_) => {
                return Err(unsupported(format!(
                    "block #{idx}: include entries are not implemented yet (#69)"
                )));
            }
        }
    }

    if matches!(include.duplicates(), Duplicates::Collapse) {
        collapse_duplicates(&mut items);
    }

    if let Mode::Count(n) = include.mode {
        items.truncate(n);
    }

    Ok(items)
}

fn resolve_item(item: &ItemEntry, defaults: Option<&ProgramMetadata>) -> ResolvedItem {
    ResolvedItem {
        id: item.id.clone(),
        source: item.source.clone(),
        in_point: item.in_point,
        out_point: item.out_point,
        program: merge_program(item.program.as_ref(), defaults),
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
    let mut seen = std::collections::HashSet::new();
    items.retain(|item| seen.insert(item.id.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChannelConfig, RuleConfig};

    fn item_entry(id: &str) -> ItemEntry {
        ItemEntry {
            id: id.into(),
            source: SourceConfig::Lavfi {
                params: format!("src={id}"),
            },
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
            output_folder: "/out".into(),
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
        let items = resolve_channel(&channel(vec![inc]), path()).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn concatenates_blocks() {
        let a = include_with(vec![Entry::Item(item_entry("a"))]);
        let b = include_with(vec![Entry::Item(item_entry("b"))]);
        let items = resolve_channel(&channel(vec![a, b]), path()).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn collapse_dedups_by_id() {
        let inc = include_with(vec![
            Entry::Item(item_entry("a")),
            Entry::Item(item_entry("a")),
            Entry::Item(item_entry("b")),
        ]);
        let items = resolve_channel(&channel(vec![inc]), path()).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn keep_preserves_duplicates() {
        let mut inc = include_with(vec![
            Entry::Item(item_entry("a")),
            Entry::Item(item_entry("a")),
        ]);
        inc.duplicates = Some(Duplicates::Keep);
        let items = resolve_channel(&channel(vec![inc]), path()).unwrap();
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
        let items = resolve_channel(&channel(vec![inc]), path()).unwrap();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
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
        let items = resolve_channel(&channel(vec![inc]), path()).unwrap();
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
        let items = resolve_channel(&channel(vec![inc]), path()).unwrap();
        let p = items[0].program.as_ref().unwrap();
        // item title wins; block category fills the gap.
        assert_eq!(p.title.as_deref(), Some("Specific"));
        assert_eq!(p.categories.as_ref().unwrap(), &vec!["Movie".to_string()]);
    }

    #[test]
    fn rejects_query_entry() {
        use crate::config::QueryEntry;
        let inc = include_with(vec![Entry::Query(QueryEntry {
            query: "type == 'movie'".into(),
            order: None,
        })]);
        let err = resolve_channel(&channel(vec![inc]), path()).unwrap_err();
        assert!(format!("{err}").contains("#68"), "err = {err}");
    }

    #[test]
    fn rejects_non_manual_order() {
        let mut inc = include_with(vec![Entry::Item(item_entry("a"))]);
        inc.order = Order::Random;
        let err = resolve_channel(&channel(vec![inc]), path()).unwrap_err();
        assert!(format!("{err}").contains("#69"), "err = {err}");
    }

    #[test]
    fn rejects_empty_channel() {
        let err = resolve_channel(&channel(vec![]), path()).unwrap_err();
        assert!(format!("{err}").contains("zero items"), "err = {err}");
    }
}

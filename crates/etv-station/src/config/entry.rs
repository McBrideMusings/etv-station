use std::path::PathBuf;
use std::time::Duration;

use ersatztv_playout::playout::ProgramMetadata;
use serde::{Deserialize, Serialize};

use super::filter::Filter;
use super::mode::Mode;
use super::order::Order;
use super::source::SourceConfig;

/// One entry in a block's flat `[[entries]]` list. The `kind` tag selects the
/// variant explicitly (#46 locked decision: explicit tag over field-inference).
///
/// ```toml
/// [[entries]]
/// kind = "item"
/// id = "diehard-1988"
/// [entries.source]
/// kind = "local"
/// path = "/media/diehard.mkv"
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    Item(ItemEntry),
    Query(QueryEntry),
    Include(IncludeEntry),
}

impl Entry {
    /// A short human label for the entry kind, for error messages.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Entry::Item(_) => "item",
            Entry::Query(_) => "query",
            Entry::Include(_) => "include",
        }
    }
}

/// A single concrete media item with an explicit source.
#[derive(Debug, Deserialize, Serialize)]
pub struct ItemEntry {
    pub id: String,

    pub source: SourceConfig,

    #[serde(default, with = "humantime_serde")]
    pub in_point: Option<Duration>,

    #[serde(default, with = "humantime_serde")]
    pub out_point: Option<Duration>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<ProgramMetadata>,
}

/// A CEL query resolved against the catalog at generation time. Resolution is
/// the query field set + CEL→SQL issue (#68); this type only fixes the shape.
#[derive(Debug, Deserialize, Serialize)]
pub struct QueryEntry {
    /// CEL expression evaluated against the catalog field set.
    pub query: String,

    /// Optional per-entry order — a query inside a `manual` block still needs
    /// an internal order for its many resolved items (#46 locked decision).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<Order>,
}

/// Include another block, with its own cursor; play-through then advance
/// (#46 include semantics). Resolution is the order/resolution engine (#69).
#[derive(Debug, Deserialize, Serialize)]
pub struct IncludeEntry {
    /// Path to the included block file, relative to the including file.
    pub block: PathBuf,

    #[serde(default)]
    pub mode: Mode,

    #[serde(default)]
    pub order: Order,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<Filter>,
}

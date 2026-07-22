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
/// [entries.source]
/// kind = "local"
/// path = "/media/diehard.mkv"
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    Item(ItemEntry),
    Query(QueryEntry),
    Collection(CollectionEntry),
    Include(IncludeEntry),
}

impl Entry {
    /// A short human label for the entry kind, for error messages.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Entry::Item(_) => "item",
            Entry::Query(_) => "query",
            Entry::Collection(_) => "collection",
            Entry::Include(_) => "include",
        }
    }
}

/// A single concrete media item with an explicit source. Identity is derived
/// from the source at resolution time (see [`crate::resolve`]) — never authored
/// — so two inline items resolving to the same file collapse within a block.
/// (Collapsing a manual item against a catalog *query* result for the same
/// physical file additionally needs the ingester to assign it the same id —
/// #92/#96 — since a GUID-carrying catalog entry derives `imdb:`/`tmdb:`, not
/// the manual item's path-hash `fs:` id.)
#[derive(Debug, Deserialize, Serialize)]
pub struct ItemEntry {
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

/// A whole catalog collection, emitted in its stored `collection_items.position`
/// order — the sequence hand-authored in the source app (#107). Reordering is a
/// drag in Plex plus a re-ingest; the config never changes.
///
/// The order rides here, on the entry that names the collection, rather than on
/// the block's `order`, because `position` is a property of the (collection,
/// item) pair: the same film sits at a different position in every collection
/// holding it. Once entries have flattened into a set of ids, the collection is
/// no longer knowable, which is why there is no `order = "collection"`.
///
/// Membership *without* the order is the other read path over the same table: a
/// `query` entry with `item.collections.contains("…")`, which yields an
/// unordered set the block's `order` is then free to sort.
#[derive(Debug, Deserialize, Serialize)]
pub struct CollectionEntry {
    /// The collection's name as its source names it (e.g. the Plex collection
    /// title). Resolved to a `collection_id` at generation time.
    pub name: String,
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

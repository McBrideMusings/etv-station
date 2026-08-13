//! Typed model for the unified catalog (#47 locked identity + field set).
//!
//! The schema stores the fixed-set discriminators (`source`, external-id
//! namespace, tag namespace) as text; these enums are the validated in-code
//! representation, converted at the sqlite boundary. The `type` column is left
//! an open string on purpose — the semantic type set (`episode`, `movie`,
//! `concert`, `bumper`, …) is deliberately open-ended.

use std::fmt;
use std::str::FromStr;

/// Provenance source for a catalog row. `entry_sources.source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Source {
    /// The default merge winner — "Plex by default, configurable" (#47).
    #[default]
    Plex,
    LocalFs,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Plex => "plex",
            Source::LocalFs => "local_fs",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Source {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "plex" => Ok(Source::Plex),
            "local_fs" => Ok(Source::LocalFs),
            other => Err(format!("unknown source {other:?}")),
        }
    }
}

/// External-id namespace, in dedup-strength order (`imdb` strongest). The
/// declaration order here IS the `entry_id` derivation priority (see
/// [`crate::catalog::identity`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ExternalNs {
    Imdb,
    Tmdb,
    Tvdb,
    Plex,
}

impl ExternalNs {
    pub fn as_str(self) -> &'static str {
        match self {
            ExternalNs::Imdb => "imdb",
            ExternalNs::Tmdb => "tmdb",
            ExternalNs::Tvdb => "tvdb",
            ExternalNs::Plex => "plex",
        }
    }

    /// Strongest-first, matching [`crate::catalog::identity::derive_entry_id`].
    pub const PRIORITY: [ExternalNs; 4] = [
        ExternalNs::Imdb,
        ExternalNs::Tmdb,
        ExternalNs::Tvdb,
        ExternalNs::Plex,
    ];
}

impl fmt::Display for ExternalNs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ExternalNs {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "imdb" => Ok(ExternalNs::Imdb),
            "tmdb" => Ok(ExternalNs::Tmdb),
            "tvdb" => Ok(ExternalNs::Tvdb),
            "plex" => Ok(ExternalNs::Plex),
            other => Err(format!("unknown external-id namespace {other:?}")),
        }
    }
}

/// Tag namespace. Collections are deliberately NOT a tag namespace — they have
/// their own tables (#47 locked, option B).
///
/// Every namespace here is authored upstream in Plex and re-read wholesale, so
/// a re-ingest can reconcile it. `fs_dir` was the one exception — written by the
/// filesystem scan, never reconciled, and therefore always accumulating — and it
/// is no longer a tag at all: `item.fs_dir` derives the directory from
/// `entry_sources.playback_path` at query time (#123).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TagNs {
    Genre,
    Label,
    Cast,
    Director,
    Writer,
    Producer,
    Country,
}

impl TagNs {
    pub fn as_str(self) -> &'static str {
        match self {
            TagNs::Genre => "genre",
            TagNs::Label => "label",
            TagNs::Cast => "cast",
            TagNs::Director => "director",
            TagNs::Writer => "writer",
            TagNs::Producer => "producer",
            TagNs::Country => "country",
        }
    }

    /// The tag namespace an *expression* field name reads, e.g. `"cast"` for
    /// `item.cast`. `None` for a field that is not multi-valued (or not a field
    /// at all), which is what makes `separate_by` reject `"title"` up front
    /// rather than separating on nothing.
    ///
    /// Deliberately the same vocabulary as the CEL field map in
    /// [`super::query`] — `separate_by: "cast"` has to mean the values
    /// `item.cast` reads, or the config says one thing and does another.
    pub fn from_query_field(name: &str) -> Option<Self> {
        Some(match name {
            "genres" => TagNs::Genre,
            "labels" => TagNs::Label,
            "cast" => TagNs::Cast,
            "directors" => TagNs::Director,
            "writers" => TagNs::Writer,
            "producers" => TagNs::Producer,
            "countries" => TagNs::Country,
            _ => return None,
        })
    }

    /// [`Self::from_query_field`], with the one rejection message every layer
    /// that validates a `separate_by` field shares: config load, the
    /// channel-level adjacency pass, and the per-pool one. Three copies of a
    /// message is three chances for them to drift apart while all claiming to
    /// describe the same rule.
    ///
    /// Returns the message alone; a caller that needs to say *where* the bad
    /// field was wraps it rather than rewriting it.
    pub fn for_separate_by(field: &str) -> Result<Self, String> {
        Self::from_query_field(field).ok_or_else(|| {
            format!(
                "separate_by = {field:?} is not a multi-valued field (expected one of: {})",
                Self::QUERY_FIELDS.join(", ")
            )
        })
    }

    /// Every field name [`Self::from_query_field`] accepts, for error messages.
    pub const QUERY_FIELDS: [&'static str; 7] = [
        "genres",
        "labels",
        "cast",
        "directors",
        "writers",
        "producers",
        "countries",
    ];
}

impl fmt::Display for TagNs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TagNs {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "genre" => Ok(TagNs::Genre),
            "label" => Ok(TagNs::Label),
            "cast" => Ok(TagNs::Cast),
            "director" => Ok(TagNs::Director),
            "writer" => Ok(TagNs::Writer),
            "producer" => Ok(TagNs::Producer),
            "country" => Ok(TagNs::Country),
            other => Err(format!("unknown tag namespace {other:?}")),
        }
    }
}

/// One logical catalog item — the merged, source-agnostic view queries resolve
/// against. Row in `entries`.
///
/// Construct via [`Entry::new`] and fill optional columns with struct-update
/// syntax: `Entry { year: Some(1977), ..Entry::new(id, "movie", "…", Source::Plex) }`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Entry {
    /// Deterministic, opaque identity. See [`crate::catalog::identity`].
    pub entry_id: String,
    /// Open-ended semantic type (`episode`, `movie`, `bumper`, …).
    pub kind: String,
    pub title: String,
    pub title_sort: Option<String>,
    pub show: Option<String>,
    pub show_id: Option<String>,
    pub season: Option<i64>,
    pub episode: Option<i64>,
    pub absolute_episode: Option<i64>,
    /// `editionTitle`; `None`/empty = theatrical.
    pub edition: Option<String>,
    pub studio: Option<String>,
    pub year: Option<i64>,
    /// ISO-8601 date string (`YYYY-MM-DD`).
    pub release_date: Option<String>,
    pub duration_ms: Option<i64>,
    pub content_rating: Option<String>,
    /// The library this entry came from, by **title** — the Plex section title
    /// ("4K Movies"), so a config author writes `item.library == "4K Movies"`.
    /// `None` for a source that has no library concept (the `fs` ingester).
    ///
    /// Storing the title rather than the section key is a deliberate trade
    /// (#128): renaming the library in Plex makes every expression naming the
    /// old title resolve to nothing, with no error.
    pub library: Option<String>,
    /// Which source's normalized values won the merge. Persisted as
    /// `entries.primary_source` text; typed here for parity with the other
    /// fixed-set discriminators.
    pub primary_source: Source,
    /// Everything not promoted to a column, as a JSON string.
    pub raw_metadata: Option<String>,
    /// Filename the entry's cached artwork is written under, relative to the
    /// station's `artwork_cache_dir` (#187). `None` when no artwork has been
    /// fetched (no Plex `thumb`, a fetch that hasn't run yet, or a fetch that
    /// failed). Written only by [`super::Catalog::set_artwork`] — never by
    /// [`super::Catalog::upsert_entry`] — so a routine metadata re-ingest
    /// can't erase a cached image.
    pub artwork_cache_path: Option<String>,
    /// The Plex `thumb` path the cached image at `artwork_cache_path` was
    /// fetched from, so a later ingest can tell a changed thumb from an
    /// unchanged one without re-downloading. Same write rule as
    /// `artwork_cache_path`.
    pub artwork_source_key: Option<String>,
}

impl Entry {
    /// A minimal entry; optional columns default to `None`.
    pub fn new(
        entry_id: impl Into<String>,
        kind: impl Into<String>,
        title: impl Into<String>,
        primary_source: Source,
    ) -> Self {
        Entry {
            entry_id: entry_id.into(),
            kind: kind.into(),
            title: title.into(),
            primary_source,
            ..Entry::default()
        }
    }
}

/// A provenance row: one `(source, source_id)` that resolves to an `entry_id`.
/// Two rows on one `entry_id` = an item deduped across Plex and local FS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntrySource {
    pub source: Source,
    pub source_id: String,
    pub entry_id: String,
    /// Source-specific path handed to the player for this provenance.
    pub playback_path: String,
    /// ISO-8601 timestamp of the last ingest that saw this row.
    pub last_seen: Option<String>,
}

/// A Plex collection. Membership + order live in `collection_items`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collection {
    pub collection_id: String,
    pub name: String,
    pub source: Source,
}

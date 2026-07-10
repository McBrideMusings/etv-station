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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TagNs {
    Genre,
    Label,
    Cast,
    Director,
    Writer,
    Producer,
    Country,
    FsDir,
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
            TagNs::FsDir => "fs_dir",
        }
    }
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
            "fs_dir" => Ok(TagNs::FsDir),
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
    /// Which source's normalized values won the merge. Persisted as
    /// `entries.primary_source` text; typed here for parity with the other
    /// fixed-set discriminators.
    pub primary_source: Source,
    /// Everything not promoted to a column, as a JSON string.
    pub raw_metadata: Option<String>,
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

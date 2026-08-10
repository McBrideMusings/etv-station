//! Plex catalog ingester (#91, second slice of #47).
//!
//! Pulls libraries → movies/episodes from the Plex API and writes `entries` +
//! `entry_external_ids` + `entry_sources` + genre `tags` into the [`Catalog`].
//! Identity follows the locked model: the strongest external GUID Plex reports
//! (`imdb → tmdb → tvdb → plex`) becomes the `entry_id`, with ingest-time
//! **path-match inherit** — a file whose canonical path already resolves to an
//! entry (e.g. one a prior FS scan created) reuses that `entry_id` and just adds
//! a `plex` provenance row, so one physical file is one entry across sources.
//!
//! [`ingest_items`] is the pure catalog-writing core (takes already-parsed
//! [`PlexItem`]s), unit-testable without a live server; [`ingest`] is the thin
//! HTTP front door that fetches and calls it. The connection details are read
//! out of the environment in exactly one place, [`PlexEnv::from_env`], and
//! handed down from there (#132).
//!
//! [`ingest_collections`] is the parallel pure core for Plex collections:
//! `collections` + ordered `collection_items`, with each member's ratingKey
//! resolved back to its `entry_id` via the `plex` provenance row.
//!
//! Plex files TV collections with `subtype="show"` — their children are
//! *shows*, which the catalog never stores (only `movie` and `episode`
//! entries exist). [`PlexClient::fetch_collections`] expands any
//! show-subtype container's member shows to their episode ratingKeys before
//! [`ingest_collections`] ever sees them (#119), so the pure core stays
//! generic: it always resolves membership ratingKeys straight to entries,
//! regardless of what kind of container produced the list.
//!
//! [`ingest_labels`] is the third pure core, for the `label` tag namespace
//! (#136). A Plex label cannot ride on the per-item record the way genre or
//! cast do: Plex's bulk section listing omits `Label` entirely (it is only
//! present on a single-item fetch, and even then only if the item has one),
//! so [`PlexClient::fetch_labels`] instead walks each section's label list
//! and, per label, asks for its members directly — the same
//! list-then-fetch-members shape [`PlexClient::fetch_collections`] already
//! uses. A label carries no `updatedAt` to delta against, so every ingest
//! pass fetches every label's complete current membership and
//! [`ingest_labels`] reconciles the whole `label` namespace wholesale rather
//! than per entry.
//!
//! [`ingest`] takes a `since` cursor: when set, each section is asked
//! only for records with `updatedAt>=since` and a collection whose own
//! `updatedAt` predates it skips its children request. That is what keeps a
//! restart cheap — see `plex_ingest_plan` in `daemon.rs` for how the cursor is
//! chosen, and note that a delta can never report a deletion, which is why a
//! full pass is still forced periodically.
//!
//! Out of scope (tracked separately): playlists.

use std::time::Duration;

use serde::Deserialize;
use time::OffsetDateTime;

use crate::catalog::identity::{canonical_path, derive_entry_id, is_blank_guid_value};
use crate::catalog::model::{Collection, Entry, EntrySource, ExternalNs, Source, TagNs};
use crate::catalog::{Catalog, CatalogError};

const HTTP_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, thiserror::Error)]
pub enum PlexIngestError {
    #[error("http: {0}")]
    Http(String),
    #[error("parse: {0}")]
    Parse(String),
    #[error("catalog: {0}")]
    Catalog(#[from] CatalogError),
}

/// Everything the ingest needs to reach a Plex server, resolved from the
/// environment once at startup and passed down from there.
///
/// This exists so nothing below [`PlexEnv::from_env`] reads the process
/// environment. A test that must not contact a live Plex hands [`ingest`]'s
/// caller a `None` instead of deleting `PLEX_URL` / `PLEX_TOKEN` out of the
/// running process — `std::env::remove_var` is unsound in Rust 2024 once any
/// other thread reads the environment, and `cargo test` runs a module's tests
/// concurrently (#132).
#[derive(Debug, Clone)]
pub struct PlexEnv {
    /// Server base URL, trailing slash already stripped.
    pub base_url: String,
    pub token: String,
    /// Optional path translation: a playback path starting with `path_from` is
    /// rewritten to start with `path_to`. Empty `path_from` disables it.
    pub path_from: String,
    pub path_to: String,
}

impl PlexEnv {
    /// `None` when `PLEX_URL` or `PLEX_TOKEN` is unset or empty — a station with
    /// no Plex at all is normal, not an error, and simply ingests without it.
    ///
    /// A *half*-configured Plex is not normal, and warns. Before the connection
    /// was resolved here, a blank `PLEX_URL` still entered the ingest and failed
    /// against the empty URL, so a typo in a deploy's env file announced itself
    /// as a `catalog.ingest.plex_failed` error on every startup. Skipping is the
    /// better behaviour — there is no server in a blank URL to contact — but
    /// skipping *silently* would take that signal away with it, leaving an
    /// operator who wrote `PLEX_URL=` staring at a catalog that never fills and
    /// no line in the log saying why.
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("PLEX_URL").unwrap_or_default();
        let token = std::env::var("PLEX_TOKEN").unwrap_or_default();
        if url.is_empty() != token.is_empty() {
            tracing::warn!(
                event = "catalog.ingest.plex_half_configured",
                have_url = !url.is_empty(),
                have_token = !token.is_empty(),
                "exactly one of PLEX_URL/PLEX_TOKEN is set; ingesting without plex",
            );
        }
        if url.is_empty() || token.is_empty() {
            return None;
        }
        Some(Self {
            base_url: url.trim_end_matches('/').to_string(),
            token,
            path_from: std::env::var("MEDIA_PATH_FROM").unwrap_or_default(),
            path_to: std::env::var("MEDIA_PATH_TO").unwrap_or_default(),
        })
    }
}

/// One playable Plex item, normalised out of the API's shape into exactly what
/// the catalog needs. Produced by [`to_plex_item`]; consumed by [`ingest_items`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlexItem {
    /// Plex `ratingKey` — the `source_id` of the `plex` provenance row.
    pub rating_key: String,
    /// External GUIDs in Plex order; strongest recognised one wins the id.
    pub external_ids: Vec<(ExternalNs, String)>,
    /// Playback path in the daemon's filesystem view (translation applied).
    pub playback_path: String,
    pub kind: String,
    pub title: String,
    /// The Plex library section this item was fetched from, by title
    /// ("4K Movies") — the value `item.library` reads. `None` when the section
    /// reports no title.
    pub library: Option<String>,
    /// Show name for an episode (`grandparentTitle`); `None` for a movie.
    pub show: Option<String>,
    pub season: Option<i64>,
    pub episode: Option<i64>,
    /// Plex `absoluteIndex` (franchise-wide episode number), when Plex provides
    /// one. The computed fallback for shows Plex leaves unset is a separate,
    /// deferred slice (needs a per-show catalog pass — see #104).
    pub absolute_episode: Option<i64>,
    pub year: Option<i64>,
    /// Plex `originallyAvailableAt` (`YYYY-MM-DD`); `None` when Plex has no
    /// value for this item.
    pub release_date: Option<String>,
    pub content_rating: Option<String>,
    /// Plex `editionTitle`; `None`/empty = theatrical.
    pub edition: Option<String>,
    /// Plex `studio` — single production-company string.
    pub studio: Option<String>,
    pub duration_ms: Option<i64>,
    pub genres: Vec<String>,
    /// Namespaced person tags: Plex `Role` (cast), `Director`, `Writer`,
    /// `Producer`, `Country`. Labels are deliberately absent here — Plex's
    /// bulk listing never carries them, so [`ingest_labels`] populates the
    /// `label` tag namespace from a separate per-label fetch instead (#136).
    pub cast: Vec<String>,
    pub directors: Vec<String>,
    pub writers: Vec<String>,
    pub producers: Vec<String>,
    pub countries: Vec<String>,
}

/// What one ingest pass touched.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PlexIngestStats {
    /// Entries upserted (Plex is authoritative — it always writes metadata).
    pub entries_written: usize,
    /// `plex` provenance rows upserted (one per item).
    pub sources_written: usize,
    /// Items that inherited an existing entry_id by path-match (FS↔Plex dedup).
    pub inherited: usize,
}

/// Write catalog rows for already-parsed Plex items. Pure over the catalog, so
/// tests exercise identity, external ids, and FS↔Plex path-match directly.
///
/// Plex is the authoritative metadata source: it always (re)writes the entry's
/// columns, even when inheriting an id a prior FS scan minted — that is how a
/// sparse `fs:` entry gets upgraded to the real Plex title/year/season.
pub fn ingest_items(
    catalog: &Catalog,
    items: &[PlexItem],
    source_roots: &[String],
) -> Result<PlexIngestStats, PlexIngestError> {
    let roots: Vec<&str> = source_roots.iter().map(String::as_str).collect();
    let index = super::canonical_index(catalog, &roots)?;
    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .ok();

    let mut stats = PlexIngestStats::default();
    for item in items {
        let canonical = canonical_path(&item.playback_path, &roots);
        // Identity precedence (locked #47 model): (1) a GUID already known to the
        // catalog wins, so every file sharing it collapses onto one entry and the
        // external-id row never flips; (2) a path-match onto a prior entry
        // (FS↔Plex dedup); (3) a fresh derivation (strongest GUID, else `fs:`).
        let entry_id = match resolve_existing(catalog, item, index.get(&canonical))? {
            Some(existing) => {
                stats.inherited += 1;
                existing
            }
            None => derive_entry_id(&item.external_ids, &canonical),
        };

        // Plex is authoritative when it HAS a value, but must not erase a column a
        // prior FS scan populated (notably the ffprobe'd duration) by overwriting
        // it with a null/empty Plex field. Merge: prefer the Plex value, else keep
        // what the entry already has.
        let existing = catalog.entry(&entry_id)?;
        let mut entry = Entry::new(
            &entry_id,
            non_empty(&item.kind).unwrap_or("video"),
            merged(
                non_empty(&item.title),
                existing.as_ref().map(|e| e.title.clone()),
            )
            .unwrap_or_default(),
            Source::Plex,
        );
        entry.show = or_existing(
            item.show.clone(),
            existing.as_ref().and_then(|e| e.show.clone()),
        );
        // The grouping key every pool's series rotation reads. Derived from the
        // show's name rather than taken from Plex's `grandparentRatingKey`,
        // because a rating key belongs to one server and this column has to
        // mean the same thing for the same show whatever source produced the
        // row — the same reason the rest of the identity model (#47) prefers
        // portable values.
        //
        // Without it every episode is its own series of one, which is a silent
        // failure rather than a loud one: the pattern still emits television,
        // it just draws a different show every slot and nothing that groups
        // episodes — `rotate = "visit"`, `advance = "resume"`, `group_by`, a
        // take-all step — can do its job. The prod catalog had 72,255 episodes
        // and not one `show_id` before this.
        entry.show_id = entry.show.as_deref().map(show_id_for);
        entry.season = item
            .season
            .or_else(|| existing.as_ref().and_then(|e| e.season));
        entry.episode = item
            .episode
            .or_else(|| existing.as_ref().and_then(|e| e.episode));
        entry.absolute_episode = item
            .absolute_episode
            .or_else(|| existing.as_ref().and_then(|e| e.absolute_episode));
        entry.year = item.year.or_else(|| existing.as_ref().and_then(|e| e.year));
        entry.release_date = or_existing(
            item.release_date.clone(),
            existing.as_ref().and_then(|e| e.release_date.clone()),
        );
        entry.content_rating = or_existing(
            item.content_rating.clone(),
            existing.as_ref().and_then(|e| e.content_rating.clone()),
        );
        entry.edition = or_existing(
            item.edition.clone(),
            existing.as_ref().and_then(|e| e.edition.clone()),
        );
        entry.studio = or_existing(
            item.studio.clone(),
            existing.as_ref().and_then(|e| e.studio.clone()),
        );
        entry.duration_ms = item
            .duration_ms
            .or_else(|| existing.as_ref().and_then(|e| e.duration_ms));
        // Same merge as the other Plex-authored columns: the section this pass
        // read the item from wins, and an item Plex reports without one keeps
        // whatever the entry already had (notably NULL, for an fs-only entry).
        // An entry holds ONE library, so when two Plex files in different
        // sections share a GUID and collapse onto it, the later of the two in
        // the fetch is the one recorded — whichever section `/library/sections`
        // happened to list second. Nothing here reorders that list, so a
        // re-ingest lands the same value; if the server ever reorders it, the
        // recorded library flips. Modelling an item as belonging to several
        // libraries at once would need a join table, which #128 deliberately
        // did not take on.
        entry.library = or_existing(
            item.library.clone(),
            existing.as_ref().and_then(|e| e.library.clone()),
        );
        catalog.upsert_entry(&entry)?;
        stats.entries_written += 1;

        // Record every GUID so the entry is reachable by any of them, even when
        // an inherited (e.g. `fs:`) id is what the entry is keyed under.
        for (ns, value) in &item.external_ids {
            catalog.add_external_id(*ns, value, &entry_id)?;
        }

        catalog.add_source(&EntrySource {
            source: Source::Plex,
            source_id: item.rating_key.clone(),
            entry_id: entry_id.clone(),
            playback_path: item.playback_path.clone(),
            last_seen: now.clone(),
        })?;

        for (ns, values) in [
            (TagNs::Genre, &item.genres),
            (TagNs::Cast, &item.cast),
            (TagNs::Director, &item.directors),
            (TagNs::Writer, &item.writers),
            (TagNs::Producer, &item.producers),
            (TagNs::Country, &item.countries),
        ] {
            // Plex authors each of these sets wholesale, so reconcile rather
            // than accumulate: without the clear, a genre removed upstream
            // stays attached forever and keeps matching queries it shouldn't.
            catalog.clear_tags(&entry_id, ns)?;
            for value in values {
                catalog.add_tag(&entry_id, ns, value)?;
            }
        }
        stats.sources_written += 1;
    }
    Ok(stats)
}

/// One Plex collection with its ordered member ratingKeys, normalised out of the
/// API shape. Produced by [`PlexClient::fetch_collections`]; consumed by
/// [`ingest_collections`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCollection {
    /// Plex collection `ratingKey` — the `collection_id`.
    pub collection_id: String,
    pub name: String,
    /// Member ratingKeys in Plex's authored order.
    pub member_rating_keys: Vec<String>,
}

/// What one collection ingest pass touched.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CollectionIngestStats {
    pub collections_written: usize,
    pub members_written: usize,
    /// Members whose ratingKey resolved to no catalog entry (not ingested, or
    /// FS-only) — skipped, never recorded as members.
    pub members_unresolved: usize,
    /// Collections dropped because a full pass no longer found them in Plex.
    pub collections_pruned: usize,
}

/// Write `collections` + `collection_items` for already-parsed Plex collections.
/// Pure over the catalog, so tests exercise membership + ordering directly.
///
/// Membership is Plex-only and references `entry_id` (locked #47 option B): each
/// member's ratingKey is resolved to its entry via the `plex` provenance row; a
/// ratingKey with no catalog entry (un-ingested, or FS-only) is skipped. Position
/// is the member's rank in Plex's authored order among the members that resolve
/// (contiguous, 0-based); the `Collection` order read sorts by it.
/// Ingest the fetched collections. `prune_absent` reconciles *deletions*: when
/// true, any collection the catalog holds but the fetch did not return is
/// dropped (its membership cascades away). It must be true only on a **full**
/// pass — a delta fetch returns just the changed collections, so absence there
/// means "unchanged", not "deleted", and pruning would wipe live collections.
pub fn ingest_collections(
    catalog: &Catalog,
    collections: &[ParsedCollection],
    prune_absent: bool,
) -> Result<CollectionIngestStats, PlexIngestError> {
    let mut stats = CollectionIngestStats::default();

    if prune_absent {
        // A collection deleted in Plex never appears in a full fetch, so
        // upsert-only never removes it and `contains("…")` keeps matching a name
        // that is gone. Drop every stored collection the fetch did not return.
        let fetched: std::collections::HashSet<&str> = collections
            .iter()
            .map(|c| c.collection_id.as_str())
            .collect();
        for id in catalog.all_collection_ids()? {
            if !fetched.contains(id.as_str()) {
                catalog.delete_collection(&id)?;
                stats.collections_pruned += 1;
            }
        }
    }

    for coll in collections {
        catalog.upsert_collection(&Collection {
            collection_id: coll.collection_id.clone(),
            name: coll.name.clone(),
            source: Source::Plex,
        })?;
        // Membership is replaced, not merged: what Plex returns now IS the
        // collection. Without the clear, an entry dragged out of a collection
        // would keep its row and keep airing, because add_collection_item only
        // ever inserts or updates.
        catalog.clear_collection_items(&coll.collection_id)?;
        stats.collections_written += 1;

        let mut position = 0i64;
        let mut seen = std::collections::HashSet::new();
        for rating_key in &coll.member_rating_keys {
            match catalog.entry_id_for_source(Source::Plex, rating_key)? {
                // A deduped item can surface as two member ratingKeys on one
                // entry (e.g. 4K + 1080p files); record it once so `position`
                // stays contiguous and the count matches the rows written.
                Some(entry_id) if seen.insert(entry_id.clone()) => {
                    catalog.add_collection_item(&coll.collection_id, &entry_id, position)?;
                    position += 1;
                    stats.members_written += 1;
                }
                Some(_) => {}
                None => stats.members_unresolved += 1,
            }
        }
        // Plex reported members for this collection, but every one of them
        // failed to resolve to a catalog entry — the collection is about to
        // sit empty in the catalog exactly like the 54 in #119, just for a
        // different reason (e.g. every member show has no ingested
        // episodes). Log it with the name so it is findable rather than a
        // silent zero, matching the no-playback-source warning in resolve.rs.
        if position == 0 && !coll.member_rating_keys.is_empty() {
            tracing::warn!(
                event = "catalog.ingest.plex_collection_unresolved",
                collection = %coll.name,
                collection_id = %coll.collection_id,
                reported_members = coll.member_rating_keys.len(),
                "Plex reported members for this collection but none resolved to a catalog entry; collection will be empty",
            );
        }
    }
    Ok(stats)
}

/// One Plex label with its section-scoped members' ratingKeys, normalised
/// out of the API shape. Produced by [`PlexClient::fetch_labels`]; consumed
/// by [`ingest_labels`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLabel {
    /// The label's display name, e.g. "🎅 Christmas Movies" — this is the
    /// value written into the `label` tag namespace, so it is also what
    /// `item.labels.contains("…")` matches against.
    pub name: String,
    /// Member ratingKeys, section-scoped (a label fetched against a movie
    /// section only ever returns movie ratingKeys, a show section only
    /// episode ratingKeys — see [`label_member_type_param`]).
    pub member_rating_keys: Vec<String>,
}

/// What one label ingest pass touched.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LabelIngestStats {
    /// `(entry_id, label)` tag rows written.
    pub members_written: usize,
    /// Members whose ratingKey resolved to no catalog entry (not ingested, or
    /// FS-only) — skipped, never tagged.
    pub members_unresolved: usize,
}

/// Write the `label` tag namespace from already-fetched Plex labels (#136).
/// Pure over the catalog, so tests exercise membership resolution and
/// reconciliation directly — same shape as [`ingest_collections`].
///
/// Unlike the other tag namespaces (genre, cast, director, …), which
/// [`ingest_items`] reconciles per entry because each item's own record
/// carries its current values, a label cannot be reconciled that way: Plex's
/// bulk listing never carries `Label`, so nothing visits "this entry's
/// labels" as a unit. Instead every ingest pass fetches every label's
/// *complete* current membership (see [`PlexClient::fetch_labels`]) and this
/// function clears the whole `label` namespace before writing that pass back
/// — the wholesale reconcile, taken one level up from per-entry because the
/// fetch itself is already complete rather than delta'd. Without the clear, a
/// label removed from an item in Plex would survive in the catalog forever
/// and keep matching `item.labels.contains("…")` queries it shouldn't.
pub fn ingest_labels(
    catalog: &Catalog,
    labels: &[ParsedLabel],
) -> Result<LabelIngestStats, PlexIngestError> {
    catalog.clear_tag_namespace(TagNs::Label)?;
    let mut stats = LabelIngestStats::default();
    for label in labels {
        let mut seen = std::collections::HashSet::new();
        for rating_key in &label.member_rating_keys {
            match catalog.entry_id_for_source(Source::Plex, rating_key)? {
                Some(entry_id) if seen.insert(entry_id.clone()) => {
                    catalog.add_tag(&entry_id, TagNs::Label, &label.name)?;
                    stats.members_written += 1;
                }
                // Already tagged for this label this pass (e.g. a 4K + 1080p
                // dedupe onto one entry), or unresolved — either way, not a
                // second write.
                Some(_) => {}
                None => stats.members_unresolved += 1,
            }
        }
    }
    Ok(stats)
}

/// Fetch every library's movies + episodes from Plex and ingest them.
/// `source_roots` canonicalise paths for identity/path-match.
///
/// `plex` carries the connection; see [`PlexEnv::from_env`] for where it comes
/// from in production.
pub fn ingest(
    catalog: &Catalog,
    source_roots: &[String],
    since: Option<i64>,
    plex: &PlexEnv,
) -> Result<PlexIngestStats, PlexIngestError> {
    let client = PlexClient::new(plex);
    let items = client.fetch_all(since)?;
    // The collection children fetch is the slow half — one sequential request
    // per collection. Log between the stages so a stall is attributable.
    tracing::info!(
        event = "catalog.ingest.plex_items_fetched",
        items = items.len(),
        "fetched library items; fetching collections next",
    );
    let collections = client.fetch_collections(since)?;
    tracing::info!(
        event = "catalog.ingest.plex_collections_fetched",
        collections = collections.len(),
        "fetched collections; fetching labels next",
    );
    // Labels carry no `updatedAt` to delta against (#136), so this is not
    // gated on `since` — every pass fetches every label's complete current
    // membership. See `ingest_labels`' doc comment for why that is what makes
    // its wholesale-clear reconcile correct.
    let labels = client.fetch_labels()?;
    tracing::info!(
        event = "catalog.ingest.plex_labels_fetched",
        labels = labels.len(),
        "fetched labels; writing to the catalog",
    );
    // One transaction for the whole write pass — a mid-ingest failure rolls back
    // rather than leaving a partial catalog. Entries are written before
    // collections/labels so member ratingKeys resolve to their entry ids.
    //
    // The ingest timestamp is written inside the same transaction, so a failed
    // pass cannot advance the delta cursor past changes it never wrote. It is
    // taken *before* the fetch, not after: anything Plex modifies while the
    // ingest is running is then re-read by the next pass rather than falling
    // into the gap between the fetch and the commit.
    let started = OffsetDateTime::now_utc().unix_timestamp();
    // Prune deleted collections only on a full pass (`since` is None); a delta
    // fetch omits unchanged collections, which must not be read as deletions.
    let prune_absent = since.is_none();
    catalog.in_transaction(|c| {
        let stats = ingest_items(c, &items, source_roots)?;
        ingest_labels(c, &labels)?;
        ingest_collections(c, &collections, prune_absent)?;
        c.set_last_plex_ingest(started)?;
        Ok(stats)
    })
}

/// Resolve the entry an item should attach to, if any: a GUID the catalog
/// already knows takes precedence (so every file sharing it collapses onto one
/// entry), then a path-match on the canonical path. `None` → mint a fresh id.
fn resolve_existing(
    catalog: &Catalog,
    item: &PlexItem,
    path_match: Option<&String>,
) -> Result<Option<String>, CatalogError> {
    for (ns, value) in &item.external_ids {
        if let Some(id) = catalog.entry_id_for_external_id(*ns, value)? {
            return Ok(Some(id));
        }
    }
    Ok(path_match.cloned())
}

/// `Some(s)` when `s` is not blank, else `None`.
fn non_empty(s: &str) -> Option<&str> {
    (!s.trim().is_empty()).then_some(s)
}

/// The grouping key for a show, from its name.
///
/// `show:` + the name verbatim, which is the shape the rest of the project
/// already reads — [`crate::resolve`] recovers the display name by trimming
/// exactly this prefix, so anything cleverer here (a slug, a hash) would come
/// back out as the show's on-screen name.
fn show_id_for(show: &str) -> String {
    format!("show:{show}")
}

/// Prefer a non-empty Plex string, else keep what the entry already had.
fn merged(primary: Option<&str>, existing: Option<String>) -> Option<String> {
    primary.map(str::to_string).or(existing)
}

/// Prefer the Plex value, else keep the existing one.
fn or_existing(primary: Option<String>, existing: Option<String>) -> Option<String> {
    primary.or(existing)
}

/// Parse a Plex `Guid.id` (`imdb://tt0095016`, `tmdb://562`) into a recognised
/// namespace + value. Unknown schemes (and malformed ids) return `None`.
///
/// **A blank value is dropped here, not merely downstream (#184).** It is not
/// enough for [`crate::catalog::derive_entry_id`] to treat an unusable value as
/// absent, because that only governs the id a *fresh* entry is minted under.
/// Every pair that survives this function is also written to
/// `entry_external_ids` by the ingest loop and read back by `resolve_existing`
/// on the next scan — so a blank value that got this far would store
/// `(imdb, "   ") -> entry_A`, and the next title carrying the same blank value
/// would resolve onto `entry_A` and merge into it. That is the same silent
/// collapse #184 is about, arriving one scan later through the reconciliation
/// path. Dropping it once, here, keeps all three consumers agreeing.
///
/// "Blank" is [`is_blank_guid_value`] — the *same* predicate `derive_entry_id`
/// uses, not a second definition of it. That matters: this filter and the
/// derivation disagreeing is how the collapse stayed reachable in the first
/// place, so they share one function rather than two matching implementations.
/// Note it is deliberately **not** the general-purpose `non_empty` used for
/// titles and studios below, whose `trim` semantics are right for those fields
/// and wrong here.
fn parse_guid(id: &str) -> Option<(ExternalNs, String)> {
    let (scheme, value) = id.split_once("://")?;
    let ns = match scheme {
        "imdb" => ExternalNs::Imdb,
        "tmdb" => ExternalNs::Tmdb,
        "tvdb" => ExternalNs::Tvdb,
        "plex" => ExternalNs::Plex,
        _ => return None,
    };
    if is_blank_guid_value(value) {
        return None;
    }
    Some((ns, value.to_string()))
}

/// Convert one Plex metadata record into a [`PlexItem`], applying `translate` to
/// the file path. `library` is the title of the section the record was fetched
/// from — the API's per-item payload does not carry it, so the caller walking
/// the sections supplies it. Returns `None` for a record with no playable file
/// part.
fn to_plex_item(
    m: &PlexMetadata,
    library: Option<&str>,
    translate: impl Fn(&str) -> String,
) -> Option<PlexItem> {
    let raw_path = m.media.first()?.part.first()?.file.as_deref()?;
    let external_ids = m
        .guid
        .iter()
        .filter_map(|g| g.id.as_deref().and_then(parse_guid))
        .collect();
    let kind = m.kind.clone().unwrap_or_else(|| "video".into());
    // Season/episode belong to episodes; a movie carrying a stray `index` must
    // not land `episode = Some(n)`.
    let is_episode = kind == "episode";
    Some(PlexItem {
        rating_key: m.rating_key.clone()?,
        external_ids,
        playback_path: translate(raw_path),
        title: m.title.clone().unwrap_or_default(),
        library: library.and_then(non_empty).map(str::to_string),
        show: m.grandparent_title.clone(),
        season: is_episode.then_some(m.parent_index).flatten(),
        episode: is_episode.then_some(m.index).flatten(),
        absolute_episode: is_episode.then_some(m.absolute_index).flatten(),
        year: m.year,
        release_date: m.originally_available_at.clone(),
        kind,
        content_rating: m.content_rating.clone(),
        // Absent/blank `editionTitle` means theatrical — normalise to `None` so
        // the merge never overwrites an existing edition with an empty string.
        edition: m
            .edition_title
            .as_deref()
            .and_then(non_empty)
            .map(str::to_string),
        studio: m.studio.as_deref().and_then(non_empty).map(str::to_string),
        // Plex `duration` is already milliseconds.
        duration_ms: m.duration,
        genres: tagged(&m.genre),
        cast: tagged(&m.role),
        directors: tagged(&m.director),
        writers: tagged(&m.writer),
        producers: tagged(&m.producer),
        countries: tagged(&m.country),
    })
}

/// Collect the non-empty `tag` strings from a Plex tagged-field array
/// (`Genre`/`Label`/`Role`/…).
fn tagged(fields: &[TaggedField]) -> Vec<String> {
    fields.iter().filter_map(|f| f.tag.clone()).collect()
}

/// Query parameters for one section's bulk listing request in [`PlexClient::fetch_all`].
///
/// `includeGuids=1` is always present (#183): Plex omits `<Guid>` elements
/// from a bulk section listing unless explicitly asked, which left every
/// production catalog entry identified by a path hash instead of its
/// `imdb`/`tmdb`/`tvdb` id. Verified against the live server to compose with
/// the delta filter — `type=4&includeGuids=1&updatedAt>=X` returns the same
/// `totalSize` as the same query without `includeGuids`.
/// The Plex `type` query-param value for a section's own kind: a show
/// section's *leaves* are episodes (`4`), a movie section's are movies (`1`).
/// An unrecognised kind gets no type filter at all. Shared by
/// [`section_list_params`] (a section's own bulk listing) and
/// [`label_member_type_param`] (a label's section-scoped member listing) so
/// the two never encode Plex's type codes independently and drift apart.
fn plex_type_code(kind: Option<&str>) -> Option<&'static str> {
    match kind {
        Some("show") => Some("4"),
        Some("movie") => Some("1"),
        _ => None,
    }
}

fn section_list_params<'a>(
    kind: Option<&str>,
    since_param: Option<&'a str>,
) -> Vec<(&'a str, &'a str)> {
    // Movies come back directly; a show section is expanded to its episode
    // leaves (type=4).
    let mut params: Vec<(&str, &str)> = plex_type_code(kind)
        .map(|t| vec![("type", t)])
        .unwrap_or_default();
    params.push(("includeGuids", "1"));
    if let Some(s) = since_param {
        // `updatedAt>` alone misses any item with no `updatedAt` attribute at
        // all (#134: 478 episodes on the live server, null rather than zero,
        // so they fail the comparison identically and wait for the daily full
        // sweep). Every item has an `addedAt`, so OR-ing it in reaches the
        // whole section.
        //
        // `or=1` MUST sit between the two filters it joins, not after both —
        // this is a Plex query-parsing quirk, not a documented API contract.
        // Verified against the live server (TV section, 73,835 items):
        // `updatedAt>=1 or=1
        // addedAt>=1` (or=1 between) returns 73,835 — the whole section, as
        // intended. `updatedAt>=1 addedAt>=1 or=1` (or=1 last, same three
        // params, only reordered) returns 73,333 — identical to `updatedAt>=1`
        // alone, i.e. `addedAt>` and `or=1` are silently ignored. At a real
        // cursor (1700000000) `or=1` last does worse than doing nothing:
        // 71,155 vs. 73,333 for `updatedAt>` alone — so this is not a
        // no-op-if-wrong mistake, it silently narrows the result. `type` and
        // `includeGuids` are view/output params, not attribute filters, so
        // their position is unaffected and they stay first.
        params.push(("updatedAt>", s));
        params.push(("or", "1"));
        params.push(("addedAt>", s));
    }
    params
}

// ---- HTTP client (thin outer layer) --------------------------------------

struct PlexClient {
    base_url: String,
    token: String,
    path_from: String,
    path_to: String,
    agent: ureq::Agent,
}

impl PlexClient {
    fn new(plex: &PlexEnv) -> Self {
        Self {
            base_url: plex.base_url.clone(),
            token: plex.token.clone(),
            path_from: plex.path_from.clone(),
            path_to: plex.path_to.clone(),
            agent: ureq::AgentBuilder::new().timeout(HTTP_TIMEOUT).build(),
        }
    }

    fn translate(&self, p: &str) -> String {
        // Only remap at a path boundary: `/media` must map `/media/x`, never
        // `/mediabackup/x`.
        if !self.path_from.is_empty()
            && let Some(rest) = p.strip_prefix(&self.path_from)
            && (rest.is_empty() || rest.starts_with('/'))
        {
            return format!("{}{}", self.path_to, rest);
        }
        p.to_string()
    }

    fn get<T: for<'de> Deserialize<'de>>(
        &self,
        endpoint: &str,
        params: &[(&str, &str)],
    ) -> Result<T, PlexIngestError> {
        let url = format!("{}{}", self.base_url, endpoint);
        let mut req = self
            .agent
            .get(&url)
            .set("X-Plex-Token", &self.token)
            .set("Accept", "application/json");
        for (k, v) in params {
            req = req.query(k, v);
        }
        req.call()
            .map_err(|e| PlexIngestError::Http(e.to_string()))?
            .into_json()
            .map_err(|e| PlexIngestError::Parse(e.to_string()))
    }

    /// Every movie and episode across all library sections, as [`PlexItem`]s.
    ///
    /// Every request carries `includeGuids=1` (#183) — see
    /// [`section_list_params`] — so `<Guid>` elements are present and
    /// [`to_plex_item`] has external ids to hand `derive_entry_id`, instead of
    /// every entry falling back to a path-hash `fs:` id.
    ///
    /// `since` (unix seconds) narrows each section to items Plex has touched
    /// after that moment, via the server-side `updatedAt>=` filter OR'd with
    /// `addedAt>=` (#134) — see [`section_list_params`]. On a library of ~86k
    /// items that is the difference between 20s of transfer and a fraction of
    /// a second. `None` fetches everything.
    ///
    /// A delta cannot report a *deletion* — an item removed from Plex simply
    /// stops appearing, which is indistinguishable from "unchanged" here. The
    /// caller is responsible for periodically running a full pass; see
    /// `full_sweep_after_secs` in the station config. The full sweep is also
    /// the only thing that catches an item whose `updatedAt` never moves
    /// (null, not zero) and whose metadata later changes: `addedAt` doesn't
    /// move either, so a delta can't see the change, only the initial add.
    fn fetch_all(&self, since: Option<i64>) -> Result<Vec<PlexItem>, PlexIngestError> {
        let sections: SectionListResp = self.get("/library/sections", &[])?;
        let mut items = Vec::new();
        // `updatedAt>` (strictly greater), not `updatedAt>=`. `ureq` builds every
        // query pair as `key=value` with the key percent-encoded, so the pair
        // `("updatedAt>", v)` goes out as `updatedAt%3E=v` — a spelling Plex
        // accepts. `("updatedAt>=", v)` would become `updatedAt%3E%3D=v`, which
        // this server answers with the *entire* unfiltered library rather than an
        // error, so the mistake reads as a working delta that silently re-ingests
        // everything. Verified against the live server: unfiltered 11,149 items,
        // `updatedAt%3E=` 105, `updatedAt%3E%3D=` 11,149.
        //
        // Since the comparison is strict, step the cursor back a second: an item
        // touched during the very second the previous pass recorded would
        // otherwise fall between the two runs and never be seen. The same
        // stepped-back value feeds both halves of the `updatedAt>`/`addedAt>`
        // disjunction below, so the step-back covers both.
        let since_param = since.map(|s| (s - 1).to_string());
        for section in &sections.media_container.directory {
            let Some(id) = section.key.as_deref() else {
                continue;
            };
            let params = section_list_params(section.kind.as_deref(), since_param.as_deref());
            let endpoint = format!("/library/sections/{id}/all");
            let resp: MediaContainerResp = self.get(&endpoint, &params)?;
            // Size of the #134 class: items this section reports with no
            // `updatedAt` at all, and so would be invisible to an
            // `updatedAt>`-only delta. Logged unconditionally (not just on a
            // delta pass) so the class is visible rather than discovered by
            // accident.
            let missing_updated_at = resp
                .media_container
                .metadata
                .iter()
                .filter(|m| m.updated_at.is_none())
                .count();
            if missing_updated_at > 0 {
                tracing::info!(
                    event = "catalog.ingest.plex_missing_updated_at",
                    section = section.title.as_deref().unwrap_or(""),
                    section_id = %id,
                    missing_updated_at,
                    total = resp.media_container.metadata.len(),
                    "items in this section have no updatedAt; reached via addedAt on delta, full sweep otherwise",
                );
            }
            for m in &resp.media_container.metadata {
                if let Some(item) = to_plex_item(m, section.title.as_deref(), |p| self.translate(p))
                {
                    items.push(item);
                }
            }
        }
        Ok(items)
    }

    /// Every collection across all library sections, with members in Plex's
    /// authored order. One request per section for its collection list, then one
    /// per collection for its ordered children.
    ///
    /// The children requests dominate the cost — measured at 72s of the 92s a
    /// full ingest spends on HTTP, because there is one sequential round trip
    /// per collection and no bulk endpoint. `since` (unix seconds) skips the
    /// children request for any collection whose own `updatedAt` predates it,
    /// which is what makes a warm restart cheap. A collection omitted this way
    /// keeps the membership already in the catalog.
    ///
    /// A show-subtype collection's children are shows, not episodes (#119):
    /// [`Self::episodes_of`] is called once per distinct member show to expand
    /// each one to its episode ratingKeys, and [`expand_show_members`] flattens
    /// the result back into authored order. `episode_cache` is shared across
    /// every collection in this one `fetch_collections` call, because a show
    /// commonly belongs to more than one collection and its ~45-episode leaf
    /// list is not worth re-fetching for each.
    fn fetch_collections(
        &self,
        since: Option<i64>,
    ) -> Result<Vec<ParsedCollection>, PlexIngestError> {
        let sections: SectionListResp = self.get("/library/sections", &[])?;
        let mut out = Vec::new();
        let mut episode_cache: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for section in &sections.media_container.directory {
            let Some(id) = section.key.as_deref() else {
                continue;
            };
            let endpoint = format!("/library/sections/{id}/collections");
            let resp: MediaContainerResp = self.get(&endpoint, &[])?;
            for c in &resp.media_container.metadata {
                let Some(collection_id) = c.rating_key.clone() else {
                    continue;
                };
                // A collection with no `updatedAt` is always fetched: unknown is
                // not the same as unchanged, and silently skipping it would
                // freeze its membership permanently.
                if let (Some(cutoff), Some(updated)) = (since, c.updated_at)
                    && updated < cutoff
                {
                    continue;
                }
                let members_ep = format!("/library/metadata/{collection_id}/children");
                let members: MediaContainerResp = self.get(&members_ep, &[])?;
                let member_keys: Vec<String> = members
                    .media_container
                    .metadata
                    .iter()
                    .filter_map(|m| m.rating_key.clone())
                    .collect();
                let name = c.title.clone().unwrap_or_default();
                let member_rating_keys = if c.subtype.as_deref() == Some("show") {
                    for show_key in &member_keys {
                        if episode_cache.contains_key(show_key) {
                            continue;
                        }
                        let keys = self.episodes_of(show_key)?;
                        episode_cache.insert(show_key.clone(), keys);
                    }
                    expand_show_members(&member_keys, &episode_cache)
                } else {
                    member_keys
                };
                // Plex's own child count says this collection has members, but
                // the children fetch (or, for a show collection, every member
                // show's leaf fetch) came back with none to record — a smart
                // collection is the known case (its `/children` reliably
                // returns zero regardless of subtype). Log it with the name
                // rather than let the collection sit silently empty.
                if member_rating_keys.is_empty() && c.child_count.unwrap_or(0) > 0 {
                    tracing::warn!(
                        event = "catalog.ingest.plex_collection_no_members_fetched",
                        collection = %name,
                        collection_id = %collection_id,
                        plex_child_count = c.child_count.unwrap_or(0),
                        "Plex reports members for this collection but the API returned none",
                    );
                }
                out.push(ParsedCollection {
                    collection_id,
                    name,
                    member_rating_keys,
                });
            }
        }
        Ok(out)
    }

    /// A show's episode ratingKeys in broadcast order, via Plex's `allLeaves`
    /// endpoint (all episodes across all seasons, recursively). Empty if the
    /// show has none.
    fn episodes_of(&self, show_rating_key: &str) -> Result<Vec<String>, PlexIngestError> {
        let endpoint = format!("/library/metadata/{show_rating_key}/allLeaves");
        let resp: MediaContainerResp = self.get(&endpoint, &[])?;
        Ok(resp
            .media_container
            .metadata
            .iter()
            .filter_map(|e| e.rating_key.clone())
            .collect())
    }

    /// Every Plex label across all library sections, with its section-scoped
    /// members' ratingKeys (#136). Two requests per label — its own listing at
    /// `/library/sections/{id}/label`, then its members at
    /// `/library/sections/{id}/all?label=<key>` — the same list-then-fetch
    /// shape [`Self::fetch_collections`] uses for a section's collections.
    ///
    /// A label carries no `updatedAt`, unlike a collection, so there is no
    /// analogue of `fetch_collections`' `since` skip: every call re-fetches
    /// every label's complete current membership. That is deliberate — see
    /// [`ingest_labels`]' doc comment for why the wholesale reconcile depends
    /// on this fetch always being complete, never delta'd.
    ///
    /// Verified directly against the live server (#136): the same label list
    /// comes back for every section regardless of its `key` — Plex's labels
    /// are server-wide — but a label's *member* listing is correctly scoped
    /// to the section + type it is queried against (a movie-only label
    /// returns zero episodes from a show section). So the label list itself
    /// is refetched once per section (cheap — one request) purely to learn
    /// which type filter its members need, per [`label_member_type_param`].
    fn fetch_labels(&self) -> Result<Vec<ParsedLabel>, PlexIngestError> {
        let sections: SectionListResp = self.get("/library/sections", &[])?;
        let mut out = Vec::new();
        for section in &sections.media_container.directory {
            let Some(id) = section.key.as_deref() else {
                continue;
            };
            let label_endpoint = format!("/library/sections/{id}/label");
            let section_labels: SectionListResp = self.get(&label_endpoint, &[])?;
            let type_param = label_member_type_param(section.kind.as_deref());
            let members_endpoint = format!("/library/sections/{id}/all");
            for label in &section_labels.media_container.directory {
                let (Some(key), Some(name)) = (label.key.as_deref(), label.title.as_deref()) else {
                    continue;
                };
                let mut params: Vec<(&str, &str)> = Vec::new();
                if let Some((k, v)) = type_param {
                    params.push((k, v));
                }
                params.push(("label", key));
                let resp: MediaContainerResp = self.get(&members_endpoint, &params)?;
                let member_rating_keys: Vec<String> = resp
                    .media_container
                    .metadata
                    .iter()
                    .filter_map(|m| m.rating_key.clone())
                    .collect();
                out.push(ParsedLabel {
                    name: name.to_string(),
                    member_rating_keys,
                });
            }
        }
        Ok(out)
    }
}

/// The `type` query param a label's member fetch needs, from the same
/// [`plex_type_code`] mapping [`section_list_params`] uses for a section's own
/// bulk listing.
fn label_member_type_param(kind: Option<&str>) -> Option<(&'static str, &'static str)> {
    plex_type_code(kind).map(|t| ("type", t))
}

/// Flatten a show-subtype collection's member shows to their episode
/// ratingKeys: shows in `show_rating_keys`' order, each show's own episodes in
/// the order `episodes` recorded them. A show absent from `episodes` (no
/// leaves, or never fetched) contributes nothing rather than erroring — the
/// caller who populates `episodes` decides whether that is worth a warning.
///
/// Pure so the fan-out/flatten order is unit-tested without a live Plex
/// server; [`PlexClient::fetch_collections`] is the only caller and is the one
/// that populates `episodes` over HTTP.
fn expand_show_members(
    show_rating_keys: &[String],
    episodes: &std::collections::HashMap<String, Vec<String>>,
) -> Vec<String> {
    show_rating_keys
        .iter()
        .flat_map(|show_key| episodes.get(show_key).into_iter().flatten().cloned())
        .collect()
}

// ---- Plex API JSON shapes -------------------------------------------------

#[derive(Debug, Deserialize)]
struct MediaContainerResp {
    #[serde(rename = "MediaContainer")]
    media_container: MediaContainer,
}

#[derive(Debug, Deserialize, Default)]
struct MediaContainer {
    #[serde(default, rename = "Metadata")]
    metadata: Vec<PlexMetadata>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PlexMetadata {
    #[serde(default)]
    rating_key: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    grandparent_title: Option<String>,
    #[serde(default)]
    parent_index: Option<i64>,
    #[serde(default)]
    index: Option<i64>,
    #[serde(default)]
    absolute_index: Option<i64>,
    #[serde(default)]
    year: Option<i64>,
    /// `YYYY-MM-DD`, present on both movies and episodes — maps straight onto
    /// `entries.release_date` (a `TEXT` column ordered lexically, which this
    /// format sorts chronologically).
    #[serde(default)]
    originally_available_at: Option<String>,
    #[serde(default)]
    duration: Option<i64>,
    #[serde(default)]
    content_rating: Option<String>,
    /// Unix seconds Plex last touched this record. Read for collections, to
    /// skip the per-collection children request when nothing has changed, and
    /// for library items in [`PlexClient::fetch_all`] to count how many carry
    /// no `updatedAt` at all (#134) — that class is invisible to a delta's
    /// `updatedAt>` filter and depends on `addedAt>` to be reached.
    #[serde(default)]
    updated_at: Option<i64>,
    /// Only read for collections: `"show"` for a TV collection (whose children
    /// are shows), `"movie"` for a movie collection (#119).
    #[serde(default)]
    subtype: Option<String>,
    /// Only read for collections: Plex's own member count, independent of
    /// whatever the children endpoint actually returns. Used solely to tell
    /// "genuinely empty" from "Plex says N members but the children fetch came
    /// back with none" so the latter can be logged instead of silently eaten.
    #[serde(default)]
    child_count: Option<i64>,
    #[serde(default)]
    edition_title: Option<String>,
    #[serde(default)]
    studio: Option<String>,
    #[serde(default, rename = "Guid")]
    guid: Vec<PlexGuid>,
    #[serde(default, rename = "Genre")]
    genre: Vec<TaggedField>,
    // No `Label` field here (#136): Plex's bulk section listing never carries
    // it, only a single-item fetch does — see `ingest_labels`' doc comment
    // for where the `label` tag namespace actually comes from.
    #[serde(default, rename = "Role")]
    role: Vec<TaggedField>,
    #[serde(default, rename = "Director")]
    director: Vec<TaggedField>,
    #[serde(default, rename = "Writer")]
    writer: Vec<TaggedField>,
    #[serde(default, rename = "Producer")]
    producer: Vec<TaggedField>,
    #[serde(default, rename = "Country")]
    country: Vec<TaggedField>,
    #[serde(default, rename = "Media")]
    media: Vec<PlexMedia>,
}

#[derive(Debug, Deserialize)]
struct PlexGuid {
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TaggedField {
    #[serde(default)]
    tag: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PlexMedia {
    #[serde(default, rename = "Part")]
    part: Vec<PlexPart>,
}

#[derive(Debug, Deserialize)]
struct PlexPart {
    #[serde(default)]
    file: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SectionListResp {
    #[serde(rename = "MediaContainer")]
    media_container: SectionList,
}

#[derive(Debug, Deserialize, Default)]
struct SectionList {
    #[serde(default, rename = "Directory")]
    directory: Vec<SectionEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SectionEntry {
    #[serde(default)]
    key: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    /// The library's display name ("4K Movies") — stamped onto every item the
    /// section yields as `entries.library`.
    #[serde(default)]
    title: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn movie(rating_key: &str, path: &str, guids: &[(ExternalNs, &str)]) -> PlexItem {
        PlexItem {
            rating_key: rating_key.into(),
            external_ids: guids
                .iter()
                .map(|(ns, v)| (*ns, (*v).to_string()))
                .collect(),
            playback_path: path.into(),
            kind: "movie".into(),
            title: "A Movie".into(),
            library: None,
            show: None,
            season: None,
            episode: None,
            absolute_episode: None,
            year: Some(1988),
            release_date: Some("1988-07-15".into()),
            content_rating: None,
            edition: None,
            studio: None,
            duration_ms: Some(7_920_000),
            genres: vec!["Action".into()],
            cast: vec![],
            directors: vec![],
            writers: vec![],
            producers: vec![],
            countries: vec![],
        }
    }

    /// #183: every section-listing request must carry `includeGuids=1`, or
    /// Plex omits `<Guid>` elements from the bulk response and every entry
    /// falls back to a path-hash `fs:` id instead of `imdb`/`tmdb`/`tvdb`.
    /// Covers every section kind (`show`, `movie`, unknown) crossed with
    /// both a full pass (`since_param = None`) and a delta pass, so the
    /// parameter can never quietly go missing from one combination.
    #[test]
    fn section_list_params_always_requests_guids() {
        for kind in [Some("show"), Some("movie"), None] {
            for since_param in [None, Some("1700000000")] {
                let params = section_list_params(kind, since_param);
                assert!(
                    params.contains(&("includeGuids", "1")),
                    "missing includeGuids=1 for kind={kind:?}, since={since_param:?}: {params:?}"
                );
            }
        }
    }

    /// `includeGuids=1` must compose with the delta filter rather than
    /// replace it — a delta pass still needs `updatedAt>` alongside it, or a
    /// warm restart would silently re-ingest the whole library (see
    /// `fetch_all`'s doc comment on the `updatedAt>` vs `updatedAt>=` bug).
    #[test]
    fn section_list_params_composes_guids_with_the_delta_filter() {
        let params = section_list_params(Some("show"), Some("1700000000"));
        assert_eq!(
            params,
            vec![
                ("type", "4"),
                ("includeGuids", "1"),
                ("updatedAt>", "1700000000"),
                ("or", "1"),
                ("addedAt>", "1700000000"),
            ]
        );
    }

    /// #134: an item with no `updatedAt` at all fails `updatedAt>` no matter
    /// what the cursor is, so the delta filter must reach it through
    /// `addedAt>` instead — every item has one. `or=1` is what makes Plex OR
    /// the two attribute filters instead of AND-ing them (which would demand
    /// both and still exclude the item) — but only when it sits *between*
    /// them. Asserting exact order, not just presence, is the point: a build
    /// with `or=1` moved to the end still contains all three params and
    /// passes a `.contains` check, but silently stops widening anything
    /// (verified live — see [`section_list_params`]'s doc comment).
    #[test]
    fn section_list_params_widens_the_delta_filter_to_added_at() {
        for kind in [Some("show"), Some("movie"), None] {
            let params = section_list_params(kind, Some("1700000000"));
            assert!(
                params.ends_with(&[
                    ("updatedAt>", "1700000000"),
                    ("or", "1"),
                    ("addedAt>", "1700000000"),
                ]),
                "delta filter must end with updatedAt>, or=1, addedAt> in that order \
                 for kind={kind:?}: {params:?}"
            );
        }
    }

    /// A full pass (`since_param = None`) has nothing to widen — there is no
    /// delta filter to OR against, so neither `addedAt>` nor `or=1` should
    /// appear.
    #[test]
    fn section_list_params_full_pass_has_no_delta_filter() {
        let params = section_list_params(Some("show"), None);
        assert_eq!(params, vec![("type", "4"), ("includeGuids", "1")]);
    }

    #[test]
    fn parse_guid_recognises_known_schemes() {
        assert_eq!(
            parse_guid("imdb://tt0095016"),
            Some((ExternalNs::Imdb, "tt0095016".into()))
        );
        assert_eq!(
            parse_guid("tmdb://562"),
            Some((ExternalNs::Tmdb, "562".into()))
        );
        assert_eq!(
            parse_guid("tvdb://12345"),
            Some((ExternalNs::Tvdb, "12345".into()))
        );
        assert_eq!(
            parse_guid("plex://movie/abc"),
            Some((ExternalNs::Plex, "movie/abc".into()))
        );
        assert_eq!(parse_guid("nonsense://x"), None);
        assert_eq!(parse_guid("imdb://"), None);
        assert_eq!(parse_guid("garbage"), None);
    }

    /// A blank GUID must not survive parsing (#184).
    ///
    /// Dropping it in `derive_entry_id` alone is not enough: a pair that gets
    /// past here is written to `entry_external_ids` by the ingest loop and read
    /// back by `resolve_existing` on the next scan, so a stored
    /// `(imdb, "   ") -> entry_A` would pull the next blank-GUID title onto
    /// `entry_A` and merge two unrelated films into one entry.
    #[test]
    fn a_blank_guid_value_does_not_survive_parsing() {
        assert_eq!(parse_guid("imdb://"), None, "empty value");
        assert_eq!(parse_guid("imdb://   "), None, "spaces only");
        assert_eq!(parse_guid("imdb://\t"), None, "tab only");
        assert_eq!(parse_guid("tmdb://\n"), None, "newline only");
        // A usable value keeps its surrounding whitespace verbatim — the id is a
        // byte-for-byte echo of the source, matching `derive_entry_id`.
        assert_eq!(
            parse_guid("imdb:// tt0095016 "),
            Some((ExternalNs::Imdb, " tt0095016 ".into()))
        );
    }

    /// Every episode of a show has to land on one `show_id`, because that is
    /// the only key the pattern engine groups a series by. Without it each
    /// episode is its own series of one and every grouping knob — `rotate =
    /// "visit"`, `advance = "resume"`, `group_by`, a take-all step — quietly
    /// does nothing while the channel still emits television. The prod catalog
    /// held 72,255 episodes and not one `show_id` before this.
    #[test]
    fn episodes_of_one_show_share_a_show_id_and_movies_have_none() {
        let cat = Catalog::open_in_memory().unwrap();
        let mut e1 = movie("plex-e1", "/data/media/tv/got/s01e01.mkv", &[]);
        e1.kind = "episode".into();
        e1.title = "Winter Is Coming".into();
        e1.show = Some("Game of Thrones".into());
        e1.season = Some(1);
        e1.episode = Some(1);
        let mut e2 = movie("plex-e2", "/data/media/tv/got/s02e03.mkv", &[]);
        e2.kind = "episode".into();
        e2.title = "What Is Dead May Never Die".into();
        e2.show = Some("Game of Thrones".into());
        e2.season = Some(2);
        e2.episode = Some(3);
        let film = movie("plex-m1", "/data/media/movies/Die Hard.mkv", &[]);

        ingest_items(&cat, &[e1, e2, film], &["/data/media".into()]).unwrap();

        let ids = cat.all_entry_ids().unwrap();
        let shows = cat.show_ids_for(&ids).unwrap();
        let mut got: Vec<&String> = shows.values().collect();
        got.sort();
        assert_eq!(
            got,
            vec!["show:Game of Thrones", "show:Game of Thrones"],
            "both episodes group under one show, the film under none"
        );

        // The name reads back out of the key, which is what `resolve` relies on.
        for value in shows.values() {
            assert_eq!(value.trim_start_matches("show:"), "Game of Thrones");
        }
    }

    #[test]
    fn id_derives_from_strongest_guid_and_records_all_external_ids() {
        let cat = Catalog::open_in_memory().unwrap();
        // Plex order puts tmdb first; imdb must still win the id.
        let item = movie(
            "plex-1",
            "/data/media/movies/Die Hard.mkv",
            &[(ExternalNs::Tmdb, "562"), (ExternalNs::Imdb, "tt0095016")],
        );
        let stats = ingest_items(&cat, &[item], &["/data/media".into()]).unwrap();
        assert_eq!(stats.entries_written, 1);
        assert_eq!(stats.inherited, 0);
        assert_eq!(
            cat.all_entry_ids().unwrap(),
            vec!["imdb:tt0095016".to_string()]
        );

        let e = cat.entry("imdb:tt0095016").unwrap().unwrap();
        assert_eq!(e.kind, "movie");
        assert_eq!(e.year, Some(1988));
        assert_eq!(e.release_date.as_deref(), Some("1988-07-15"));
        assert_eq!(e.duration_ms, Some(7_920_000));
        assert_eq!(
            cat.tags_for("imdb:tt0095016", TagNs::Genre).unwrap(),
            vec!["Action".to_string()]
        );
        // Provenance row is the plex ratingKey.
        let sources = cat.sources_for("imdb:tt0095016").unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].source, Source::Plex);
        assert_eq!(sources[0].source_id, "plex-1");
    }

    #[test]
    fn guidless_item_falls_back_to_fs_path_hash() {
        let cat = Catalog::open_in_memory().unwrap();
        let item = movie("plex-2", "/data/media/home/clip.mkv", &[]);
        ingest_items(&cat, &[item], &["/data/media".into()]).unwrap();
        assert!(cat.all_entry_ids().unwrap()[0].starts_with("fs:"));
    }

    #[test]
    fn plex_item_dedupes_onto_a_prior_fs_entry() {
        let cat = Catalog::open_in_memory().unwrap();
        // An FS scan already created a sparse fs: entry for this file (reached
        // under a different mount root) with a local_fs provenance row.
        crate::catalog::ingest::fs::ingest_files(
            &cat,
            &[(
                std::path::PathBuf::from("/mnt/media/movies/Die Hard.mkv"),
                Some(120.0),
            )],
            &["/mnt/media".into(), "/data/media".into()],
            false,
        )
        .unwrap();
        let fs_id = cat.all_entry_ids().unwrap()[0].clone();
        assert!(fs_id.starts_with("fs:"));

        // Plex ingests the same physical file (its own mount view + a real GUID).
        let item = movie(
            "plex-9",
            "/data/media/movies/Die Hard.mkv",
            &[(ExternalNs::Imdb, "tt0095016")],
        );
        let stats =
            ingest_items(&cat, &[item], &["/mnt/media".into(), "/data/media".into()]).unwrap();

        // One entry (the inherited fs: id), two provenance rows, imdb reachable,
        // and Plex upgraded the sparse title.
        assert_eq!(stats.inherited, 1);
        assert_eq!(cat.all_entry_ids().unwrap(), vec![fs_id.clone()]);
        let sources = cat.sources_for(&fs_id).unwrap();
        assert_eq!(sources.len(), 2);
        assert!(sources.iter().any(|s| s.source == Source::Plex));
        assert!(sources.iter().any(|s| s.source == Source::LocalFs));
        assert_eq!(
            cat.resolve_query("item.title == \"A Movie\"").unwrap(),
            vec![fs_id]
        );
    }

    #[test]
    fn to_plex_item_translates_path_and_extracts_guids() {
        let json = r#"{
            "ratingKey": "12345",
            "type": "movie",
            "title": "Die Hard",
            "year": 1988,
            "originallyAvailableAt": "1988-07-15",
            "duration": 7920000,
            "Guid": [{"id": "imdb://tt0095016"}, {"id": "tmdb://562"}],
            "Genre": [{"tag": "Action"}],
            "Media": [{"Part": [{"file": "/media/Movies/Die Hard.mkv"}]}]
        }"#;
        let m: PlexMetadata = serde_json::from_str(json).unwrap();
        let item = to_plex_item(&m, None, |p| p.replace("/media", "/data/media")).unwrap();
        assert_eq!(item.playback_path, "/data/media/Movies/Die Hard.mkv");
        assert_eq!(
            item.external_ids,
            vec![
                (ExternalNs::Imdb, "tt0095016".into()),
                (ExternalNs::Tmdb, "562".into())
            ]
        );
        assert_eq!(item.rating_key, "12345");
        assert_eq!(item.duration_ms, Some(7_920_000));
        assert_eq!(item.release_date.as_deref(), Some("1988-07-15"));
    }

    /// A record with no `originallyAvailableAt` (Plex omits it for some
    /// library items — #135's live-catalog numbers: 6 of 12,292 movies, 405
    /// of 72,434 episodes) must map to `None` — the same missing-means-null
    /// contract `year` already has.
    #[test]
    fn to_plex_item_leaves_release_date_none_when_plex_omits_it() {
        let json = r#"{
            "ratingKey": "12345",
            "type": "movie",
            "title": "Die Hard",
            "Media": [{"Part": [{"file": "/media/Movies/Die Hard.mkv"}]}]
        }"#;
        let m: PlexMetadata = serde_json::from_str(json).unwrap();
        let item = to_plex_item(&m, None, |p| p.to_string()).unwrap();
        assert_eq!(item.release_date, None);
    }

    #[test]
    fn to_plex_item_promotes_edition_and_studio() {
        let json = r#"{
            "ratingKey": "1",
            "type": "movie",
            "title": "The Lord of the Rings: The Fellowship of the Ring",
            "editionTitle": "Extended Edition",
            "studio": "New Line Cinema",
            "Media": [{"Part": [{"file": "/media/lotr.mkv"}]}]
        }"#;
        let m: PlexMetadata = serde_json::from_str(json).unwrap();
        let item = to_plex_item(&m, None, |p| p.to_string()).unwrap();
        assert_eq!(item.edition.as_deref(), Some("Extended Edition"));
        assert_eq!(item.studio.as_deref(), Some("New Line Cinema"));
    }

    #[test]
    fn theatrical_item_has_no_edition() {
        // A film with no `editionTitle` (and a blank one) is theatrical — both
        // normalise to `None` so the merge never overwrites with an empty string.
        let json = r#"{
            "ratingKey": "2",
            "type": "movie",
            "title": "Theatrical Cut",
            "editionTitle": "",
            "Media": [{"Part": [{"file": "/media/x.mkv"}]}]
        }"#;
        let m: PlexMetadata = serde_json::from_str(json).unwrap();
        let item = to_plex_item(&m, None, |p| p.to_string()).unwrap();
        assert_eq!(item.edition, None);
        assert_eq!(item.studio, None);
    }

    #[test]
    fn ingest_writes_edition_and_studio_queryable() {
        let cat = Catalog::open_in_memory().unwrap();
        let mut item = movie(
            "plex-e",
            "/data/media/m/x.mkv",
            &[(ExternalNs::Imdb, "tt-e")],
        );
        item.edition = Some("Extended Edition".into());
        item.studio = Some("New Line Cinema".into());
        ingest_items(&cat, &[item], &["/data/media".into()]).unwrap();

        let e = cat.entry("imdb:tt-e").unwrap().unwrap();
        assert_eq!(e.edition.as_deref(), Some("Extended Edition"));
        assert_eq!(e.studio.as_deref(), Some("New Line Cinema"));
        // Both promoted columns are queryable via the CEL→SQL surface.
        assert_eq!(
            cat.resolve_query(r#"item.studio == "New Line Cinema""#)
                .unwrap(),
            vec!["imdb:tt-e".to_string()]
        );
        assert_eq!(
            cat.resolve_query(r#"item.edition == "Extended Edition""#)
                .unwrap(),
            vec!["imdb:tt-e".to_string()]
        );
    }

    /// The section title is stamped onto every item the section yields — the
    /// per-item payload never carries it, so the section walk is the only place
    /// it can come from.
    #[test]
    fn to_plex_item_stamps_the_section_title_as_the_library() {
        let json = r#"{
            "ratingKey": "1",
            "type": "movie",
            "title": "A Film",
            "Media": [{"Part": [{"file": "/media/x.mkv"}]}]
        }"#;
        let m: PlexMetadata = serde_json::from_str(json).unwrap();
        let item = to_plex_item(&m, Some("4K Movies"), |p| p.to_string()).unwrap();
        assert_eq!(item.library.as_deref(), Some("4K Movies"));

        // A section with no title (or a blank one) yields no library rather than
        // an empty string, so the merge never overwrites a real value with "".
        let blank = to_plex_item(&m, Some("   "), |p| p.to_string()).unwrap();
        assert_eq!(blank.library, None);
        let none = to_plex_item(&m, None, |p| p.to_string()).unwrap();
        assert_eq!(none.library, None);
    }

    #[test]
    fn section_list_parses_the_library_title() {
        let json = r#"{"MediaContainer": {"Directory": [
            {"key": "1", "type": "movie", "title": "Movies"},
            {"key": "2", "type": "movie", "title": "4K Movies"},
            {"key": "3", "type": "show"}
        ]}}"#;
        let resp: SectionListResp = serde_json::from_str(json).unwrap();
        let titles: Vec<Option<&str>> = resp
            .media_container
            .directory
            .iter()
            .map(|s| s.title.as_deref())
            .collect();
        assert_eq!(titles, vec![Some("Movies"), Some("4K Movies"), None]);
    }

    /// Two movie libraries are indistinguishable by `type` alone — separating
    /// them is the whole point of the column (#128).
    #[test]
    fn ingest_separates_entries_by_library_and_leaves_fs_entries_null() {
        let cat = Catalog::open_in_memory().unwrap();
        let roots = ["/data/media".to_string()];
        let mut hd = movie(
            "rk-hd",
            "/data/media/m/hd.mkv",
            &[(ExternalNs::Imdb, "tt-hd")],
        );
        hd.library = Some("Movies".into());
        let mut uhd = movie(
            "rk-uhd",
            "/data/media/m/uhd.mkv",
            &[(ExternalNs::Imdb, "tt-uhd")],
        );
        uhd.library = Some("4K Movies".into());
        ingest_items(&cat, &[hd, uhd], &roots).unwrap();

        // An fs-sourced file no Plex library covers.
        crate::catalog::ingest::fs::ingest_files(
            &cat,
            &[(std::path::PathBuf::from("/data/media/bumpers/b.mkv"), None)],
            &roots,
            false,
        )
        .unwrap();

        assert_eq!(
            cat.entry("imdb:tt-uhd")
                .unwrap()
                .unwrap()
                .library
                .as_deref(),
            Some("4K Movies")
        );
        assert_eq!(
            cat.entry("imdb:tt-hd").unwrap().unwrap().library.as_deref(),
            Some("Movies")
        );

        // The CEL surface selects exactly one library and excludes the other —
        // `item.type == "movie"` matches both, which is the gap being closed.
        assert_eq!(
            cat.resolve_query(r#"item.library == "4K Movies""#).unwrap(),
            vec!["imdb:tt-uhd".to_string()]
        );
        assert_eq!(
            cat.resolve_query(r#"item.type == "movie""#).unwrap().len(),
            2
        );

        // The fs entry has no library, is matched by no `library == "…"`
        // expression, and querying it is not an error.
        let fs_id = cat
            .all_entry_ids()
            .unwrap()
            .into_iter()
            .find(|id| id.starts_with("fs:"))
            .expect("fs entry");
        assert_eq!(cat.entry(&fs_id).unwrap().unwrap().library, None);
        for expr in [
            r#"item.library == "Movies""#,
            r#"item.library == "4K Movies""#,
        ] {
            assert!(
                !cat.resolve_query(expr).unwrap().contains(&fs_id),
                "{expr} must not match the fs entry"
            );
        }
    }

    /// A re-ingest must land the same library value, not flap — and must not
    /// erase one when Plex hands the item back without a section title.
    #[test]
    fn library_is_stable_across_reingest_and_survives_a_titleless_pass() {
        let cat = Catalog::open_in_memory().unwrap();
        let roots = ["/data/media".to_string()];
        let mut item = movie("rk-a", "/data/media/m/a.mkv", &[(ExternalNs::Imdb, "tt-a")]);
        item.library = Some("4K Movies".into());
        ingest_items(&cat, std::slice::from_ref(&item), &roots).unwrap();
        ingest_items(&cat, std::slice::from_ref(&item), &roots).unwrap();
        assert_eq!(cat.all_entry_ids().unwrap(), vec!["imdb:tt-a".to_string()]);
        assert_eq!(cat.all_sources().unwrap().len(), 1);
        assert_eq!(
            cat.entry("imdb:tt-a").unwrap().unwrap().library.as_deref(),
            Some("4K Movies")
        );

        item.library = None;
        ingest_items(&cat, std::slice::from_ref(&item), &roots).unwrap();
        assert_eq!(
            cat.entry("imdb:tt-a").unwrap().unwrap().library.as_deref(),
            Some("4K Movies"),
            "a pass with no section title must not clear the library"
        );
    }

    #[test]
    fn to_plex_item_promotes_crew_cast_and_country_tags() {
        // No `Label` field here (#136): Plex's bulk listing never carries
        // one, so `to_plex_item` has nothing to parse it from — see
        // `ingest_labels_writes_the_label_tag_queryable` for how the `label`
        // namespace actually gets populated.
        let json = r#"{
            "ratingKey": "1",
            "type": "movie",
            "title": "Die Hard",
            "Role": [{"tag": "Bruce Willis"}, {"tag": "Alan Rickman"}],
            "Director": [{"tag": "John McTiernan"}],
            "Writer": [{"tag": "Jeb Stuart"}],
            "Producer": [{"tag": "Joel Silver"}],
            "Country": [{"tag": "United States"}],
            "Media": [{"Part": [{"file": "/media/x.mkv"}]}]
        }"#;
        let m: PlexMetadata = serde_json::from_str(json).unwrap();
        let item = to_plex_item(&m, None, |p| p.to_string()).unwrap();
        assert_eq!(item.cast, vec!["Bruce Willis", "Alan Rickman"]);
        assert_eq!(item.directors, vec!["John McTiernan"]);
        assert_eq!(item.writers, vec!["Jeb Stuart"]);
        assert_eq!(item.producers, vec!["Joel Silver"]);
        assert_eq!(item.countries, vec!["United States"]);
    }

    #[test]
    fn ingest_writes_crew_and_cast_tags_queryable() {
        let cat = Catalog::open_in_memory().unwrap();
        let mut item = movie(
            "plex-t",
            "/data/media/m/x.mkv",
            &[(ExternalNs::Imdb, "tt-t")],
        );
        item.cast = vec!["Jackie Chan".into()];
        item.directors = vec!["Stanley Tong".into()];
        ingest_items(&cat, &[item], &["/data/media".into()]).unwrap();

        assert_eq!(
            cat.tags_for("imdb:tt-t", TagNs::Cast).unwrap(),
            vec!["Jackie Chan".to_string()]
        );
        assert_eq!(
            cat.tags_for("imdb:tt-t", TagNs::Director).unwrap(),
            vec!["Stanley Tong".to_string()]
        );
        // Reachable through the CEL→SQL surface: dedicated fields and generic `tags`.
        assert_eq!(
            cat.resolve_query(r#"item.cast.contains("Jackie Chan")"#)
                .unwrap(),
            vec!["imdb:tt-t".to_string()]
        );
    }

    #[test]
    fn to_plex_item_promotes_absolute_episode_for_episodes() {
        let json = r#"{
            "ratingKey": "1",
            "type": "episode",
            "title": "The Arrival of Raditz",
            "grandparentTitle": "Dragon Ball Z",
            "parentIndex": 1,
            "index": 1,
            "absoluteIndex": 154,
            "Media": [{"Part": [{"file": "/media/dbz/e.mkv"}]}]
        }"#;
        let m: PlexMetadata = serde_json::from_str(json).unwrap();
        let item = to_plex_item(&m, None, |p| p.to_string()).unwrap();
        assert_eq!(item.absolute_episode, Some(154));
        assert_eq!(item.season, Some(1));
        assert_eq!(item.episode, Some(1));
    }

    #[test]
    fn movie_never_carries_absolute_episode() {
        // A movie with a stray `absoluteIndex` must not land `absolute_episode`
        // — same is_episode guard as season/episode.
        let json = r#"{
            "ratingKey": "2",
            "type": "movie",
            "title": "A Film",
            "absoluteIndex": 7,
            "Media": [{"Part": [{"file": "/media/x.mkv"}]}]
        }"#;
        let m: PlexMetadata = serde_json::from_str(json).unwrap();
        let item = to_plex_item(&m, None, |p| p.to_string()).unwrap();
        assert_eq!(item.absolute_episode, None);
    }

    #[test]
    fn ingest_writes_absolute_episode_queryable() {
        let cat = Catalog::open_in_memory().unwrap();
        let mut item = movie(
            "plex-ae",
            "/data/media/m/x.mkv",
            &[(ExternalNs::Imdb, "tt-ae")],
        );
        item.absolute_episode = Some(154);
        ingest_items(&cat, &[item], &["/data/media".into()]).unwrap();

        assert_eq!(
            cat.entry("imdb:tt-ae").unwrap().unwrap().absolute_episode,
            Some(154)
        );
        assert_eq!(
            cat.resolve_query("item.absolute_episode == 154").unwrap(),
            vec!["imdb:tt-ae".to_string()]
        );
    }

    #[test]
    fn ingest_collections_records_ordered_membership() {
        let cat = Catalog::open_in_memory().unwrap();
        // Two ingested movies (their `plex` provenance source_id is the ratingKey).
        let a = movie("rk-a", "/data/media/m/a.mkv", &[(ExternalNs::Imdb, "tt-a")]);
        let b = movie("rk-b", "/data/media/m/b.mkv", &[(ExternalNs::Imdb, "tt-b")]);
        ingest_items(&cat, &[a, b], &["/data/media".into()]).unwrap();

        // The collection lists b before a, then a member never ingested.
        let coll = ParsedCollection {
            collection_id: "coll-1".into(),
            name: "Halloween Marathon".into(),
            member_rating_keys: vec!["rk-b".into(), "rk-a".into(), "rk-missing".into()],
        };
        let stats = ingest_collections(&cat, std::slice::from_ref(&coll), false).unwrap();
        assert_eq!(stats.collections_written, 1);
        assert_eq!(stats.members_written, 2);
        assert_eq!(stats.members_unresolved, 1);

        // Read back in authored order (b, a); the unresolved ratingKey is absent,
        // not a positional gap.
        assert_eq!(
            cat.collection_members("coll-1").unwrap(),
            vec!["imdb:tt-b".to_string(), "imdb:tt-a".to_string()]
        );
        // Membership is queryable by collection name via the CEL→SQL surface.
        assert_eq!(
            cat.resolve_query(r#"item.collections.contains("Halloween Marathon")"#)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn ingest_collections_counts_a_deduped_member_once() {
        let cat = Catalog::open_in_memory().unwrap();
        // Two Plex files (4K + 1080p) share one GUID → one entry, two `plex`
        // provenance rows (two ratingKeys).
        let dupes = [
            movie(
                "rk-4k",
                "/data/media/m/a-4k.mkv",
                &[(ExternalNs::Imdb, "tt-a")],
            ),
            movie(
                "rk-hd",
                "/data/media/m/a-hd.mkv",
                &[(ExternalNs::Imdb, "tt-a")],
            ),
        ];
        ingest_items(&cat, &dupes, &["/data/media".into()]).unwrap();
        let b = movie("rk-b", "/data/media/m/b.mkv", &[(ExternalNs::Imdb, "tt-b")]);
        ingest_items(&cat, std::slice::from_ref(&b), &["/data/media".into()]).unwrap();

        // The collection lists both ratingKeys of the one entry, then another.
        let coll = ParsedCollection {
            collection_id: "coll-1".into(),
            name: "C".into(),
            member_rating_keys: vec!["rk-4k".into(), "rk-hd".into(), "rk-b".into()],
        };
        let stats = ingest_collections(&cat, &[coll], false).unwrap();
        // The deduped entry counts once; positions stay contiguous (a=0, b=1).
        assert_eq!(stats.members_written, 2);
        assert_eq!(
            cat.collection_members("coll-1").unwrap(),
            vec!["imdb:tt-a".to_string(), "imdb:tt-b".to_string()]
        );
    }

    /// A member dragged out of a collection in Plex has to disappear from the
    /// catalog. `add_collection_item` only inserts and updates, so without the
    /// clear in `ingest_collections` the stale row would survive every future
    /// ingest and the entry would keep airing on a collection channel.
    #[test]
    fn ingest_collections_drops_a_member_removed_upstream() {
        let cat = Catalog::open_in_memory().unwrap();
        for (rk, id) in [("rk-a", "tt-a"), ("rk-b", "tt-b")] {
            let m = movie(
                rk,
                &format!("/data/media/m/{rk}.mkv"),
                &[(ExternalNs::Imdb, id)],
            );
            ingest_items(&cat, std::slice::from_ref(&m), &["/data/media".into()]).unwrap();
        }
        let both = ParsedCollection {
            collection_id: "coll-1".into(),
            name: "C".into(),
            member_rating_keys: vec!["rk-a".into(), "rk-b".into()],
        };
        ingest_collections(&cat, std::slice::from_ref(&both), false).unwrap();
        assert_eq!(cat.collection_members("coll-1").unwrap().len(), 2);

        // Plex now reports only one member.
        let one = ParsedCollection {
            collection_id: "coll-1".into(),
            name: "C".into(),
            member_rating_keys: vec!["rk-a".into()],
        };
        ingest_collections(&cat, std::slice::from_ref(&one), false).unwrap();
        assert_eq!(
            cat.collection_members("coll-1").unwrap(),
            vec!["imdb:tt-a".to_string()]
        );
    }

    /// A whole collection deleted in Plex vanishes from a full pass, so it must
    /// be pruned — but only on a full pass. A delta, which omits unchanged
    /// collections, must leave them alone.
    #[test]
    fn full_pass_prunes_a_collection_gone_from_plex_but_a_delta_does_not() {
        let cat = Catalog::open_in_memory().unwrap();
        let m = movie("rk-a", "/data/media/m/a.mkv", &[(ExternalNs::Imdb, "tt-a")]);
        ingest_items(&cat, std::slice::from_ref(&m), &["/data/media".into()]).unwrap();
        let keep = ParsedCollection {
            collection_id: "keep".into(),
            name: "Keep".into(),
            member_rating_keys: vec!["rk-a".into()],
        };
        let gone = ParsedCollection {
            collection_id: "gone".into(),
            name: "Gone".into(),
            member_rating_keys: vec!["rk-a".into()],
        };
        ingest_collections(&cat, &[keep.clone(), gone], false).unwrap();
        assert_eq!(cat.all_collection_ids().unwrap().len(), 2);

        // A delta pass returns only "keep" (it changed); "gone" is merely absent,
        // not deleted, so it must survive.
        ingest_collections(&cat, std::slice::from_ref(&keep), false).unwrap();
        assert_eq!(
            cat.all_collection_ids().unwrap().len(),
            2,
            "delta keeps absent"
        );

        // A full pass returns only "keep": "gone" is really gone and is pruned,
        // its membership cascading away.
        let stats = ingest_collections(&cat, std::slice::from_ref(&keep), true).unwrap();
        assert_eq!(stats.collections_pruned, 1);
        assert_eq!(cat.all_collection_ids().unwrap(), vec!["keep".to_string()]);
        assert!(cat.collection_members("gone").unwrap().is_empty());
    }

    #[test]
    fn ingest_collections_is_idempotent() {
        let cat = Catalog::open_in_memory().unwrap();
        let a = movie("rk-a", "/data/media/m/a.mkv", &[(ExternalNs::Imdb, "tt-a")]);
        ingest_items(&cat, std::slice::from_ref(&a), &["/data/media".into()]).unwrap();
        let coll = ParsedCollection {
            collection_id: "coll-1".into(),
            name: "C".into(),
            member_rating_keys: vec!["rk-a".into()],
        };
        ingest_collections(&cat, std::slice::from_ref(&coll), false).unwrap();
        // A second pass must not duplicate the membership row.
        ingest_collections(&cat, &[coll], false).unwrap();
        assert_eq!(
            cat.collection_members("coll-1").unwrap(),
            vec!["imdb:tt-a".to_string()]
        );
    }

    #[test]
    fn expand_show_members_preserves_show_then_episode_order() {
        let episodes: std::collections::HashMap<String, Vec<String>> = [
            ("show-b".to_string(), vec!["b-e1".into(), "b-e2".into()]),
            ("show-a".to_string(), vec!["a-e1".into()]),
        ]
        .into_iter()
        .collect();
        // Shows in their Plex-authored order (b before a); each show's own
        // episodes in the order its leaf fetch recorded them.
        let show_keys = vec!["show-b".to_string(), "show-a".to_string()];
        assert_eq!(
            expand_show_members(&show_keys, &episodes),
            vec!["b-e1".to_string(), "b-e2".to_string(), "a-e1".to_string()]
        );
    }

    #[test]
    fn expand_show_members_skips_a_show_with_no_recorded_episodes() {
        let episodes: std::collections::HashMap<String, Vec<String>> =
            [("show-a".to_string(), vec!["a-e1".to_string()])]
                .into_iter()
                .collect();
        // "show-never-fetched" is absent from the map — a show with no
        // leaves, or one the caller never looked up — and must contribute
        // nothing rather than a gap or an error.
        let show_keys = vec!["show-a".to_string(), "show-never-fetched".to_string()];
        assert_eq!(
            expand_show_members(&show_keys, &episodes),
            vec!["a-e1".to_string()]
        );
    }

    /// A show-subtype container's members are expanded to episode ratingKeys
    /// before `ingest_collections` ever runs, so a fan-out where every member
    /// show has no ingested episodes looks, from here, exactly like a
    /// collection whose reported ratingKeys resolve to nothing — the same
    /// warning path a movie collection with all-uningested members would hit.
    #[test]
    fn ingest_collections_warns_when_reported_members_all_fail_to_resolve() {
        use std::sync::{Arc, Mutex};

        use tracing::field::{Field, Visit};
        use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

        #[derive(Default)]
        struct WarnFields {
            event: Option<String>,
            collection: Option<String>,
            reported_members: Option<u64>,
        }
        impl Visit for WarnFields {
            fn record_u64(&mut self, field: &Field, value: u64) {
                if field.name() == "reported_members" {
                    self.reported_members = Some(value);
                }
            }
            fn record_str(&mut self, field: &Field, value: &str) {
                if field.name() == "event" {
                    self.event = Some(value.to_string());
                }
            }
            // `%coll.name` records via `record_debug` (a `Display` value is
            // wrapped so its `Debug` impl delegates to `Display`), not
            // `record_str` — only a literal `&str` field goes through that.
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                if field.name() == "collection" {
                    self.collection = Some(format!("{value:?}"));
                }
            }
        }

        struct CaptureWarn(Arc<Mutex<Vec<(String, u64)>>>);
        impl<S: tracing::Subscriber> Layer<S> for CaptureWarn {
            fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
                let mut fields = WarnFields::default();
                event.record(&mut fields);
                if fields.event.as_deref() != Some("catalog.ingest.plex_collection_unresolved") {
                    return;
                }
                self.0.lock().unwrap().push((
                    fields.collection.unwrap_or_default(),
                    fields.reported_members.unwrap_or_default(),
                ));
            }
        }

        let cat = Catalog::open_in_memory().unwrap();
        // Plex reported two episode ratingKeys, but neither was ever ingested
        // (no `plex` provenance row exists for them).
        let coll = ParsedCollection {
            collection_id: "coll-1".into(),
            name: "New Episodes".into(),
            member_rating_keys: vec!["ep-1".into(), "ep-2".into()],
        };

        let seen = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(CaptureWarn(Arc::clone(&seen)));
        let stats = tracing::subscriber::with_default(subscriber, || {
            ingest_collections(&cat, std::slice::from_ref(&coll), false).unwrap()
        });

        assert_eq!(stats.members_written, 0);
        assert_eq!(stats.members_unresolved, 2);
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [("New Episodes".to_string(), 2)]
        );
    }

    /// A collection Plex genuinely reports zero members for must not trip the
    /// same warning — only a *reported-but-unresolved* member list should.
    #[test]
    fn ingest_collections_does_not_warn_for_a_genuinely_empty_collection() {
        use std::sync::{Arc, Mutex};

        use tracing::field::{Field, Visit};
        use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

        #[derive(Default)]
        struct SeenEvent(Option<String>);
        impl Visit for SeenEvent {
            fn record_str(&mut self, field: &Field, value: &str) {
                if field.name() == "event" {
                    self.0 = Some(value.to_string());
                }
            }
            fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
        }

        struct CaptureAny(Arc<Mutex<Vec<String>>>);
        impl<S: tracing::Subscriber> Layer<S> for CaptureAny {
            fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
                let mut seen = SeenEvent::default();
                event.record(&mut seen);
                if let Some(event) = seen.0 {
                    self.0.lock().unwrap().push(event);
                }
            }
        }

        let cat = Catalog::open_in_memory().unwrap();
        let coll = ParsedCollection {
            collection_id: "coll-1".into(),
            name: "Streaming Collections".into(),
            member_rating_keys: vec![],
        };

        let seen = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(CaptureAny(Arc::clone(&seen)));
        tracing::subscriber::with_default(subscriber, || {
            ingest_collections(&cat, std::slice::from_ref(&coll), false).unwrap()
        });

        assert!(
            !seen
                .lock()
                .unwrap()
                .contains(&"catalog.ingest.plex_collection_unresolved".to_string())
        );
    }

    #[test]
    fn label_member_type_param_matches_section_kind() {
        assert_eq!(label_member_type_param(Some("movie")), Some(("type", "1")));
        assert_eq!(label_member_type_param(Some("show")), Some(("type", "4")));
        assert_eq!(label_member_type_param(Some("other")), None);
        assert_eq!(label_member_type_param(None), None);
    }

    /// #136: the actual bug — a channel querying `item.labels.contains(…)`
    /// resolves the items Plex has that label on, sourced from a per-label
    /// membership fetch rather than the (always-empty, for bulk) per-item
    /// `Label` field.
    #[test]
    fn ingest_labels_writes_the_label_tag_queryable() {
        let cat = Catalog::open_in_memory().unwrap();
        let item = movie(
            "plex-x",
            "/data/media/movies/Xmas.mkv",
            &[(ExternalNs::Imdb, "tt-x")],
        );
        ingest_items(&cat, &[item], &["/data/media".into()]).unwrap();

        let labels = [ParsedLabel {
            name: "🎅 Christmas Movies".into(),
            member_rating_keys: vec!["plex-x".into()],
        }];
        let stats = ingest_labels(&cat, &labels).unwrap();
        assert_eq!(stats.members_written, 1);
        assert_eq!(stats.members_unresolved, 0);
        assert_eq!(
            cat.tags_for("imdb:tt-x", TagNs::Label).unwrap(),
            vec!["🎅 Christmas Movies".to_string()]
        );
        assert_eq!(
            cat.resolve_query(r#"item.labels.contains("🎅 Christmas Movies")"#)
                .unwrap(),
            vec!["imdb:tt-x".to_string()]
        );
    }

    /// A ratingKey a label reports that never resolved to a catalog entry
    /// (not ingested, or FS-only) is skipped, not tagged — same shape as
    /// `ingest_collections`' `members_unresolved`.
    #[test]
    fn ingest_labels_counts_an_unresolved_member() {
        let cat = Catalog::open_in_memory().unwrap();
        let labels = [ParsedLabel {
            name: "Christmas".into(),
            member_rating_keys: vec!["plex-ghost".into()],
        }];
        let stats = ingest_labels(&cat, &labels).unwrap();
        assert_eq!(stats.members_written, 0);
        assert_eq!(stats.members_unresolved, 1);
    }

    /// A member ratingKey a dedupe already collapsed onto another entry (e.g.
    /// 4K + 1080p) must not double-write the same `(entry, label)` tag row —
    /// mirrors `ingest_collections_counts_a_deduped_member_once`.
    #[test]
    fn ingest_labels_counts_a_deduped_member_once() {
        let cat = Catalog::open_in_memory().unwrap();
        let items = [
            movie(
                "plex-4k",
                "/data/media/movies/X-4k.mkv",
                &[(ExternalNs::Imdb, "tt-dup")],
            ),
            movie(
                "plex-hd",
                "/data/media/movies/X-1080.mkv",
                &[(ExternalNs::Imdb, "tt-dup")],
            ),
        ];
        ingest_items(&cat, &items, &["/data/media".into()]).unwrap();

        let labels = [ParsedLabel {
            name: "Christmas".into(),
            member_rating_keys: vec!["plex-4k".into(), "plex-hd".into()],
        }];
        let stats = ingest_labels(&cat, &labels).unwrap();
        assert_eq!(stats.members_written, 1, "one entry, tagged once");
        assert_eq!(
            cat.tags_for("imdb:tt-dup", TagNs::Label).unwrap(),
            vec!["Christmas".to_string()]
        );
    }

    /// The root cause this issue fixes, in reverse: a label removed from an
    /// item in Plex must not survive a re-ingest. `ingest_labels` is not
    /// handed a per-entry record to reconcile against (unlike genre/cast/…),
    /// so it has to clear the whole `label` namespace up front — this proves
    /// that clear actually reaches an entry absent from the new fetch
    /// entirely, not just an entry whose label list shrank.
    #[test]
    fn ingest_labels_reconciles_wholesale_a_removed_label_does_not_survive() {
        let cat = Catalog::open_in_memory().unwrap();
        let item = movie(
            "plex-y",
            "/data/media/movies/Y.mkv",
            &[(ExternalNs::Imdb, "tt-y")],
        );
        ingest_items(&cat, &[item], &["/data/media".into()]).unwrap();

        ingest_labels(
            &cat,
            &[ParsedLabel {
                name: "Christmas".into(),
                member_rating_keys: vec!["plex-y".into()],
            }],
        )
        .unwrap();
        assert_eq!(
            cat.tags_for("imdb:tt-y", TagNs::Label).unwrap(),
            vec!["Christmas".to_string()]
        );

        // Next pass: Plex reports this item under no label at all.
        ingest_labels(&cat, &[]).unwrap();
        assert!(
            cat.tags_for("imdb:tt-y", TagNs::Label).unwrap().is_empty(),
            "a label removed upstream must not survive a re-ingest"
        );
    }

    #[test]
    fn item_without_a_file_part_is_skipped() {
        let json = r#"{"ratingKey": "1", "type": "movie", "title": "x", "Media": []}"#;
        let m: PlexMetadata = serde_json::from_str(json).unwrap();
        assert!(to_plex_item(&m, None, |p| p.to_string()).is_none());
    }

    /// Plex authors the whole tag set per namespace, so a re-ingest has to
    /// reconcile it rather than accumulate. Without the clear in `ingest_items`,
    /// `add_tag`'s `INSERT OR IGNORE` leaves a genre removed upstream attached
    /// forever, and it keeps matching queries that should no longer select it.
    /// Same shape as `ingest_collections_drops_a_member_removed_upstream`, one
    /// level down.
    #[test]
    fn ingest_items_drops_tags_removed_upstream() {
        let cat = Catalog::open_in_memory().unwrap();
        let roots = ["/data/media".to_string()];
        let mut item = movie("rk-a", "/data/media/m/a.mkv", &[(ExternalNs::Imdb, "tt-a")]);
        item.genres = vec!["Action".into(), "Comedy".into()];
        item.directors = vec!["Someone".into()];
        ingest_items(&cat, std::slice::from_ref(&item), &roots).unwrap();
        assert_eq!(
            cat.tags_for("imdb:tt-a", TagNs::Genre).unwrap(),
            vec!["Action".to_string(), "Comedy".to_string()]
        );

        // The Comedy genre is removed in Plex; the director is untouched.
        item.genres = vec!["Action".into()];
        ingest_items(&cat, std::slice::from_ref(&item), &roots).unwrap();
        assert_eq!(
            cat.tags_for("imdb:tt-a", TagNs::Genre).unwrap(),
            vec!["Action".to_string()],
            "a genre removed upstream must not survive a re-ingest"
        );
        assert_eq!(
            cat.tags_for("imdb:tt-a", TagNs::Director).unwrap(),
            vec!["Someone".to_string()],
            "reconciling one namespace must not disturb another"
        );
    }

    // A second guard on the per-(entry, namespace) scope of `clear_tags` used to
    // live here, seeding an `fs_dir` tag the filesystem scan owned and checking
    // a Plex re-ingest left it alone. `fs_dir` is no longer a stored tag (#123),
    // so there is no second author to guard against, and the same widening it
    // watched for — `clear_tags` going per-entry — already fails the
    // "reconciling one namespace must not disturb another" assertion above.

    #[test]
    fn rescans_are_idempotent() {
        let cat = Catalog::open_in_memory().unwrap();
        let item = movie(
            "plex-1",
            "/data/media/m/x.mkv",
            &[(ExternalNs::Imdb, "tt1")],
        );
        let roots = ["/data/media".to_string()];
        ingest_items(&cat, std::slice::from_ref(&item), &roots).unwrap();
        let stats = ingest_items(&cat, &[item], &roots).unwrap();
        assert_eq!(stats.inherited, 1);
        assert_eq!(cat.all_entry_ids().unwrap(), vec!["imdb:tt1".to_string()]);
        assert_eq!(cat.all_sources().unwrap().len(), 1);
    }

    #[test]
    fn two_files_sharing_a_guid_collapse_to_one_entry() {
        // A movie present as two files (4K + 1080p), same imdb GUID, distinct
        // paths → one entry keyed on the GUID, two plex provenance rows, the
        // external-id row stable (not flipped between them).
        let cat = Catalog::open_in_memory().unwrap();
        let items = [
            movie(
                "plex-4k",
                "/data/media/movies/DieHard-4k.mkv",
                &[(ExternalNs::Imdb, "tt0095016")],
            ),
            movie(
                "plex-hd",
                "/data/media/movies/DieHard-1080.mkv",
                &[(ExternalNs::Imdb, "tt0095016")],
            ),
        ];
        ingest_items(&cat, &items, &["/data/media".into()]).unwrap();
        assert_eq!(
            cat.all_entry_ids().unwrap(),
            vec!["imdb:tt0095016".to_string()]
        );
        assert_eq!(cat.sources_for("imdb:tt0095016").unwrap().len(), 2);
        assert_eq!(
            cat.entry_id_for_external_id(ExternalNs::Imdb, "tt0095016")
                .unwrap(),
            Some("imdb:tt0095016".to_string())
        );
    }

    #[test]
    fn plex_null_duration_does_not_clobber_an_fs_probed_duration() {
        let cat = Catalog::open_in_memory().unwrap();
        let path = "/data/media/movies/x.mkv";
        // FS scan records a probed duration.
        crate::catalog::ingest::fs::ingest_files(
            &cat,
            &[(std::path::PathBuf::from(path), Some(120.0))],
            &["/data/media".into()],
            false,
        )
        .unwrap();
        let id = cat.all_entry_ids().unwrap()[0].clone();
        assert_eq!(cat.entry(&id).unwrap().unwrap().duration_ms, Some(120_000));

        // Plex ingests the same file but has NOT analysed it (duration None).
        let mut item = movie("plex-1", path, &[]);
        item.duration_ms = None;
        item.year = None;
        ingest_items(&cat, &[item], &["/data/media".into()]).unwrap();

        // The fs-probed duration survives; Plex only fills gaps.
        let e = cat.entry(&id).unwrap().unwrap();
        assert_eq!(e.duration_ms, Some(120_000));
    }

    #[test]
    fn translate_only_maps_at_a_path_boundary() {
        let client = PlexClient {
            base_url: "http://x".into(),
            token: "t".into(),
            path_from: "/media".into(),
            path_to: "/data/media".into(),
            agent: ureq::AgentBuilder::new().build(),
        };
        assert_eq!(
            client.translate("/media/Movies/A.mkv"),
            "/data/media/Movies/A.mkv"
        );
        assert_eq!(client.translate("/media"), "/data/media");
        // Sibling prefix must NOT be remapped.
        assert_eq!(client.translate("/mediabackup/x.mkv"), "/mediabackup/x.mkv");
        assert_eq!(client.translate("/other/path"), "/other/path");
    }
}

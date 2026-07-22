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
//! [`PlexItem`]s), unit-testable without a live server; [`ingest_from_env`] is
//! the thin HTTP front door that reads `PLEX_URL`/`PLEX_TOKEN`, fetches, and
//! calls it.
//!
//! Out of scope for this slice (tracked on #91): collections / playlists (need
//! the ordered-children fetch), delta sync via `lastUpdated`, and the refresh
//! TTL / manual trigger — all land with the catalog-into-daemon wiring (#96).

use std::time::Duration;

use serde::Deserialize;

use crate::catalog::identity::{canonical_path, derive_entry_id};
use crate::catalog::model::{Entry, EntrySource, ExternalNs, Source, TagNs};
use crate::catalog::{Catalog, CatalogError};

const HTTP_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, thiserror::Error)]
pub enum PlexIngestError {
    #[error("missing env var: {0}")]
    MissingEnv(&'static str),
    #[error("http: {0}")]
    Http(String),
    #[error("parse: {0}")]
    Parse(String),
    #[error("catalog: {0}")]
    Catalog(#[from] CatalogError),
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
    /// Show name for an episode (`grandparentTitle`); `None` for a movie.
    pub show: Option<String>,
    pub season: Option<i64>,
    pub episode: Option<i64>,
    pub year: Option<i64>,
    pub content_rating: Option<String>,
    /// Plex `editionTitle`; `None`/empty = theatrical.
    pub edition: Option<String>,
    /// Plex `studio` — single production-company string.
    pub studio: Option<String>,
    pub duration_ms: Option<i64>,
    pub genres: Vec<String>,
    /// Namespaced person/label tags: Plex `Label`, `Role` (cast), `Director`,
    /// `Writer`, `Producer`, `Country`.
    pub labels: Vec<String>,
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
            merged(non_empty(&item.title), existing.as_ref().map(|e| e.title.clone())).unwrap_or_default(),
            Source::Plex,
        );
        entry.show = or_existing(item.show.clone(), existing.as_ref().and_then(|e| e.show.clone()));
        entry.season = item.season.or_else(|| existing.as_ref().and_then(|e| e.season));
        entry.episode = item.episode.or_else(|| existing.as_ref().and_then(|e| e.episode));
        entry.year = item.year.or_else(|| existing.as_ref().and_then(|e| e.year));
        entry.content_rating = or_existing(
            item.content_rating.clone(),
            existing.as_ref().and_then(|e| e.content_rating.clone()),
        );
        entry.edition = or_existing(item.edition.clone(), existing.as_ref().and_then(|e| e.edition.clone()));
        entry.studio = or_existing(item.studio.clone(), existing.as_ref().and_then(|e| e.studio.clone()));
        entry.duration_ms = item.duration_ms.or_else(|| existing.as_ref().and_then(|e| e.duration_ms));
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
            (TagNs::Label, &item.labels),
            (TagNs::Cast, &item.cast),
            (TagNs::Director, &item.directors),
            (TagNs::Writer, &item.writers),
            (TagNs::Producer, &item.producers),
            (TagNs::Country, &item.countries),
        ] {
            for value in values {
                catalog.add_tag(&entry_id, ns, value)?;
            }
        }
        stats.sources_written += 1;
    }
    Ok(stats)
}

/// Fetch every library's movies + episodes from Plex and ingest them.
/// `source_roots` canonicalise paths for identity/path-match.
///
/// Reads `PLEX_URL` / `PLEX_TOKEN` (required) and `MEDIA_PATH_FROM` /
/// `MEDIA_PATH_TO` (optional path translation) from the environment.
pub fn ingest_from_env(
    catalog: &Catalog,
    source_roots: &[String],
) -> Result<PlexIngestStats, PlexIngestError> {
    let client = PlexClient::from_env()?;
    let items = client.fetch_all()?;
    // One transaction for the whole write pass — a mid-ingest failure rolls back
    // rather than leaving a partial catalog.
    catalog.in_transaction(|c| ingest_items(c, &items, source_roots))
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
fn parse_guid(id: &str) -> Option<(ExternalNs, String)> {
    let (scheme, value) = id.split_once("://")?;
    let ns = match scheme {
        "imdb" => ExternalNs::Imdb,
        "tmdb" => ExternalNs::Tmdb,
        "tvdb" => ExternalNs::Tvdb,
        "plex" => ExternalNs::Plex,
        _ => return None,
    };
    if value.is_empty() {
        return None;
    }
    Some((ns, value.to_string()))
}

/// Convert one Plex metadata record into a [`PlexItem`], applying `translate` to
/// the file path. Returns `None` for a record with no playable file part.
fn to_plex_item(m: &PlexMetadata, translate: impl Fn(&str) -> String) -> Option<PlexItem> {
    let raw_path = m.media.first()?.part.first()?.file.as_deref()?;
    let external_ids = m.guid.iter().filter_map(|g| g.id.as_deref().and_then(parse_guid)).collect();
    let kind = m.kind.clone().unwrap_or_else(|| "video".into());
    // Season/episode belong to episodes; a movie carrying a stray `index` must
    // not land `episode = Some(n)`.
    let is_episode = kind == "episode";
    Some(PlexItem {
        rating_key: m.rating_key.clone()?,
        external_ids,
        playback_path: translate(raw_path),
        title: m.title.clone().unwrap_or_default(),
        show: m.grandparent_title.clone(),
        season: is_episode.then_some(m.parent_index).flatten(),
        episode: is_episode.then_some(m.index).flatten(),
        year: m.year,
        kind,
        content_rating: m.content_rating.clone(),
        // Absent/blank `editionTitle` means theatrical — normalise to `None` so
        // the merge never overwrites an existing edition with an empty string.
        edition: m.edition_title.as_deref().and_then(non_empty).map(str::to_string),
        studio: m.studio.as_deref().and_then(non_empty).map(str::to_string),
        // Plex `duration` is already milliseconds.
        duration_ms: m.duration,
        genres: tagged(&m.genre),
        labels: tagged(&m.label),
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

// ---- HTTP client (thin outer layer) --------------------------------------

struct PlexClient {
    base_url: String,
    token: String,
    path_from: String,
    path_to: String,
    agent: ureq::Agent,
}

impl PlexClient {
    fn from_env() -> Result<Self, PlexIngestError> {
        let base_url =
            std::env::var("PLEX_URL").map_err(|_| PlexIngestError::MissingEnv("PLEX_URL"))?;
        let token =
            std::env::var("PLEX_TOKEN").map_err(|_| PlexIngestError::MissingEnv("PLEX_TOKEN"))?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            path_from: std::env::var("MEDIA_PATH_FROM").unwrap_or_default(),
            path_to: std::env::var("MEDIA_PATH_TO").unwrap_or_default(),
            agent: ureq::AgentBuilder::new().timeout(HTTP_TIMEOUT).build(),
        })
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
    fn fetch_all(&self) -> Result<Vec<PlexItem>, PlexIngestError> {
        let sections: SectionListResp = self.get("/library/sections", &[])?;
        let mut items = Vec::new();
        for section in &sections.media_container.directory {
            let Some(id) = section.key.as_deref() else {
                continue;
            };
            // Movies come back directly; a show section is expanded to its
            // episode leaves (type=4).
            let params: &[(&str, &str)] = match section.kind.as_deref() {
                Some("show") => &[("type", "4")],
                Some("movie") => &[("type", "1")],
                _ => &[],
            };
            let endpoint = format!("/library/sections/{id}/all");
            let resp: MediaContainerResp = self.get(&endpoint, params)?;
            for m in &resp.media_container.metadata {
                if let Some(item) = to_plex_item(m, |p| self.translate(p)) {
                    items.push(item);
                }
            }
        }
        Ok(items)
    }
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
    year: Option<i64>,
    #[serde(default)]
    duration: Option<i64>,
    #[serde(default)]
    content_rating: Option<String>,
    #[serde(default)]
    edition_title: Option<String>,
    #[serde(default)]
    studio: Option<String>,
    #[serde(default, rename = "Guid")]
    guid: Vec<PlexGuid>,
    #[serde(default, rename = "Genre")]
    genre: Vec<TaggedField>,
    #[serde(default, rename = "Label")]
    label: Vec<TaggedField>,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn movie(rating_key: &str, path: &str, guids: &[(ExternalNs, &str)]) -> PlexItem {
        PlexItem {
            rating_key: rating_key.into(),
            external_ids: guids.iter().map(|(ns, v)| (*ns, (*v).to_string())).collect(),
            playback_path: path.into(),
            kind: "movie".into(),
            title: "A Movie".into(),
            show: None,
            season: None,
            episode: None,
            year: Some(1988),
            content_rating: None,
            edition: None,
            studio: None,
            duration_ms: Some(7_920_000),
            genres: vec!["Action".into()],
            labels: vec![],
            cast: vec![],
            directors: vec![],
            writers: vec![],
            producers: vec![],
            countries: vec![],
        }
    }

    #[test]
    fn parse_guid_recognises_known_schemes() {
        assert_eq!(parse_guid("imdb://tt0095016"), Some((ExternalNs::Imdb, "tt0095016".into())));
        assert_eq!(parse_guid("tmdb://562"), Some((ExternalNs::Tmdb, "562".into())));
        assert_eq!(parse_guid("tvdb://12345"), Some((ExternalNs::Tvdb, "12345".into())));
        assert_eq!(parse_guid("plex://movie/abc"), Some((ExternalNs::Plex, "movie/abc".into())));
        assert_eq!(parse_guid("nonsense://x"), None);
        assert_eq!(parse_guid("imdb://"), None);
        assert_eq!(parse_guid("garbage"), None);
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
        assert_eq!(cat.all_entry_ids().unwrap(), vec!["imdb:tt0095016".to_string()]);

        let e = cat.entry("imdb:tt0095016").unwrap().unwrap();
        assert_eq!(e.kind, "movie");
        assert_eq!(e.year, Some(1988));
        assert_eq!(e.duration_ms, Some(7_920_000));
        assert_eq!(cat.tags_for("imdb:tt0095016", TagNs::Genre).unwrap(), vec!["Action".to_string()]);
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
            &[(std::path::PathBuf::from("/mnt/media/movies/Die Hard.mkv"), Some(120.0))],
            &["/mnt/media".into(), "/data/media".into()],
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
        let stats = ingest_items(&cat, &[item], &["/mnt/media".into(), "/data/media".into()]).unwrap();

        // One entry (the inherited fs: id), two provenance rows, imdb reachable,
        // and Plex upgraded the sparse title.
        assert_eq!(stats.inherited, 1);
        assert_eq!(cat.all_entry_ids().unwrap(), vec![fs_id.clone()]);
        let sources = cat.sources_for(&fs_id).unwrap();
        assert_eq!(sources.len(), 2);
        assert!(sources.iter().any(|s| s.source == Source::Plex));
        assert!(sources.iter().any(|s| s.source == Source::LocalFs));
        assert_eq!(cat.resolve_query("item.title == \"A Movie\"").unwrap(), vec![fs_id]);
    }

    #[test]
    fn to_plex_item_translates_path_and_extracts_guids() {
        let json = r#"{
            "ratingKey": "12345",
            "type": "movie",
            "title": "Die Hard",
            "year": 1988,
            "duration": 7920000,
            "Guid": [{"id": "imdb://tt0095016"}, {"id": "tmdb://562"}],
            "Genre": [{"tag": "Action"}],
            "Media": [{"Part": [{"file": "/media/Movies/Die Hard.mkv"}]}]
        }"#;
        let m: PlexMetadata = serde_json::from_str(json).unwrap();
        let item = to_plex_item(&m, |p| p.replace("/media", "/data/media")).unwrap();
        assert_eq!(item.playback_path, "/data/media/Movies/Die Hard.mkv");
        assert_eq!(
            item.external_ids,
            vec![(ExternalNs::Imdb, "tt0095016".into()), (ExternalNs::Tmdb, "562".into())]
        );
        assert_eq!(item.rating_key, "12345");
        assert_eq!(item.duration_ms, Some(7_920_000));
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
        let item = to_plex_item(&m, |p| p.to_string()).unwrap();
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
        let item = to_plex_item(&m, |p| p.to_string()).unwrap();
        assert_eq!(item.edition, None);
        assert_eq!(item.studio, None);
    }

    #[test]
    fn ingest_writes_edition_and_studio_queryable() {
        let cat = Catalog::open_in_memory().unwrap();
        let mut item = movie("plex-e", "/data/media/m/x.mkv", &[(ExternalNs::Imdb, "tt-e")]);
        item.edition = Some("Extended Edition".into());
        item.studio = Some("New Line Cinema".into());
        ingest_items(&cat, &[item], &["/data/media".into()]).unwrap();

        let e = cat.entry("imdb:tt-e").unwrap().unwrap();
        assert_eq!(e.edition.as_deref(), Some("Extended Edition"));
        assert_eq!(e.studio.as_deref(), Some("New Line Cinema"));
        // Both promoted columns are queryable via the CEL→SQL surface.
        assert_eq!(
            cat.resolve_query(r#"item.studio == "New Line Cinema""#).unwrap(),
            vec!["imdb:tt-e".to_string()]
        );
        assert_eq!(
            cat.resolve_query(r#"item.edition == "Extended Edition""#).unwrap(),
            vec!["imdb:tt-e".to_string()]
        );
    }

    #[test]
    fn to_plex_item_promotes_crew_cast_and_label_tags() {
        let json = r#"{
            "ratingKey": "1",
            "type": "movie",
            "title": "Die Hard",
            "Role": [{"tag": "Bruce Willis"}, {"tag": "Alan Rickman"}],
            "Director": [{"tag": "John McTiernan"}],
            "Writer": [{"tag": "Jeb Stuart"}],
            "Producer": [{"tag": "Joel Silver"}],
            "Country": [{"tag": "United States"}],
            "Label": [{"tag": "Christmas"}],
            "Media": [{"Part": [{"file": "/media/x.mkv"}]}]
        }"#;
        let m: PlexMetadata = serde_json::from_str(json).unwrap();
        let item = to_plex_item(&m, |p| p.to_string()).unwrap();
        assert_eq!(item.cast, vec!["Bruce Willis", "Alan Rickman"]);
        assert_eq!(item.directors, vec!["John McTiernan"]);
        assert_eq!(item.writers, vec!["Jeb Stuart"]);
        assert_eq!(item.producers, vec!["Joel Silver"]);
        assert_eq!(item.countries, vec!["United States"]);
        assert_eq!(item.labels, vec!["Christmas"]);
    }

    #[test]
    fn ingest_writes_crew_cast_and_label_tags_queryable() {
        let cat = Catalog::open_in_memory().unwrap();
        let mut item = movie("plex-t", "/data/media/m/x.mkv", &[(ExternalNs::Imdb, "tt-t")]);
        item.cast = vec!["Jackie Chan".into()];
        item.directors = vec!["Stanley Tong".into()];
        item.labels = vec!["Kung Fu".into()];
        ingest_items(&cat, &[item], &["/data/media".into()]).unwrap();

        assert_eq!(cat.tags_for("imdb:tt-t", TagNs::Cast).unwrap(), vec!["Jackie Chan".to_string()]);
        assert_eq!(cat.tags_for("imdb:tt-t", TagNs::Director).unwrap(), vec!["Stanley Tong".to_string()]);
        assert_eq!(cat.tags_for("imdb:tt-t", TagNs::Label).unwrap(), vec!["Kung Fu".to_string()]);
        // Reachable through the CEL→SQL surface: dedicated fields and generic `tags`.
        assert_eq!(
            cat.resolve_query(r#"item.cast.contains("Jackie Chan")"#).unwrap(),
            vec!["imdb:tt-t".to_string()]
        );
        assert_eq!(
            cat.resolve_query(r#"item.labels.contains("Kung Fu")"#).unwrap(),
            vec!["imdb:tt-t".to_string()]
        );
    }

    #[test]
    fn item_without_a_file_part_is_skipped() {
        let json = r#"{"ratingKey": "1", "type": "movie", "title": "x", "Media": []}"#;
        let m: PlexMetadata = serde_json::from_str(json).unwrap();
        assert!(to_plex_item(&m, |p| p.to_string()).is_none());
    }

    #[test]
    fn rescans_are_idempotent() {
        let cat = Catalog::open_in_memory().unwrap();
        let item = movie("plex-1", "/data/media/m/x.mkv", &[(ExternalNs::Imdb, "tt1")]);
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
            movie("plex-4k", "/data/media/movies/DieHard-4k.mkv", &[(ExternalNs::Imdb, "tt0095016")]),
            movie("plex-hd", "/data/media/movies/DieHard-1080.mkv", &[(ExternalNs::Imdb, "tt0095016")]),
        ];
        ingest_items(&cat, &items, &["/data/media".into()]).unwrap();
        assert_eq!(cat.all_entry_ids().unwrap(), vec!["imdb:tt0095016".to_string()]);
        assert_eq!(cat.sources_for("imdb:tt0095016").unwrap().len(), 2);
        assert_eq!(
            cat.entry_id_for_external_id(ExternalNs::Imdb, "tt0095016").unwrap(),
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
        assert_eq!(client.translate("/media/Movies/A.mkv"), "/data/media/Movies/A.mkv");
        assert_eq!(client.translate("/media"), "/data/media");
        // Sibling prefix must NOT be remapped.
        assert_eq!(client.translate("/mediabackup/x.mkv"), "/mediabackup/x.mkv");
        assert_eq!(client.translate("/other/path"), "/other/path");
    }
}

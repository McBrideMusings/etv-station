use std::collections::{HashMap, HashSet};
use std::env;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

use crate::normalize::NormalizedItem;

const HTTP_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Error)]
pub enum PlexError {
    #[error("missing env var: {0}")]
    MissingEnv(&'static str),
    #[error("http: {0}")]
    Http(String),
    #[error("parse: {0}")]
    Parse(String),
}

struct PlexClient {
    base_url: String,
    token: String,
    path_from: String,
    path_to: String,
    agent: ureq::Agent,
}

impl PlexClient {
    fn from_env() -> Result<Self, PlexError> {
        let base_url = env::var("PLEX_URL").map_err(|_| PlexError::MissingEnv("PLEX_URL"))?;
        let token = env::var("PLEX_TOKEN").map_err(|_| PlexError::MissingEnv("PLEX_TOKEN"))?;
        let path_from = env::var("MEDIA_PATH_FROM").unwrap_or_default();
        let path_to = env::var("MEDIA_PATH_TO").unwrap_or_default();
        let agent = ureq::AgentBuilder::new().timeout(HTTP_TIMEOUT).build();
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            path_from,
            path_to,
            agent,
        })
    }

    fn translate(&self, p: &str) -> String {
        if !self.path_from.is_empty() && p.starts_with(&self.path_from) {
            format!("{}{}", self.path_to, &p[self.path_from.len()..])
        } else {
            p.to_string()
        }
    }

    fn get<T: for<'de> Deserialize<'de>>(&self, endpoint: &str) -> Result<T, PlexError> {
        self.get_with_query(endpoint, &[])
    }

    fn get_with_query<T: for<'de> Deserialize<'de>>(
        &self,
        endpoint: &str,
        params: &[(&str, &str)],
    ) -> Result<T, PlexError> {
        let url = format!("{}{}", self.base_url, endpoint);
        let mut req = self
            .agent
            .get(&url)
            .set("X-Plex-Token", &self.token)
            .set("Accept", "application/json");
        for (k, v) in params {
            req = req.query(k, v);
        }
        let response = req.call().map_err(|e| PlexError::Http(e.to_string()))?;
        response
            .into_json()
            .map_err(|e| PlexError::Parse(e.to_string()))
    }
}

#[derive(Debug, Deserialize)]
struct MediaContainerResp {
    #[serde(rename = "MediaContainer")]
    media_container: MediaContainer,
}

#[derive(Debug, Deserialize, Default)]
struct MediaContainer {
    #[serde(default, rename = "Metadata")]
    metadata: Vec<PlexMetadata>,
    #[serde(default, rename = "librarySectionTitle")]
    library_section_title: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlexMetadata {
    #[serde(default)]
    rating_key: Option<String>,
    #[serde(default)]
    grandparent_rating_key: Option<String>,
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
    library_section_title: Option<String>,
    #[serde(default, rename = "Genre")]
    genre: Vec<TaggedField>,
    #[serde(default, rename = "Collection")]
    collection: Vec<TaggedField>,
    #[serde(default, rename = "Media")]
    media: Vec<PlexMedia>,
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
    #[serde(default)]
    title: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

/// Derive semantic type from Plex section name, falling back to Plex's
/// own type field when the section name doesn't map to anything specific.
pub fn type_from_section(section_name: &str, plex_type: &str) -> String {
    match section_name.to_lowercase().as_str() {
        "concerts" | "concert" => "concert",
        "power hours" | "power_hours" | "powerhours" => "power_hour",
        "music videos" | "music_videos" | "musicvideos" => "music_video",
        "home movies" | "home_movies" => "home_movie",
        _ => match plex_type {
            "episode" => "episode",
            "movie" => "movie",
            "show" => "show",
            other if !other.is_empty() => other,
            _ => "video",
        },
    }
    .into()
}

fn to_normalized(
    client: &PlexClient,
    m: &PlexMetadata,
    library_fallback: &str,
    section_name: &str,
    show_collections: Option<&HashMap<String, Vec<String>>>,
) -> Option<NormalizedItem> {
    let path = m.media.first()?.part.first()?.file.as_deref()?;
    let title = m
        .grandparent_title
        .clone()
        .or_else(|| m.title.clone())
        .unwrap_or_default();
    let sub_title = if m.grandparent_title.is_some() {
        m.title.clone()
    } else {
        None
    };
    let library = m
        .library_section_title
        .clone()
        .unwrap_or_else(|| library_fallback.to_string());
    let categories = tags_of(&m.genre);
    let mut collections = tags_of(&m.collection);
    if let Some(map) = show_collections
        && let Some(grandparent_key) = m.grandparent_rating_key.as_deref()
        && let Some(extra) = map.get(grandparent_key)
    {
        for c in extra {
            if !collections.contains(c) {
                collections.push(c.clone());
            }
        }
    }
    let media_type = type_from_section(section_name, m.kind.as_deref().unwrap_or(""));
    Some(NormalizedItem {
        sources: vec!["plex".into()],
        media_type,
        library,
        title,
        sub_title,
        season: m.parent_index,
        episode: m.index,
        year: m.year,
        categories,
        collections,
        franchise: None,
        content_rating: m.content_rating.clone(),
        runtime_secs: m.duration.map(|ms| ms as f64 / 1000.0),
        path: client.translate(path),
        rating_key: m.rating_key.clone(),
    })
}

/// Resolve a free-form Plex source value into a deduped item pool.
///
/// Numeric value → treated as a Plex ratingKey (show, collection, or single
/// item). Non-numeric value → exact case-insensitive match against library
/// section names and show titles; matches are unioned and deduped by
/// ratingKey. An unresolved value returns an empty Vec rather than an error.
pub fn resolve(value: &str) -> Result<Vec<NormalizedItem>, PlexError> {
    let client = PlexClient::from_env()?;

    if value.is_empty() {
        return resolve_all_sections(&client);
    }

    if value.chars().all(|c| c.is_ascii_digit()) {
        return resolve_rating_key(&client, value);
    }

    let mut items: Vec<NormalizedItem> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let sections: SectionListResp = client.get("/library/sections")?;

    for section in &sections.media_container.directory {
        let Some(title) = section.title.as_deref() else {
            continue;
        };
        if !title.eq_ignore_ascii_case(value) {
            continue;
        }
        let Some(id) = section.key.as_deref() else {
            continue;
        };
        let type_filter = match section.kind.as_deref() {
            Some("show") => Some("4"),
            Some("movie") => Some("1"),
            _ => None,
        };
        for it in fetch_section_items(&client, id, type_filter)? {
            push_unique(&mut items, &mut seen, it);
        }
    }

    for section in &sections.media_container.directory {
        if section.kind.as_deref() != Some("show") {
            continue;
        }
        let Some(id) = section.key.as_deref() else {
            continue;
        };
        let endpoint = format!("/library/sections/{id}/all");
        let resp: MediaContainerResp = client.get_with_query(&endpoint, &[("type", "2")])?;
        for show in &resp.media_container.metadata {
            let Some(show_title) = show.title.as_deref() else {
                continue;
            };
            if !show_title.eq_ignore_ascii_case(value) {
                continue;
            }
            let Some(rk) = show.rating_key.as_deref() else {
                continue;
            };
            for ep in fetch_show_episodes_with_client(&client, rk)? {
                push_unique(&mut items, &mut seen, ep);
            }
        }
    }

    Ok(items)
}

fn resolve_all_sections(client: &PlexClient) -> Result<Vec<NormalizedItem>, PlexError> {
    let sections: SectionListResp = client.get("/library/sections")?;
    let mut items: Vec<NormalizedItem> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for section in &sections.media_container.directory {
        let Some(id) = section.key.as_deref() else {
            continue;
        };
        let type_filter = match section.kind.as_deref() {
            Some("show") => Some("4"),
            Some("movie") => Some("1"),
            _ => None,
        };
        for it in fetch_section_items(client, id, type_filter)? {
            push_unique(&mut items, &mut seen, it);
        }
    }
    Ok(items)
}

fn resolve_rating_key(client: &PlexClient, key: &str) -> Result<Vec<NormalizedItem>, PlexError> {
    // Try as a show (or season) first.
    let leaves: MediaContainerResp = client.get(&format!("/library/metadata/{key}/allLeaves"))?;
    if !leaves.media_container.metadata.is_empty() {
        return fetch_show_episodes_with_client(client, key);
    }
    // Then as a collection.
    let children: Result<MediaContainerResp, _> =
        client.get(&format!("/library/collections/{key}/children"));
    if let Ok(resp) = children
        && !resp.media_container.metadata.is_empty()
    {
        let library_fallback = resp
            .media_container
            .library_section_title
            .clone()
            .unwrap_or_default();
        // Children may be shows (need expansion) or playable items (movies).
        let mut items: Vec<NormalizedItem> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for child in &resp.media_container.metadata {
            if let Some(rk) = child.rating_key.as_deref() {
                let child_leaves: MediaContainerResp =
                    client.get(&format!("/library/metadata/{rk}/allLeaves"))?;
                if !child_leaves.media_container.metadata.is_empty() {
                    for ep in fetch_show_episodes_with_client(client, rk)? {
                        push_unique(&mut items, &mut seen, ep);
                    }
                    continue;
                }
            }
            if let Some(it) =
                to_normalized(client, child, &library_fallback, &library_fallback, None)
            {
                push_unique(&mut items, &mut seen, it);
            }
        }
        return Ok(items);
    }
    // Fall back to a single-item lookup (movie / episode by direct key).
    let single: MediaContainerResp = client.get(&format!("/library/metadata/{key}"))?;
    let library_fallback = single
        .media_container
        .library_section_title
        .clone()
        .unwrap_or_default();
    Ok(single
        .media_container
        .metadata
        .iter()
        .filter_map(|m| to_normalized(client, m, &library_fallback, &library_fallback, None))
        .collect())
}

fn push_unique(items: &mut Vec<NormalizedItem>, seen: &mut HashSet<String>, item: NormalizedItem) {
    match &item.rating_key {
        Some(rk) => {
            if seen.insert(rk.clone()) {
                items.push(item);
            }
        }
        None => items.push(item),
    }
}

fn fetch_section_items(
    client: &PlexClient,
    section_id: &str,
    type_filter: Option<&str>,
) -> Result<Vec<NormalizedItem>, PlexError> {
    let show_collections = if type_filter == Some("4") {
        Some(fetch_show_collections_map(client, section_id)?)
    } else {
        None
    };

    let endpoint = format!("/library/sections/{section_id}/all");
    let resp: MediaContainerResp = match type_filter {
        Some(t) => client.get_with_query(&endpoint, &[("type", t)])?,
        None => client.get(&endpoint)?,
    };
    let library_fallback = resp
        .media_container
        .library_section_title
        .clone()
        .unwrap_or_default();
    Ok(resp
        .media_container
        .metadata
        .iter()
        .filter_map(|m| {
            to_normalized(
                client,
                m,
                &library_fallback,
                &library_fallback,
                show_collections.as_ref(),
            )
        })
        .collect())
}

fn tags_of(fields: &[TaggedField]) -> Vec<String> {
    fields.iter().filter_map(|t| t.tag.clone()).collect()
}

fn fetch_show_collections_map(
    client: &PlexClient,
    section_id: &str,
) -> Result<HashMap<String, Vec<String>>, PlexError> {
    let endpoint = format!("/library/sections/{section_id}/all");
    let resp: MediaContainerResp = client.get_with_query(&endpoint, &[("type", "2")])?;
    let mut map = HashMap::new();
    for show in &resp.media_container.metadata {
        let Some(rating_key) = show.rating_key.clone() else {
            continue;
        };
        let collections = tags_of(&show.collection);
        if !collections.is_empty() {
            map.insert(rating_key, collections);
        }
    }
    Ok(map)
}

fn fetch_show_episodes_with_client(
    client: &PlexClient,
    show_rating_key: &str,
) -> Result<Vec<NormalizedItem>, PlexError> {
    let show_meta: MediaContainerResp =
        client.get(&format!("/library/metadata/{show_rating_key}"))?;
    let mut show_collections_map: HashMap<String, Vec<String>> = HashMap::new();
    if let Some(show) = show_meta.media_container.metadata.first() {
        let collections = tags_of(&show.collection);
        if !collections.is_empty() {
            show_collections_map.insert(show_rating_key.to_string(), collections);
        }
    }

    let resp: MediaContainerResp =
        client.get(&format!("/library/metadata/{show_rating_key}/allLeaves"))?;
    let library_fallback = resp
        .media_container
        .library_section_title
        .clone()
        .unwrap_or_default();
    Ok(resp
        .media_container
        .metadata
        .iter()
        .filter_map(|m| {
            to_normalized(
                client,
                m,
                &library_fallback,
                &library_fallback,
                Some(&show_collections_map),
            )
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_translation() {
        let client = PlexClient {
            base_url: "http://x".into(),
            token: "t".into(),
            path_from: "/media".into(),
            path_to: "/data/media".into(),
            agent: ureq::Agent::new(),
        };
        assert_eq!(
            client.translate("/media/Movies/A.mkv"),
            "/data/media/Movies/A.mkv"
        );
        assert_eq!(client.translate("/other/path"), "/other/path");
    }

    #[test]
    fn path_translation_no_prefix() {
        let client = PlexClient {
            base_url: "http://x".into(),
            token: "t".into(),
            path_from: String::new(),
            path_to: String::new(),
            agent: ureq::Agent::new(),
        };
        assert_eq!(client.translate("/abc"), "/abc");
    }
}

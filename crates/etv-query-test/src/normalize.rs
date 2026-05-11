use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedItem {
    /// All catalog sources that have a record for this file.
    /// Values: "plex", "fs". Always at least one entry.
    /// `source` (singular) = sources[0] = the primary/richest source.
    pub sources: Vec<String>,
    /// Semantic type: "episode", "movie", "concert", "power_hour",
    /// "music_video", "bumper", "commercial", "ident", "video", …
    pub media_type: String,
    pub library: String,
    pub title: String,
    pub sub_title: Option<String>,
    pub season: Option<i64>,
    pub episode: Option<i64>,
    pub year: Option<i64>,
    pub categories: Vec<String>,
    pub collections: Vec<String>,
    pub franchise: Option<String>,
    pub content_rating: Option<String>,
    pub runtime_secs: Option<f64>,
    pub path: String,
    pub rating_key: Option<String>,
}

impl NormalizedItem {
    /// Primary source — the richest contributor. Plex wins over FS when both
    /// are present.
    pub fn primary_source(&self) -> &str {
        self.sources.first().map(String::as_str).unwrap_or("")
    }
}

pub fn sort_by_keys(items: &mut [NormalizedItem], keys: &str) {
    let keys: Vec<&str> = keys.split(',').map(str::trim).collect();
    items.sort_by(|a, b| {
        for key in &keys {
            let ord = match *key {
                "title" => a.title.cmp(&b.title),
                "season" => a.season.cmp(&b.season),
                "episode" => a.episode.cmp(&b.episode),
                "year" => a.year.cmp(&b.year),
                "library" => a.library.cmp(&b.library),
                "runtime_secs" => a
                    .runtime_secs
                    .partial_cmp(&b.runtime_secs)
                    .unwrap_or(std::cmp::Ordering::Equal),
                _ => std::cmp::Ordering::Equal,
            };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, season: Option<i64>, episode: Option<i64>) -> NormalizedItem {
        NormalizedItem {
            sources: vec!["plex".into()],
            media_type: "episode".into(),
            library: "TV Shows".into(),
            title: title.into(),
            sub_title: None,
            season,
            episode,
            year: None,
            categories: vec![],
            collections: vec![],
            franchise: None,
            content_rating: None,
            runtime_secs: None,
            path: String::new(),
            rating_key: None,
        }
    }

    #[test]
    fn sort_by_season_episode() {
        let mut items = vec![
            item("a", Some(2), Some(1)),
            item("a", Some(1), Some(3)),
            item("a", Some(1), Some(1)),
        ];
        sort_by_keys(&mut items, "season,episode");
        let ord: Vec<_> = items
            .iter()
            .map(|i| (i.season.unwrap(), i.episode.unwrap()))
            .collect();
        assert_eq!(ord, vec![(1, 1), (1, 3), (2, 1)]);
    }

    #[test]
    fn round_trip_json() {
        let i = item("x", Some(1), Some(2));
        let s = serde_json::to_string(&i).unwrap();
        let back: NormalizedItem = serde_json::from_str(&s).unwrap();
        assert_eq!(back.title, "x");
        assert_eq!(back.season, Some(1));
    }
}

//! Chronologies (#164): an in-universe watch order across several shows and
//! films, declared once in a file outside any channel and referenced by name
//! from a block's `kind: chronology` entry.
//!
//! A chronology cannot be derived from anything the catalog knows (Dragon Ball
//! films slot at arbitrary points inside seasons of other series), so it is
//! authored: an ordered list of item references, each with an explicit
//! `position`. The load checks everything it can without the catalog — the
//! shape of each reference and that no two entries claim one position. Whether
//! each reference names a catalog item can only be known at resolve time, where
//! a miss fails naming the entry.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// A chronology file: one named, ordered list of item references.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChronologyFile {
    pub name: String,
    pub entries: Vec<ChronologyRef>,
}

/// One item reference. Either an episode of a series (`show`, `season`,
/// `episode`) or a standalone film (`title`, optionally `year`).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChronologyRef {
    /// Where this item sits in the order. Unique within the chronology; gaps
    /// are fine, so a film can be slotted in later without renumbering.
    pub position: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub season: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<u32>,
}

impl ChronologyRef {
    /// How the entry reads in an error: `Dragon Ball S1E3` or `Title (1986)`.
    pub fn describe(&self) -> String {
        match (&self.show, &self.title) {
            (Some(show), _) => format!(
                "{show} S{}E{}",
                self.season.unwrap_or_default(),
                self.episode.unwrap_or_default()
            ),
            (None, Some(title)) => match self.year {
                Some(year) => format!("{title} ({year})"),
                None => title.clone(),
            },
            (None, None) => "(empty reference)".to_string(),
        }
    }

    /// The CEL query that selects exactly the item this reference names.
    pub fn query(&self) -> String {
        match &self.show {
            Some(show) => format!(
                "item.show == {} && item.season == {} && item.episode == {}",
                cel_string(show),
                self.season.unwrap_or_default(),
                self.episode.unwrap_or_default()
            ),
            None => {
                let mut q = format!(
                    "item.type == \"movie\" && item.title == {}",
                    cel_string(self.title.as_deref().unwrap_or_default())
                );
                if let Some(year) = self.year {
                    q.push_str(&format!(" && item.year == {year}"));
                }
                q
            }
        }
    }

    fn check_shape(&self) -> Result<(), String> {
        let episode_fields = [
            self.show.is_some(),
            self.season.is_some(),
            self.episode.is_some(),
        ];
        let is_episode = episode_fields.iter().any(|&b| b);
        let is_film = self.title.is_some() || self.year.is_some();
        match (is_episode, is_film) {
            (true, true) => Err(
                "mixes episode fields (show/season/episode) with film fields (title/year); \
                 use one form"
                    .into(),
            ),
            (false, false) => Err(
                "names nothing: give show + season + episode for an episode, or title for a film"
                    .into(),
            ),
            (true, false) if !episode_fields.iter().all(|&b| b) => {
                Err("an episode reference needs all of show, season and episode".into())
            }
            (false, true) if self.title.is_none() => Err("a film reference needs a title".into()),
            _ => Ok(()),
        }
    }
}

/// A CEL double-quoted string literal for `s`.
fn cel_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A validated chronology, entries sorted by position.
#[derive(Debug, Serialize)]
pub struct Chronology {
    pub name: String,
    pub entries: Vec<ChronologyRef>,
}

/// Every declared chronology, keyed by name.
#[derive(Debug, Default)]
pub struct Chronologies {
    by_name: BTreeMap<String, Arc<Chronology>>,
}

impl Chronologies {
    /// Build from `(file, parsed)` pairs in load order. A name declared twice,
    /// a malformed reference, or two entries claiming one position fails
    /// naming the file and both entries.
    pub fn from_files(files: Vec<(PathBuf, ChronologyFile)>) -> Result<Self, String> {
        let mut by_name: BTreeMap<String, Arc<Chronology>> = BTreeMap::new();
        let mut at: BTreeMap<String, PathBuf> = BTreeMap::new();
        for (path, file) in files {
            let here = path.display();
            if file.name.trim().is_empty() {
                return Err(format!("chronology in {here}: name is empty"));
            }
            if file.entries.is_empty() {
                return Err(format!(
                    "chronology {:?} in {here}: entries is empty",
                    file.name
                ));
            }
            if let Some(first) = at.get(&file.name) {
                return Err(format!(
                    "chronology {:?} is declared twice: in {} and in {here}",
                    file.name,
                    first.display()
                ));
            }
            let mut claimed: BTreeMap<u32, usize> = BTreeMap::new();
            for (idx, entry) in file.entries.iter().enumerate() {
                entry.check_shape().map_err(|e| {
                    format!(
                        "chronology {:?} in {here}: entry #{} (position {}) {e}",
                        file.name,
                        idx + 1,
                        entry.position
                    )
                })?;
                if let Some(&other) = claimed.get(&entry.position) {
                    return Err(format!(
                        "chronology {:?} in {here}: position {} is claimed twice, by entry #{} \
                         ({}) and entry #{} ({})",
                        file.name,
                        entry.position,
                        other + 1,
                        file.entries[other].describe(),
                        idx + 1,
                        entry.describe()
                    ));
                }
                claimed.insert(entry.position, idx);
            }
            let mut entries = file.entries;
            entries.sort_by_key(|e| e.position);
            at.insert(file.name.clone(), path);
            by_name.insert(
                file.name.clone(),
                Arc::new(Chronology {
                    name: file.name,
                    entries,
                }),
            );
        }
        Ok(Self { by_name })
    }

    /// The chronology called `name`; an unknown name lists the ones that exist.
    pub fn get(&self, name: &str) -> Result<Arc<Chronology>, String> {
        self.by_name.get(name).cloned().ok_or_else(|| {
            if self.by_name.is_empty() {
                format!("unknown chronology {name:?}; no chronologies are defined")
            } else {
                let names: Vec<&str> = self.by_name.keys().map(String::as_str).collect();
                format!(
                    "unknown chronology {name:?}; defined chronologies: {}",
                    names.join(", ")
                )
            }
        })
    }
}

/// Read one chronology file (YAML or TOML by extension, like every config file).
pub fn read_chronology_file(path: &Path) -> Result<ChronologyFile, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let is_yaml = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"));
    if is_yaml {
        serde_norway::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    } else {
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(position: u32, show: &str, season: u32, episode: u32) -> ChronologyRef {
        ChronologyRef {
            position,
            show: Some(show.into()),
            season: Some(season),
            episode: Some(episode),
            title: None,
            year: None,
        }
    }

    fn film(position: u32, title: &str, year: Option<u32>) -> ChronologyRef {
        ChronologyRef {
            position,
            show: None,
            season: None,
            episode: None,
            title: Some(title.into()),
            year,
        }
    }

    fn one(entries: Vec<ChronologyRef>) -> Result<Chronologies, String> {
        Chronologies::from_files(vec![(
            PathBuf::from("c.yaml"),
            ChronologyFile {
                name: "db".into(),
                entries,
            },
        )])
    }

    #[test]
    fn entries_sort_by_position_not_authored_order() {
        let c = one(vec![
            ep(20, "DB", 1, 2),
            film(5, "Movie", None),
            ep(10, "DB", 1, 1),
        ])
        .unwrap()
        .get("db")
        .unwrap();
        let positions: Vec<u32> = c.entries.iter().map(|e| e.position).collect();
        assert_eq!(positions, [5, 10, 20]);
    }

    #[test]
    fn two_entries_on_one_position_fail_naming_both() {
        let err = one(vec![ep(1, "DB", 1, 1), film(1, "Blood Rubies", Some(1986))]).unwrap_err();
        assert!(err.contains("position 1 is claimed twice"), "{err}");
        assert!(err.contains("DB S1E1"), "{err}");
        assert!(err.contains("Blood Rubies (1986)"), "{err}");
    }

    #[test]
    fn a_half_written_reference_fails_the_load() {
        let mut half = ep(1, "DB", 1, 1);
        half.episode = None;
        let err = one(vec![half]).unwrap_err();
        assert!(
            err.contains("needs all of show, season and episode"),
            "{err}"
        );
    }

    #[test]
    fn episode_and_film_fields_cannot_mix() {
        let mut both = ep(1, "DB", 1, 1);
        both.title = Some("x".into());
        assert!(one(vec![both]).unwrap_err().contains("use one form"));
    }

    #[test]
    fn a_name_declared_twice_fails_naming_both_files() {
        let file = |entries| ChronologyFile {
            name: "db".into(),
            entries,
        };
        let err = Chronologies::from_files(vec![
            (PathBuf::from("a.yaml"), file(vec![film(1, "A", None)])),
            (PathBuf::from("b.yaml"), file(vec![film(1, "B", None)])),
        ])
        .unwrap_err();
        assert!(err.contains("a.yaml") && err.contains("b.yaml"), "{err}");
    }

    #[test]
    fn an_unknown_name_lists_the_defined_ones() {
        let err = one(vec![film(1, "A", None)])
            .unwrap()
            .get("nope")
            .unwrap_err();
        assert!(err.contains("defined chronologies: db"), "{err}");
    }

    #[test]
    fn queries_escape_quotes_and_pin_a_film_to_movies() {
        assert_eq!(
            ep(1, "Say \"hi\"", 2, 3).query(),
            r#"item.show == "Say \"hi\"" && item.season == 2 && item.episode == 3"#
        );
        assert_eq!(
            film(1, "M", Some(1986)).query(),
            r#"item.type == "movie" && item.title == "M" && item.year == 1986"#
        );
    }
}

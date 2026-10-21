//! `etv-station import-arcs` (#171): write a show's arc ranges from the
//! published anibridge/anibridge-mappings data instead of typing them.
//!
//! The source supplies boundaries, not names. Its data is keyed by an upstream
//! id (`anilist:97940`), and under it each season of a tvdb or tmdb show lists
//! the episode range it covers:
//!
//! ```json
//! "anilist:21": { "tmdb_show:37854:s1": { "1-61": "1-61" },
//!                 "tmdb_show:37854:s2": { "62-77": "62-77" } }
//! ```
//!
//! The inner map's key is the range in the upstream entry's own numbering,
//! which runs across seasons (One Piece's season 2 is `"9-30"`); its value is
//! the range inside the season (`"1-22"`). The arcs loader wants absolute
//! numbering, so the key is the one written. Season 0 (Specials) is left out.
//!
//! Each season becomes one arc with the placeholder name `Season N`, in the
//! shape [`crate::config`]'s arcs loader reads. The result is a file for a
//! person to rename, edit and commit; nothing here runs at generation time.
//!
//! The data is written through as published. Overlapping ranges or an interior
//! gap make the arcs loader reject the file, and the import says so rather than
//! repairing a disagreement between the source and the library.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::config::{ArcFile, Arcs};

/// Where the mappings are fetched from when no local file is given.
pub const DEFAULT_SOURCE_URL: &str =
    "https://github.com/anibridge/anibridge-mappings/releases/download/v3/mappings.min.json";

/// The provider whose seasons become arcs. Plex's own season numbering follows
/// TheTVDB first, so it is the one preferred when the entry carries both.
const PROVIDERS: [&str; 2] = ["tvdb_show", "tmdb_show"];

/// One imported range of a season, as the source wrote it.
#[derive(Debug, PartialEq, Eq)]
struct SeasonRange {
    season: u32,
    /// `"62-77"`, `"7"` or the open-ended `"1089-"`.
    episodes: String,
    first: i64,
}

/// A finished import: the file text and what the loader thinks of it.
#[derive(Debug)]
pub struct Import {
    pub text: String,
    /// The error the arcs loader gives the written file, when it gives one.
    pub load_error: Option<String>,
}

/// Build the arcs file for `show` from `mappings` (the parsed source document)
/// under upstream `key`. `source` names where the document came from and goes
/// into the file's header.
pub fn import(show: &str, key: &str, source: &str, mappings: &Value) -> Result<Import, String> {
    let meta = mappings.get("$meta");
    let meta_str = |field: &str| {
        meta.and_then(|m| m.get(field))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
    };
    let entry = mappings
        .get(key)
        .and_then(Value::as_object)
        .ok_or_else(|| format!("show {show:?}: {source} has no entry {key:?}"))?;

    let (provider, provider_id, ranges) = seasons_of(entry, show, key, source)?;

    let mut parts: BTreeMap<u32, usize> = BTreeMap::new();
    for r in &ranges {
        *parts.entry(r.season).or_default() += 1;
    }
    let mut seen: BTreeMap<u32, usize> = BTreeMap::new();
    let mut body = String::new();
    for r in &ranges {
        let n = seen.entry(r.season).or_default();
        *n += 1;
        let name = if parts[&r.season] > 1 {
            format!("Season {} part {n}", r.season)
        } else {
            format!("Season {}", r.season)
        };
        body.push_str(&format!(
            "  - {{ name: {}, episodes: {} }}\n",
            quote(&name),
            quote(&r.episodes)
        ));
    }

    let mut text = format!(
        "# Imported by `etv-station import-arcs` (#171). Review before committing.\n\
         # source:       anibridge/anibridge-mappings, {source}\n\
         # schema:       {} generated on {}\n\
         # upstream key: {key} -> {provider}:{provider_id}\n\
         # The names below are placeholders derived from the upstream season\n\
         # numbers. The source has no arc names: replace each one.\n\
         show: {}\n\
         arcs:\n",
        meta_str("schema_version"),
        meta_str("generated_on"),
        quote(show),
    );
    text.push_str(&body);

    let load_error = check_load(&text);
    if let Some(err) = &load_error {
        let note = format!(
            "# WARNING: the import wrote the source's ranges as published and the arcs\n\
             # loader rejects them: {err}\n"
        );
        let at = text.find("show: ").unwrap_or(0);
        text.insert_str(at, &note);
    }
    Ok(Import { text, load_error })
}

/// The ranges of every season of one provider's show under `entry`, season 0
/// (Specials) left out, sorted by first episode.
fn seasons_of(
    entry: &serde_json::Map<String, Value>,
    show: &str,
    key: &str,
    source: &str,
) -> Result<(&'static str, String, Vec<SeasonRange>), String> {
    for provider in PROVIDERS {
        let mut ids: BTreeMap<String, Vec<SeasonRange>> = BTreeMap::new();
        for (target, map) in entry {
            let Some((id, season)) = split_target(target, provider) else {
                continue;
            };
            if season == 0 {
                continue;
            }
            let Some(map) = map.as_object() else {
                continue;
            };
            for range in map.keys() {
                let first = range_start(range).ok_or_else(|| {
                    format!("show {show:?}: {key} lists {target} with a range {range:?} this importer cannot read")
                })?;
                ids.entry(id.to_string()).or_default().push(SeasonRange {
                    season,
                    episodes: range.clone(),
                    first,
                });
            }
        }
        match ids.len() {
            0 => continue,
            1 => {
                let (id, mut ranges) = ids.into_iter().next().expect("one id");
                ranges.sort_by_key(|r| (r.first, r.season));
                return Ok((provider, id, ranges));
            }
            _ => {
                let names: Vec<_> = ids.keys().map(|i| format!("{provider}:{i}")).collect();
                return Err(format!(
                    "show {show:?}: {key} in {source} maps to several {provider} shows ({}); \
                     one arcs file cannot cover them",
                    names.join(", ")
                ));
            }
        }
    }
    Err(format!(
        "show {show:?}: {key} in {source} has no tvdb_show or tmdb_show seasons to cut arcs from"
    ))
}

/// `tvdb_show:331753:s2` with provider `tvdb_show` is `("331753", 2)`.
fn split_target<'a>(target: &'a str, provider: &str) -> Option<(&'a str, u32)> {
    let rest = target.strip_prefix(provider)?.strip_prefix(':')?;
    let (id, season) = rest.split_once(':')?;
    let season = season.strip_prefix('s')?.parse().ok()?;
    Some((id, season))
}

/// The first episode of `"62-77"`, `"7"` or `"1089-"`; `None` for anything
/// else, such as a compound `"1-5,8"`.
fn range_start(range: &str) -> Option<i64> {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let (first, last) = range.split_once('-').unwrap_or((range, range));
    if !digits(first) || !(last.is_empty() || digits(last)) {
        return None;
    }
    first.parse().ok()
}

/// A YAML double-quoted scalar. JSON string syntax is a subset of YAML's.
fn quote(s: &str) -> String {
    serde_json::to_string(s).expect("a string always serializes")
}

/// What the arcs loader says about the file text, `None` when it loads.
fn check_load(text: &str) -> Option<String> {
    let file: ArcFile = match serde_norway::from_str(text) {
        Ok(f) => f,
        Err(e) => return Some(e.to_string()),
    };
    Arcs::from_files(vec![(std::path::PathBuf::from("import-arcs"), file)]).err()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SRC: &str = "mappings.min.json";

    fn doc(entry: Value) -> Value {
        json!({
            "$meta": { "schema_version": "3.0.3", "generated_on": "2026-10-21T06:52:13Z" },
            "anilist:21": entry,
        })
    }

    #[test]
    fn seasons_become_placeholder_named_arcs() {
        let d = doc(json!({
            "mal:21": { "1-1168": "1-1168" },
            "tmdb_show:37854:s0": { "1": "1" },
            "tmdb_show:37854:s2": { "62-77": "62-77" },
            "tmdb_show:37854:s1": { "1-61": "1-61" },
            "tmdb_show:37854:s3": { "78-": "78-" },
        }));
        let imp = import("One Piece", "anilist:21", SRC, &d).unwrap();
        assert_eq!(imp.load_error, None, "{}", imp.text);
        let body: Vec<&str> = imp.text.lines().filter(|l| l.starts_with("  - ")).collect();
        assert_eq!(
            body,
            [
                r#"  - { name: "Season 1", episodes: "1-61" }"#,
                r#"  - { name: "Season 2", episodes: "62-77" }"#,
                r#"  - { name: "Season 3", episodes: "78-" }"#,
            ]
        );
        assert!(imp.text.contains("generated on 2026-10-21T06:52:13Z"));
        assert!(imp.text.contains("anilist:21 -> tmdb_show:37854"));
        assert!(imp.text.contains("placeholders"));
        assert!(imp.text.contains("show: \"One Piece\"\n"));
    }

    #[test]
    fn tvdb_is_preferred_over_tmdb() {
        let d = doc(json!({
            "tmdb_show:1:s1": { "1-5": "1-5" },
            "tvdb_show:2:s1": { "1-9": "1-9" },
        }));
        let imp = import("X", "anilist:21", SRC, &d).unwrap();
        assert!(imp.text.contains("tvdb_show:2"), "{}", imp.text);
        assert!(imp.text.contains(r#"episodes: "1-9""#));
    }

    #[test]
    fn the_same_data_writes_the_same_file() {
        let d = doc(json!({ "tvdb_show:2:s1": { "1-9": "1-9" } }));
        let a = import("X", "anilist:21", SRC, &d).unwrap().text;
        let b = import("X", "anilist:21", SRC, &d).unwrap().text;
        assert_eq!(a, b);
    }

    #[test]
    fn an_unknown_key_fails_naming_the_show_and_the_source() {
        let err = import("Nope", "anilist:999", SRC, &doc(json!({}))).unwrap_err();
        assert!(err.contains("\"Nope\"") && err.contains(SRC), "{err}");
    }

    #[test]
    fn an_entry_without_seasons_fails_instead_of_writing_nothing() {
        let d = doc(json!({ "mal:21": { "1-9": "1-9" } }));
        let err = import("X", "anilist:21", SRC, &d).unwrap_err();
        assert!(err.contains("no tvdb_show or tmdb_show"), "{err}");
    }

    #[test]
    fn an_overlap_is_written_through_and_flagged() {
        let d = doc(json!({
            "tvdb_show:2:s1": { "1-20": "1-20" },
            "tvdb_show:2:s2": { "18-30": "18-30" },
        }));
        let imp = import("X", "anilist:21", SRC, &d).unwrap();
        assert!(imp.load_error.as_deref().unwrap().contains("overlap"));
        assert!(imp.text.contains("# WARNING"));
        assert!(imp.text.contains(r#"episodes: "18-30""#));
    }

    #[test]
    fn an_interior_gap_is_written_through_and_flagged() {
        let d = doc(json!({
            "tvdb_show:2:s1": { "1-20": "1-20" },
            "tvdb_show:2:s2": { "41-60": "41-60" },
        }));
        let imp = import("X", "anilist:21", SRC, &d).unwrap();
        assert!(imp.load_error.as_deref().unwrap().contains("21-40"));
    }

    #[test]
    fn the_key_not_the_in_season_value_is_written() {
        let d = doc(json!({
            "tvdb_show:2:s1": { "1-8": "1-8" },
            "tvdb_show:2:s2": { "9-30": "1-22" },
        }));
        let imp = import("X", "anilist:21", SRC, &d).unwrap();
        assert!(imp.text.contains(r#"episodes: "9-30""#), "{}", imp.text);
    }

    #[test]
    fn a_compound_range_fails_instead_of_being_written() {
        let d = doc(json!({ "tvdb_show:2:s1": { "1-5,8": "1-6" } }));
        let err = import("X", "anilist:21", SRC, &d).unwrap_err();
        assert!(err.contains("cannot read"), "{err}");
    }

    #[test]
    fn two_ranges_for_one_season_get_part_numbers() {
        let d = doc(json!({ "tvdb_show:2:s1": { "1-5": "1-5", "8-9": "8-9" } }));
        let imp = import("X", "anilist:21", SRC, &d).unwrap();
        assert!(imp.text.contains(r#""Season 1 part 2""#), "{}", imp.text);
    }
}

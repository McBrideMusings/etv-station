//! A pool's taste profile (etv-station-sctf.2): signed weights on keywords,
//! catalog tag values, single items and CEL-defined sets.
//!
//! **The station resolves references; the script does all the weight math**
//! (ADR 0002). This module turns what a channel author wrote into
//! `ctx.profile` — a keyword into its stored form, an item reference into one
//! catalog entry, a set into its members — and fails, naming the entry, when a
//! reference cannot be resolved. It never adds, nets or spreads a weight.
//!
//! Two stages, because they need different things:
//!
//! - [`load`] reads `profile_files` and the inline `profile`, and checks each
//!   entry's shape. It needs only the filesystem, so config validation runs it
//!   and a malformed entry fails the load.
//! - [`resolve`] looks every reference up in the catalog. It runs in the
//!   catalog-reading half of a generation ([`crate::score::ScoreCache`]'s
//!   prepare step), so an unmatched item fails the generation, not the load.

use std::path::Path;

use rhai::{Array, Dynamic, Map};
use serde::{Deserialize, Serialize};

use crate::catalog::Catalog;
use crate::catalog::model::ExternalNs;
use crate::config::Pool;

/// One profile entry as authored: exactly one reference key and a `weight`.
///
/// Every reference key is optional here so that "two keys" and "no key" reach
/// [`check`] and get a message naming the entry, rather than a serde error
/// that can only say which field it tripped on.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyword: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cast: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub director: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub studio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_rating: Option<String>,
    /// An external id (`imdb:tt0113277`, `tmdb:949`, `tvdb:…`) or an exact
    /// `"Title (Year)"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    /// A CEL expression over `item`, resolved like a pool's `sources`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<String>,
    /// Optional here only so a missing one reaches [`check`] and is refused
    /// naming the entry; every entry that passes has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<f64>,
}

/// What one entry points at, once its shape has been checked.
#[derive(Debug, Clone, PartialEq)]
pub enum Reference {
    /// A keyword, already in the form [`normalize_keyword`] gives it.
    Keyword(String),
    /// A catalog tag value. `namespace` is the key the value sits under on a
    /// `ctx.sets` item map (`genres`, `directors`, `studio`, …), so a script
    /// reads `item[namespace]` without a translation table of its own.
    Tag {
        namespace: &'static str,
        value: String,
    },
    /// An item reference as written.
    Item(ItemRef),
    /// A CEL expression.
    Set(String),
}

/// An `item:` reference, parsed.
#[derive(Debug, Clone, PartialEq)]
pub enum ItemRef {
    External { ns: ExternalNs, value: String },
    TitleYear { title: String, year: i64 },
}

/// One checked entry and where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedEntry {
    pub reference: Reference,
    /// The reference exactly as authored, for error messages and the audit.
    pub written: String,
    pub weight: f64,
    /// The profile file's path as written in `profile_files`, or `inline`.
    pub origin: String,
    /// 1-based position within its origin.
    pub position: usize,
}

impl LoadedEntry {
    fn locate(&self) -> String {
        format!("profile entry {} in {}", self.position, self.origin)
    }
}

/// A pool's profile, resolved against the catalog: what `ctx.profile` and
/// `ctx.exclude_keywords` hold.
#[derive(Debug, Clone, Default)]
pub struct ResolvedProfile {
    pub entries: Array,
    pub exclude_keywords: Array,
}

/// The one rule a keyword reference goes through: lowercase, trimmed, runs of
/// whitespace collapsed to one space. Stored keywords are already lowercase,
/// so this is an exact match against them.
pub fn normalize_keyword(raw: &str) -> String {
    raw.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The catalog tag keys an entry may name, each paired with the key its
/// values sit under on a `ctx.sets` item map.
const TAG_KEYS: &[(&str, &str)] = &[
    ("genre", "genres"),
    ("label", "labels"),
    ("cast", "cast"),
    ("director", "directors"),
    ("writer", "writers"),
    ("producer", "producers"),
    ("country", "countries"),
    ("studio", "studio"),
    ("content_rating", "content_rating"),
];

/// Check one authored entry: exactly one reference key, a non-empty value,
/// a finite non-zero weight, and an `item:` in one of its two shapes.
pub fn check(entry: &ProfileEntry) -> Result<(Reference, String, f64), String> {
    let tags = [
        &entry.genre,
        &entry.label,
        &entry.cast,
        &entry.director,
        &entry.writer,
        &entry.producer,
        &entry.country,
        &entry.studio,
        &entry.content_rating,
    ];
    let mut named: Vec<(&str, &String)> = Vec::new();
    if let Some(v) = &entry.keyword {
        named.push(("keyword", v));
    }
    for ((key, _), value) in TAG_KEYS.iter().zip(tags) {
        if let Some(v) = value {
            named.push((key, v));
        }
    }
    if let Some(v) = &entry.item {
        named.push(("item", v));
    }
    if let Some(v) = &entry.set {
        named.push(("set", v));
    }

    let (key, value) = match named.as_slice() {
        [one] => *one,
        [] => {
            return Err(
                "names no reference — give exactly one of `keyword`, `item`, `set`, or a \
                 catalog tag (`genre`, `label`, `cast`, `director`, `writer`, `producer`, \
                 `country`, `studio`, `content_rating`)"
                    .into(),
            );
        }
        many => {
            let keys: Vec<&str> = many.iter().map(|(k, _)| *k).collect();
            return Err(format!(
                "names {} references ({}) — an entry weights exactly one thing",
                keys.len(),
                keys.join(", ")
            ));
        }
    };

    if value.trim().is_empty() {
        return Err(format!("has an empty `{key}`"));
    }
    let Some(weight) = entry.weight else {
        return Err(format!("`{key}: {value}` has no `weight`"));
    };
    if !weight.is_finite() {
        return Err(format!("`{key}: {value}` has a non-finite weight"));
    }
    if weight == 0.0 {
        return Err(format!(
            "`{key}: {value}` has weight 0, which weights nothing — remove the entry"
        ));
    }

    let reference = match key {
        "keyword" => Reference::Keyword(normalize_keyword(value)),
        "item" => Reference::Item(parse_item(value)?),
        "set" => Reference::Set(value.clone()),
        tag => {
            let namespace = TAG_KEYS
                .iter()
                .find(|(k, _)| *k == tag)
                .map(|(_, ns)| *ns)
                .ok_or_else(|| format!("unknown reference key {tag:?}"))?;
            Reference::Tag {
                namespace,
                value: value.trim().to_lowercase(),
            }
        }
    };
    Ok((reference, value.clone(), weight))
}

fn parse_item(raw: &str) -> Result<ItemRef, String> {
    let raw = raw.trim();
    if let Some((ns, value)) = raw.split_once(':')
        && let Ok(ns) = ns.parse::<ExternalNs>()
        && !value.trim().is_empty()
    {
        return Ok(ItemRef::External {
            ns,
            value: value.trim().to_string(),
        });
    }
    if let Some(body) = raw.strip_suffix(')')
        && let Some((title, year)) = body.rsplit_once(" (")
        && year.len() == 4
        && let Ok(year) = year.parse::<i64>()
        && !title.trim().is_empty()
    {
        return Ok(ItemRef::TitleYear {
            title: title.trim().to_string(),
            year,
        });
    }
    Err(format!(
        "`item: {raw}` is neither an external id (`imdb:tt0113277`, `tmdb:949`) nor \
         `Title (Year)`"
    ))
}

/// Read a pool's `profile_files` (in list order) then its inline `profile`,
/// checking every entry. Paths are relative to `base_dir`, the channel config's
/// directory.
pub fn load(pool: &Pool, base_dir: &Path) -> Result<Vec<LoadedEntry>, String> {
    let mut out = Vec::new();
    for file in &pool.profile_files {
        let origin = file.display().to_string();
        let path = crate::score::resolve_plugin_path(base_dir, file);
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("profile file {origin}: {e}"))?;
        let entries: Vec<ProfileEntry> =
            serde_norway::from_str(&text).map_err(|e| format!("profile file {origin}: {e}"))?;
        push_checked(&mut out, &entries, &origin)?;
    }
    push_checked(&mut out, &pool.profile, "inline")?;
    Ok(out)
}

fn push_checked(
    out: &mut Vec<LoadedEntry>,
    entries: &[ProfileEntry],
    origin: &str,
) -> Result<(), String> {
    for (i, entry) in entries.iter().enumerate() {
        let position = i + 1;
        let (reference, written, weight) =
            check(entry).map_err(|m| format!("profile entry {position} in {origin} {m}"))?;
        out.push(LoadedEntry {
            reference,
            written,
            weight,
            origin: origin.to_string(),
            position,
        });
    }
    Ok(())
}

/// Check a pool's `exclude_keywords`: none may be empty.
pub fn check_exclude_keywords(pool: &Pool) -> Result<(), String> {
    if pool.exclude_keywords.iter().any(|k| k.trim().is_empty()) {
        return Err("`exclude_keywords` has an empty entry".into());
    }
    Ok(())
}

/// Resolve every loaded entry against the catalog.
pub fn resolve(
    catalog: &Catalog,
    entries: &[LoadedEntry],
    exclude_keywords: &[String],
) -> Result<ResolvedProfile, String> {
    let mut out = Array::with_capacity(entries.len());
    for entry in entries {
        let mut m = Map::new();
        match &entry.reference {
            Reference::Keyword(k) => {
                m.insert("kind".into(), "keyword".into());
                m.insert("namespace".into(), "keywords".into());
                m.insert("value".into(), k.clone().into());
            }
            Reference::Tag { namespace, value } => {
                m.insert("kind".into(), "tag".into());
                m.insert("namespace".into(), (*namespace).into());
                m.insert("value".into(), value.clone().into());
            }
            Reference::Item(item) => {
                let id = resolve_item(catalog, item).map_err(|msg| {
                    format!("{}: `item: {}` {msg}", entry.locate(), entry.written)
                })?;
                let items = crate::score::load_items(catalog, &[id])?;
                m.insert("kind".into(), "item".into());
                m.insert("items".into(), Dynamic::from_array(items));
            }
            Reference::Set(cel) => {
                let ids = catalog.resolve_query(cel).map_err(|e| {
                    format!("{}: channel-authored `set` {cel:?}: {e}", entry.locate())
                })?;
                if ids.is_empty() {
                    return Err(format!(
                        "{}: `set` {cel:?} matches no catalog items, so it would weight nothing",
                        entry.locate()
                    ));
                }
                let items = crate::score::load_items(catalog, &ids)?;
                m.insert("kind".into(), "set".into());
                m.insert("items".into(), Dynamic::from_array(items));
            }
        }
        m.insert("weight".into(), entry.weight.into());
        m.insert("reference".into(), entry.written.clone().into());
        m.insert("origin".into(), entry.origin.clone().into());
        out.push(Dynamic::from_map(m));
    }
    let exclude = exclude_keywords
        .iter()
        .map(|k| Dynamic::from(normalize_keyword(k)))
        .collect();
    Ok(ResolvedProfile {
        entries: out,
        exclude_keywords: exclude,
    })
}

fn resolve_item(catalog: &Catalog, item: &ItemRef) -> Result<String, String> {
    let ids = match item {
        ItemRef::External { ns, value } => {
            let as_entry_id = format!("{}:{value}", ns.as_str());
            if catalog
                .entry(&as_entry_id)
                .map_err(|e| e.to_string())?
                .is_some()
            {
                vec![as_entry_id]
            } else {
                catalog
                    .entry_ids_for_external_value(*ns, value)
                    .map_err(|e| e.to_string())?
            }
        }
        ItemRef::TitleYear { title, year } => catalog
            .entry_ids_by_title_year(title, *year)
            .map_err(|e| e.to_string())?,
    };
    match ids.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err("matches no catalog item".into()),
        many => Err(format!(
            "matches {} catalog items ({}) — use an external id to name one",
            many.len(),
            many.join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(yaml: &str) -> ProfileEntry {
        serde_norway::from_str(yaml).unwrap()
    }

    #[test]
    fn a_keyword_entry_normalizes_its_value() {
        let (r, _, _) = check(&entry("{ keyword: \"  Bank   Heist \", weight: 2.0 }")).unwrap();
        assert_eq!(r, Reference::Keyword("bank heist".into()));
    }

    #[test]
    fn a_tag_entry_names_the_item_map_key() {
        let (r, _, _) = check(&entry("{ director: Michael Mann, weight: 1.0 }")).unwrap();
        assert_eq!(
            r,
            Reference::Tag {
                namespace: "directors",
                value: "michael mann".into()
            }
        );
    }

    #[test]
    fn two_references_are_refused() {
        let msg = check(&entry("{ keyword: heist, genre: Crime, weight: 1.0 }")).unwrap_err();
        assert!(msg.contains("keyword, genre"), "msg = {msg}");
    }

    #[test]
    fn no_reference_is_refused() {
        let msg = check(&entry("{ weight: 1.0 }")).unwrap_err();
        assert!(msg.contains("names no reference"), "msg = {msg}");
    }

    #[test]
    fn a_zero_weight_is_refused() {
        let msg = check(&entry("{ keyword: heist, weight: 0 }")).unwrap_err();
        assert!(msg.contains("weight 0"), "msg = {msg}");
    }

    #[test]
    fn a_missing_weight_is_refused_naming_the_entry() {
        let msg = check(&entry("{ keyword: heist }")).unwrap_err();
        assert!(
            msg.contains("`keyword: heist` has no `weight`"),
            "msg = {msg}"
        );
    }

    #[test]
    fn a_non_finite_weight_is_refused() {
        let msg = check(&entry("{ keyword: heist, weight: .inf }")).unwrap_err();
        assert!(msg.contains("non-finite"), "msg = {msg}");
    }

    #[test]
    fn an_unknown_key_is_refused_by_the_parser() {
        let err = serde_norway::from_str::<ProfileEntry>("{ mood: dark, weight: 1.0 }");
        assert!(err.is_err());
    }

    #[test]
    fn item_references_parse_in_both_shapes() {
        let (r, _, _) = check(&entry("{ item: \"imdb:tt0113277\", weight: -1 }")).unwrap();
        assert_eq!(
            r,
            Reference::Item(ItemRef::External {
                ns: ExternalNs::Imdb,
                value: "tt0113277".into()
            })
        );
        let (r, _, _) = check(&entry("{ item: \"Heat (1995)\", weight: -1 }")).unwrap();
        assert_eq!(
            r,
            Reference::Item(ItemRef::TitleYear {
                title: "Heat".into(),
                year: 1995
            })
        );
        let msg = check(&entry("{ item: Heat, weight: -1 }")).unwrap_err();
        assert!(msg.contains("Title (Year)"), "msg = {msg}");
    }
}

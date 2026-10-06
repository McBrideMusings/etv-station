//! A pool's taste profile (etv-station-sctf.2): signed weights on keywords,
//! catalog tag values, single items and CEL-defined sets — and, for a tag
//! value or an item, `exclude: true`, which removes every matching title from
//! the pool's candidates before the scorer sees them (etv-station-sctf.7).
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
//! - [`resolve`] looks every reference up in the catalog, and a `keyword`
//!   reference against the pool's granted datastore's `keyword_forms` table
//!   ([`plexdb_reader::Reader::keyword_for_surface`]). It runs in the
//!   catalog-reading half of a generation ([`crate::score::ScoreCache`]'s
//!   prepare step), so an unmatched item or keyword spelling fails the
//!   generation, not the load.

use std::collections::BTreeSet;
use std::path::Path;

use rhai::{Array, Dynamic, Map};
use serde::{Deserialize, Serialize};

use crate::catalog::Catalog;
use crate::catalog::TagNs;
use crate::catalog::model::ExternalNs;
use crate::config::Pool;

/// One profile entry as authored: exactly one reference key and either a
/// `weight` or `exclude: true`.
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
    /// naming the entry; every entry that passes has one or `exclude: true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<f64>,
    /// `true` removes every title this entry's tag or item reference matches
    /// from the pool's candidates, in place of a `weight`. Unlike a negative
    /// weight, which only scores a title down, an excluded title never reaches
    /// the scorer. Unlike the pool's `exclude_keywords`, which zeroes a
    /// keyword's contribution and leaves its titles in the pool, this removes
    /// titles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<bool>,
}

/// What a checked entry does with what it references.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Effect {
    /// Handed to the scorer in `ctx.profile` with this signed weight.
    Weight(f64),
    /// Removed from the pool's candidates; never reaches the scorer.
    Exclude,
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
    pub effect: Effect,
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
/// `ctx.exclude_keywords` hold, and which titles leave the pool's candidates.
#[derive(Debug, Clone, Default)]
pub struct ResolvedProfile {
    /// The weighted entries — `exclude: true` entries are not among them.
    pub entries: Array,
    pub exclude_keywords: Array,
    /// Each `exclude: true` entry and the titles it matched.
    pub exclusions: Vec<ResolvedExclusion>,
    /// Every entry id any of [`Self::exclusions`] matched — what
    /// [`crate::score::ScoreCache::prepare`] drops from the pool's sets.
    pub excluded: BTreeSet<String>,
}

/// One `exclude: true` entry, resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedExclusion {
    /// `key: value` as authored, e.g. `genre: Horror`.
    pub reference: String,
    pub origin: String,
    /// The catalog entries it matched, in id order — whether or not they are
    /// in the pool's sources.
    pub entry_ids: Vec<String>,
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

/// Where a tag key's values live in the catalog.
#[derive(Debug, Clone, Copy)]
enum TagColumn {
    Tag(TagNs),
    Studio,
    ContentRating,
}

/// The catalog tag keys an entry may name, each paired with the key its
/// values sit under on a `ctx.sets` item map and where the catalog keeps them.
const TAG_KEYS: &[(&str, &str, TagColumn)] = &[
    ("genre", "genres", TagColumn::Tag(TagNs::Genre)),
    ("label", "labels", TagColumn::Tag(TagNs::Label)),
    ("cast", "cast", TagColumn::Tag(TagNs::Cast)),
    ("director", "directors", TagColumn::Tag(TagNs::Director)),
    ("writer", "writers", TagColumn::Tag(TagNs::Writer)),
    ("producer", "producers", TagColumn::Tag(TagNs::Producer)),
    ("country", "countries", TagColumn::Tag(TagNs::Country)),
    ("studio", "studio", TagColumn::Studio),
    ("content_rating", "content_rating", TagColumn::ContentRating),
];

/// Check one authored entry: exactly one reference key, a non-empty value,
/// either a finite non-zero weight or `exclude: true` on a tag or item, and an
/// `item:` in one of its two shapes.
pub fn check(entry: &ProfileEntry) -> Result<(Reference, String, Effect), String> {
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
    for ((key, _, _), value) in TAG_KEYS.iter().zip(tags) {
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
    let effect = match (entry.exclude, entry.weight) {
        (Some(true), Some(_)) => {
            return Err(format!(
                "`{key}: {value}` sets both `weight` and `exclude` — an excluded title is \
                 never scored, so drop the weight"
            ));
        }
        (Some(true), None) => {
            if key == "keyword" || key == "set" {
                return Err(format!(
                    "`{key}: {value}` cannot be excluded — `exclude` takes a catalog tag or an \
                     `item`; drop a keyword from scoring with the pool's `exclude_keywords`, \
                     and narrow candidates by query with the pool's `sources`"
                ));
            }
            Effect::Exclude
        }
        (Some(false), _) => {
            return Err(format!(
                "`{key}: {value}` has `exclude: false`, which excludes nothing — remove it"
            ));
        }
        (None, None) => {
            return Err(format!(
                "`{key}: {value}` has no `weight` (or `exclude: true`)"
            ));
        }
        (None, Some(weight)) => {
            if !weight.is_finite() {
                return Err(format!("`{key}: {value}` has a non-finite weight"));
            }
            if weight == 0.0 {
                return Err(format!(
                    "`{key}: {value}` has weight 0, which weights nothing — remove the entry"
                ));
            }
            Effect::Weight(weight)
        }
    };

    let reference = match key {
        "keyword" => Reference::Keyword(normalize_keyword(value)),
        "item" => Reference::Item(parse_item(value)?),
        "set" => Reference::Set(value.clone()),
        tag => {
            let namespace = TAG_KEYS
                .iter()
                .find(|(k, _, _)| *k == tag)
                .map(|(_, ns, _)| *ns)
                .ok_or_else(|| format!("unknown reference key {tag:?}"))?;
            Reference::Tag {
                namespace,
                value: value.trim().to_lowercase(),
            }
        }
    };
    Ok((reference, value.clone(), effect))
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
        let (reference, written, effect) =
            check(entry).map_err(|m| format!("profile entry {position} in {origin} {m}"))?;
        out.push(LoadedEntry {
            reference,
            written,
            effect,
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

/// The stored keyword a normalized surface spelling maps to, via the
/// datastore's `keyword_forms` table (etv-station-sctf.1) — the one function
/// a keyword's resolution goes through, so a spelling `keyword_forms` has
/// never seen fails naming itself rather than being passed through unmatched.
fn resolve_keyword_surface(
    reader: &plexdb_reader::Reader,
    surface: &str,
) -> Result<String, String> {
    reader
        .keyword_for_surface(surface)
        .map_err(|e| format!("keyword {surface:?}: {e}"))?
        .ok_or_else(|| format!("keyword {surface:?} matches no stored keyword"))
}

/// Resolve every loaded entry against the catalog. `keyword_store` is the
/// pool's granted datastore (its first grant — the same one a channel author
/// points a `keyword:`-reading scorer at), needed only when an entry or
/// `exclude_keywords` names a keyword; `None` fails such an entry naming the
/// pool's missing grant, rather than resolving nothing.
pub fn resolve(
    catalog: &Catalog,
    entries: &[LoadedEntry],
    exclude_keywords: &[String],
    keyword_store: Option<&plexdb_reader::Reader>,
) -> Result<ResolvedProfile, String> {
    let mut out = Array::with_capacity(entries.len());
    let mut exclusions = Vec::new();
    for entry in entries {
        let weight = match entry.effect {
            Effect::Weight(w) => w,
            Effect::Exclude => {
                exclusions.push(resolve_exclusion(catalog, entry)?);
                continue;
            }
        };
        let mut m = Map::new();
        match &entry.reference {
            Reference::Keyword(k) => {
                let reader = keyword_store.ok_or_else(|| {
                    format!(
                        "{}: `keyword: {}` needs a granted datastore to resolve its spelling \
                         against — this pool grants none",
                        entry.locate(),
                        entry.written
                    )
                })?;
                let stored = resolve_keyword_surface(reader, k)
                    .map_err(|msg| format!("{}: {msg}", entry.locate()))?;
                m.insert("kind".into(), "keyword".into());
                m.insert("namespace".into(), "keywords".into());
                m.insert("value".into(), stored.into());
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
        m.insert("weight".into(), weight.into());
        m.insert("reference".into(), entry.written.clone().into());
        m.insert("origin".into(), entry.origin.clone().into());
        out.push(Dynamic::from_map(m));
    }
    let exclude = if exclude_keywords.is_empty() {
        Array::new()
    } else {
        let reader = keyword_store.ok_or_else(|| {
            "`exclude_keywords` needs a granted datastore to resolve its spellings against — \
             this pool grants none"
                .to_string()
        })?;
        let mut resolved = Array::with_capacity(exclude_keywords.len());
        for raw in exclude_keywords {
            let surface = normalize_keyword(raw);
            let stored = resolve_keyword_surface(reader, &surface)
                .map_err(|msg| format!("exclude_keywords: {msg}"))?;
            resolved.push(Dynamic::from(stored));
        }
        resolved
    };
    let excluded = exclusions
        .iter()
        .flat_map(|x| x.entry_ids.iter().cloned())
        .collect();
    Ok(ResolvedProfile {
        entries: out,
        exclude_keywords: exclude,
        exclusions,
        excluded,
    })
}

/// The catalog entries one `exclude: true` entry matches. A tag value matches
/// ASCII-case-insensitively (`Horror` = `horror`, but `É` ≠ `é`); an item resolves exactly as a weighted `item:` does.
/// Matching nothing fails, naming the entry, on the same terms as a `set`
/// that matches nothing: a misspelt exclusion would otherwise exclude nothing
/// and say so nowhere.
fn resolve_exclusion(catalog: &Catalog, entry: &LoadedEntry) -> Result<ResolvedExclusion, String> {
    let (key, entry_ids) = match &entry.reference {
        Reference::Tag { namespace, .. } => {
            let (key, _, column) = TAG_KEYS
                .iter()
                .find(|(_, ns, _)| ns == namespace)
                .ok_or_else(|| format!("unknown tag namespace {namespace:?}"))?;
            let value = entry.written.trim();
            let ids = match column {
                TagColumn::Tag(ns) => catalog.entry_ids_with_tag(*ns, value),
                TagColumn::Studio => catalog.entry_ids_with_studio(value),
                TagColumn::ContentRating => catalog.entry_ids_with_content_rating(value),
            }
            .map_err(|e| format!("{}: `{key}: {value}`: {e}", entry.locate()))?;
            (*key, ids)
        }
        Reference::Item(item) => {
            let id = resolve_item(catalog, item)
                .map_err(|msg| format!("{}: `item: {}` {msg}", entry.locate(), entry.written))?;
            ("item", vec![id])
        }
        Reference::Keyword(_) | Reference::Set(_) => {
            return Err(format!(
                "{}: only a tag or an `item` can be excluded",
                entry.locate()
            ));
        }
    };
    let reference = format!("{key}: {}", entry.written.trim());
    if entry_ids.is_empty() {
        return Err(format!(
            "{}: excluded `{reference}` matches no catalog items, so it would exclude nothing",
            entry.locate()
        ));
    }
    Ok(ResolvedExclusion {
        reference,
        origin: entry.origin.clone(),
        entry_ids,
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
    fn a_tag_or_item_may_be_excluded_in_place_of_a_weight() {
        let (_, _, effect) = check(&entry("{ genre: Horror, exclude: true }")).unwrap();
        assert_eq!(effect, Effect::Exclude);
        let (_, _, effect) = check(&entry("{ item: \"Heat (1995)\", exclude: true }")).unwrap();
        assert_eq!(effect, Effect::Exclude);
    }

    #[test]
    fn an_exclusion_with_a_weight_is_refused() {
        let msg = check(&entry("{ genre: Horror, exclude: true, weight: -1 }")).unwrap_err();
        assert!(msg.contains("both `weight` and `exclude`"), "msg = {msg}");
    }

    /// `exclude_keywords` already owns keywords, and `sources` owns queries.
    #[test]
    fn a_keyword_or_set_cannot_be_excluded() {
        for yaml in [
            "{ keyword: heist, exclude: true }",
            "{ set: 'item.year < 1970', exclude: true }",
        ] {
            let msg = check(&entry(yaml)).unwrap_err();
            assert!(msg.contains("cannot be excluded"), "msg = {msg}");
        }
    }

    #[test]
    fn exclude_false_is_refused() {
        let msg = check(&entry("{ genre: Horror, exclude: false }")).unwrap_err();
        assert!(msg.contains("excludes nothing"), "msg = {msg}");
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

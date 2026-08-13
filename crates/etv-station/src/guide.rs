//! The `guide:` config block (#158): what each programme carries in the
//! XMLTV guide, cascading channel → block → item, plus the two things that
//! need no config at all — genre tags become `<category>`, and an episode's
//! `<title>` becomes its series name (settled, not configurable; see the
//! series-convention default in [`crate::resolve::catalog_item`]).
//!
//! # Template syntax
//!
//! Plain `{field}` substitution with a `{a|b}` fallback (first non-empty
//! branch wins; the final branch is used even when it, too, is empty — so
//! `"{a|b}"` never fails, it just may render empty). Not Rhai, not CEL: the
//! one thing plain substitution lacks — "the summary if there is one, else
//! the tagline" — is this fallback operator, not a language (issue #158
//! decision #2).
//!
//! A `categories:` list entry that is *exactly* `"{field}"` for a
//! multi-valued field (`genres`, `cast`, `directors`, `writers`,
//! `countries`) fans out to one `<category>` per value instead of joining
//! them into one string (decision: `{genres}` in a `categories` list fans
//! out).
//!
//! # Addressable sources
//!
//! - the item: `title`, `show`, `season`, `episode`, `absolute_episode`,
//!   `year`, `release_date`, `studio`, `edition`, `content_rating`,
//!   `duration`, `library`, `genres`, `cast`, `directors`, `writers`,
//!   `countries`, `summary` (#186's catalog column — already the default
//!   `description` with no `guide:` config needed; addressable here too, for
//!   a template that wants it alongside other fields, e.g. `{summary|title}`)
//! - the watch history: `watched_by` (the #113 credit line, or empty when
//!   nobody on record has watched this entry)
//! - the channel: `channel_identity`, `channel_name` (its display name, #158
//!   decision #5), `program_title` (this item's own title before any guide
//!   override — the series-convention default)
//! - the schedule: `next_title`, `next_start`, `prev_title` — a one-item
//!   lookahead/lookbehind over this generation's ordered list; empty at
//!   either end
//! - time: `start`, `stop` (this item's own airing window), `now` (wall
//!   clock at generation time)

use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// A `guide:` block, at whichever cascade level (channel / block / item)
/// authored it. `None` on every field means "say nothing here" — cascading
/// falls through to the next-less-specific level, and falling through all
/// three leaves the built-in default (series-title convention /
/// auto-categories / no description).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct GuideConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `None` = unset (falls through / defaults to genre tags); `Some(v)` =
    /// authored, even `Some(vec![])` (an author who writes `categories: []`
    /// means "no categories on this item", which must not fall back to the
    /// genre-tag default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub categories: Option<Vec<String>>,
}

impl GuideConfig {
    fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.sub_title.is_none()
            && self.description.is_none()
            && self.categories.is_none()
    }

    /// Field-wise cascade: item wins, then block, then channel. Mirrors
    /// `resolve::merge_program`'s field-by-field pick, three levels instead
    /// of two. Returns `None` only when every level said nothing at every
    /// field — the "no guide: block anywhere" case, which must produce
    /// exactly the built-in defaults and nothing else.
    pub fn cascade(
        item: Option<&GuideConfig>,
        block: Option<&GuideConfig>,
        channel: Option<&GuideConfig>,
    ) -> Option<GuideConfig> {
        macro_rules! pick {
            ($field:ident) => {
                item.and_then(|g| g.$field.clone())
                    .or_else(|| block.and_then(|g| g.$field.clone()))
                    .or_else(|| channel.and_then(|g| g.$field.clone()))
            };
        }
        let merged = GuideConfig {
            title: pick!(title),
            sub_title: pick!(sub_title),
            description: pick!(description),
            categories: pick!(categories),
        };
        if merged.is_empty() {
            None
        } else {
            Some(merged)
        }
    }
}

/// Raw, unrendered catalog attributes for one item — everything the
/// `guide:` template surface can address beyond what [`GuideConfig`] and
/// `ProgramMetadata` already carry. Populated from the catalog row plus its
/// tag namespaces in [`crate::resolve::catalog_item`]; every field is empty
/// for an inline item, which has no catalog row behind it.
#[derive(Debug, Clone, Default)]
pub struct GuideFields {
    pub show: Option<String>,
    pub absolute_episode: Option<i64>,
    pub release_date: Option<String>,
    pub studio: Option<String>,
    pub edition: Option<String>,
    pub library: Option<String>,
    pub duration: Option<Duration>,
    pub genres: Vec<String>,
    pub cast: Vec<String>,
    pub directors: Vec<String>,
    pub writers: Vec<String>,
    pub countries: Vec<String>,
    /// The catalog's `summary` column (#186) — `None` for a source that
    /// filled none, or for an inline item with no catalog row at all.
    pub summary: Option<String>,
}

/// Every scalar (single-value) field name a template may address, grouped by
/// where it comes from. Used both to validate an authored template at load
/// (an unrecognised name fails the load, naming the field and the channel)
/// and to look the value up at render time.
const ITEM_SCALAR_FIELDS: &[&str] = &[
    "title",
    "show",
    "season",
    "episode",
    "absolute_episode",
    "year",
    "release_date",
    "studio",
    "edition",
    "content_rating",
    "duration",
    "library",
    "summary",
];
const ITEM_MULTI_FIELDS: &[&str] = &["genres", "cast", "directors", "writers", "countries"];
const OTHER_SCALAR_FIELDS: &[&str] = &[
    "watched_by",
    "channel_identity",
    "channel_name",
    "program_title",
    "next_title",
    "next_start",
    "prev_title",
    "start",
    "stop",
    "now",
];

/// Every field name a template is allowed to address — the vocabulary
/// [`validate_template`] checks an authored `{field}` against.
pub fn known_fields() -> Vec<&'static str> {
    ITEM_SCALAR_FIELDS
        .iter()
        .chain(ITEM_MULTI_FIELDS)
        .chain(OTHER_SCALAR_FIELDS)
        .copied()
        .collect()
}

fn is_known_field(name: &str) -> bool {
    ITEM_SCALAR_FIELDS.contains(&name)
        || ITEM_MULTI_FIELDS.contains(&name)
        || OTHER_SCALAR_FIELDS.contains(&name)
}

/// Whether `name` names a multi-valued field (`genres`, `cast`, …) — the set
/// a `categories:` entry that is exactly `"{name}"` fans out on.
pub fn is_multi_valued(name: &str) -> bool {
    ITEM_MULTI_FIELDS.contains(&name)
}

/// One `{...}` placeholder split on `|`: try each branch's value in order,
/// first non-empty wins; the last branch is used verbatim (even empty) if
/// every branch was empty.
fn placeholder_branches(inner: &str) -> Vec<&str> {
    inner.split('|').map(str::trim).collect()
}

/// Validate every `{field}` (or `{a|b}` branch) named in `template` against
/// [`known_fields`]. Called at config load so a typo'd field name fails the
/// load with the field and the channel named, rather than silently rendering
/// empty forever.
pub fn validate_template(template: &str) -> Result<(), String> {
    for placeholder in extract_placeholders(template) {
        for branch in placeholder_branches(placeholder) {
            if branch.is_empty() {
                return Err(format!("empty field name in template {template:?}"));
            }
            if !is_known_field(branch) {
                return Err(format!(
                    "unknown guide field {branch:?} in template {template:?} (known fields: {})",
                    known_fields().join(", ")
                ));
            }
        }
    }
    Ok(())
}

/// Every `{...}` span's inner text, in order. A `{` with no matching `}`
/// (or vice versa) is reported by the caller as an unknown/empty field
/// rather than silently dropped — `extract_placeholders` only returns
/// well-formed spans, and [`render`] treats unmatched braces as literal
/// text, matching how a config author would expect a typo'd brace to show up
/// on screen rather than vanish.
fn extract_placeholders(template: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && let Some(end) = template[i..].find('}')
        {
            out.push(&template[i + 1..i + end]);
            i += end + 1;
        } else {
            i += 1;
        }
    }
    out
}

/// Everything a rendered template can address for one item, resolved to
/// concrete values by the caller (which alone knows the schedule, the watch
/// history, and wall-clock time).
pub struct RenderContext<'a> {
    pub fields: &'a GuideFields,
    /// The item's own program metadata as computed by the built-in default
    /// (series-title convention, catalog columns) — `title`/`season`/etc
    /// come from here, since `ProgramMetadata` already carries them and
    /// `GuideFields` only adds what it does not.
    pub base_title: Option<&'a str>,
    pub base_season: Option<u32>,
    pub base_episode: Option<u32>,
    pub base_year: Option<u32>,
    pub base_content_rating: Option<&'a str>,
    pub channel_identity: &'a str,
    pub channel_name: &'a str,
    /// The item's own default title (post series-convention, pre guide
    /// override) — addressed as `{program_title}`.
    pub program_title: Option<&'a str>,
    pub watched_by: Option<&'a str>,
    pub next_title: Option<&'a str>,
    pub next_start: Option<OffsetDateTime>,
    pub prev_title: Option<&'a str>,
    pub start: OffsetDateTime,
    pub stop: OffsetDateTime,
    pub now: OffsetDateTime,
}

impl RenderContext<'_> {
    fn scalar(&self, name: &str) -> String {
        match name {
            "title" => self.base_title.unwrap_or_default().to_string(),
            "show" => self.fields.show.clone().unwrap_or_default(),
            "season" => opt_num(self.base_season),
            "episode" => opt_num(self.base_episode),
            "absolute_episode" => opt_num(self.fields.absolute_episode),
            "year" => opt_num(self.base_year),
            "release_date" => self.fields.release_date.clone().unwrap_or_default(),
            "studio" => self.fields.studio.clone().unwrap_or_default(),
            "edition" => self.fields.edition.clone().unwrap_or_default(),
            "content_rating" => self.base_content_rating.unwrap_or_default().to_string(),
            "duration" => self
                .fields
                .duration
                .map(|d| humantime::format_duration(round_to_secs(d)).to_string())
                .unwrap_or_default(),
            "library" => self.fields.library.clone().unwrap_or_default(),
            "summary" => self.fields.summary.clone().unwrap_or_default(),
            "genres" => self.fields.genres.join(", "),
            "cast" => self.fields.cast.join(", "),
            "directors" => self.fields.directors.join(", "),
            "writers" => self.fields.writers.join(", "),
            "countries" => self.fields.countries.join(", "),
            "watched_by" => self.watched_by.unwrap_or_default().to_string(),
            "channel_identity" => self.channel_identity.to_string(),
            "channel_name" => self.channel_name.to_string(),
            "program_title" => self.program_title.unwrap_or_default().to_string(),
            "next_title" => self.next_title.unwrap_or_default().to_string(),
            "next_start" => self.next_start.map(fmt_time).unwrap_or_default(),
            "prev_title" => self.prev_title.unwrap_or_default().to_string(),
            "start" => fmt_time(self.start),
            "stop" => fmt_time(self.stop),
            "now" => fmt_time(self.now),
            // Every other name is rejected at load by `validate_template`, so
            // a template that reaches render already named only known
            // fields. Render defensively anyway rather than panicking on a
            // hand-built `GuideConfig` (tests, or a future caller that skips
            // validation).
            _ => String::new(),
        }
    }

    /// The multi-valued list behind a field name, for `categories:` fan-out.
    /// Empty (never `None`) for a scalar field — [`render_categories`] only
    /// calls this after confirming the field is multi-valued.
    fn multi(&self, name: &str) -> &[String] {
        match name {
            "genres" => &self.fields.genres,
            "cast" => &self.fields.cast,
            "directors" => &self.fields.directors,
            "writers" => &self.fields.writers,
            "countries" => &self.fields.countries,
            _ => &[],
        }
    }
}

fn round_to_secs(d: Duration) -> Duration {
    Duration::from_secs(d.as_secs())
}

fn opt_num<N: std::fmt::Display>(v: Option<N>) -> String {
    v.map(|n| n.to_string()).unwrap_or_default()
}

fn fmt_time(t: OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Render one template string: literal text passes through, each
/// `{a|b|...}` resolves to the first non-empty branch's value (the last
/// branch's value if every branch was empty).
pub fn render(template: &str, ctx: &RenderContext) -> String {
    let mut out = String::with_capacity(template.len());
    // Byte offsets, not char offsets — but every slice point below is either
    // the start of `template`, or immediately after a `{`/`}` (both
    // single-byte ASCII), so every slice lands on a UTF-8 char boundary
    // regardless of what non-ASCII text sits between the placeholders.
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && let Some(end) = template[i..].find('}')
        {
            let inner = &template[i + 1..i + end];
            let branches = placeholder_branches(inner);
            let mut resolved = String::new();
            for (n, branch) in branches.iter().enumerate() {
                resolved = ctx.scalar(branch);
                if !resolved.is_empty() || n == branches.len() - 1 {
                    break;
                }
            }
            out.push_str(&resolved);
            i += end + 1;
        } else {
            let next_brace = template[i..].find('{').map(|off| i + off);
            let literal_end = next_brace.unwrap_or(bytes.len());
            out.push_str(&template[i..literal_end]);
            i = literal_end;
            // A `{` with no matching `}` falls through to here on the next
            // loop iteration (the `if` above requires a matching `}`), and
            // `find('{')` at that exact position returns `Some(0)` forever —
            // so treat an unmatched `{` as one literal byte to guarantee
            // progress.
            if i < bytes.len() && bytes[i] == b'{' && template[i..].find('}').is_none() {
                out.push('{');
                i += 1;
            }
        }
    }
    out
}

/// Render a `categories:` list. An entry that is *exactly* `"{field}"` for a
/// multi-valued field fans out to one category per value (skipping empty
/// values); every other entry renders as one string via [`render`] and is
/// kept only if non-empty.
pub fn render_categories(templates: &[String], ctx: &RenderContext) -> Vec<String> {
    let mut out = Vec::new();
    for template in templates {
        let trimmed = template.trim();
        if let Some(field) = trimmed.strip_prefix('{').and_then(|s| s.strip_suffix('}'))
            && is_multi_valued(field)
        {
            out.extend(ctx.multi(field).iter().filter(|v| !v.is_empty()).cloned());
            continue;
        }
        let rendered = render(template, ctx);
        if !rendered.is_empty() {
            out.push(rendered);
        }
    }
    out
}

/// A description template's raw (pre-render) text referenced `{watched_by}`
/// explicitly — the signal that the author has taken over the #113
/// attribution line themselves, so the auto-append in
/// [`crate::daemon`] must not also add it (see #158 decision #4).
pub fn references_watched_by(template: &str) -> bool {
    extract_placeholders(template)
        .into_iter()
        .any(|p| placeholder_branches(p).contains(&"watched_by"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(fields: &'a GuideFields, title: &'a str) -> RenderContext<'a> {
        let t = time::macros::datetime!(2026-08-13 12:00 UTC);
        RenderContext {
            fields,
            base_title: Some(title),
            base_season: Some(2),
            base_episode: Some(5),
            base_year: Some(2020),
            base_content_rating: Some("TV-14"),
            channel_identity: "diehard",
            channel_name: "Die Hard 24/7",
            program_title: Some(title),
            watched_by: None,
            next_title: Some("Die Hard 2"),
            next_start: Some(t),
            prev_title: None,
            start: t,
            stop: t,
            now: t,
        }
    }

    #[test]
    fn plain_substitution() {
        let fields = GuideFields::default();
        let c = ctx(&fields, "Die Hard");
        assert_eq!(render("{title}", &c), "Die Hard");
        assert_eq!(render("now playing: {title}", &c), "now playing: Die Hard");
    }

    #[test]
    fn fallback_uses_the_first_non_empty_branch() {
        let mut fields = GuideFields::default();
        let c = ctx(&fields, "Die Hard");
        assert_eq!(render("{summary|title}", &c), "Die Hard");
        fields.summary = Some("A cop fights terrorists.".into());
        let c = ctx(&fields, "Die Hard");
        assert_eq!(render("{summary|title}", &c), "A cop fights terrorists.");
    }

    #[test]
    fn a_fallback_with_every_branch_empty_renders_empty() {
        let fields = GuideFields::default();
        let c = ctx(&fields, "Die Hard");
        assert_eq!(render("{summary|library}", &c), "");
    }

    #[test]
    fn genres_fans_out_one_category_per_tag() {
        let fields = GuideFields {
            genres: vec!["Action".into(), "Thriller".into()],
            ..Default::default()
        };
        let c = ctx(&fields, "Die Hard");
        assert_eq!(
            render_categories(&["{genres}".into()], &c),
            vec!["Action", "Thriller"]
        );
    }

    #[test]
    fn a_non_fanout_category_entry_renders_as_one_string() {
        let fields = GuideFields::default();
        let c = ctx(&fields, "Die Hard");
        assert_eq!(
            render_categories(&["Movie".into(), "{channel_name}".into()], &c),
            vec!["Movie", "Die Hard 24/7"]
        );
    }

    #[test]
    fn an_empty_rendered_category_is_dropped() {
        let fields = GuideFields::default();
        let c = ctx(&fields, "Die Hard");
        assert_eq!(
            render_categories(&["{summary}".into(), "Movie".into()], &c),
            vec!["Movie"]
        );
    }

    #[test]
    fn validate_accepts_every_known_field() {
        for field in known_fields() {
            validate_template(&format!("{{{field}}}")).unwrap();
        }
        validate_template("{summary|title}").unwrap();
    }

    #[test]
    fn validate_rejects_an_unknown_field() {
        let err = validate_template("{tagline}").unwrap_err();
        assert!(err.contains("tagline"), "{err}");
    }

    #[test]
    fn cascade_prefers_the_most_specific_level_per_field() {
        let channel = GuideConfig {
            title: Some("channel title".into()),
            description: Some("channel desc".into()),
            ..Default::default()
        };
        let block = GuideConfig {
            description: Some("block desc".into()),
            ..Default::default()
        };
        let item = GuideConfig {
            sub_title: Some("item sub".into()),
            ..Default::default()
        };
        let merged = GuideConfig::cascade(Some(&item), Some(&block), Some(&channel)).unwrap();
        assert_eq!(merged.title.as_deref(), Some("channel title"));
        assert_eq!(merged.description.as_deref(), Some("block desc"));
        assert_eq!(merged.sub_title.as_deref(), Some("item sub"));
    }

    #[test]
    fn cascade_of_nothing_anywhere_is_none() {
        assert!(GuideConfig::cascade(None, None, None).is_none());
    }

    #[test]
    fn an_authored_empty_categories_list_is_not_the_same_as_unset() {
        let item = GuideConfig {
            categories: Some(vec![]),
            ..Default::default()
        };
        let merged = GuideConfig::cascade(Some(&item), None, None).unwrap();
        assert_eq!(merged.categories, Some(vec![]));
    }

    #[test]
    fn multibyte_text_around_a_placeholder_renders_intact() {
        let fields = GuideFields::default();
        let c = ctx(&fields, "Amélie");
        assert_eq!(
            render("café — {title} — 日本語", &c),
            "café — Amélie — 日本語"
        );
    }

    #[test]
    fn an_unmatched_brace_is_left_as_a_literal() {
        let fields = GuideFields::default();
        let c = ctx(&fields, "Die Hard");
        assert_eq!(render("open { brace", &c), "open { brace");
    }

    #[test]
    fn watched_by_reference_is_detected_only_when_actually_named() {
        assert!(references_watched_by("{summary}\n\n{watched_by}"));
        assert!(!references_watched_by("{summary|title}"));
    }
}

use cel::{Context, Program};
use thiserror::Error;

use crate::normalize::NormalizedItem;

const HELPER_NAMES: &[&str] = &[
    "season_in",
    "in_collection",
    "in_franchise",
    "has_category",
    "shorter_than",
    "longer_than",
    "icontains",
];

pub struct CompiledProgram {
    program: Program,
    expr: String,
}

impl CompiledProgram {
    pub fn matches(&self, item: &NormalizedItem) -> Result<bool, CelError> {
        let mut context = Context::default();
        bind_item(&mut context, item)?;
        register_helpers(&mut context, item);
        let value = self
            .program
            .execute(&context)
            .map_err(|e| CelError::Execute(e.to_string()))?;
        match value {
            cel::Value::Bool(b) => Ok(b),
            other => Err(CelError::NonBoolResult(format!("{other:?}"))),
        }
    }

    pub fn helpers_referenced(&self) -> Vec<&'static str> {
        HELPER_NAMES
            .iter()
            .copied()
            .filter(|h| self.expr.contains(h))
            .collect()
    }
}

pub fn compile(expr: &str) -> Result<CompiledProgram, CelError> {
    // String comparisons are case-insensitive across this crate: bound string
    // fields are lowercased, and so is the user expression. CEL identifiers in
    // our schema are already all lowercase, so lowercasing the whole expression
    // is safe.
    let lowered = expr.to_lowercase();
    let program = Program::compile(&lowered).map_err(|e| CelError::Compile(e.to_string()))?;
    Ok(CompiledProgram {
        program,
        expr: lowered,
    })
}

fn bind_item(context: &mut Context<'_>, item: &NormalizedItem) -> Result<(), CelError> {
    fn bind<V: serde::Serialize>(
        context: &mut Context<'_>,
        name: &str,
        value: V,
    ) -> Result<(), CelError> {
        context
            .add_variable(name, value)
            .map_err(|e| CelError::Bind(name.to_string(), e.to_string()))
    }
    let lc_opt = |opt: &Option<String>| opt.as_deref().unwrap_or("").to_lowercase();
    let lc_vec = |v: &[String]| -> Vec<String> { v.iter().map(|s| s.to_lowercase()).collect() };

    // All string fields are lowercased on bind. Paired with lowercasing the
    // user's CEL expression at compile time, this makes native ==, .contains(),
    // .startsWith(), and list `c == "..."` comparisons case-insensitive without
    // having to override operators.
    bind(context, "sources", lc_vec(&item.sources))?;
    // `source` = primary source (first in list, Plex wins when both present).
    bind(context, "source", item.primary_source().to_lowercase())?;
    bind(context, "title", item.title.to_lowercase())?;
    bind(context, "type", item.media_type.to_lowercase())?;
    bind(context, "library", item.library.to_lowercase())?;
    bind(context, "sub_title", lc_opt(&item.sub_title))?;
    bind(context, "content_rating", lc_opt(&item.content_rating))?;
    bind(context, "franchise", lc_opt(&item.franchise))?;
    bind(context, "path", item.path.to_lowercase())?;
    bind(context, "season", item.season.unwrap_or(0))?;
    bind(context, "episode", item.episode.unwrap_or(0))?;
    bind(context, "year", item.year.unwrap_or(0))?;
    bind(context, "runtime_secs", item.runtime_secs.unwrap_or(0.0))?;
    bind(context, "categories", lc_vec(&item.categories))?;
    bind(context, "collections", lc_vec(&item.collections))?;
    Ok(())
}

fn register_helpers(context: &mut Context<'_>, item: &NormalizedItem) {
    let season = item.season.unwrap_or(0);
    context.add_function("season_in", move |lo: i64, hi: i64| -> bool {
        season >= lo && season <= hi
    });

    // Captured strings are lowercased to match the case-insensitive convention.
    // The CEL expression has already been lowercased by compile(), so helper
    // arguments arrive lowercased too.
    let collections: Vec<String> = item.collections.iter().map(|s| s.to_lowercase()).collect();
    context.add_function(
        "in_collection",
        move |name: std::sync::Arc<String>| -> bool {
            collections.iter().any(|c| c == name.as_str())
        },
    );

    let franchise = item.franchise.as_deref().unwrap_or("").to_lowercase();
    context.add_function(
        "in_franchise",
        move |name: std::sync::Arc<String>| -> bool { franchise == name.as_str() },
    );

    let categories: Vec<String> = item.categories.iter().map(|s| s.to_lowercase()).collect();
    context.add_function(
        "has_category",
        move |name: std::sync::Arc<String>| -> bool {
            categories.iter().any(|c| c == name.as_str())
        },
    );

    let runtime = item.runtime_secs.unwrap_or(0.0);
    context.add_function("shorter_than", move |secs: f64| -> bool { runtime < secs });
    context.add_function("longer_than", move |secs: f64| -> bool { runtime > secs });

    context.add_function(
        "icontains",
        |haystack: std::sync::Arc<String>, needle: std::sync::Arc<String>| -> bool {
            haystack.to_lowercase().contains(&needle.to_lowercase())
        },
    );
}

/// Build a free-text CEL expression for a bare search term. Used when the
/// user's input doesn't parse as CEL or evaluates to a non-bool — interpret
/// it as a substring search across the common string fields.
pub fn free_text_expression(term: &str) -> String {
    let clean: String = term.chars().filter(|c| !c.is_control()).collect();
    let escaped = clean.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "icontains(title, \"{escaped}\") || categories.exists(c, icontains(c, \"{escaped}\")) || collections.exists(c, icontains(c, \"{escaped}\"))"
    )
}

#[derive(Debug, Error)]
pub enum CelError {
    #[error("compile: {0}")]
    Compile(String),
    #[error("bind variable {0}: {1}")]
    Bind(String, String),
    #[error("execute: {0}")]
    Execute(String),
    #[error("expression must return bool, got: {0}")]
    NonBoolResult(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item() -> NormalizedItem {
        NormalizedItem {
            sources: vec!["plex".into()],
            media_type: "episode".into(),
            library: "TV Shows".into(),
            title: "Star Trek: The Next Generation".into(),
            sub_title: None,
            season: Some(3),
            episode: Some(1),
            year: Some(1989),
            categories: vec!["Sci-Fi".into(), "Adventure".into()],
            collections: vec!["Star Trek".into()],
            franchise: Some("Star Trek".into()),
            content_rating: None,
            runtime_secs: Some(2640.0),
            path: "/media/tng/s03e01.mkv".into(),
            rating_key: None,
        }
    }

    #[test]
    fn season_range() {
        let p = compile("season >= 3 && season <= 5").unwrap();
        assert!(p.matches(&item()).unwrap());
    }

    #[test]
    fn season_in_helper() {
        let p = compile("season_in(3, 5)").unwrap();
        assert!(p.matches(&item()).unwrap());
        let p2 = compile("season_in(6, 7)").unwrap();
        assert!(!p2.matches(&item()).unwrap());
    }

    #[test]
    fn collection_membership() {
        let p = compile("in_collection(\"Star Trek\")").unwrap();
        assert!(p.matches(&item()).unwrap());
    }

    #[test]
    fn category_exists_native_cel() {
        let p = compile("categories.exists(c, c == \"Sci-Fi\")").unwrap();
        assert!(p.matches(&item()).unwrap());
    }

    #[test]
    fn helpers_referenced_listed() {
        let p = compile("season_in(3, 5) && in_collection(\"x\")").unwrap();
        let helpers = p.helpers_referenced();
        assert!(helpers.contains(&"season_in"));
        assert!(helpers.contains(&"in_collection"));
    }
}

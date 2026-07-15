//! CEL `query` → SQL `WHERE` translation (#68).
//!
//! A channel `query` entry carries a CEL boolean expression over the `item.*`
//! field set. This module compiles that expression to a parameterised SQL
//! `WHERE` clause against the catalog (see [`super::schema`]), which
//! [`super::Catalog::resolve_query`] runs to get the matching `entry_id`s.
//!
//! Field kinds map to SQL three ways:
//! - **scalar columns** (`title`, `year`, …) → a direct `entries.<col>` predicate;
//! - **tag/multi-valued fields** (`genres`, `cast`, …) → an `EXISTS` over `tags`,
//!   set-membership only (a comparison on one is a config error);
//! - **`source` / `collections`** → an `EXISTS` sub-query (an entry has many
//!   sources; collection membership is per-`entry_id`).
//!
//! Everything binds through `?` placeholders — no caller value is ever
//! interpolated into SQL text.

use cel::Program;
use cel::common::ast::{Expr, IdedExpr, LiteralValue};
use rusqlite::Connection;
use rusqlite::functions::FunctionFlags;
use rusqlite::types::Value;

use super::error::CatalogError;

/// A translated `WHERE` clause plus its positional bind parameters.
pub struct WhereClause {
    pub sql: String,
    pub params: Vec<Value>,
}

/// Register the `regexp(pattern, text)` function sqlite's `X REGEXP Y` syntax
/// dispatches to (`X REGEXP Y` ⇒ `regexp(Y, X)`), backing the `matches`
/// operator. Called once per connection on catalog open.
pub fn register_regexp(conn: &Connection) -> Result<(), CatalogError> {
    conn.create_scalar_function(
        "regexp",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let pattern = ctx.get::<String>(0)?;
            let text = ctx.get::<String>(1)?;
            let re = regex::Regex::new(&pattern)
                .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?;
            Ok(re.is_match(&text))
        },
    )?;
    Ok(())
}

/// Compile a CEL expression and translate it to a SQL `WHERE` clause.
pub fn translate(cel_src: &str) -> Result<WhereClause, CatalogError> {
    let program = Program::compile(cel_src)
        .map_err(|e| CatalogError::Query(format!("could not parse CEL: {e}")))?;
    let mut params = Vec::new();
    let sql = predicate(program.expression(), &mut params)?;
    Ok(WhereClause { sql, params })
}

/// One field of the `item.*` query surface, resolved to its backing SQL.
enum FieldKind {
    /// A text column on `entries`.
    Str(&'static str),
    /// A numeric column on `entries`.
    Num(&'static str),
    /// A `tags` namespace; `None` means "any namespace" (`item.tags`).
    Tag(Option<&'static str>),
    /// `entry_sources.source` — membership via `EXISTS`.
    Source,
    /// Collection membership by name via `EXISTS`.
    Collection,
}

impl FieldKind {
    fn resolve(name: &str) -> Option<FieldKind> {
        use FieldKind::*;
        Some(match name {
            "title" => Str("title"),
            "show" => Str("show"),
            "type" => Str("type"),
            "content_rating" => Str("content_rating"),
            "edition" => Str("edition"),
            "studio" => Str("studio"),
            "year" => Num("year"),
            "season" => Num("season"),
            "episode" => Num("episode"),
            "absolute_episode" => Num("absolute_episode"),
            "duration_ms" => Num("duration_ms"),
            "genres" => Tag(Some("genre")),
            "labels" => Tag(Some("label")),
            "cast" => Tag(Some("cast")),
            "directors" => Tag(Some("director")),
            "tags" => Tag(None),
            "source" => Source,
            "collections" => Collection,
            _ => return None,
        })
    }
}

fn err(msg: impl Into<String>) -> CatalogError {
    CatalogError::Query(msg.into())
}

/// Translate one boolean CEL node into an SQL predicate fragment.
fn predicate(node: &IdedExpr, params: &mut Vec<Value>) -> Result<String, CatalogError> {
    match &node.expr {
        Expr::Call(call) => match &call.target {
            // Method call: `item.<field>.<method>(arg)` — contains/startsWith/matches.
            Some(target) => method_predicate(&call.func_name, target, &call.args, params),
            // Free call: a boolean/comparison operator.
            None => operator_predicate(&call.func_name, &call.args, params),
        },
        other => Err(err(format!(
            "query expression must be a boolean predicate, found {}",
            describe(other)
        ))),
    }
}

/// `_&&_`, `_||_`, `!_`, and the comparison operators (`_==_`, `@in`, …).
fn operator_predicate(
    func: &str,
    args: &[IdedExpr],
    params: &mut Vec<Value>,
) -> Result<String, CatalogError> {
    match func {
        "_&&_" | "_||_" => {
            let joiner = if func == "_&&_" { "AND" } else { "OR" };
            let left = predicate(&args[0], params)?;
            let right = predicate(&args[1], params)?;
            Ok(format!("({left} {joiner} {right})"))
        }
        "!_" => {
            let inner = predicate(&args[0], params)?;
            Ok(format!("(NOT {inner})"))
        }
        "_==_" | "_!=_" | "_>=_" | "_<=_" | "_>_" | "_<_" => {
            comparison(func, &args[0], &args[1], params)
        }
        "@in" => in_list(&args[0], &args[1], params),
        other => Err(err(format!("unsupported operator {other:?} in query"))),
    }
}

/// A scalar comparison: `item.<field> <op> <literal>`. The field may be on
/// either side — `2002 <= item.year` is as valid CEL as `item.year >= 2002` —
/// so we locate the `item.<field>` operand and, when it's on the right, mirror
/// the operator (`<=` on the right reads as `>=` on the left) before building
/// the SQL.
fn comparison(
    func: &str,
    lhs: &IdedExpr,
    rhs: &IdedExpr,
    params: &mut Vec<Value>,
) -> Result<String, CatalogError> {
    let (field_name, literal, field_on_right) = field_and_literal(lhs, rhs)?;
    let kind = FieldKind::resolve(&field_name)
        .ok_or_else(|| err(format!("unknown query field item.{field_name}")))?;

    let func = if field_on_right {
        mirror_comparison(func)
    } else {
        func
    };
    let op = match func {
        "_==_" => "=",
        "_!=_" => "<>",
        "_>=_" => ">=",
        "_<=_" => "<=",
        "_>_" => ">",
        "_<_" => "<",
        _ => unreachable!("comparison called with non-comparison op"),
    };

    match kind {
        FieldKind::Str(col) => {
            if op != "=" && op != "<>" {
                return Err(err(format!(
                    "operator {op} needs a numeric field; item.{field_name} is text"
                )));
            }
            params.push(literal_text(literal)?);
            Ok(format!("entries.{col} {op} ?"))
        }
        FieldKind::Num(col) => {
            params.push(literal_int(literal)?);
            Ok(format!("entries.{col} {op} ?"))
        }
        // `item.source == "plex"` is the one comparison allowed on a
        // membership field — it reads as "has a source of".
        FieldKind::Source if op == "=" => {
            params.push(literal_text(literal)?);
            Ok(source_exists())
        }
        _ => Err(err(format!(
            "item.{field_name} is multi-valued; use membership (contains), not {op}"
        ))),
    }
}

/// `item.<field> in [<literals>]`.
fn in_list(
    lhs: &IdedExpr,
    rhs: &IdedExpr,
    params: &mut Vec<Value>,
) -> Result<String, CatalogError> {
    let field_name = field_of(lhs)?;
    let kind = FieldKind::resolve(&field_name)
        .ok_or_else(|| err(format!("unknown query field item.{field_name}")))?;
    let Expr::List(list) = &rhs.expr else {
        return Err(err(format!(
            "`in` on item.{field_name} needs a list literal"
        )));
    };
    if list.elements.is_empty() {
        // `x in []` is always false — a valid, matchless predicate.
        return Ok("0".to_string());
    }
    let col = match kind {
        FieldKind::Str(col) => {
            for el in &list.elements {
                params.push(literal_text(el)?);
            }
            col
        }
        FieldKind::Num(col) => {
            for el in &list.elements {
                params.push(literal_int(el)?);
            }
            col
        }
        _ => {
            return Err(err(format!(
                "item.{field_name} is multi-valued; use membership (contains), not `in`"
            )));
        }
    };
    let placeholders = vec!["?"; list.elements.len()].join(", ");
    Ok(format!("entries.{col} IN ({placeholders})"))
}

/// `item.<field>.contains|startsWith|matches(<literal>)`.
fn method_predicate(
    method: &str,
    target: &IdedExpr,
    args: &[IdedExpr],
    params: &mut Vec<Value>,
) -> Result<String, CatalogError> {
    let field_name = field_of(target)?;
    let kind = FieldKind::resolve(&field_name)
        .ok_or_else(|| err(format!("unknown query field item.{field_name}")))?;
    let arg = args
        .first()
        .ok_or_else(|| err(format!("item.{field_name}.{method}() needs one argument")))?;

    match method {
        "contains" => match kind {
            FieldKind::Str(col) => {
                params.push(literal_like(arg)?);
                // Substring match — e.g. `item.title.contains("X")` → title LIKE %X%.
                // `%`/`_` in the value are escaped so they match literally.
                Ok(format!("entries.{col} LIKE '%' || ? || '%' ESCAPE '\\'"))
            }
            FieldKind::Tag(ns) => tag_exists(ns, arg, params),
            FieldKind::Collection => collection_exists(arg, params),
            FieldKind::Source | FieldKind::Num(_) => Err(err(format!(
                "item.{field_name} does not support contains()"
            ))),
        },
        "startsWith" => {
            let FieldKind::Str(col) = kind else {
                return Err(err(format!(
                    "item.{field_name}.startsWith() needs a text field"
                )));
            };
            params.push(literal_like(arg)?);
            Ok(format!("entries.{col} LIKE ? || '%' ESCAPE '\\'"))
        }
        "matches" => {
            let FieldKind::Str(col) = kind else {
                return Err(err(format!(
                    "item.{field_name}.matches() needs a text field"
                )));
            };
            params.push(literal_text(arg)?);
            Ok(format!("entries.{col} REGEXP ?"))
        }
        other => Err(err(format!("unsupported method .{other}() in query"))),
    }
}

/// `EXISTS` over `tags` for a membership test; `None` namespace = any.
fn tag_exists(
    namespace: Option<&str>,
    value: &IdedExpr,
    params: &mut Vec<Value>,
) -> Result<String, CatalogError> {
    let val = literal_text(value)?;
    match namespace {
        Some(ns) => {
            params.push(Value::Text(ns.to_string()));
            params.push(val);
            Ok(
                "EXISTS (SELECT 1 FROM tags t WHERE t.entry_id = entries.entry_id \
                 AND t.namespace = ? AND t.value = ?)"
                    .to_string(),
            )
        }
        None => {
            params.push(val);
            Ok(
                "EXISTS (SELECT 1 FROM tags t WHERE t.entry_id = entries.entry_id \
                 AND t.value = ?)"
                    .to_string(),
            )
        }
    }
}

/// `EXISTS` over `collection_items` joined to `collections` by name.
fn collection_exists(value: &IdedExpr, params: &mut Vec<Value>) -> Result<String, CatalogError> {
    params.push(literal_text(value)?);
    Ok("EXISTS (SELECT 1 FROM collection_items ci \
         JOIN collections c ON c.collection_id = ci.collection_id \
         WHERE ci.entry_id = entries.entry_id AND c.name = ?)"
        .to_string())
}

fn source_exists() -> String {
    "EXISTS (SELECT 1 FROM entry_sources s WHERE s.entry_id = entries.entry_id AND s.source = ?)"
        .to_string()
}

/// Given the two operands of a comparison, find the `item.<field>` one and
/// return its field name, the *other* (literal) operand, and whether the field
/// was on the right. A comparison must have exactly one field operand; if
/// neither side is `item.<field>`, the standard `field_of` error is returned.
fn field_and_literal<'a>(
    lhs: &'a IdedExpr,
    rhs: &'a IdedExpr,
) -> Result<(String, &'a IdedExpr, bool), CatalogError> {
    match field_of(lhs) {
        Ok(field) => Ok((field, rhs, false)),
        Err(lhs_err) => match field_of(rhs) {
            Ok(field) => Ok((field, lhs, true)),
            Err(_) => Err(lhs_err),
        },
    }
}

/// Mirror a directional comparison operator so a field found on the right-hand
/// side reads correctly once rewritten as `field <op> literal`. Symmetric
/// operators (`==`, `!=`) are returned unchanged.
fn mirror_comparison(func: &str) -> &str {
    match func {
        "_>=_" => "_<=_",
        "_<=_" => "_>=_",
        "_>_" => "_<_",
        "_<_" => "_>_",
        other => other,
    }
}

/// Extract the field name from an `item.<field>` selection.
fn field_of(node: &IdedExpr) -> Result<String, CatalogError> {
    if let Expr::Select(sel) = &node.expr
        && let Expr::Ident(root) = &sel.operand.expr
        && root == "item"
    {
        return Ok(sel.field.clone());
    }
    Err(err("query fields must be accessed as item.<field>"))
}

fn literal_text(node: &IdedExpr) -> Result<Value, CatalogError> {
    match &node.expr {
        Expr::Literal(LiteralValue::String(s)) => Ok(Value::Text(s.inner().to_string())),
        other => Err(err(format!(
            "expected a string literal, found {}",
            describe(other)
        ))),
    }
}

/// A string literal escaped for use inside a `LIKE ... ESCAPE '\'` pattern, so
/// `%` and `_` in the query value match literally instead of as wildcards.
fn literal_like(node: &IdedExpr) -> Result<Value, CatalogError> {
    let Value::Text(s) = literal_text(node)? else {
        unreachable!("literal_text only yields Value::Text")
    };
    let escaped = s
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    Ok(Value::Text(escaped))
}

fn literal_int(node: &IdedExpr) -> Result<Value, CatalogError> {
    match &node.expr {
        Expr::Literal(LiteralValue::Int(i)) => Ok(Value::Integer(*i.inner())),
        other => Err(err(format!(
            "expected an integer literal, found {}",
            describe(other)
        ))),
    }
}

fn describe(expr: &Expr) -> &'static str {
    match expr {
        Expr::Unspecified => "an empty expression",
        Expr::Call(_) => "a call",
        Expr::Comprehension(_) => "a comprehension",
        Expr::Ident(_) => "an identifier",
        Expr::List(_) => "a list",
        Expr::Literal(_) => "a literal",
        Expr::Map(_) => "a map",
        Expr::Select(_) => "a field selection",
        Expr::Struct(_) => "a struct",
    }
}

#[cfg(test)]
mod tests {
    use crate::catalog::{Catalog, Entry, EntrySource, Source, TagNs};

    fn seed_movie(c: &Catalog, id: &str, title: &str, year: i64, genre: &str) {
        let mut e = Entry::new(id, "movie", title, Source::Plex);
        e.year = Some(year);
        c.upsert_entry(&e).unwrap();
        c.add_tag(id, TagNs::Genre, genre).unwrap();
        c.add_source(&EntrySource {
            source: Source::Plex,
            source_id: format!("plex-{id}"),
            entry_id: id.to_string(),
            playback_path: format!("/plex/{id}.mkv"),
            last_seen: None,
        })
        .unwrap();
    }

    fn seeded() -> Catalog {
        let c = Catalog::open_in_memory().unwrap();
        seed_movie(
            &c,
            "imdb:tt0120737",
            "The Fellowship of the Ring",
            2001,
            "Fantasy",
        );
        seed_movie(&c, "imdb:tt0167261", "The Two Towers", 2002, "Fantasy");
        seed_movie(
            &c,
            "imdb:tt0325980",
            "Pirates of the Caribbean",
            2003,
            "Adventure",
        );
        c
    }

    #[test]
    fn contains_compiles_to_substring_like() {
        let c = seeded();
        let got = c.resolve_query(r#"item.title.contains("Ring")"#).unwrap();
        assert_eq!(got, vec!["imdb:tt0120737"]);
    }

    #[test]
    fn numeric_comparison() {
        let c = seeded();
        let got = c.resolve_query("item.year >= 2002").unwrap();
        assert_eq!(got, vec!["imdb:tt0167261", "imdb:tt0325980"]);
    }

    #[test]
    fn boolean_and_combines_predicates() {
        let c = seeded();
        let got = c
            .resolve_query(r#"item.genres.contains("Fantasy") && item.year < 2002"#)
            .unwrap();
        assert_eq!(got, vec!["imdb:tt0120737"]);
    }

    #[test]
    fn tag_membership_and_negation() {
        let c = seeded();
        let fantasy = c
            .resolve_query(r#"item.genres.contains("Fantasy")"#)
            .unwrap();
        assert_eq!(fantasy, vec!["imdb:tt0120737", "imdb:tt0167261"]);
        let not_fantasy = c
            .resolve_query(r#"!item.genres.contains("Fantasy")"#)
            .unwrap();
        assert_eq!(not_fantasy, vec!["imdb:tt0325980"]);
    }

    #[test]
    fn in_list_on_scalar() {
        let c = seeded();
        let got = c.resolve_query("item.year in [2001, 2003]").unwrap();
        assert_eq!(got, vec!["imdb:tt0120737", "imdb:tt0325980"]);
    }

    #[test]
    fn matches_uses_regexp() {
        let c = seeded();
        let got = c
            .resolve_query(r#"item.title.matches("^The Two")"#)
            .unwrap();
        assert_eq!(got, vec!["imdb:tt0167261"]);
    }

    #[test]
    fn like_metacharacters_match_literally() {
        let c = seeded();
        // `_` is a LIKE wildcard; unescaped, "T_o" would match "Two Towers".
        // Escaped, it matches only a literal underscore — nothing here.
        let got = c.resolve_query(r#"item.title.contains("T_o")"#).unwrap();
        assert!(got.is_empty(), "underscore must be literal, got {got:?}");
    }

    #[test]
    fn empty_result_is_not_an_error() {
        let c = seeded();
        let got = c
            .resolve_query(r#"item.title.contains("Nonexistent")"#)
            .unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn unknown_field_is_a_config_error() {
        let c = seeded();
        let e = c.resolve_query(r#"item.bogus == "x""#).unwrap_err();
        assert!(e.to_string().contains("unknown query field"));
    }

    #[test]
    fn comparison_on_tag_field_is_an_error() {
        let c = seeded();
        let e = c.resolve_query(r#"item.genres == "Fantasy""#).unwrap_err();
        assert!(e.to_string().contains("multi-valued"));
    }

    #[test]
    fn source_membership() {
        let c = seeded();
        let got = c.resolve_query(r#"item.source == "plex""#).unwrap();
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn numeric_comparison_with_field_on_right() {
        let c = seeded();
        // `2002 <= item.year` must resolve identically to `item.year >= 2002`.
        let got = c.resolve_query("2002 <= item.year").unwrap();
        assert_eq!(got, vec!["imdb:tt0167261", "imdb:tt0325980"]);
    }

    #[test]
    fn strict_comparison_field_on_right_mirrors_operator() {
        let c = seeded();
        // `2002 > item.year` reads as `item.year < 2002` — only the 2001 film.
        let got = c.resolve_query("2002 > item.year").unwrap();
        assert_eq!(got, vec!["imdb:tt0120737"]);
    }

    #[test]
    fn equality_field_on_right() {
        let c = seeded();
        // `==` is symmetric; the field-on-right path must still resolve it.
        let got = c.resolve_query(r#""plex" == item.source"#).unwrap();
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn comparison_with_no_field_operand_is_an_error() {
        let c = seeded();
        // Neither side is `item.<field>` — the standard access error stands.
        let e = c.resolve_query("2001 == 2002").unwrap_err();
        assert!(
            e.to_string().contains("item.<field>"),
            "expected field-access error, got {e}"
        );
    }
}

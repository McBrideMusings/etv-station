//! Named categories (#170): a keyword set and its match threshold, declared
//! once in a file outside any channel and referenced by name from pool
//! expressions on any channel.
//!
//! `category("kung-fu")` in a pool expression stands for
//! `keyword_score([…the category's keywords…]) >= <threshold>`. The reference
//! is expanded at load time, before the channel validates, so the catalog and
//! every downstream consumer only ever see the inline `keyword_score` form.
//! Retuning a category and reloading retunes every channel that names it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// A category file: `categories:` holds one or more declarations.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct CategoryFile {
    #[serde(default)]
    pub categories: Vec<CategoryDecl>,
}

/// One declaration: the same three things the inline `keyword_score` form takes.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CategoryDecl {
    pub name: String,
    pub keywords: Vec<String>,
    /// An item belongs to the category when it matches at least this many of
    /// the keywords.
    pub threshold: u32,
}

/// Every declared category, keyed by name.
#[derive(Debug, Default)]
pub struct Categories {
    by_name: BTreeMap<String, CategoryDecl>,
}

static REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"category\s*\(\s*(?:"([^"]*)"|'([^']*)')\s*\)"#).expect("category regex compiles")
});

impl Categories {
    /// Build from `(file, parsed)` pairs in load order. A name declared twice,
    /// in one file or two, fails naming both places.
    pub fn from_files(files: Vec<(PathBuf, CategoryFile)>) -> Result<Self, String> {
        let mut by_name: BTreeMap<String, CategoryDecl> = BTreeMap::new();
        let mut at: BTreeMap<String, String> = BTreeMap::new();
        for (path, file) in files {
            for (idx, decl) in file.categories.into_iter().enumerate() {
                let place = format!("{} (entry #{})", path.display(), idx + 1);
                check_decl(&decl).map_err(|e| format!("category in {place}: {e}"))?;
                if let Some(first) = at.get(&decl.name) {
                    return Err(format!(
                        "category {:?} is declared twice: at {first} and at {place}",
                        decl.name
                    ));
                }
                at.insert(decl.name.clone(), place);
                by_name.insert(decl.name.clone(), decl);
            }
        }
        Ok(Self { by_name })
    }

    /// Replace every `category("name")` in `expr` with its inline form.
    /// An undefined name fails, listing the categories that exist.
    pub fn expand(&self, expr: &str) -> Result<String, String> {
        let mut out = String::with_capacity(expr.len());
        let mut last = 0;
        for caps in REFERENCE.captures_iter(expr) {
            let whole = caps.get(0).expect("group 0 always matches");
            // `x.category("a")` is a method call on something else, not ours.
            let start = whole.start();
            let before = expr[..start].chars().next_back();
            if before.is_some_and(|c| c == '.' || c == '_' || c.is_alphanumeric()) {
                continue;
            }
            let name = caps
                .get(1)
                .or_else(|| caps.get(2))
                .map_or("", |m| m.as_str());
            let decl = self.by_name.get(name).ok_or_else(|| {
                if self.by_name.is_empty() {
                    format!("unknown category {name:?}; no categories are defined")
                } else {
                    let names: Vec<&str> = self.by_name.keys().map(String::as_str).collect();
                    format!(
                        "unknown category {name:?}; defined categories: {}",
                        names.join(", ")
                    )
                }
            })?;
            out.push_str(&expr[last..start]);
            out.push_str(&inline_form(decl));
            last = whole.end();
        }
        out.push_str(&expr[last..]);
        Ok(out)
    }
}

fn check_decl(decl: &CategoryDecl) -> Result<(), String> {
    if decl.name.trim().is_empty() {
        return Err("name is empty".into());
    }
    let mut distinct: Vec<String> = Vec::new();
    for k in &decl.keywords {
        let k = k.to_ascii_lowercase();
        if !distinct.contains(&k) {
            distinct.push(k);
        }
    }
    if distinct.is_empty() {
        return Err(format!("{:?} has an empty keyword set", decl.name));
    }
    if decl.threshold == 0 || decl.threshold as usize > distinct.len() {
        return Err(format!(
            "{:?} has threshold {} but {} distinct keyword(s); it must be between 1 and {}",
            decl.name,
            decl.threshold,
            distinct.len(),
            distinct.len()
        ));
    }
    Ok(())
}

fn inline_form(decl: &CategoryDecl) -> String {
    let list: Vec<String> = decl
        .keywords
        .iter()
        .map(|k| format!("\"{}\"", k.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    format!(
        "(keyword_score([{}]) >= {})",
        list.join(", "),
        decl.threshold
    )
}

/// Read one category file (YAML or TOML by extension, like every config file).
pub fn read_category_file(path: &Path) -> Result<CategoryFile, String> {
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

    fn decl(name: &str, keywords: &[&str], threshold: u32) -> CategoryDecl {
        CategoryDecl {
            name: name.into(),
            keywords: keywords.iter().map(|s| s.to_string()).collect(),
            threshold,
        }
    }

    fn cats(decls: Vec<CategoryDecl>) -> Categories {
        Categories::from_files(vec![(
            PathBuf::from("/c/a.yaml"),
            CategoryFile { categories: decls },
        )])
        .unwrap()
    }

    #[test]
    fn a_reference_expands_to_the_inline_keyword_score_form() {
        let c = cats(vec![decl("kung-fu", &["kung fu", "martial arts"], 1)]);
        assert_eq!(
            c.expand(r#"category("kung-fu") && item.type == "movie""#)
                .unwrap(),
            r#"(keyword_score(["kung fu", "martial arts"]) >= 1) && item.type == "movie""#
        );
        assert_eq!(c.expand("no reference here").unwrap(), "no reference here");
    }

    #[test]
    fn an_undefined_category_names_it_and_lists_the_ones_that_exist() {
        let c = cats(vec![decl("b", &["x"], 1), decl("a", &["y"], 1)]);
        let msg = c.expand(r#"category("zzz")"#).unwrap_err();
        assert!(msg.contains("\"zzz\"") && msg.contains("a, b"), "{msg}");
        let msg = cats(vec![]).expand(r#"category("zzz")"#).unwrap_err();
        assert!(msg.contains("no categories are defined"), "{msg}");
    }

    #[test]
    fn a_method_call_named_category_is_left_alone() {
        let c = cats(vec![decl("a", &["x"], 1)]);
        assert_eq!(
            c.expand(r#"item.category("a")"#).unwrap(),
            r#"item.category("a")"#
        );
    }

    #[test]
    fn a_duplicate_name_fails_naming_both_locations() {
        let file = |d| CategoryFile {
            categories: vec![d],
        };
        let err = Categories::from_files(vec![
            (PathBuf::from("/c/one.yaml"), file(decl("a", &["x"], 1))),
            (PathBuf::from("/c/two.yaml"), file(decl("a", &["y"], 1))),
        ])
        .unwrap_err();
        assert!(
            err.contains("/c/one.yaml") && err.contains("/c/two.yaml") && err.contains("\"a\""),
            "{err}"
        );
    }

    #[test]
    fn a_threshold_the_set_cannot_reach_fails_naming_the_category() {
        let err = Categories::from_files(vec![(
            PathBuf::from("/c/a.yaml"),
            CategoryFile {
                categories: vec![decl("a", &["x", "X"], 2)],
            },
        )])
        .unwrap_err();
        assert!(
            err.contains("\"a\"") && err.contains("threshold 2"),
            "{err}"
        );
    }
}

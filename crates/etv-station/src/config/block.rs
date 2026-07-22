use ersatztv_playout::playout::ProgramMetadata;
use serde::{Deserialize, Serialize};

use super::constraints::Constraints;
use super::entry::Entry;

/// Within-block duplicate policy (#46 locked decision). Block-scoped:
/// cross-block repeats are always allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Duplicates {
    /// First occurrence wins; later duplicates of the same item id are dropped.
    #[default]
    Collapse,
    /// Keep every occurrence.
    Keep,
}

/// The body of a block: optional `[program]` metadata defaults, a `duplicates`
/// policy, and the flat `[[entries]]` list. This is the on-disk shape of a
/// referenced block *file*; the same fields appear inline on a channel's
/// `[[rule.blocks]]` entry (see [`super::rule::BlockInclude`]).
#[derive(Debug, Deserialize, Serialize)]
pub struct BlockFile {
    /// Program-metadata defaults applied to items that omit their own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<ProgramMetadata>,

    #[serde(default)]
    pub duplicates: Duplicates,

    /// Post-order adjacency constraints (#73). `None` leaves the block
    /// unconstrained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraints: Option<Constraints>,

    pub entries: Vec<Entry>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::entry::Entry;

    #[test]
    fn parses_block_file_with_item() {
        let toml = r#"
duplicates = "keep"

[program]
title = "Defaults"
categories = ["Movie"]

[[entries]]
kind = "item"
[entries.source]
kind = "lavfi"
params = "testsrc"
"#;
        let block: BlockFile = toml::from_str(toml).unwrap();
        assert_eq!(block.duplicates, Duplicates::Keep);
        assert_eq!(block.entries.len(), 1);
        assert!(matches!(block.entries[0], Entry::Item(_)));
        assert_eq!(
            block.program.as_ref().unwrap().title.as_deref(),
            Some("Defaults")
        );
    }

    #[test]
    fn duplicates_defaults_to_collapse() {
        let toml = r#"
[[entries]]
kind = "item"
[entries.source]
kind = "lavfi"
params = "testsrc"
"#;
        let block: BlockFile = toml::from_str(toml).unwrap();
        assert_eq!(block.duplicates, Duplicates::Collapse);
    }

    #[test]
    fn parses_block_file_from_yaml() {
        let yaml = r#"
duplicates: keep
program:
  title: Defaults
  categories: [Movie]
entries:
  - kind: item
    source:
      kind: lavfi
      params: testsrc
"#;
        let block: BlockFile = serde_norway::from_str(yaml).unwrap();
        assert_eq!(block.duplicates, Duplicates::Keep);
        assert_eq!(block.entries.len(), 1);
        assert!(matches!(block.entries[0], Entry::Item(_)));
        assert_eq!(
            block.program.as_ref().unwrap().title.as_deref(),
            Some("Defaults")
        );
    }

    #[test]
    fn parses_query_and_include_entries_from_yaml() {
        // Exercises the internally-tagged `Entry` enum and the `Mode`
        // deserialize_any visitor (mapping form) through the YAML deserializer.
        let yaml = r#"
entries:
  - kind: query
    query: "type == 'movie'"
    order: "release_date:asc"
  - kind: include
    block: "../blocks/bumpers.toml"
    mode:
      count: 1
"#;
        let block: BlockFile = serde_norway::from_str(yaml).unwrap();
        assert_eq!(block.entries.len(), 2);
        assert_eq!(block.entries[0].kind_name(), "query");
        assert_eq!(block.entries[1].kind_name(), "include");
    }

    #[test]
    fn parses_query_and_include_entries() {
        let toml = r#"
[[entries]]
kind = "query"
query = "type == 'movie'"
order = "release_date:asc"

[[entries]]
kind = "include"
block = "../blocks/bumpers.toml"
mode = { count = 1 }
"#;
        let block: BlockFile = toml::from_str(toml).unwrap();
        assert_eq!(block.entries.len(), 2);
        assert_eq!(block.entries[0].kind_name(), "query");
        assert_eq!(block.entries[1].kind_name(), "include");
    }
}

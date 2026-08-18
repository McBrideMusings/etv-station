use ersatztv_playout::playout::ProgramMetadata;
use serde::{Deserialize, Serialize};

use super::constraints::Constraints;
use super::entry::{Entry, Fallback};
use super::overlay::OverlayDecl;
use super::pool::{PatternStep, Pool};
use crate::guide::GuideConfig;

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
/// policy, and **either** the flat `[[entries]]` list **or** the `pools` +
/// `pattern` interleave (#72). This is the on-disk shape of a referenced block
/// *file*; the same fields appear inline on a channel's `[[rule.blocks]]` entry
/// (see [`super::rule::BlockInclude`]).
#[derive(Debug, Deserialize, Serialize)]
pub struct BlockFile {
    /// Program-metadata defaults applied to items that omit their own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<ProgramMetadata>,

    /// Block-level `guide:` defaults (#158), the middle of the three cascade
    /// levels (channel → block → item).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guide: Option<GuideConfig>,

    /// Block-level overlay (#48), the deepest of that cascade's three levels
    /// (station → channel → block). Unlike `guide:` above, this replaces the
    /// channel's config outright rather than merging field by field — see
    /// [`crate::config::OverlayDecl`] for why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay: Option<OverlayDecl>,

    /// `None` means unset — an entries block then resolves to the [`Duplicates`]
    /// default (`collapse`) while a pattern block forces `keep`. Splicing the
    /// literal `None` through (rather than defaulting here) is what lets
    /// validation tell "the author wrote `collapse`" apart from "the author
    /// said nothing".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicates: Option<Duplicates>,

    /// Post-order adjacency constraints (#73). `None` leaves the block
    /// unconstrained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraints: Option<Constraints>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<Entry>,

    /// Resolved instead of `entries` when `entries` resolves to nothing
    /// eligible (#97). `None` keeps today's behavior — an empty resolution
    /// stays empty. See [`Fallback`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<Fallback>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pools: Vec<Pool>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pattern: Vec<PatternStep>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycles: Option<usize>,

    /// Sequencer form (#169): a plugin script that draws this block's
    /// timeline from `pools` itself, instead of walking `pattern`. Path is
    /// relative to the channel config's directory, exactly like a pool's own
    /// `plugin:`. Mutually exclusive with `pattern` — a block authors one or
    /// the other, enforced in validation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequencer: Option<std::path::PathBuf>,
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
        assert_eq!(block.duplicates, Some(Duplicates::Keep));
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
        // Unset on disk stays unset; `BlockInclude::duplicates()` applies the
        // per-block-kind default.
        assert_eq!(block.duplicates, None);
    }

    #[test]
    fn fallback_defaults_to_none() {
        let toml = r#"
[[entries]]
kind = "item"
[entries.source]
kind = "lavfi"
params = "testsrc"
"#;
        let block: BlockFile = toml::from_str(toml).unwrap();
        assert!(block.fallback.is_none());
    }

    #[test]
    fn parses_a_query_fallback() {
        use crate::config::{Fallback, Order};

        let yaml = r#"
entries:
  - kind: item
    source:
      kind: lavfi
      params: testsrc
fallback:
  kind: query
  query: "item.type == 'movie'"
  order: "random"
"#;
        let block: BlockFile = serde_norway::from_str(yaml).unwrap();
        match block.fallback {
            Some(Fallback::Query(q)) => {
                assert_eq!(q.query, "item.type == 'movie'");
                assert_eq!(q.order, Some(Order::Random));
            }
            other => panic!("expected a query fallback, got {other:?}"),
        }
    }

    #[test]
    fn parses_an_item_fallback() {
        use crate::config::{Fallback, SourceConfig};

        let toml = r#"
[[entries]]
kind = "item"
[entries.source]
kind = "lavfi"
params = "testsrc"

[fallback]
kind = "item"
[fallback.source]
kind = "lavfi"
params = "standby"
"#;
        let block: BlockFile = toml::from_str(toml).unwrap();
        match block.fallback {
            Some(Fallback::Item(item)) => match item.source {
                SourceConfig::Lavfi { params } => assert_eq!(params, "standby"),
                other => panic!("expected a lavfi source, got {other:?}"),
            },
            other => panic!("expected an item fallback, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_pattern_block_file() {
        let yaml = r#"
pools:
  - name: movies
    expr: 'item.type == "movie"'
  - name: shows
    expr: 'item.type == "episode"'
    order: "season:asc,episode:asc"
    advance: resume
pattern:
  - pool: movies
    take: 1
  - pool: shows
    take: 3
"#;
        let block: BlockFile = serde_norway::from_str(yaml).unwrap();
        assert!(block.entries.is_empty());
        assert_eq!(block.pools.len(), 2);
        assert_eq!(block.pattern.len(), 2);
        assert_eq!(block.pattern[1].pool, "shows");
        assert_eq!(block.pattern[1].take, crate::config::Take::Count(3));
        assert_eq!(block.cycles, None);
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
        assert_eq!(block.duplicates, Some(Duplicates::Keep));
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

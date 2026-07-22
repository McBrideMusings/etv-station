use std::path::PathBuf;

use ersatztv_playout::playout::ProgramMetadata;
use serde::{Deserialize, Serialize};

use super::block::{BlockFile, Duplicates};
use super::constraints::Constraints;
use super::entry::Entry;
use super::filter::Filter;
use super::mode::Mode;
use super::order::Order;
use super::pool::{PatternStep, Pool};

/// A channel's sequencing rule: an ordered list of block-includes that compose
/// into the channel's playout. Replaces the v1 `loop_forever` rule (#46).
#[derive(Debug, Deserialize, Serialize)]
pub struct RuleConfig {
    pub blocks: Vec<BlockInclude>,
}

/// One `[[rule.blocks]]` entry. The block source is **either** a path to a
/// reusable block file (`block = "../blocks/x.toml"`) **or** an inline body
/// (`[rule.blocks.program]` + `[[rule.blocks.entries]]`) — exactly one, enforced
/// in validation (#46 "both" decision). The `mode` / `order` / `filter`
/// composition fields apply regardless of which form supplies the body.
///
/// After [`super::load::load`] resolves a path form, the loaded body is spliced
/// into `program` / `duplicates` / `entries` and `block` is cleared, so all
/// consumers downstream of load see a normalized inline shape.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BlockInclude {
    /// Path form: a reusable block file, relative to the channel file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block: Option<PathBuf>,

    /// Inline form: program-metadata defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<ProgramMetadata>,

    /// Inline form: within-block duplicate policy. `None` resolves to the
    /// [`Duplicates`] default (`collapse`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicates: Option<Duplicates>,

    /// Inline form: post-order adjacency constraints (#73). `None` resolves to
    /// the [`Constraints`] default (unconstrained).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraints: Option<Constraints>,

    /// Inline form: the flat entries list (empty in path form until resolved).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<Entry>,

    /// Pattern form: the named resolved sets this block interleaves (#72).
    /// Mutually exclusive with `entries` — a block is either an entries block
    /// or a pattern block, enforced in validation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pools: Vec<Pool>,

    /// Pattern form: the repeating template walked to fill the window.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pattern: Vec<PatternStep>,

    /// Pattern form: how many times to walk the pattern. Unset derives it —
    /// enough cycles for the largest pool to drain once (see
    /// [`crate::pattern`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycles: Option<usize>,

    #[serde(default)]
    pub mode: Mode,

    #[serde(default)]
    pub order: Order,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<Filter>,
}

impl BlockInclude {
    /// The block's resolved entries. Valid only after [`super::load::load`] has
    /// spliced a path-form body into `entries`.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The effective program-metadata defaults for this block.
    pub fn program(&self) -> Option<&ProgramMetadata> {
        self.program.as_ref()
    }

    /// The effective duplicate policy. An entries block defaults to `collapse`;
    /// a pattern block is always `keep`, because collapse would delete every
    /// repeat a looping pool deliberately produces (validation rejects an
    /// explicit `collapse` on a pattern block rather than silently overriding).
    pub fn duplicates(&self) -> Duplicates {
        if self.is_pattern() {
            return Duplicates::Keep;
        }
        self.duplicates.unwrap_or_default()
    }

    /// The effective adjacency constraints (defaulting to unconstrained).
    pub fn constraints(&self) -> Constraints {
        self.constraints.unwrap_or_default()
    }

    /// Whether this block interleaves pools via a pattern rather than playing a
    /// flat `entries` list. True as soon as either pattern field is present, so
    /// a half-specified block (pools without pattern) reaches validation and
    /// gets a clear error instead of being read as an empty entries block.
    pub fn is_pattern(&self) -> bool {
        !self.pools.is_empty() || !self.pattern.is_empty()
    }

    /// Splice a loaded block-file body into this include's inline fields and
    /// clear the path reference. Called by `load` after reading a path-form
    /// block file.
    pub(super) fn apply_body(&mut self, body: BlockFile) {
        self.program = body.program;
        self.duplicates = body.duplicates;
        self.constraints = body.constraints;
        self.entries = body.entries;
        self.pools = body.pools;
        self.pattern = body.pattern;
        self.cycles = body.cycles;
        self.block = None;
    }
}

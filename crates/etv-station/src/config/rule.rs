use std::path::PathBuf;

use ersatztv_playout::playout::ProgramMetadata;
use serde::{Deserialize, Serialize};

use super::block::{BlockFile, Duplicates};
use super::entry::Entry;
use super::filter::Filter;
use super::mode::Mode;
use super::order::Order;

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

    /// Inline form: the flat entries list (empty in path form until resolved).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<Entry>,

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

    /// The effective duplicate policy (defaulting to `collapse`).
    pub fn duplicates(&self) -> Duplicates {
        self.duplicates.unwrap_or_default()
    }

    /// Splice a loaded block-file body into this include's inline fields and
    /// clear the path reference. Called by `load` after reading a path-form
    /// block file.
    pub(super) fn apply_body(&mut self, body: BlockFile) {
        self.program = body.program;
        self.duplicates = Some(body.duplicates);
        self.entries = body.entries;
        self.block = None;
    }
}

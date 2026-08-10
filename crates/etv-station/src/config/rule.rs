use std::path::PathBuf;

use ersatztv_playout::playout::ProgramMetadata;
use serde::{Deserialize, Serialize};

use super::block::{BlockFile, Duplicates};
use super::constraints::Constraints;
use super::entry::{Entry, Fallback};
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

    /// Resolved instead of `entries` when `entries` resolves to nothing
    /// eligible (#97). `None` keeps today's behavior — an empty resolution
    /// stays empty. Entries-block only; rejected on a pattern block at
    /// validation, since a pool's own `on_short` already covers a pattern
    /// block going short. See [`Fallback`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<Fallback>,

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

    /// Sequencer form (#169): a plugin script that draws this block's
    /// timeline from `pools` itself, instead of interleaving them via
    /// `pattern`. Mutually exclusive with `pattern` — a block authors one or
    /// the other, enforced in validation. See [`crate::sequence`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequencer: Option<PathBuf>,

    #[serde(default)]
    pub mode: Mode,

    /// Sort applied to this block's resolved list. `None` means the author
    /// wrote nothing, which is distinct from an authored `order = "manual"`
    /// (`Some(Order::Manual)`): an unset order over an episode-typed
    /// resolved set takes the `(season, episode)` default at resolution
    /// (#95), while an explicit `manual` always keeps authored order
    /// regardless of item type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<Order>,

    /// Narrows the resolved `entries` list by `seasons`/`episode_ids`
    /// (#197), applied right after `entries` (and any `fallback`
    /// substitution) settle, before `duplicates`/`order` see the list.
    /// Entries-block only; rejected on a `pattern` or `sequencer` block at
    /// validation — the resolve pipeline only ever applies it to a flat
    /// `entries` list, never to an interleaved pool draw. See [`Filter`].
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
    /// a pattern or sequencer block is always `keep`, because collapse would
    /// delete every repeat the pool interleave — or the sequencer script —
    /// deliberately produces (validation rejects an explicit `collapse` on
    /// either rather than silently overriding).
    pub fn duplicates(&self) -> Duplicates {
        if self.is_pattern() || self.is_sequencer() {
            return Duplicates::Keep;
        }
        self.duplicates.unwrap_or_default()
    }

    /// The effective adjacency constraints (defaulting to unconstrained).
    ///
    /// On a **pattern** block this is the default every pool inherits rather
    /// than a rule applied to the block's own interleaved list — the pass runs
    /// per pool, inside [`crate::pattern`] (#115). Use [`Pool::constraints`] to
    /// read what actually governs a given pool.
    pub fn constraints(&self) -> Constraints {
        self.constraints.clone().unwrap_or_default()
    }

    /// How much aired history this block's constraints need at the generation
    /// seam, in **channel** positions.
    ///
    /// An entries block is constrained over the channel list directly, so its
    /// reach is already in channel positions. A pattern block's pools are
    /// constrained over their *own* lists, so a pool reaching back N draws
    /// reaches back further than N aired items: the pattern lays other pools'
    /// items between consecutive draws. The conversion is the pattern's own
    /// arithmetic — the cycles that hold N draws of this pool, times the items
    /// one cycle emits — because a tail sized in pool positions would let a wide
    /// window hold inside a generation and quietly lapse across the seam.
    pub fn adjacency_reach(&self) -> usize {
        if self.is_sequencer() {
            // A sequencer draws no fixed count per cycle — there is no cycle
            // — so there is no way to convert "N draws of this pool" into "N
            // channel positions" the way a pattern block's `take` counts
            // allow above (#169). Reporting the pool's own reach directly, in
            // pool-draw units, is a safe floor: it can undersize the tail
            // when the script draws this pool more than once per aired
            // position, which is unknowable here, but it never oversizes it,
            // and it is exactly what a pool with no `constraints` at all
            // already reports (zero).
            let block = self.constraints.as_ref();
            return self
                .pools
                .iter()
                .map(|pool| pool.constraints(block).reach())
                .max()
                .unwrap_or(0);
        }
        if !self.is_pattern() {
            return self.constraints().reach();
        }
        let block = self.constraints.as_ref();
        // `take: "all"` contributes nothing to either sum, and it cannot: the
        // count it stands for depends on which bucket the visit picks, which is
        // not known until the pool has been resolved against the catalog — and
        // this runs at load. Guessing a size would quietly under-report the
        // seam and let a repeat cross a generation boundary with nothing said,
        // so validation refuses a block that mixes `take: "all"` with any
        // `constraints` instead. Every pool here therefore has `reach() == 0`
        // whenever a take-all step is present, and both sums go unread.
        let cycle_total: usize = self
            .pattern
            .iter()
            .filter_map(|s| s.take.count())
            .sum::<usize>();
        self.pools
            .iter()
            .map(|pool| {
                let per_cycle: usize = self
                    .pattern
                    .iter()
                    .filter(|s| s.pool == pool.name)
                    .filter_map(|s| s.take.count())
                    .sum();
                let reach = pool.constraints(block).reach();
                // A pool no step draws from never reaches the seam at all.
                if reach == 0 || per_cycle == 0 {
                    return 0;
                }
                reach.div_ceil(per_cycle).saturating_mul(cycle_total)
            })
            .max()
            .unwrap_or(0)
    }

    /// Whether this block interleaves pools via a pattern rather than playing a
    /// flat `entries` list. True as soon as either pattern field is present
    /// AND no `sequencer` is named (#169: a block naming a sequencer sets
    /// `pools` too, but is not a pattern block — see [`Self::is_sequencer`]),
    /// so a half-specified block (pools without pattern or sequencer) still
    /// reaches validation and gets a clear error instead of being read as an
    /// empty entries block.
    pub fn is_pattern(&self) -> bool {
        (!self.pools.is_empty() || !self.pattern.is_empty()) && self.sequencer.is_none()
    }

    /// Whether this block draws its timeline from a `sequencer` plugin rather
    /// than interleaving pools via `pattern` (#169). See [`crate::sequence`].
    pub fn is_sequencer(&self) -> bool {
        self.sequencer.is_some()
    }

    /// Splice a loaded block-file body into this include's inline fields and
    /// clear the path reference. Called by `load` after reading a path-form
    /// block file.
    pub(super) fn apply_body(&mut self, body: BlockFile) {
        self.program = body.program;
        self.duplicates = body.duplicates;
        self.constraints = body.constraints;
        self.entries = body.entries;
        self.fallback = body.fallback;
        self.pools = body.pools;
        self.sequencer = body.sequencer;
        self.pattern = body.pattern;
        self.cycles = body.cycles;
        self.block = None;
    }
}

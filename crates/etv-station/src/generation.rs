//! One generation's pure step: what the daemon's catch-up loop
//! (`daemon::pattern_catch_up`) and the schedule simulator
//! ([`crate::simulate::simulate`]) both do between "the schedule is covered up
//! to `from`" and "here are the next items".
//!
//! [`generate`] projects the airings ledger into the resolver's
//! [`GenerationState`] and the scorer's [`ScoreInputs`], runs
//! [`resolve_channel_with_resume`] with `fill = target - from`, and looks up
//! each item's show. Everything after that reads or writes files — probing,
//! track selection, the align/fill pass, chunk emission, the ledger write — and
//! stays with the caller, as does advancing `from` by the span the caller laid
//! down.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use time::OffsetDateTime;

use crate::catalog::Catalog;
use crate::config::{ChannelConfig, ScoringConfig};
use crate::errors::StationError;
use crate::history::HistoryDb;
use crate::resolve::{ResolvedItem, resolve_channel_with_resume};
use crate::resume::{GenerationState, Progress, ResumeMap};
use crate::score::{ScoreInputs, WatchEvent};

/// Bound on how many generations one catch-up will chain before giving up, so
/// a pathological config (a sequence with no wall-clock length) can't spin.
pub const MAX_GENERATIONS_PER_TICK: usize = 512;

/// What one generation reads of the airings ledger. The daemon reads the
/// `airings` table ([`ChannelLedger`]); the simulator keeps the same
/// projections in memory.
pub trait AiringsLedger {
    /// `series_key -> last aired entry_id` on this channel, where the key is
    /// the show, or the entry itself for a movie ([`HistoryDb::series_cursor`]).
    fn series_cursor(&self) -> Result<BTreeMap<String, String>, StationError>;
    /// `show_id -> last aired entry_id` across every channel, movies excluded
    /// ([`HistoryDb::cross_channel_show_cursor`]).
    fn global_cursor(&self) -> Result<BTreeMap<String, String>, StationError>;
    /// The last `n` airings on this channel, oldest first ([`HistoryDb::tail`]).
    fn tail(&self, n: usize) -> Result<Vec<String>, StationError>;
}

/// One channel's view of the play-history store.
pub struct ChannelLedger<'a> {
    pub db: &'a HistoryDb,
    pub channel: &'a str,
}

impl AiringsLedger for ChannelLedger<'_> {
    fn series_cursor(&self) -> Result<BTreeMap<String, String>, StationError> {
        Ok(self.db.series_cursor(self.channel)?)
    }

    fn global_cursor(&self) -> Result<BTreeMap<String, String>, StationError> {
        Ok(self.db.cross_channel_show_cursor()?)
    }

    fn tail(&self, n: usize) -> Result<Vec<String>, StationError> {
        Ok(self.db.tail(self.channel, n)?)
    }
}

/// What every generation of one catch-up shares.
pub struct GenerationEnv<'a> {
    pub config: &'a ChannelConfig,
    /// The channel file's path: relative `plugin:` paths resolve against its
    /// directory.
    pub config_path: &'a Path,
    pub identity_roots: &'a [String],
    pub path_index: Option<&'a HashMap<String, String>>,
    /// Watch history for `ctx.history`, fetched once per tick.
    pub history: Arc<[WatchEvent]>,
    pub account_id: Option<i64>,
    pub tz: Option<&'static time_tz::Tz>,
    /// The tick's clock, handed to the scorer as `ctx.now`.
    pub now: OffsetDateTime,
}

/// One resolver run's output.
pub struct Generation {
    pub items: Vec<ResolvedItem>,
    /// The pool state after the last item; hand it to [`ResumeMap::adopt`]
    /// once the caller has laid the items down.
    pub resume: ResumeMap,
    /// The state entering each item, one per item.
    pub progress: Vec<Progress>,
    /// `entry_id -> show_id` for the items that belong to a show.
    pub show_ids: HashMap<String, String>,
    /// The `ctx.target_count` the scorer was handed.
    pub target_count: usize,
}

/// Run one generation covering `from` toward `target`, continuing from
/// `resume` and the ledger's airings so far.
///
/// `catalog` is passed apart from `env` so the daemon can borrow it only for
/// this synchronous call: a `&Catalog` held across its `.await`s would make
/// the channel task's future non-`Send`.
pub fn generate(
    env: &GenerationEnv<'_>,
    catalog: Option<&Catalog>,
    ledger: &impl AiringsLedger,
    resume: &ResumeMap,
    from: OffsetDateTime,
    target: OffsetDateTime,
) -> Result<Generation, StationError> {
    let config = env.config;
    let state = GenerationState {
        resume: resume.clone(),
        cursor: ledger.series_cursor()?,
        global_cursor: ledger.global_cursor()?,
        tail: ledger.tail(config.adjacency_reach())?,
    };
    // Sized to one chunk, not to the whole remaining window: the generation
    // lays whatever the plugin returns end-to-end, so asking for a month's
    // worth would make a single generation try to cover the month.
    let scoring = ScoreInputs {
        target_count: target_count(config, from, target),
        history: Arc::clone(&env.history),
        recent: ledger.tail(recent_depth(config))?,
        now: env.now.unix_timestamp(),
        // A sequencer block (#169) reads this to place its pools against the
        // local clock (ADR 0004).
        tz: env.tz,
        account_id: env.account_id,
    };
    let (items, resume, progress) = resolve_channel_with_resume(
        config,
        env.config_path,
        env.identity_roots,
        env.path_index,
        catalog,
        &state,
        &scoring,
        // How much airtime is still missing, and what bounds one generation. A
        // pattern block with no authored `cycles` stops once it has laid this
        // much down rather than running its pools to the end — eleven years in
        // one pass on a 51-pool channel (#140) — and a flat `entries` channel
        // cuts its authored list here and resumes at that position next time
        // rather than laying all 950 items and idling for a month (#118).
        // Callers loop while `from < target`, so this is positive.
        Some((target - from).unsigned_abs()),
        // The absolute instant this generation begins airing at — a sequencer
        // block (#169) reads it as `ctx.window.from`, so a sequencer script
        // asks "what hour does this generation start" rather than "what hour
        // is it while the daemon happens to be computing this".
        from,
    )?;
    // The ledger needs each airing's show, and only the catalog knows it. One
    // query for the whole generation rather than one per item.
    let show_ids = match catalog {
        Some(cat) => {
            let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
            cat.show_ids_for(&ids)?
        }
        None => HashMap::new(),
    };
    Ok(Generation {
        items,
        resume,
        progress,
        show_ids,
        target_count: scoring.target_count,
    })
}

/// How many of a channel's last airings a generation reads: the widest of the
/// adjacency reach and the scorer's recent depth. A ledger holding this many
/// answers every `tail` a generation asks.
pub fn ledger_reach(config: &ChannelConfig) -> usize {
    config.adjacency_reach().max(recent_depth(config))
}

/// How deep a recently-aired tail this channel's scorer sees
/// (`scoring.recent_depth`).
fn recent_depth(config: &ChannelConfig) -> usize {
    config
        .scoring
        .as_ref()
        .map(|s| s.recent_depth)
        .unwrap_or_else(|| ScoringConfig::default().recent_depth)
}

/// Hint a scorer plugin about how many items this generation needs.
///
/// Sized to **one chunk**, not to the whole remaining window. A generation lays
/// the plugin's returned list end-to-end, so a hint covering a 30-day window
/// would push a single generation to materialize the entire month in one pass.
/// Near the end of the window the remaining span is smaller than a chunk, and
/// that smaller span wins.
///
/// Clamped at both ends: at least one item, so a nearly-covered window still
/// asks for something rather than handing a plugin a target of zero, and capped
/// so no configuration can ask a plugin to rank an entire library.
pub fn target_count(config: &ChannelConfig, from: OffsetDateTime, target: OffsetDateTime) -> usize {
    const MAX: i64 = 500;
    let per_item = config
        .scoring
        .as_ref()
        .map(|s| s.nominal_item_secs)
        .unwrap_or_else(|| ScoringConfig::default().nominal_item_secs)
        .max(1) as i64;
    let chunk = i64::from(config.chunk_hours) * 3600;
    let remaining = (target - from).whole_seconds().max(0);
    remaining.min(chunk).div_euclid(per_item).clamp(1, MAX) as usize
}

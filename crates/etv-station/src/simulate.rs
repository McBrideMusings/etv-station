//! Replays the daemon's schedule generation against an in-memory play history,
//! so a tool can show what a channel would air over several days — including
//! how soon a title comes back — without a running daemon, a playout folder or
//! a `history.db`.
//!
//! The loop is the daemon's own (`daemon::pattern_catch_up`): on each roll tick
//! the window's target moves to `now + window_days`, and while the schedule is
//! covered only up to `from < target` the resolver runs again. Each run gets
//!
//! - `ctx.target_count` from [`crate::daemon::target_count`] — the same function
//!   the daemon calls, so a chunk's worth of items, not a library's worth;
//! - `ctx.recent` from what earlier generations aired, newest last, capped at
//!   `scoring.recent_depth`, episodes included — a scorer's repeat suppression
//!   sees exactly what it would see in production;
//! - the pool rotation, the per-series cursor and the adjacency tail those
//!   earlier generations left behind.
//!
//! The resolver ([`crate::resolve::resolve_channel_with_resume`]) is the real
//! one, so the pattern engine's pool rotation, `advance: resume` and the
//! drained-pool refill all run as they do on air. What is not modelled: file
//! probing (an item's length is the catalog's `duration_ms`, or the channel's
//! `nominal_item_secs` when the catalog has none), error cards, the Tautulli
//! feed behind `ctx.history`, the station time zone (a `sequencer` block reads
//! UTC here), the station's identity roots and path index (a manual `local`
//! item keeps its own id), and a channel's `anchor` join on the first
//! generation.
//!
//! The first tick fills a whole `window_days`, as the daemon's startup catch-up
//! does, so the schedule reaches `days + window_days` past `start`.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;
use time::{Duration, OffsetDateTime};

use crate::catalog::Catalog;
use crate::config::{ChannelConfig, ScoringConfig};
use crate::daemon::{target_count, window_duration};
use crate::resolve::resolve_channel_with_resume;
use crate::resume::{GenerationState, ResumeMap};
use crate::score::ScoreInputs;

/// Bounds one simulated tick's catch-up, like the daemon's own
/// `MAX_GENERATIONS_PER_TICK`. A channel that lays nothing forward would
/// otherwise never reach its target.
const MAX_GENERATIONS_PER_TICK: usize = 512;

/// One scheduled airing.
#[derive(Debug, Clone)]
pub struct SimulatedAiring {
    pub id: String,
    /// Which `rule.blocks` index produced it.
    pub block: usize,
    pub show_id: Option<String>,
    pub start: OffsetDateTime,
    pub duration: Duration,
    /// The pool's per-airing record: the scorer's `audit` stages and the
    /// pattern engine's `select` stage.
    pub metadata: Option<Value>,
}

/// One resolver run and what it scheduled.
#[derive(Debug, Clone)]
pub struct SimulatedGeneration {
    pub index: usize,
    /// When this generation's first item airs.
    pub from: OffsetDateTime,
    /// The `ctx.target_count` the scorer was handed.
    pub target_count: usize,
    pub airings: Vec<SimulatedAiring>,
}

#[derive(Debug, Clone)]
pub struct Simulation {
    pub start: OffsetDateTime,
    pub generations: Vec<SimulatedGeneration>,
}

/// The airings so far, in schedule order — the in-memory stand-in for the
/// `airings` table (`history::HistoryDb`), holding only what the generation
/// inputs project from it.
#[derive(Default)]
struct Ledger {
    entry_ids: Vec<String>,
    /// `series_key -> last aired entry_id`, where the key is the show, or the
    /// entry itself for a movie (`HistoryDb::series_cursor`).
    cursor: BTreeMap<String, String>,
}

impl Ledger {
    fn record(&mut self, entry_id: &str, show_id: Option<&str>) {
        self.entry_ids.push(entry_id.to_string());
        self.cursor.insert(
            show_id.unwrap_or(entry_id).to_string(),
            entry_id.to_string(),
        );
    }

    /// The last `n` airings, oldest first (`HistoryDb::tail`).
    fn tail(&self, n: usize) -> Vec<String> {
        let skip = self.entry_ids.len().saturating_sub(n);
        self.entry_ids[skip..].to_vec()
    }
}

/// Run the daemon's generation loop for `days` days of ticks starting at
/// `start`, and return every generation it laid down.
///
/// `config_path` is the channel file's path: relative `plugin:` paths resolve
/// against its directory, as they do in the daemon.
pub fn simulate(
    config: &ChannelConfig,
    config_path: &Path,
    catalog: &Catalog,
    account_id: Option<i64>,
    days: u32,
    start: OffsetDateTime,
) -> Result<Simulation, String> {
    let recent_depth = config
        .scoring
        .as_ref()
        .map(|s| s.recent_depth)
        .unwrap_or_else(|| ScoringConfig::default().recent_depth);
    let nominal = std::time::Duration::from_secs(u64::from(
        config
            .scoring
            .as_ref()
            .map(|s| s.nominal_item_secs)
            .unwrap_or_else(|| ScoringConfig::default().nominal_item_secs)
            .max(1),
    ));
    let roll = Duration::seconds_f64(config.roll_interval.as_secs_f64());
    if roll <= Duration::ZERO {
        return Err("roll_interval must be positive".to_string());
    }
    let window = window_duration(config.window_days);
    let end = start + Duration::days(i64::from(days));

    let mut ledger = Ledger::default();
    let mut resume = ResumeMap::new();
    let mut generations: Vec<SimulatedGeneration> = Vec::new();
    let mut from = start;
    let mut now = start;

    while now < end {
        let target = now + window;
        let mut this_tick = 0;
        while from < target {
            if this_tick >= MAX_GENERATIONS_PER_TICK {
                break;
            }
            this_tick += 1;

            let state = GenerationState {
                resume: resume.clone(),
                cursor: ledger.cursor.clone(),
                tail: ledger.tail(config.adjacency_reach()),
            };
            let scoring = ScoreInputs {
                target_count: target_count(config, from, target),
                recent: ledger.tail(recent_depth),
                now: now.unix_timestamp(),
                account_id,
                ..Default::default()
            };
            let (items, resume_out, _progress) = resolve_channel_with_resume(
                config,
                config_path,
                &[],
                None,
                Some(catalog),
                &state,
                &scoring,
                Some((target - from).unsigned_abs()),
                from,
            )
            .map_err(|e| e.to_string())?;

            let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
            let show_ids = catalog.show_ids_for(&ids).map_err(|e| e.to_string())?;

            let mut airing_start = from;
            let mut airings = Vec::with_capacity(items.len());
            for item in items {
                let length = item.catalog_duration.unwrap_or(nominal);
                let duration = Duration::seconds_f64(length.as_secs_f64());
                let show_id = show_ids.get(&item.id).cloned();
                ledger.record(&item.id, show_id.as_deref());
                airings.push(SimulatedAiring {
                    id: item.id,
                    block: item.block,
                    show_id,
                    start: airing_start,
                    duration,
                    metadata: item.metadata,
                });
                airing_start += duration;
            }

            let span = airing_start - from;
            if span <= Duration::ZERO {
                return Err(format!(
                    "generation {} produced no playable duration",
                    generations.len()
                ));
            }
            generations.push(SimulatedGeneration {
                index: generations.len(),
                from,
                target_count: scoring.target_count,
                airings,
            });
            resume = resume_out;
            from += span;
        }
        now += roll;
    }

    Ok(Simulation { start, generations })
}

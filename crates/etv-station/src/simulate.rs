//! Replays the daemon's schedule generation against an in-memory play history,
//! so a tool can show what a channel would air over several days — including
//! how soon a title comes back — without a running daemon, a playout folder or
//! a `history.db`.
//!
//! The loop is the daemon's own (`daemon::pattern_catch_up`): on each roll tick
//! the window's target moves to `now + window_days`, and while the schedule is
//! covered only up to `from < target` it runs another generation. Each one is
//! [`crate::generation::generate`], the step the daemon calls, reading an
//! in-memory ledger of what earlier generations aired in place of the
//! `airings` table — so `ctx.target_count`, `ctx.recent`, the per-series
//! cursor, the adjacency tail and the pool rotation are what they would be in
//! production, and the pattern engine's `advance: resume` and drained-pool
//! refill run as they do on air. What is not modelled: file
//! probing (an item's length is the catalog's `duration_ms`, or the channel's
//! `nominal_item_secs` when the catalog has none), error cards, the Tautulli
//! feed behind `ctx.history`, the station's identity roots and path index (a manual `local`
//! item keeps its own id), and a channel's `anchor` join on the first
//! generation.
//!
//! Each airing keeps the program its playout item would carry: the resolver's
//! defaults with the channel's `guide:` templates rendered over them by the
//! daemon's own pass. Attribution lines are left off, there being no watch
//! history to name anyone from.
//!
//! The first tick fills a whole `window_days`, as the daemon's startup catch-up
//! does, so the schedule reaches `days + window_days` past `start`.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;
use time::{Duration, OffsetDateTime};

use crate::catalog::Catalog;
use crate::config::{ChannelConfig, ScoringConfig};
use crate::daemon::{render_guide, window_duration};
use crate::errors::StationError;
use crate::generation::{AiringsLedger, GenerationEnv, MAX_GENERATIONS_PER_TICK, generate};
use crate::resume::ResumeMap;

/// One scheduled airing.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    /// The playout item's `program` object as the chunk file would hold it,
    /// `guide:` templates rendered.
    pub program: Option<Value>,
}

/// One resolver run and what it scheduled.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SimulatedGeneration {
    pub index: usize,
    /// When this generation's first item airs.
    pub from: OffsetDateTime,
    /// The `ctx.target_count` the scorer was handed.
    pub target_count: usize,
    pub airings: Vec<SimulatedAiring>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    /// `show_id -> last aired entry_id`, movies excluded
    /// (`HistoryDb::cross_channel_show_cursor`). The simulated channel is the
    /// only one on air, so this is its own airings.
    show_cursor: BTreeMap<String, String>,
}

impl Ledger {
    fn record(&mut self, entry_id: &str, show_id: Option<&str>) {
        self.entry_ids.push(entry_id.to_string());
        self.cursor.insert(
            show_id.unwrap_or(entry_id).to_string(),
            entry_id.to_string(),
        );
        if let Some(show_id) = show_id {
            self.show_cursor
                .insert(show_id.to_string(), entry_id.to_string());
        }
    }
}

impl AiringsLedger for Ledger {
    fn series_cursor(&self) -> Result<BTreeMap<String, String>, StationError> {
        Ok(self.cursor.clone())
    }

    fn global_cursor(&self) -> Result<BTreeMap<String, String>, StationError> {
        Ok(self.show_cursor.clone())
    }

    fn tail(&self, n: usize) -> Result<Vec<String>, StationError> {
        let skip = self.entry_ids.len().saturating_sub(n);
        Ok(self.entry_ids[skip..].to_vec())
    }
}

/// Run the daemon's generation loop for `days` days of ticks starting at
/// `start`, and return every generation it laid down.
///
/// `config_path` is the channel file's path: relative `plugin:` paths resolve
/// against its directory, as they do in the daemon. `tz` is the station's
/// zone.
pub fn simulate(
    config: &ChannelConfig,
    config_path: &Path,
    catalog: &Catalog,
    account_id: Option<i64>,
    days: u32,
    start: OffsetDateTime,
    tz: Option<&'static time_tz::Tz>,
) -> Result<Simulation, String> {
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
        let env = GenerationEnv {
            config,
            config_path,
            identity_roots: &[],
            path_index: None,
            history: Default::default(),
            account_id,
            tz,
            now,
        };
        let mut this_tick = 0;
        while from < target {
            if this_tick >= MAX_GENERATIONS_PER_TICK {
                break;
            }
            this_tick += 1;

            let generation = generate(&env, Some(catalog), &ledger, &resume, from, target)
                .map_err(|e| e.to_string())?;
            let mut items = generation.items;
            let show_ids = generation.show_ids;

            let lengths: Vec<std::time::Duration> = items
                .iter()
                .map(|i| i.catalog_duration.unwrap_or(nominal))
                .collect();
            render_guide(
                &mut items,
                &lengths,
                from,
                config.name.trim(),
                config.display_name.as_deref(),
                config.explains_why(),
            );

            let mut airing_start = from;
            let mut airings = Vec::with_capacity(items.len());
            for (item, length) in items.into_iter().zip(lengths) {
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
                    program: item
                        .program
                        .as_ref()
                        .and_then(|p| serde_json::to_value(p).ok()),
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
                target_count: generation.target_count,
                airings,
            });
            resume.adopt(generation.resume, now);
            from += span;
        }
        now += roll;
    }

    Ok(Simulation { start, generations })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generation::ChannelLedger;
    use crate::history::{HistoryDb, PlayRecord};

    /// The simulator's ledger stands in for the `airings` table, so every
    /// projection `generate` reads must answer what the table answers after the
    /// same airings — a movie included, which the cross-channel cursor leaves
    /// out.
    #[test]
    fn in_memory_ledger_projects_like_the_airings_table() {
        let airings = [
            ("ep1", Some("show-a")),
            ("movie", None),
            ("ep2", Some("show-a")),
            ("other", Some("show-b")),
        ];
        let db = HistoryDb::open_in_memory().unwrap();
        let mut ledger = Ledger::default();
        let start = OffsetDateTime::UNIX_EPOCH;
        let records: Vec<PlayRecord> = airings
            .iter()
            .enumerate()
            .map(|(i, (entry, show))| {
                ledger.record(entry, *show);
                PlayRecord {
                    entry_id: entry.to_string(),
                    show_id: show.map(str::to_string),
                    start: start + Duration::hours(i as i64),
                    played_at: start,
                    ..Default::default()
                }
            })
            .collect();
        db.record("ch", &records).unwrap();
        let table = ChannelLedger {
            db: &db,
            channel: "ch",
        };

        assert_eq!(
            ledger.series_cursor().unwrap(),
            table.series_cursor().unwrap()
        );
        assert_eq!(
            ledger.global_cursor().unwrap(),
            table.global_cursor().unwrap()
        );
        for n in [0, 2, 10] {
            assert_eq!(ledger.tail(n).unwrap(), table.tail(n).unwrap(), "tail({n})");
        }
    }
}

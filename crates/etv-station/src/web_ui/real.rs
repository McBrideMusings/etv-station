//! A channel's real schedule for the Schedule view: the airings the daemon has
//! published or aired, read from the chunk files in its output folder and the
//! play history, with no simulation. The same reader gives the state a
//! projection continues from.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use ersatztv_playout::playout::{Playout, is_bumper_id};
use time::{Duration, OffsetDateTime};

use crate::catalog::Catalog;
use crate::config::LoadedChannel;
use crate::history::{AiringFilter, AiringKind, HistoryDb};
use crate::resume::{self, ResumeMap};
use crate::scan;
use crate::simulate::SimulatedAiring;

/// The `Real` airings' `block`: a published item does not record which
/// `rule.blocks` entry produced it.
pub const UNKNOWN_BLOCK: usize = usize::MAX;

/// The most history rows read for one view.
const HISTORY_ROW_LIMIT: usize = 100_000;

/// A bumper segment the align/fill pass placed in the published playout.
pub struct RealBumper {
    pub id: String,
    pub start: OffsetDateTime,
    pub duration: Duration,
}

pub struct Real {
    /// Programme airings from the span's start, in time order.
    pub airings: Vec<SimulatedAiring>,
    /// The published bumper segments from the span's start, in time order.
    pub bumpers: Vec<RealBumper>,
    /// Where the published playout ends; `None` when nothing is published.
    pub frontier: Option<OffsetDateTime>,
}

/// What a projection starts from, read from the daemon's own files.
pub struct Seed {
    pub tail: Vec<String>,
    pub cursor: BTreeMap<String, String>,
    pub show_cursor: BTreeMap<String, String>,
    pub resume: ResumeMap,
    /// The `.resume` sidecar's bytes, empty when there is none.
    pub resume_bytes: Vec<u8>,
}

/// Runs one future to completion on a throwaway runtime: the UI server is
/// synchronous, the scanner is not.
fn block_on<T>(future: impl std::future::Future<Output = T>) -> Result<T, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map(|rt| rt.block_on(future))
        .map_err(|e| e.to_string())
}

/// `output_base/history.db` opened read-only, or `None` when the daemon has
/// not written one.
pub fn open_history(output_base: &Path) -> Result<Option<HistoryDb>, String> {
    let path = output_base.join("history.db");
    if !path.is_file() {
        return Ok(None);
    }
    HistoryDb::open_readonly(&path)
        .map(Some)
        .map_err(|e| format!("open {}: {e}", path.display()))
}

/// Every item of the channel's chunk files finishing after `since`,
/// deduplicated on `(id, start)` (a boundary-straddling item is written into
/// both neighbouring chunks).
fn published_items(
    lc: &LoadedChannel,
    since: OffsetDateTime,
) -> Result<Vec<ersatztv_playout::playout::PlayoutItem>, String> {
    let files = block_on(scan::scan_output_folder(&lc.output_folder))?
        .map_err(|e| format!("scan {}: {e}", lc.output_folder.display()))?;
    let mut items = Vec::new();
    for file in files.iter().filter(|f| f.finish > since) {
        let bytes = match std::fs::read(&file.path) {
            Ok(b) => b,
            // The daemon renames and sweeps chunks under a reader.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("read {}: {e}", file.path.display())),
        };
        let playout: Playout = serde_json::from_slice(&bytes)
            .map_err(|e| format!("parse {}: {e}", file.path.display()))?;
        items.extend(playout.items.into_iter().filter(|i| i.finish > since));
    }
    items.sort_by_key(|i| i.start);
    items.dedup_by(|a, b| a.id == b.id && a.start == b.start);
    Ok(items)
}

/// The channel's real airings from `since`: the play history's programme rows
/// (aired and scheduled), each carrying the audit trail and program of the
/// published item at the same start. Without a history file the published
/// items stand alone.
pub fn read(
    lc: &LoadedChannel,
    history: Option<&HistoryDb>,
    catalog: &Catalog,
    since: OffsetDateTime,
    nominal: Duration,
) -> Result<Real, String> {
    let (bumper_items, items): (Vec<_>, Vec<_>) = published_items(lc, since)?
        .into_iter()
        .partition(|i| is_bumper_id(&i.id));
    let bumpers = bumper_items
        .into_iter()
        .map(|i| RealBumper {
            id: i.id,
            start: i.start,
            duration: i.finish - i.start,
        })
        .collect();
    let frontier = items.iter().map(|i| i.finish).max();
    let mut by_start: HashMap<(String, i64), &ersatztv_playout::playout::PlayoutItem> = items
        .iter()
        .map(|i| ((i.id.clone(), i.start.unix_timestamp_nanos() as i64), i))
        .collect();
    let key =
        |id: &str, start: OffsetDateTime| (id.to_string(), start.unix_timestamp_nanos() as i64);

    let mut airings: Vec<SimulatedAiring> = Vec::new();
    match history {
        Some(db) => {
            let rows = db
                .airings(&AiringFilter {
                    channel: Some(lc.name.clone()),
                    kind: Some(AiringKind::Programme),
                    // A day back, so an airing that began before `since` and runs
                    // past it is found; those that end before it are dropped below.
                    from: Some(since - Duration::days(1)),
                    limit: HISTORY_ROW_LIMIT,
                    ..Default::default()
                })
                .map_err(|e| e.to_string())?;
            let ids: Vec<String> = rows.iter().map(|r| r.record.entry_id.clone()).collect();
            let entries = catalog.entries_for(&ids).map_err(|e| e.to_string())?;
            for (i, row) in rows.iter().enumerate() {
                let r = &row.record;
                let item = by_start.remove(&key(&r.entry_id, r.start));
                let duration = match item {
                    Some(item) => item.finish - item.start,
                    None => entries
                        .get(&r.entry_id)
                        .and_then(|e| e.duration_ms)
                        .map(Duration::milliseconds)
                        .or_else(|| rows.get(i + 1).map(|next| next.record.start - r.start))
                        .unwrap_or(nominal),
                };
                if r.start + duration <= since {
                    continue;
                }
                airings.push(SimulatedAiring {
                    id: r.entry_id.clone(),
                    block: UNKNOWN_BLOCK,
                    show_id: r.show_id.clone(),
                    start: r.start,
                    duration,
                    metadata: item.and_then(|i| i.metadata.clone()),
                    program: item
                        .and_then(|i| i.program.as_ref())
                        .and_then(|p| serde_json::to_value(p).ok()),
                });
            }
        }
        None => {
            let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
            let shows = catalog.show_ids_for(&ids).map_err(|e| e.to_string())?;
            for item in &items {
                airings.push(SimulatedAiring {
                    id: item.id.clone(),
                    block: UNKNOWN_BLOCK,
                    show_id: shows.get(&item.id).cloned(),
                    start: item.start,
                    duration: item.finish - item.start,
                    metadata: item.metadata.clone(),
                    program: item
                        .program
                        .as_ref()
                        .and_then(|p| serde_json::to_value(p).ok()),
                });
            }
        }
    }
    // A published item whose history row is not written yet still airs.
    if history.is_some() && !by_start.is_empty() {
        let ids: Vec<String> = by_start.values().map(|i| i.id.clone()).collect();
        let shows = catalog.show_ids_for(&ids).map_err(|e| e.to_string())?;
        for item in by_start.values() {
            airings.push(SimulatedAiring {
                id: item.id.clone(),
                block: UNKNOWN_BLOCK,
                show_id: shows.get(&item.id).cloned(),
                start: item.start,
                duration: item.finish - item.start,
                metadata: item.metadata.clone(),
                program: item
                    .program
                    .as_ref()
                    .and_then(|p| serde_json::to_value(p).ok()),
            });
        }
        airings.sort_by_key(|a| a.start);
    }
    Ok(Real {
        airings,
        bumpers,
        frontier,
    })
}

/// The state a projection from the channel's frontier starts from.
pub fn seed(lc: &LoadedChannel, history: Option<&HistoryDb>, reach: usize) -> Result<Seed, String> {
    let resume_bytes = std::fs::read(resume::sidecar_path(&lc.output_folder)).unwrap_or_default();
    let (resume, _) = if resume_bytes.is_empty() {
        (ResumeMap::new(), resume::ResumeLoad::Fresh)
    } else {
        resume::parse(&resume_bytes)
    };
    let (tail, cursor, show_cursor) = match history {
        Some(db) => (
            db.tail(&lc.name, reach).map_err(|e| e.to_string())?,
            db.series_cursor(&lc.name).map_err(|e| e.to_string())?,
            db.cross_channel_show_cursor().map_err(|e| e.to_string())?,
        ),
        None => Default::default(),
    };
    Ok(Seed {
        tail,
        cursor,
        show_cursor,
        resume,
        resume_bytes,
    })
}

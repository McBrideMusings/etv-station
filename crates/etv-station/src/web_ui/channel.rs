//! A channel's review (`GET /api/channels/{ch}/review`) and its tune-in
//! stills: the real schedule as a report, with a projection past its frontier
//! on request, cached by `projection`.

use super::lineup::{channel_key, lineup_keys};
use super::*;
use crate::generation;

/// Where a plugin pool's pick stood in the list it walked: its `rank` (the
/// walk position, after any temperature), its `score_rank` (by score alone),
/// the `temperature` that separated the two, and whether the slot was an
/// exploration `draw` — read off the plugin's own `pool` audit stage, `null`
/// for an airing no plugin ranked. Under a rotating seed this is what shows
/// the walk: ranks climb 1, 2, 3 through one period's list and start over at
/// a rollover.
///
/// The `select` stage (the core's own record of the draw) adds the seed and
/// temperature the visit drew with, the period index each was in, and where
/// in that period's list the pick sat — `null` fields on a pool nothing
/// rotates.
fn ranking_of(metadata: Option<&Value>) -> Value {
    let stage = |name: &str| {
        metadata
            .and_then(|m| m.get("audit"))
            .and_then(Value::as_array)
            .and_then(|stages| {
                stages
                    .iter()
                    .find(|stage| stage.get("stage").and_then(Value::as_str) == Some(name))
            })
            .and_then(|stage| stage.get("detail"))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let (pool, select) = (stage("pool"), stage("select"));
    if pool.is_null() && select.is_null() {
        return Value::Null;
    }
    json!({
        "rank": pool.get("rank"),
        "score_rank": pool.get("score_rank"),
        "draw": pool.get("draw"),
        "seed": select.get("seed"),
        "temperature": select.get("temperature"),
        "seed_period": select.get("seed_period"),
        "temperature_period": select.get("temperature_period"),
        "list_position": select.get("list_position"),
        "list_length": select.get("list_length"),
    })
}

/// The pool an airing was drawn from, read off the pattern engine's `select`
/// audit stage (`detail.pool`).
pub(super) fn drawn_from_pool(metadata: Option<&Value>) -> Option<String> {
    metadata?
        .get("audit")?
        .as_array()?
        .iter()
        .find(|stage| stage.get("stage").and_then(Value::as_str) == Some("select"))?
        .get("detail")?
        .get("pool")?
        .as_str()
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Channel review (the page's Schedule, `/api/channels/{ch}/review`): one channel's schedule as
// a report — depth before repeats, film/series share, overlap with the rest of
// the lineup, seeded tune-in samples and a day-by-day guide. It opens on the
// channel's real airings (`real`: the published playout and the play history),
// with no simulation. `?project=Nd` adds N days simulated from the real
// frontier, seeded from the daemon's `.resume` state and recent airings; the
// projection is cached by `projection` on a content hash of its inputs and
// filled by a worker thread, so a request that misses the cache gets 202 with
// the real span and the page asks again. The numbers are `crate::review`'s,
// computed over whatever span is on screen.
// ---------------------------------------------------------------------------

/// How many seeded tune-in moments the view samples.
const TUNE_IN_SAMPLES: usize = 5;

/// What the Review view keeps between requests.
pub(super) struct ReviewState {
    station_path: PathBuf,
    /// Each channel's candidate entry-id set, keyed by config path and
    /// recomputed when the file's mtime changes. A failure is not cached, so
    /// the next review retries it.
    fingerprints: HashMap<PathBuf, (std::time::SystemTime, Arc<HashSet<String>>)>,
    /// Every movie id and every episode id in the catalog, for telling a
    /// channel that draws from the whole library apart from one that doesn't.
    library: Option<(HashSet<String>, HashSet<String>)>,
    /// The last review's comparable numbers per channel, for "Since last run".
    previous: HashMap<String, Vec<(String, Value)>>,
    /// Every tune-in still a review has offered, keyed by the file name its
    /// URL carries, so `/api/stills/{id}` renders it on first request.
    stills: HashMap<String, TuneInStill>,
    /// Where rendered stills are kept. A still's file name is a digest of
    /// everything that decides it and the directory names this binary's
    /// build, so a file here is never stale.
    pub(super) still_dir: PathBuf,
    /// Each channel's candidate set as one digest, with the channel file's
    /// mtime and when it was computed: resolving every candidate costs more
    /// than a poll should, and a catalog change shows within the memo's life.
    candidate_digests: HashMap<PathBuf, (std::time::SystemTime, std::time::Instant, String)>,
    /// Progress of [`ReviewState::warm_step`], which fills `library` and
    /// `fingerprints` while the server is idle.
    warm: Warm,
    /// Projections, filled by a worker thread so no request waits on one.
    pub(super) sims: Arc<projection::SimCache>,
    /// Graphics renders, filled by a worker thread so no request waits on one.
    pub(super) renders: super::graphics::RenderQueue,
}

/// Where the idle-time cache warm-up stands.
enum Warm {
    Pending,
    /// The station as loaded for the warm-up, and the next lineup channel
    /// whose fingerprint is to be computed.
    Running {
        station: Box<config::Station>,
        next: usize,
        began: std::time::Instant,
    },
    Done,
}

/// How long a channel's candidate digest is reused.
const CANDIDATE_DIGEST_LIFE: std::time::Duration = std::time::Duration::from_secs(30);

impl ReviewState {
    /// Fills the caches the review's overlap section reads (the library's
    /// movie and episode ids, then each channel's fingerprint) one unit at a
    /// time, so a first review after a start finds them warm. Each call is one
    /// catalog query or one channel's resolve, so a request that arrives
    /// during the warm-up waits for one unit, not for all of them. Returns
    /// whether more work remains.
    pub(super) fn warm_step(&mut self, catalog: &Catalog) -> bool {
        let began_step = std::time::Instant::now();
        match std::mem::replace(&mut self.warm, Warm::Done) {
            Warm::Done => false,
            Warm::Pending => match config::load_for_inspection(&self.station_path) {
                Ok(station) => {
                    self.warm = Warm::Running {
                        station: Box::new(station),
                        next: 0,
                        began: began_step,
                    };
                    true
                }
                Err(e) => {
                    tracing::warn!(event = "ui.warm.failed", error = %e, "review warm-up: station did not load");
                    false
                }
            },
            Warm::Running {
                station,
                mut next,
                began,
            } => {
                if self.library.is_none() {
                    if let Err(e) = self.ensure_library(catalog) {
                        tracing::warn!(event = "ui.warm.failed", error = %e, "review warm-up: library ids did not resolve");
                        return false;
                    }
                } else if let Some(lc) = station.channels.get(next) {
                    if let Err(e) = channel_fingerprint(self, catalog, lc) {
                        tracing::warn!(event = "ui.warm.channel_failed", channel = %channel_key(&lc.config_path), error = %e, "review warm-up: fingerprint did not resolve");
                    }
                    next += 1;
                } else {
                    tracing::info!(
                        event = "ui.warm.done",
                        channels = station.channels.len(),
                        ms = began.elapsed().as_millis() as u64,
                        "review warm-up: overlap caches filled"
                    );
                    return false;
                }
                self.warm = Warm::Running {
                    station,
                    next,
                    began,
                };
                true
            }
        }
    }

    /// Fills `library` when it is empty.
    fn ensure_library(&mut self, catalog: &Catalog) -> Result<(), String> {
        if self.library.is_none() {
            let movies = catalog
                .resolve_query(r#"item.type == "movie""#)
                .map_err(|e| e.to_string())?;
            let episodes = catalog
                .resolve_query(r#"item.type == "episode""#)
                .map_err(|e| e.to_string())?;
            self.library = Some((movies.into_iter().collect(), episodes.into_iter().collect()));
        }
        Ok(())
    }

    pub(super) fn new(station_path: PathBuf, catalog_path: PathBuf) -> Self {
        // One directory per build of this binary, so a rebuilt renderer or
        // simulator never serves what the previous one produced.
        let build = std::env::current_exe()
            .and_then(|p| p.metadata())
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs().to_string())
            .unwrap_or_default();
        Self {
            // On the data volume beside the catalog, so a projection survives a
            // deploy; nothing in its key names the build.
            sims: projection::SimCache::new(
                station_path.clone(),
                catalog_path.clone(),
                catalog_path
                    .parent()
                    .map(|p| p.join("ui-projections"))
                    .unwrap_or_else(|| std::env::temp_dir().join("etv-station-ui-projections")),
            ),
            renders: super::graphics::RenderQueue::new(
                catalog_path
                    .parent()
                    .map(|p| p.join("ui-renders"))
                    .unwrap_or_else(|| std::env::temp_dir().join("etv-station-ui-renders")),
            ),
            candidate_digests: HashMap::new(),
            warm: Warm::Pending,
            station_path,
            fingerprints: HashMap::new(),
            library: None,
            previous: HashMap::new(),
            stills: HashMap::new(),
            still_dir: std::env::temp_dir()
                .join("etv-station-ui-stills")
                .join(&build),
        }
    }
}

fn file_mtime(path: &Path) -> Result<std::time::SystemTime, String> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// How many upcoming placements per rotation the Review JSON previews.
const ROTATION_PREVIEW_PLACEMENTS: usize = 14;

/// `GET /api/channels/{key}/review[?project=Nd][&draft=ID]`: the channel's
/// review over its real airings, plus N days projected past the frontier when
/// asked. While the worker simulates the projection the answer is 202 with the
/// real span and `"pending": true`. A key that is not in the lineup is a 404, a
/// `project` outside [`projection::DAY_CHOICES`] a 400.
///
/// With `draft` the answer is `{deployed, draft, diff}`: the deployed review
/// and the same review re-simulated with the draft's pools swapped in, over
/// the same window, and what moved between them. The window defaults to
/// [`DRAFT_PROJECT_DAYS`]. The answer is 202 while either simulation runs.
pub(super) fn api_channel_review(
    state: &mut AppState,
    key: &str,
    query: &str,
) -> Result<(u16, Value), String> {
    let params = parse_query(query);
    let Some(draft_id) = params.get("draft").filter(|d| !d.is_empty()).cloned() else {
        return channel_review(state, key, &params, None);
    };
    let edits = match super::pools::draft_edits(state, key, &draft_id) {
        Ok(e) => e,
        Err(e) => return Ok((404, json!({ "error": e }))),
    };
    let mut params = params;
    params
        .entry("project".to_string())
        .or_insert_with(|| format!("{DRAFT_PROJECT_DAYS}d"));
    // Both are asked before either answer is read, so a cold cache queues the
    // two simulations together.
    let (dep_status, deployed) = channel_review(state, key, &params, None)?;
    let (draft_status, draft) = channel_review(state, key, &params, Some(&edits))?;
    for (status, body) in [(dep_status, &deployed), (draft_status, &draft)] {
        if status != 200 && status != 202 {
            return Ok((status, body.clone()));
        }
    }
    if deployed.get("load_error").is_some() {
        return Ok((200, deployed));
    }
    let diff = review_diff(&deployed, &draft);
    let pending = dep_status == 202 || draft_status == 202;
    tracing::info!(
        event = "ui.review.draft",
        channel = %key,
        draft = %draft_id,
        pending,
        changed_airings = diff["changed_airings"].as_array().map(Vec::len).unwrap_or(0),
        "review: draft graded against deployed"
    );
    Ok((
        if pending { 202 } else { 200 },
        json!({
            "pending": pending,
            "deployed_pending": dep_status == 202,
            "draft_pending": draft_status == 202,
            "deployed": deployed,
            "draft": draft,
            "diff": diff,
        }),
    ))
}

/// How many days a draft's review projects when the request names no length:
/// a draft re-simulation costs about 10s a generation (etv-station-1ne2).
const DRAFT_PROJECT_DAYS: u32 = 3;

/// What moved between a deployed review and its draft: each principle's grade
/// with a direction in words, each depth row, the first repeat, and the draft
/// airings that are not the deployed schedule's airing at the same instant.
fn review_diff(deployed: &Value, draft: &Value) -> Value {
    let direction = |was: Option<i64>, now: Option<i64>| match (was, now) {
        (Some(w), Some(n)) if n > w => "up",
        (Some(w), Some(n)) if n < w => "down",
        _ => "same",
    };
    let principles =
        |r: &Value| -> Vec<Value> { r["principles"].as_array().cloned().unwrap_or_default() };
    let dep_principles = principles(deployed);
    let grades: Vec<Value> = principles(draft)
        .iter()
        .map(|p| {
            let was = dep_principles.iter().find(|d| d["name"] == p["name"]);
            let (was_grade, now_grade) =
                (was.and_then(|w| w["grade"].as_i64()), p["grade"].as_i64());
            json!({
                "name": p["name"],
                "deployed": was.map(|w| w["grade"].clone()).unwrap_or(Value::Null),
                "draft": p["grade"],
                "deployed_measure": was.map(|w| w["measure"].clone()).unwrap_or(Value::Null),
                "draft_measure": p["measure"],
                "direction": direction(was_grade, now_grade),
            })
        })
        .collect();

    let row_id = |r: &Value| format!("{}|{}|{}", r["kind"], r["name"], r["pool"]);
    let dep_depth: Vec<Value> = deployed["depth"].as_array().cloned().unwrap_or_default();
    let depth: Vec<Value> = draft["depth"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|r| {
                    let was = dep_depth.iter().find(|d| row_id(d) == row_id(r));
                    json!({
                        "kind": r["kind"],
                        "name": r["name"],
                        "pool": r["pool"],
                        "aired": { "deployed": was.map(|w| w["aired"].clone()), "draft": r["aired"] },
                        "distinct": { "deployed": was.map(|w| w["distinct"].clone()), "draft": r["distinct"] },
                        "first_repeat_days": {
                            "deployed": was.map(|w| w["first_repeat_days"].clone()),
                            "draft": r["first_repeat_days"],
                        },
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let at = |a: &Value| (a["start"].as_i64(), a["id"].as_str().map(str::to_string));
    let dep_airings: Vec<Value> = deployed["airings"].as_array().cloned().unwrap_or_default();
    let deployed_set: HashSet<(Option<i64>, Option<String>)> = dep_airings.iter().map(at).collect();
    let was_at: HashMap<Option<i64>, Value> = dep_airings
        .iter()
        .map(|a| (a["start"].as_i64(), a["id"].clone()))
        .collect();
    let changed: Vec<Value> = draft["airings"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter(|a| !deployed_set.contains(&at(a)))
                .map(|a| {
                    json!({
                        "index": a["index"],
                        "id": a["id"],
                        "label": a["label"],
                        "start": a["start"],
                        "was": was_at.get(&a["start"].as_i64()).cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    json!({
        "grades": grades,
        "depth": depth,
        "first_repeat": { "deployed": deployed["first_repeat"], "draft": draft["first_repeat"] },
        "changed_airings": changed,
    })
}

/// One review: the deployed channel's, or `draft`'s pools swapped in.
fn channel_review(
    state: &mut AppState,
    key: &str,
    params: &HashMap<String, String>,
    draft: Option<&projection::DraftEdits>,
) -> Result<(u16, Value), String> {
    let began = std::time::Instant::now();
    let key = key.to_string();
    let station = config::load_for_inspection(&state.review.station_path)
        .map_err(|e| format!("load {}: {e}", state.review.station_path.display()))?;

    // Every channel file the station's `channels:` patterns found, loaded or
    // not, in station.yaml order.
    let base = state
        .review
        .station_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let keys = lineup_keys(&station.lineup, &base);
    let Some(position) = keys.iter().position(|k| *k == key) else {
        return Ok((
            404,
            json!({ "error": format!("no channel {key:?} in {}", state.review.station_path.display()) }),
        ));
    };
    let project_days = match params.get("project") {
        None => None,
        Some(p) => match p
            .strip_suffix('d')
            .and_then(|n| n.parse::<u32>().ok())
            .filter(|n| projection::DAY_CHOICES.contains(n))
        {
            Some(n) => Some(n),
            None => {
                return Ok((
                    400,
                    json!({ "error": format!("project={p:?} is not one of {:?} days, as 1d, 3d, 7d or 14d", projection::DAY_CHOICES) }),
                ));
            }
        },
    };

    let config_path = &station.lineup[position];
    if let Some(dropped) = station
        .dropped
        .iter()
        .find(|d| d.config_path == *config_path)
    {
        tracing::warn!(event = "ui.review.load_failed", channel = %key, reason = %dropped.reason, "review: channel failed to load");
        return Ok((
            200,
            json!({
                "channel": key,
                "config_path": dropped.config_path.display().to_string(),
                "load_error": dropped.reason,
            }),
        ));
    }
    let lc = station
        .channels
        .iter()
        .find(|c| c.config_path == *config_path)
        .ok_or_else(|| format!("{key} is in the lineup but neither loaded nor dropped"))?;

    let tz = crate::tz::parse(&station.station.tz).map_err(|e| e.to_string())?;
    let now = time::OffsetDateTime::now_utc();
    let start = crate::tz::local_midnight_at_or_before(now, tz);
    let nominal_secs = lc
        .config
        .scoring
        .as_ref()
        .map(|s| s.nominal_item_secs)
        .unwrap_or_else(|| config::ScoringConfig::default().nominal_item_secs);

    let history = real::open_history(&station.station.output_base)?;
    let real = real::read(
        lc,
        history.as_ref(),
        &state.catalog,
        start,
        time::Duration::seconds(i64::from(nominal_secs.max(1))),
    )?;
    let real_ms = began.elapsed().as_millis() as u64;
    // With nothing published a projection starts at the span's first instant:
    // a fixed point, so its cache key does not turn over every second.
    let frontier = real.frontier.unwrap_or(start);

    let mut projected: Vec<crate::simulate::SimulatedAiring> = Vec::new();
    let mut pending = false;
    let mut projection_json = Value::Null;
    let mut projected_until: Option<time::OffsetDateTime> = None;
    if let Some(n) = project_days {
        let seed = real::seed(lc, history.as_ref(), generation::ledger_reach(&lc.config))?;
        let candidates = candidates_digest(&mut state.review, &state.catalog, lc)?;
        let digests = projection::digests(
            &state.options.station,
            &lc.config_path,
            &lc.config,
            projection::frontier_digest(
                frontier,
                &seed.resume_bytes,
                &seed.tail,
                &seed.cursor,
                &seed.show_cursor,
            ),
            candidates,
            draft,
        );
        let request = projection::Request {
            channel: key.clone(),
            config_path: lc.config_path.clone(),
            days: n,
            frontier,
            tail: seed.tail,
            cursor: seed.cursor,
            show_cursor: seed.show_cursor,
            resume: seed.resume,
            digests,
            draft: draft.cloned(),
        };
        let request_key = request.key();
        match state.review.sims.lookup(request) {
            projection::Lookup::Ready(p) => {
                let until = frontier + time::Duration::days(i64::from(n));
                projected = p
                    .simulation
                    .generations
                    .iter()
                    .flat_map(|g| g.airings.iter().cloned())
                    .filter(|a| a.start >= frontier && a.start < until)
                    .collect();
                projected_until = Some(until);
                projection_json = json!({
                    "key": p.key,
                    "days": p.days,
                    "inputs": p.inputs,
                    "changed": p.changed,
                });
            }
            projection::Lookup::Failed(e) => {
                tracing::warn!(event = "ui.review.projection_failed", channel = %key, project_days = n, error = %e, "review: projection failed");
                projection_json = json!({ "error": e, "days": n });
            }
            projection::Lookup::Pending => {
                tracing::info!(event = "ui.review.pending", channel = %key, project_days = n, key = %request_key, "review: projection queued");
                pending = true;
                projection_json = json!({ "pending": true, "days": n, "key": request_key });
            }
        }
    }

    let span_end = projected_until.unwrap_or(frontier);
    let mut day_starts = vec![start];
    while *day_starts.last().expect("non-empty") < span_end || day_starts.len() < 2 {
        let last = *day_starts.last().expect("non-empty");
        day_starts.push(crate::tz::add_chunk(last, 24, tz));
    }
    let end = *day_starts.last().expect("non-empty");
    let real_count = real.airings.len();
    let all: Vec<crate::simulate::SimulatedAiring> =
        real.airings.into_iter().chain(projected).collect();

    let block_names: Vec<String> = lc
        .config
        .rule
        .all_blocks()
        .enumerate()
        .map(|(i, b)| {
            b.program()
                .and_then(|p| p.title.clone())
                .or_else(|| lc.config.rule.slot_name(i).map(str::to_string))
                .unwrap_or_else(|| format!("block {i}"))
        })
        .collect();
    let mut entries: HashMap<String, Option<CatalogEntry>> = HashMap::new();
    let mut airings: Vec<crate::review::ReviewAiring> = Vec::new();
    let mut simulated: Vec<&crate::simulate::SimulatedAiring> = Vec::new();
    for a in &all {
        // Every airing, past the span's end too, so the last one in the span
        // still has a successor for its tune-in still's NEXT card. Airings
        // run in time order, so `simulated[i]` is `airings[i]` for each `i`
        // the span keeps.
        simulated.push(a);
        if a.start >= end {
            continue;
        }
        let entry = entries
            .entry(a.id.clone())
            .or_insert_with(|| state.catalog.entry(&a.id).ok().flatten());
        airings.push(crate::review::ReviewAiring {
            id: a.id.clone(),
            title: entry
                .as_ref()
                .map(airing_label)
                .unwrap_or_else(|| a.id.clone()),
            kind: entry
                .as_ref()
                .map(|e| e.kind.clone())
                .unwrap_or_else(|| "unknown".to_string()),
            show: entry.as_ref().and_then(|e| e.show.clone()),
            show_id: a.show_id.clone(),
            season: entry.as_ref().and_then(|e| e.season),
            episode: entry.as_ref().and_then(|e| e.episode),
            pool: drawn_from_pool(a.metadata.as_ref())
                .unwrap_or_else(|| block_names.get(a.block).cloned().unwrap_or_default()),
            start: a.start,
            duration: a.duration,
        });
    }

    let review = build_review(state, &station, lc, &airings, &simulated, start, end)?;
    let summary = comparable_numbers(&review);
    // A draft's numbers are not the channel's "last run".
    let since_last = match draft {
        None => state.review.previous.insert(key.clone(), summary.clone()),
        Some(_) => None,
    };
    let since_last_json = since_last.map(|old| {
        let old: HashMap<String, Value> = old.into_iter().collect();
        summary
            .iter()
            .map(|(label, new)| {
                let old = old.get(label).cloned().unwrap_or(Value::Null);
                json!({ "label": label, "old": old, "new": new, "changed": old != *new })
            })
            .collect::<Vec<_>>()
    });

    let mut notices = Vec::new();
    let projected_airings = &airings[real_count.min(airings.len())..];
    let unprobed = projected_airings
        .iter()
        .filter(|a| {
            entries
                .get(&a.id)
                .and_then(|e| e.as_ref())
                .and_then(|e| e.duration_ms)
                .is_none()
        })
        .count();
    if unprobed > 0 {
        notices.push(format!(
            "{unprobed} of {} projected airings have no catalog length and run at nominal_item_secs ({nominal_secs}s); \
             the daemon would probe the file instead.",
            projected_airings.len()
        ));
    }
    if project_days.is_some() && lc.config.anchor.is_some() {
        notices.push(
            "This channel sets an anchor; the anchor join on its first generation is not modelled."
                .to_string(),
        );
    }

    // What the airing inspector shows beside each airing's audit trail: the
    // catalog fields a pool reads, and a label for every item a trail names
    // as passed over that never airs in the span (an aired one is labelled
    // in `airings`), so the page never prints a bare entry id.
    let ids: Vec<String> = airings.iter().map(|a| a.id.clone()).collect();
    let genres = state
        .catalog
        .tags_for_many(&ids, TagNs::Genre)
        .map_err(|e| e.to_string())?;
    let directors = state
        .catalog
        .tags_for_many(&ids, TagNs::Director)
        .map_err(|e| e.to_string())?;
    let collections = state
        .catalog
        .collections_for_many(&ids)
        .map_err(|e| e.to_string())?;
    let audits: Vec<Value> = (0..airings.len())
        .map(|i| crate::audit_report::split_audit(simulated[i].metadata.as_ref()).0)
        .collect();
    let titles = near_miss_titles(&state.catalog, &audits, |id| !entries.contains_key(id))?;

    // Fields that belong to the item, not the airing: one copy per entry id,
    // however many times it airs. The page merges them back onto each airing.
    let mut items: HashMap<&str, Value> = HashMap::new();
    for a in &airings {
        items.entry(a.id.as_str()).or_insert_with(|| {
            let e = entries.get(&a.id).and_then(|e| e.as_ref());
            json!({
                "title": e.map(|e| e.title.clone()),
                "season": e.and_then(|e| e.season),
                "episode": e.and_then(|e| e.episode),
                "year": e.and_then(|e| e.year),
                "genres": genres.get(&a.id),
                "collections": collections.get(&a.id),
                "directors": directors.get(&a.id),
                "content_rating": e.and_then(|e| e.content_rating.clone()),
                "summary": e.and_then(|e| e.summary.clone()),
            })
        });
    }

    let airings_json: Vec<Value> = airings
        .iter()
        .enumerate()
        .map(|(i, a)| {
            json!({
                "index": i,
                "id": a.id,
                "label": a.title,
                "show": a.show,
                "kind": a.kind,
                "pool": a.pool,
                "start": a.start.unix_timestamp(),
                "projected": i >= real_count,
                "offset_secs": (a.start - start).whole_seconds(),
                "duration_secs": a.duration.whole_seconds(),
                "ranking": ranking_of(simulated[i].metadata.as_ref()),
                "audit": audits[i],
            })
        })
        .collect();

    tracing::info!(
        event = "ui.review",
        channel = %key,
        project_days,
        pending,
        real_airings = real_count,
        airings = airings.len(),
        overlap_channels = review["overlap"]["channels"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0),
        real_ms,
        ms = began.elapsed().as_millis() as u64,
        "review: schedule replayed"
    );

    let span_label = match project_days {
        Some(1) if !pending => "real + 1 day projected".to_string(),
        Some(n) if !pending => format!("real + {n} days projected"),
        _ => "real only".to_string(),
    };
    let mut out = review;
    let obj = out.as_object_mut().expect("build_review returns an object");
    obj.insert("channel".into(), json!(key));
    obj.insert(
        "frontier".into(),
        json!(real.frontier.map(|f| f.unix_timestamp())),
    );
    obj.insert(
        "span".into(),
        json!({
            "real_until": real.frontier.map(|f| f.unix_timestamp()),
            "projected_until": projected_until.map(|f| f.unix_timestamp()),
            "label": span_label,
        }),
    );
    obj.insert("projection".into(), projection_json);
    if pending {
        obj.insert("pending".into(), json!(true));
    }
    obj.insert("since_last".into(), json!(since_last_json));
    obj.insert("notices".into(), json!(notices));
    obj.insert(
        "rotations".into(),
        crate::rotation::rows_json(&crate::rotation::preview_channel(
            &lc.config,
            tz,
            &crate::rotation::read_rotations(&lc.output_folder),
            time::OffsetDateTime::now_utc(),
            ROTATION_PREVIEW_PLACEMENTS,
        )),
    );
    obj.insert("airings".into(), json!(airings_json));
    obj.insert("items".into(), json!(items));
    obj.insert("titles".into(), json!(titles));
    obj.insert(
        "day_starts".into(),
        json!(
            day_starts
                .iter()
                .map(|d| d.unix_timestamp())
                .collect::<Vec<_>>()
        ),
    );
    Ok((if pending { 202 } else { 200 }, out))
}

/// A label for every item the audit trails in `audits` name as passed over
/// (`detail.near_misses[].id`) and `wanted` accepts, keyed by the id as named:
/// an entry id gets its airing label, a series id (`show_id`) its show name.
pub(super) fn near_miss_titles(
    catalog: &Catalog,
    audits: &[Value],
    wanted: impl Fn(&str) -> bool,
) -> Result<HashMap<String, String>, String> {
    let ids: Vec<String> = audits
        .iter()
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|stage| stage["detail"]["near_misses"].as_array())
        .flatten()
        .filter_map(|m| m["id"].as_str())
        .filter(|id| wanted(id))
        .map(str::to_string)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let entries = catalog.entries_for(&ids).map_err(|e| e.to_string())?;
    // An episode pool near-misses on a series id, which is no entry id.
    let unresolved: Vec<String> = ids
        .iter()
        .filter(|id| !entries.contains_key(*id))
        .cloned()
        .collect();
    let shows = catalog
        .show_names_for(&unresolved)
        .map_err(|e| e.to_string())?;
    Ok(entries
        .iter()
        .map(|(id, e)| (id.clone(), airing_label(e)))
        .chain(shows)
        .collect())
}

/// A movie as `Title (Year)`, an episode as `Show S01E02 · Title`.
pub(super) fn airing_label(e: &CatalogEntry) -> String {
    match (&e.show, e.season, e.episode) {
        (Some(show), Some(s), Some(ep)) => format!("{show} S{s:02}E{ep:02} · {}", e.title),
        (Some(show), _, _) => format!("{show} · {}", e.title),
        _ => match e.year {
            Some(y) => format!("{} ({y})", e.title),
            None => e.title.clone(),
        },
    }
}

/// The channel's `rule.blocks` as the Review page lays them out: per block,
/// its title and whichever of a daypart sequencer, pattern steps, an
/// hour-switched schedule of patterns, a plain pool list or a fixed entry
/// count drives it. A step's `take` is `[min, max]`, `null` for `take: all`.
fn blocks_json(channel: &config::ChannelConfig) -> Value {
    let blocks: Vec<Value> = channel
        .rule
        .all_blocks()
        .enumerate()
        .map(|(i, b)| {
            let name = b
                .program()
                .and_then(|p| p.title.clone())
                .or_else(|| channel.rule.slot_name(i).map(str::to_string))
                .unwrap_or_else(|| format!("block {i}"));
            json!({
                "name": name,
                "sequencer": b.sequencer.as_ref()
                    .and_then(|s| s.file_name())
                    .map(|n| n.to_string_lossy().into_owned()),
                "steps": steps_json(&b.pattern),
                "pools": b.pools.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
                "entries": b.entries().len(),
                "placed": placements(&channel.rule, i),
            })
        })
        .collect();
    json!(blocks)
}

/// Where `rule.schedules` place block `idx`, one line per range: the
/// schedule's name, its days, and the clock range. Empty for an any-time block.
fn placements(rule: &config::RuleConfig, idx: usize) -> Vec<String> {
    let Some(slot) = rule.slot_name(idx) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for schedule in &rule.schedules {
        let mut days: Vec<String> = schedule.dates.iter().map(|d| String::from(*d)).collect();
        days.extend(
            schedule
                .weekdays
                .iter()
                .map(|w| format!("{w:?}").to_lowercase()),
        );
        let days = if days.is_empty() {
            "every day".to_string()
        } else {
            days.join(", ")
        };
        for (range, placed) in &schedule.hours {
            if placed == slot {
                out.push(format!("{}: {days}, {range}", schedule.name));
            }
        }
    }
    out
}

/// One pattern's steps: pool, `take` as `[min, max]` (`null` for `all`), and
/// the step's `chance`.
fn steps_json(steps: &[config::PatternStep]) -> Vec<Value> {
    steps
        .iter()
        .map(|s| json!({ "pool": s.pool, "take": s.take.bounds(), "chance": s.chance }))
        .collect()
}

/// The channel's candidate set (entry ids and durations) as one digest. Not
/// the `channel_fingerprint` cache: that one is keyed on the channel file's
/// mtime and would hide a catalog change; this one recomputes after
/// [`CANDIDATE_DIGEST_LIFE`].
fn candidates_digest(
    review: &mut ReviewState,
    catalog: &Catalog,
    lc: &config::LoadedChannel,
) -> Result<String, String> {
    let mtime = file_mtime(&lc.config_path)?;
    if let Some((cached_mtime, at, digest)) = review.candidate_digests.get(&lc.config_path)
        && *cached_mtime == mtime
        && at.elapsed() < CANDIDATE_DIGEST_LIFE
    {
        return Ok(digest.clone());
    }
    let ids = crate::resolve::resolve_channel_fingerprint_ids(
        &lc.config,
        &lc.config_path,
        &[],
        None,
        Some(catalog),
    )
    .map_err(|e| e.to_string())?;
    let entries = catalog.entries_for(&ids).map_err(|e| e.to_string())?;
    let digest = projection::candidates_digest(
        ids.into_iter()
            .map(|id| {
                let ms = entries.get(&id).and_then(|e| e.duration_ms);
                (id, ms)
            })
            .collect(),
    );
    review.candidate_digests.insert(
        lc.config_path.clone(),
        (mtime, std::time::Instant::now(), digest.clone()),
    );
    Ok(digest)
}

/// One channel's candidate entry ids, resolved from its config with no
/// scorer run, cached until the file changes.
fn channel_fingerprint(
    review: &mut ReviewState,
    catalog: &Catalog,
    lc: &config::LoadedChannel,
) -> Result<Arc<HashSet<String>>, String> {
    let mtime = file_mtime(&lc.config_path)?;
    if let Some((cached_mtime, ids)) = review.fingerprints.get(&lc.config_path)
        && *cached_mtime == mtime
    {
        return Ok(ids.clone());
    }
    let ids = crate::resolve::resolve_channel_fingerprint_ids(
        &lc.config,
        &lc.config_path,
        &[],
        None,
        Some(catalog),
    )
    .map(|ids| Arc::new(ids.into_iter().collect::<HashSet<_>>()))
    .map_err(|e| e.to_string())?;
    review
        .fingerprints
        .insert(lc.config_path.clone(), (mtime, ids.clone()));
    Ok(ids)
}

/// Whether a channel's candidates hold every movie or every episode in the
/// catalog — a channel that can air anything shares everything, so listing
/// its overlap says nothing.
fn is_whole_library(library: &(HashSet<String>, HashSet<String>), ids: &HashSet<String>) -> bool {
    let (movies, episodes) = library;
    (!movies.is_empty() && movies.is_subset(ids))
        || (!episodes.is_empty() && episodes.is_subset(ids))
}

/// Every other channel sharing candidates with `lc`, largest first, each with
/// the shared titles (episodes rolled up to their show).
fn overlap_json(
    state: &mut AppState,
    station: &config::Station,
    lc: &config::LoadedChannel,
) -> Result<Value, String> {
    let began = std::time::Instant::now();
    let cold = state.review.library.is_none() || state.review.fingerprints.is_empty();
    state.review.ensure_library(&state.catalog)?;
    let own = channel_fingerprint(&mut state.review, &state.catalog, lc)?;
    let library = state.review.library.as_ref().expect("filled above");
    let own_whole = is_whole_library(library, &own);

    let mut whole_library = Vec::new();
    let mut errors = Vec::new();
    let mut shared: Vec<(&config::LoadedChannel, Vec<String>)> = Vec::new();
    let mut others = Vec::new();
    for other in station
        .channels
        .iter()
        .filter(|c| c.config_path != lc.config_path)
    {
        match channel_fingerprint(&mut state.review, &state.catalog, other) {
            Ok(ids) => others.push((other, ids)),
            Err(e) => {
                errors.push(json!({ "channel": channel_key(&other.config_path), "error": e }))
            }
        }
    }
    let library = state.review.library.as_ref().expect("filled above");
    for (other, ids) in others {
        if is_whole_library(library, &ids) {
            whole_library.push(channel_key(&other.config_path));
            continue;
        }
        if own_whole {
            continue;
        }
        let common: Vec<String> = own.intersection(&ids).cloned().collect();
        if !common.is_empty() {
            shared.push((other, common));
        }
    }
    shared.sort_by(|a, b| {
        b.1.len()
            .cmp(&a.1.len())
            .then(a.0.config.number.cmp(&b.0.config.number))
    });

    let channels: Vec<Value> = shared
        .iter()
        .map(|(other, ids)| {
            json!({
                "channel": channel_key(&other.config_path),
                "number": other.config.number,
                "display_name": other.config.display_name,
                "shared": ids.len(),
                "titles": shared_titles(&state.catalog, ids),
            })
        })
        .collect();
    tracing::info!(
        event = "ui.review.overlap",
        channel = %channel_key(&lc.config_path),
        cold,
        channels = station.channels.len(),
        ms = began.elapsed().as_millis() as u64,
        "review: overlap computed"
    );
    Ok(json!({
        "this_channel_whole_library": own_whole,
        "candidates": own.len(),
        "channels": channels,
        "whole_library": whole_library,
        "errors": errors,
    }))
}

/// Shared ids as titles: a film as `Title (Year)`, a show's episodes as one
/// `Show (n episodes)` line. Most shared first.
fn shared_titles(catalog: &Catalog, ids: &[String]) -> Vec<String> {
    let mut shows: HashMap<String, usize> = HashMap::new();
    let mut films: Vec<String> = Vec::new();
    for id in ids {
        match catalog.entry(id).ok().flatten() {
            Some(e) => match &e.show {
                Some(show) => *shows.entry(show.clone()).or_insert(0) += 1,
                None => films.push(airing_label(&e)),
            },
            None => films.push(id.clone()),
        }
    }
    let mut shows: Vec<(String, usize)> = shows.into_iter().collect();
    shows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    films.sort();
    shows
        .into_iter()
        .map(|(show, n)| {
            if n == 1 {
                format!("{show} (1 episode)")
            } else {
                format!("{show} ({n} episodes)")
            }
        })
        .chain(films)
        .collect()
}

/// The report itself: header, tiles, depth rows, overlap and tune-in samples.
fn build_review(
    state: &mut AppState,
    station: &config::Station,
    lc: &config::LoadedChannel,
    airings: &[crate::review::ReviewAiring],
    simulated: &[&crate::simulate::SimulatedAiring],
    start: time::OffsetDateTime,
    end: time::OffsetDateTime,
) -> Result<Value, String> {
    use crate::review;

    let days = (end - start).as_seconds_f64() / 86_400.0;
    let first_repeat = review::first_repeat(airings, start, |_| true)
        .map(|r| json!({ "days": r.days, "index": r.index, "label": airings[r.index].title }));

    // The last airing can run past the span's end; only its share inside the
    // span counts toward airtime.
    let clipped: Vec<review::ReviewAiring> = airings
        .iter()
        .map(|a| review::ReviewAiring {
            duration: a.duration.min(end - a.start),
            ..a.clone()
        })
        .collect();
    let by_kind = review::airtime_by_kind(&clipped);
    let total: f64 = by_kind.values().sum();
    let share = |kind: &str| {
        if total > 0.0 {
            by_kind.get(kind).copied().unwrap_or(0.0) / total
        } else {
            0.0
        }
    };

    let depth: Vec<Value> = review::depth_rows(airings, start)
        .into_iter()
        .map(|row| {
            json!({
                "kind": row.kind,
                "name": row.name,
                "pool": row.pool,
                "aired": row.aired,
                "distinct": row.distinct,
                "first_repeat_days": row.first_repeat.as_ref().map(|r| r.days),
                "first_repeat_label": row.first_repeat.as_ref().map(|r| airings[r.index].title.clone()),
            })
        })
        .collect();

    let seed = review_seed(station, lc, start);
    let mut samples: Vec<Value> = Vec::new();
    for (n, t) in review::tune_in_samples(airings, start, end, seed, TUNE_IN_SAMPLES)
        .into_iter()
        .enumerate()
    {
        let next: Vec<usize> = (t.index + 1..airings.len()).take(2).collect();
        let still = offer_still(
            state,
            station,
            lc,
            Neighbours {
                prev: t.index.checked_sub(1).map(|i| simulated[i]),
                airing: simulated[t.index],
                next: simulated.get(t.index + 1).copied(),
            },
            t.into,
            n + 1,
        );
        samples.push(json!({
            "n": n + 1,
            "at": t.at.unix_timestamp(),
            "index": t.index,
            "minutes_in": t.into.whole_minutes(),
            "minutes_total": airings[t.index].duration.whole_minutes(),
            "next": next,
            "still": still,
        }));
    }

    let distinct = airings
        .iter()
        .map(|a| a.id.as_str())
        .collect::<HashSet<_>>()
        .len();
    let overlap = overlap_json(state, station, lc)?;

    let random_order_pools: HashSet<String> = lc
        .config
        .rule
        .blocks
        .iter()
        .flat_map(|b| &b.pools)
        .filter(|p| matches!(p.order, Some(config::Order::Random)))
        .map(|p| p.name.clone())
        .collect();
    let show_ids: Vec<String> = airings
        .iter()
        .filter_map(|a| a.show_id.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let episode_counts = state
        .catalog
        .episode_counts_for(&show_ids)
        .map_err(|e| e.to_string())?;
    let principles: Vec<Value> = review::grade_principles(&review::GradeInputs {
        airings,
        start,
        end,
        tz: crate::tz::parse(&station.station.tz).map_err(|e| e.to_string())?,
        random_order_pools: &random_order_pools,
        episode_counts: &episode_counts,
    })
    .into_iter()
    .map(|p| {
        json!({
            "name": p.name,
            "grade": p.grade,
            "value": p.value,
            "measure": p.measure,
            "not_scored": p.not_scored,
        })
    })
    .collect();

    Ok(json!({
        "channel": channel_key(&lc.config_path),
        "config_path": lc.config_path.display().to_string(),
        "number": lc.config.number,
        "name": lc.name,
        "display_name": lc.config.display_name,
        "seed": lc.config.seed,
        "tz": station.station.tz,
        "days": days.round() as u32,
        "start": start.unix_timestamp(),
        "end": end.unix_timestamp(),
        "blocks": blocks_json(&lc.config),
        "first_repeat": first_repeat,
        "airtime": {
            "total_secs": total,
            "by_kind": by_kind,
            "film_share": share("movie"),
            "series_share": share("episode"),
        },
        "items_aired": airings.len(),
        "distinct_items": distinct,
        "depth": depth,
        "principles": principles,
        "overlap": overlap,
        "samples": samples,
    }))
}

/// The channel's seed at `at` — what the review's seeded tune-in samples and
/// still names key on, so a rerun of the same span picks the same moments.
fn review_seed(
    station: &config::Station,
    lc: &config::LoadedChannel,
    at: time::OffsetDateTime,
) -> u64 {
    let tz = crate::tz::parse(&station.station.tz).unwrap_or(time_tz::timezones::db::UTC);
    crate::variety::Variety::new(&lc.config, tz)
        .dice(at, None)
        .seed
}

/// One tune-in sample's still, as the page reads it: `{url}` when there is a
/// media file to grab a frame from, else `{unavailable, path?}` naming why
/// not. Registers the still so its URL renders it on first request.
fn offer_still(
    state: &mut AppState,
    station: &config::Station,
    lc: &config::LoadedChannel,
    at: Neighbours<'_>,
    into: time::Duration,
    n: usize,
) -> Value {
    let still = match TuneInStill::offer(
        state.catalog.sources_for(&at.airing.id),
        &MediaRemap::from_env(),
        &lc.overlays,
        &station.station.normalization.video,
        &station.station.fonts,
        at,
        into,
    ) {
        Ok(still) => still,
        Err(Unavailable { why, path: None }) => return json!({ "unavailable": why }),
        Err(Unavailable {
            why,
            path: Some(path),
        }) => return json!({ "unavailable": why, "path": path.display().to_string() }),
    };
    let channel: String = channel_key(&lc.config_path)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let id = format!(
        "{channel}-s{}-n{n}-{}.jpg",
        review_seed(station, lc, at.airing.start),
        still.digest()
    );
    state.review.stills.insert(id.clone(), still);
    json!({ "url": format!("/api/stills/{id}") })
}

/// `GET /api/stills/{id}`: the still a review offered under `id`, or a 404
/// for an id no review offered.
pub(super) fn still_reply(state: &AppState, id: &str) -> Reply {
    match state.review.stills.get(id) {
        Some(still) => Reply::Still(id.to_string(), Box::new(still.clone())),
        None => Reply::Json(404, json!({ "error": "no such still; reload the review" })),
    }
}

/// Answers a still request, rendering on first request and serving from the
/// still directory after. The render runs on its own thread, which answers
/// the request, so a slow media read holds up that one image and not the rest
/// of the tool. A render that fails is a 500 naming why.
pub(super) fn respond_still(
    dir: PathBuf,
    id: String,
    still: TuneInStill,
    request: tiny_http::Request,
) {
    std::thread::spawn(
        move || match render_review_still_serially(&dir, &id, &still) {
            Ok(jpeg) => respond_jpeg(request, jpeg),
            Err(e) => respond_json(request, 500, &json!({ "error": e })),
        },
    );
}

/// One still renders at a time, whatever the number of request threads
/// waiting: a Review page asks for every sample's still at once, and each
/// render is an ffmpeg reading the media array the channels transcode from.
fn render_review_still_serially(
    dir: &Path,
    id: &str,
    still: &TuneInStill,
) -> Result<Vec<u8>, String> {
    static RENDER: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one_at_a_time = RENDER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let jpeg = render_review_still(dir, id, still)?;
    prune_still_dir(dir, STILL_DIR_MAX_FILES);
    Ok(jpeg)
}

/// The most rendered stills the still directory keeps; the oldest go first.
const STILL_DIR_MAX_FILES: usize = 200;

/// Deletes the oldest files in `dir` until at most `max` remain, so the
/// directory (the container's writable layer) stays bounded however many
/// distinct stills reviews offer. A file that cannot be listed or removed is
/// left; the next render tries again.
pub(super) fn prune_still_dir(dir: &Path, max: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some((meta.modified().ok()?, e.path()))
        })
        .collect();
    if files.len() <= max {
        return;
    }
    files.sort();
    for (_, path) in &files[..files.len() - max] {
        if std::fs::remove_file(path).is_ok() {
            tracing::debug!(event = "ui.still.pruned", path = %path.display(), "still: pruned");
        }
    }
}

fn render_review_still(dir: &Path, id: &str, still: &TuneInStill) -> Result<Vec<u8>, String> {
    let began = std::time::Instant::now();
    let served = still
        .render_cached(Path::new("ffmpeg"), dir, id)
        .inspect_err(|e| {
            tracing::warn!(
                event = "ui.still.failed",
                id,
                media = %still.media.display(),
                at_secs = still.at_secs,
                error = %e,
                "still: render failed"
            )
        })?;
    let (jpeg, rendered) = match served {
        StillJpeg::Cached(jpeg) => {
            tracing::debug!(event = "ui.still.cached", id, "still: served from cache");
            return Ok(jpeg);
        }
        StillJpeg::Rendered(jpeg, rendered) => (jpeg, rendered),
    };
    tracing::info!(
        event = "ui.still.rendered",
        id,
        media = %still.media.display(),
        at_secs = still.at_secs,
        title = ?still.program.title,
        next_title = ?still.program.next_title,
        overlay_time = ?rendered.overlay_time,
        shows_next = rendered.shows_next,
        ms = began.elapsed().as_millis() as u64,
        "still: rendered"
    );
    Ok(jpeg)
}

/// The numbers "Since last run" compares, in display order.
fn comparable_numbers(review: &Value) -> Vec<(String, Value)> {
    let pct = |v: &Value| v.as_f64().map(|f| json!((f * 1000.0).round() / 10.0));
    let mut out = vec![
        ("Days shown".to_string(), review["days"].clone()),
        (
            "Days until first repeat".to_string(),
            review["first_repeat"]["days"]
                .as_f64()
                .map(|d| json!((d * 10.0).round() / 10.0))
                .unwrap_or(Value::Null),
        ),
        (
            "Film share of airtime (%)".to_string(),
            pct(&review["airtime"]["film_share"]).unwrap_or(Value::Null),
        ),
        (
            "Series share of airtime (%)".to_string(),
            pct(&review["airtime"]["series_share"]).unwrap_or(Value::Null),
        ),
        (
            "Channels sharing content".to_string(),
            json!(
                review["overlap"]["channels"]
                    .as_array()
                    .map(Vec::len)
                    .unwrap_or(0)
            ),
        ),
        ("Items aired".to_string(), review["items_aired"].clone()),
        (
            "Distinct items".to_string(),
            review["distinct_items"].clone(),
        ),
    ];
    for row in review["depth"].as_array().into_iter().flatten() {
        if row["kind"] != "pool" {
            continue;
        }
        out.push((
            format!(
                "Pool {} — days until first repeat",
                row["name"].as_str().unwrap_or("")
            ),
            row["first_repeat_days"]
                .as_f64()
                .map(|d| json!((d * 10.0).round() / 10.0))
                .unwrap_or(Value::Null),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn near_miss_series_id_is_labelled_with_the_show_name() {
        use crate::catalog::{Entry, Source};
        let catalog = Catalog::open_in_memory().unwrap();
        let mut ep = Entry::new("ep1", "episode", "Encounter at Farpoint", Source::Plex);
        ep.show = Some("Star Trek: The Next Generation".into());
        ep.show_id = Some("imdb:tt0092455".into());
        catalog.upsert_entry(&ep).unwrap();
        let audits = vec![json!([{"detail": {"near_misses": [
            {"id": "imdb:tt0092455"}, {"id": "ep1"}, {"id": "unknown"}
        ]}}])];
        let titles = near_miss_titles(&catalog, &audits, |_| true).unwrap();
        assert_eq!(
            titles.get("imdb:tt0092455").map(String::as_str),
            Some("Star Trek: The Next Generation")
        );
        assert!(titles["ep1"].contains("Encounter at Farpoint"));
        assert!(!titles.contains_key("unknown"));
    }

    #[test]
    fn prune_still_dir_keeps_the_newest_max_files() {
        let dir = std::env::temp_dir().join(format!("etv-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let base = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        for i in 0..10u64 {
            let f = std::fs::File::create(dir.join(format!("{i}.jpg"))).unwrap();
            f.set_modified(base + std::time::Duration::from_secs(i))
                .unwrap();
        }
        prune_still_dir(&dir, 4);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["6.jpg", "7.jpg", "8.jpg", "9.jpg"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

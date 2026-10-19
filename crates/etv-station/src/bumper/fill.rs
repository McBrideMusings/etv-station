//! The align/fill pass: after a generation's programmes are laid out and
//! probed, and before they are cut into chunks, fill every gap between a
//! programme's end and its block's next `align:` grid line with bumpers drawn
//! from the block's `fill:` list.
//!
//! The pass reads only programme end times, so it does not matter who ordered
//! the programmes: a pattern block, a sequencer block and a flat list all go
//! through it. A block with no `align:` (or `align: none`) is passed over, and
//! a generation with no aligned block comes back untouched.
//!
//! Gap policy:
//!
//! - A gap always fills to the boundary, chaining bumpers for as long as it
//!   takes.
//! - A gap shorter than the block's `min_fill` is one black item with the
//!   channel's own overlay (its logo) instead of a bumper. The playout schema
//!   has no fade filter on an item, so that black item is the dip out of the
//!   programme and into the next.
//! - Each draw is a weighted pick among the `fill:` entries whose `when`
//!   holds and whose `min_gap` admits the remaining gap. When some entry's
//!   `max_gap` covers the rest of the gap, this draw is the last and must
//!   reach the boundary, so an entry whose timeline can stretch (a looped
//!   segment, or flex room up to the gap) is chosen over one that would leave
//!   a black filler. When none covers it, the draw is any admitted entry,
//!   fitted to that entry's `max_gap`.
//!
//! Every choice derives from the channel, the following programme and the air
//! date, so two runs over the same inputs produce the same bumpers.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ersatztv_playout::playout::{BUMPER_ID_PREFIX, PlayoutItem, PlayoutItemSource};
use rhai::{Dynamic, Map};
use serde_json::json;
use time::OffsetDateTime;
use time_tz::{OffsetDateTimeExt, Tz};

use super::emit::{EmitContext, emit_timeline};
use super::hook::{BumperInput, ChannelInfo, build_or_black, rng_seed};
use super::timeline::{ResolvedTimeline, SegmentSource, Timeline, fit};
use crate::config::{Align, BlockInclude, SourceConfig};
use crate::history::{HistoryDb, PlayRecord};
use crate::resolve::ResolvedItem;
use crate::resume::Progress;

/// The framerate bumper timelines are snapped to. Every channel runs at one
/// framerate (the overlay default), and the station config does not carry it.
pub const FILL_FPS: f64 = 30.0;

/// Longest a chain may run before the pass gives up and airs black for the
/// rest. A script that cannot reach its own `max_gap` never loops forever.
const MAX_DRAWS_PER_GAP: u32 = 400;

/// Most programmes after the next one that a script's `ctx.upcoming` lists.
const UPCOMING_LIMIT: usize = 12;

/// A block's fill settings, resolved against the channel config's directory.
#[derive(Debug, Clone)]
pub struct FillPlan {
    pub every: Duration,
    pub min_fill: Duration,
    pub entries: Vec<FillCandidate>,
}

#[derive(Debug, Clone)]
pub struct FillCandidate {
    pub script: PathBuf,
    pub weight: u32,
    pub when: Option<String>,
    pub min_gap_ms: u64,
    pub max_gap_ms: Option<u64>,
    pub granted: Vec<String>,
    /// The entry's `config:`, handed to the script as `ctx.config`.
    pub config: Option<serde_json::Value>,
}

impl FillPlan {
    /// The plan for `block`, or `None` when it has no `align:` (or
    /// `align: none`). `base_dir` is the channel config's directory.
    pub fn from_block(block: &BlockInclude, base_dir: &Path) -> Option<Self> {
        let every = match block.align? {
            Align::Off => return None,
            Align::Every(d) => d,
        };
        Some(Self {
            every,
            min_fill: block.min_fill.unwrap_or(crate::config::DEFAULT_MIN_FILL),
            entries: block
                .fill
                .iter()
                .map(|e| FillCandidate {
                    script: crate::score::resolve_plugin_path(base_dir, &e.script),
                    weight: e.weight,
                    when: e.when.clone(),
                    min_gap_ms: e.min_gap.map_or(0, |d| d.as_millis() as u64),
                    max_gap_ms: e.max_gap.map(|d| d.as_millis() as u64),
                    granted: e.capabilities.clone(),
                    config: e.config.clone(),
                })
                .collect(),
        })
    }
}

/// Everything the pass needs that is not the schedule itself.
pub struct FillEnv<'a> {
    pub channel: ChannelInfo,
    pub tz: &'static Tz,
    /// Directory a bumper's relative overlay script path resolves against.
    pub overlay_root: &'a Path,
    pub history: Option<Arc<HistoryDb>>,
    /// One plan per block index (`ResolvedItem::block`); `None` for a block
    /// that does not align.
    pub plans: Vec<Option<FillPlan>>,
    /// The item map `ctx.next` / `ctx.prev` carry for a programme.
    pub item_map: &'a dyn Fn(&ResolvedItem) -> Map,
}

/// A generation's schedule: items, their lengths and the resume state entering
/// each, in parallel.
#[derive(Debug)]
pub struct Laid {
    pub items: Vec<ResolvedItem>,
    pub durations: Vec<Duration>,
    pub progress: Vec<Progress>,
    /// The airings-ledger rows for the bumper segments [`fill_gaps`] inserted,
    /// for the caller to record once the generation is on disk. Empty on input.
    pub bumper_records: Vec<PlayRecord>,
}

/// One bumper playout item, as the schedule's next entry.
#[derive(Debug)]
pub struct Piece {
    pub item: ResolvedItem,
    pub duration: Duration,
}

/// The first grid line at or after `t`: lines every `every` from local
/// midnight in `tz`.
pub fn boundary_at_or_after(t: OffsetDateTime, every: Duration, tz: &'static Tz) -> OffsetDateTime {
    let midnight = crate::tz::local_midnight_at_or_before(t, tz);
    let step = time::Duration::try_from(every).expect("an align fits in a time::Duration");
    let step_ns = step.whole_nanoseconds().max(1);
    let lines = ((t - midnight).whole_nanoseconds() + step_ns - 1) / step_ns;
    midnight + step * i32::try_from(lines).expect("a day holds few grid lines")
}

/// Insert bumpers into every gap the aligned blocks leave. Items before `skip`
/// are carried through untouched and take no time. `start` is when item
/// `skip` begins. With `fill_leading`, the gap between `start` and the first
/// grid line at or after it is filled before item `skip`. `end_progress` is
/// the resume state after the last item, which the trailing bumpers carry.
pub fn fill_gaps(
    env: &FillEnv<'_>,
    laid: Laid,
    skip: usize,
    start: OffsetDateTime,
    fill_leading: bool,
    end_progress: &Progress,
) -> Laid {
    let n = laid.items.len();
    if env.plans.iter().all(Option::is_none) || n <= skip {
        return laid;
    }

    // Pass 1, over borrowed items: the pieces that lead item `skip` and the
    // ones that follow each item.
    let mut lead: Vec<Piece> = Vec::new();
    let mut records: Vec<PlayRecord> = Vec::new();
    let mut after: Vec<Vec<Piece>> = (0..n).map(|_| Vec::new()).collect();
    let mut at = start;
    if fill_leading {
        let first = &laid.items[skip];
        if let Some(plan) = plan_for(env, first) {
            lead = fill_gap(
                env,
                plan,
                at,
                None,
                &laid.items[skip..],
                first.block,
                &mut records,
            );
            at = advance(at, &lead);
        }
    }
    for (i, (item, duration)) in laid
        .items
        .iter()
        .zip(&laid.durations)
        .enumerate()
        .skip(skip)
    {
        at += time::Duration::seconds_f64(duration.as_secs_f64());
        if let Some(plan) = plan_for(env, item) {
            after[i] = fill_gap(
                env,
                plan,
                at,
                Some(item),
                laid.items.get(i + 1..).unwrap_or_default(),
                item.block,
                &mut records,
            );
            at = advance(at, &after[i]);
        }
    }

    // Pass 2: move the programmes into a new schedule with the pieces between.
    // A bumper carries the resume state entering the item after it, so a wipe
    // cut at its start rewinds to there and the item before it is not aired
    // twice.
    let states = laid.progress.clone();
    let mut out = Laid {
        items: Vec::with_capacity(n),
        durations: Vec::with_capacity(n),
        progress: Vec::with_capacity(n),
        bumper_records: records,
    };
    let push = |out: &mut Laid, pieces: Vec<Piece>, state: &Progress| {
        for piece in pieces {
            out.items.push(piece.item);
            out.durations.push(piece.duration);
            out.progress.push(state.clone());
        }
    };
    let mut lead = Some(lead);
    for (i, ((item, duration), state)) in laid
        .items
        .into_iter()
        .zip(laid.durations)
        .zip(laid.progress)
        .enumerate()
    {
        if i == skip
            && let Some(pieces) = lead.take()
        {
            push(&mut out, pieces, &state);
        }
        out.items.push(item);
        out.durations.push(duration);
        out.progress.push(state);
        let entering = states.get(i + 1).unwrap_or(end_progress);
        push(&mut out, std::mem::take(&mut after[i]), entering);
    }
    out
}

fn plan_for<'a>(env: &'a FillEnv<'_>, item: &ResolvedItem) -> Option<&'a FillPlan> {
    env.plans.get(item.block).and_then(Option::as_ref)
}

/// `at` advanced by `pieces`, walked the way [`crate::rule::Sequential`] walks.
fn advance(mut at: OffsetDateTime, pieces: &[Piece]) -> OffsetDateTime {
    for p in pieces {
        at += time::Duration::seconds_f64(p.duration.as_secs_f64());
    }
    at
}

/// The pieces that fill the gap from `at` to the next grid line. Empty when
/// `at` is on the grid. `ahead` is the schedule after the gap: its first item
/// is the programme the gap leads into, the rest are `ctx.upcoming`.
fn fill_gap(
    env: &FillEnv<'_>,
    plan: &FillPlan,
    at: OffsetDateTime,
    prev: Option<&ResolvedItem>,
    ahead: &[ResolvedItem],
    block: usize,
    records: &mut Vec<PlayRecord>,
) -> Vec<Piece> {
    let boundary = boundary_at_or_after(at, plan.every, env.tz);
    // A generation's next start is summed in a different order than the walk
    // that landed on the line, so it can sit a few nanoseconds past it. That is
    // on the grid, not a whole step early for the next line.
    let step = time::Duration::try_from(plan.every).unwrap_or(time::Duration::ZERO);
    if boundary <= at || (boundary - at) > step - time::Duration::milliseconds(1) {
        return Vec::new();
    }
    let air_date = at.to_timezone(env.tz).date().to_string();
    let prev_map = prev.map(env.item_map);
    let next_map = ahead.first().map(env.item_map);
    let upcoming: Vec<Map> = ahead
        .iter()
        .skip(1)
        .take(UPCOMING_LIMIT)
        .map(env.item_map)
        .collect();
    let seed = rng_seed(
        &env.channel.name,
        ahead.first().map(|n| n.id.as_str()),
        &air_date,
    );

    let mut pieces: Vec<Piece> = Vec::new();
    let mut cursor = at;
    let mut draw = 0u32;
    while cursor < boundary {
        let remaining = boundary - cursor;
        let remaining_ms = round_ms(remaining);
        // Under a millisecond is rounding noise, not a gap: with a bumper
        // already in the gap its length absorbs it (`land_on`); with none, the
        // programme simply ends that much short of the grid line.
        if remaining_ms == 0 {
            break;
        }
        let under_min = remaining.unsigned_abs() < plan.min_fill;
        if under_min || draw >= MAX_DRAWS_PER_GAP {
            let why = if under_min {
                "under_min_fill"
            } else {
                "draw_cap"
            };
            pieces.push(black_piece(env, block, cursor, remaining, why));
            break;
        }

        let admitted: Vec<&FillCandidate> = plan
            .entries
            .iter()
            .filter(|e| remaining_ms >= e.min_gap_ms)
            .filter(|e| when_holds(e, remaining_ms, prev_map.as_ref(), next_map.as_ref(), env))
            .collect();
        if admitted.is_empty() {
            tracing::warn!(
                event = "bumper.no_candidate",
                channel = %env.channel.name,
                gap_ms = remaining_ms,
                "no fill entry admits this gap; airing black for it",
            );
            pieces.push(black_piece(env, block, cursor, remaining, "no_candidate"));
            break;
        }
        let covering: Vec<&FillCandidate> = admitted
            .iter()
            .copied()
            .filter(|e| e.max_gap_ms.is_none_or(|max| max >= remaining_ms))
            .collect();
        let last_draw = !covering.is_empty();
        let pool = if last_draw { &covering } else { &admitted };
        let order = weighted_order(pool, crate::pattern::mix_seed(seed, u64::from(draw)));

        let mut chosen: Option<Drawn> = None;
        for cand in order {
            let draw_ms = if last_draw {
                remaining_ms
            } else {
                cand.max_gap_ms.unwrap_or(remaining_ms).min(remaining_ms)
            };
            let drawn = draw_one(
                env,
                cand,
                draw_ms,
                draw,
                &Around {
                    prev: prev_map.as_ref(),
                    next: next_map.as_ref(),
                    upcoming: &upcoming,
                },
                &air_date,
            );
            let good = drawn.error.is_none() && (!last_draw || can_reach(&drawn.timeline, draw_ms));
            if chosen.is_none() || good {
                chosen = Some(drawn);
            }
            if good {
                break;
            }
        }
        let drawn = chosen.expect("a non-empty pool yields a draw");

        let Some(resolved) = drawn.resolved else {
            pieces.push(black_piece(env, block, cursor, remaining, "unfittable"));
            break;
        };
        let emit_ctx = EmitContext {
            channel: &env.channel.name,
            fps: FILL_FPS,
            overlay_root: env.overlay_root,
            media_len_ms: &crate::duration::media_len_ms,
        };
        let items = emit_timeline(&resolved, &emit_ctx, cursor);
        records.extend(super::ledger::records(
            &resolved,
            cursor,
            OffsetDateTime::now_utc(),
            &drawn.script.display().to_string(),
        ));
        log_draw(
            env,
            &drawn.script,
            remaining_ms,
            draw,
            &resolved,
            &drawn.error,
            cursor,
        );
        let before = pieces.len();
        for item in items {
            pieces.push(piece_from(item, block));
        }
        let moved = advance(cursor, &pieces[before..]);
        if moved <= cursor {
            // A timeline that resolved to nothing would draw forever.
            pieces.push(black_piece(env, block, cursor, remaining, "empty_draw"));
            break;
        }
        cursor = moved;
        draw += 1;
    }

    land_on(&mut pieces, at, boundary);
    pieces
}

/// One script's built and fitted timeline.
struct Drawn {
    script: PathBuf,
    timeline: Timeline,
    resolved: Option<ResolvedTimeline>,
    error: Option<String>,
}

/// The programmes a draw sits between, as the item maps `ctx` carries.
struct Around<'a> {
    prev: Option<&'a Map>,
    next: Option<&'a Map>,
    upcoming: &'a [Map],
}

fn draw_one(
    env: &FillEnv<'_>,
    cand: &FillCandidate,
    gap_ms: u64,
    draw: u32,
    around: &Around<'_>,
    air_date: &str,
) -> Drawn {
    let input = BumperInput {
        gap_ms,
        next: around.next.cloned(),
        prev: around.prev.cloned(),
        upcoming: around.upcoming.to_vec(),
        config: cand.config.clone(),
        channel: env.channel.clone(),
        air_date: air_date.to_string(),
        draw,
        granted: cand.granted.clone(),
        history: env.history.clone(),
    };
    let (built, error) = build_or_black(&cand.script, &input);
    let resolved = match fit(&built.timeline, gap_ms, FILL_FPS) {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::warn!(
                event = "bumper.fit_failed",
                channel = %env.channel.name,
                script = %cand.script.display(),
                gap_ms = gap_ms,
                error = %e,
                "a bumper timeline did not fit its gap; airing black for it",
            );
            None
        }
    };
    Drawn {
        script: cand.script.clone(),
        timeline: built.timeline,
        resolved,
        error: error.map(|e| e.to_string()),
    }
}

/// Whether `timeline` can cover `gap_ms` without a black filler: a looped
/// segment stretches, otherwise the segments' longest lengths must add up.
fn can_reach(timeline: &Timeline, gap_ms: u64) -> bool {
    use super::timeline::Duration as D;
    if timeline.segments.iter().any(|s| s.loop_) {
        return true;
    }
    let longest: u64 = timeline
        .segments
        .iter()
        .map(|s| match s.duration {
            D::Fixed(ms) => ms,
            D::Flex { max_ms, .. } => max_ms,
        })
        .sum();
    longest >= gap_ms
}

/// `entries` in a deterministic weighted order: each position is a draw
/// without replacement, weighted by `weight`.
fn weighted_order<'a>(entries: &[&'a FillCandidate], seed: u64) -> Vec<&'a FillCandidate> {
    let mut left: Vec<&FillCandidate> = entries.to_vec();
    let mut out = Vec::with_capacity(left.len());
    let mut n = 0u64;
    while !left.is_empty() {
        let total: u64 = left.iter().map(|e| u64::from(e.weight.max(1))).sum();
        let mut pick = crate::pattern::mix_seed(seed, n) % total;
        let index = left
            .iter()
            .position(|e| {
                let w = u64::from(e.weight.max(1));
                if pick < w {
                    true
                } else {
                    pick -= w;
                    false
                }
            })
            .unwrap_or(0);
        out.push(left.remove(index));
        n += 1;
    }
    out
}

/// Evaluate an entry's `when:` CEL over `next`, `prev`, `gap_ms` and
/// `channel`. A missing `when` holds; an expression that fails to run, or
/// does not yield a bool, does not.
fn when_holds(
    cand: &FillCandidate,
    gap_ms: u64,
    prev: Option<&Map>,
    next: Option<&Map>,
    env: &FillEnv<'_>,
) -> bool {
    let Some(src) = &cand.when else {
        return true;
    };
    let json = |m: Option<&Map>| -> serde_json::Value {
        m.and_then(|m| rhai::serde::from_dynamic(&Dynamic::from_map(m.clone())).ok())
            .unwrap_or(serde_json::Value::Null)
    };
    let result = (|| -> Result<bool, String> {
        let program = cel::Program::compile(src).map_err(|e| e.to_string())?;
        let mut ctx = cel::Context::default();
        let vars = [
            ("next", json(next)),
            ("prev", json(prev)),
            ("gap_ms", json!(gap_ms)),
            (
                "channel",
                json!({"name": env.channel.name, "number": env.channel.number}),
            ),
        ];
        for (name, value) in vars {
            ctx.add_variable(name, value).map_err(|e| e.to_string())?;
        }
        match program.execute(&ctx).map_err(|e| e.to_string())? {
            cel::Value::Bool(b) => Ok(b),
            other => Err(format!("yielded {other:?}, not a bool")),
        }
    })();
    match result {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                event = "bumper.when_failed",
                channel = %env.channel.name,
                script = %cand.script.display(),
                when = %src,
                error = %e,
                "a fill entry's `when` did not evaluate; skipping the entry",
            );
            false
        }
    }
}

fn log_draw(
    env: &FillEnv<'_>,
    script: &Path,
    gap_ms: u64,
    draw: u32,
    resolved: &ResolvedTimeline,
    error: &Option<String>,
    at: OffsetDateTime,
) {
    let segments: Vec<serde_json::Value> = resolved
        .segments
        .iter()
        .map(|s| {
            json!({
                "name": s.segment.name,
                "duration_ms": s.duration_ms,
                "source": source_label(&s.segment.source),
                "filler": s.filler,
            })
        })
        .collect();
    tracing::info!(
        event = "bumper.fill",
        channel = %env.channel.name,
        script = %script.display(),
        at = %at,
        gap_ms = gap_ms,
        draw = draw,
        segments = %serde_json::Value::Array(segments),
        fallback = error.as_deref().unwrap_or("none"),
        "filled part of a gap with a bumper",
    );
}

fn source_label(source: &SegmentSource) -> String {
    match source {
        SegmentSource::Clip { path, .. } => path.display().to_string(),
        SegmentSource::Lavfi { params } => format!("lavfi:{params}"),
        SegmentSource::Still { image_path } => image_path.display().to_string(),
        SegmentSource::Render { path, .. } => path.display().to_string(),
        SegmentSource::Pending { path, stand_in } => {
            format!(
                "pending:{} (stand-in {})",
                path.display(),
                stand_in.display()
            )
        }
        SegmentSource::Http { url, .. } => {
            format!("http:{}", crate::plugin_http::display_host(url))
        }
    }
}

/// One black item of `length`, drawing the channel's own overlay (its logo).
fn black_piece(
    env: &FillEnv<'_>,
    block: usize,
    at: OffsetDateTime,
    length: time::Duration,
    why: &str,
) -> Piece {
    let unix_ms = at.unix_timestamp() * 1000 + i64::from(at.millisecond());
    let id = format!("{BUMPER_ID_PREFIX}{}:{unix_ms}:black", env.channel.name);
    let mut item = PlayoutItem::scheduled(
        id,
        at,
        at + length,
        PlayoutItemSource::lavfi("color=c=black".to_string()),
    );
    item.metadata = Some(json!({"bumper": true, "segment": "black", "why": why}));
    tracing::info!(
        event = "bumper.fill",
        channel = %env.channel.name,
        script = "",
        at = %at,
        gap_ms = round_ms(length),
        draw = 0,
        segments = %json!([{"name": "black", "duration_ms": round_ms(length), "source": "lavfi:color=c=black"}]),
        fallback = why,
        "aired black for a gap",
    );
    piece_from(item, block)
}

/// A playout item as a schedule entry: its identity and source for the
/// bookkeeping that reads them, the whole item for the emitter.
fn piece_from(item: PlayoutItem, block: usize) -> Piece {
    let ns = (item.finish - item.start).whole_nanoseconds().max(0) as u64;
    let ms = |v: Option<u64>| v.map(Duration::from_millis);
    let (source, in_point, out_point) = match &item.source {
        Some(PlayoutItemSource::Local {
            path,
            in_point_ms,
            out_point_ms,
            ..
        }) => (
            SourceConfig::Local { path: path.clone() },
            ms(*in_point_ms),
            ms(*out_point_ms),
        ),
        Some(PlayoutItemSource::Lavfi { params, .. }) => (
            SourceConfig::Lavfi {
                params: params.clone(),
            },
            None,
            None,
        ),
        _ => (
            SourceConfig::Lavfi {
                params: "color=c=black".into(),
            },
            None,
            None,
        ),
    };
    let stored = serde_json::to_value(&item).unwrap_or(serde_json::Value::Null);
    Piece {
        duration: Duration::from_nanos(ns),
        item: ResolvedItem {
            id: item.id,
            block,
            source,
            in_point,
            out_point,
            program: None,
            catalog_duration: None,
            error_card: false,
            metadata: item.metadata,
            guide: None,
            guide_fields: Default::default(),
            tracks: None,
            selected: None,
            bumper: Some(stored),
        },
    }
}

/// Nudge the last piece's length so that walking `at` through `pieces` the way
/// [`crate::rule::Sequential`] does lands exactly on `boundary`. Segment
/// lengths are whole milliseconds and the programme before ends on an
/// arbitrary nanosecond, so the residual belongs to the last piece.
fn land_on(pieces: &mut [Piece], at: OffsetDateTime, boundary: OffsetDateTime) {
    for _ in 0..8 {
        let end = advance(at, pieces);
        let miss = boundary - end;
        if miss.is_zero() {
            return;
        }
        let Some(last) = pieces.last_mut() else {
            return;
        };
        let ns = last.duration.as_nanos() as i128 + miss.whole_nanoseconds();
        last.duration = Duration::from_nanos(ns.max(0) as u64);
    }
}

fn round_ms(d: time::Duration) -> u64 {
    (d.whole_nanoseconds().max(0) as u64 + 500_000) / 1_000_000
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn tz() -> &'static Tz {
        crate::tz::parse("UTC").unwrap()
    }

    const LOOPY: &str = r#"
fn hooks() { ["bumper"] }
fn build(ctx) {
    let t = timeline();
    t.push(segment().black().flex(2.0, 600.0, 1).looped());
    t
}
"#;

    /// Fixed 20s and nothing that stretches: it cannot reach a long gap.
    const RIGID: &str = r#"
fn hooks() { ["bumper"] }
fn build(ctx) {
    let t = timeline();
    t.push(segment().black().flex(20.0, 20.0, 1));
    t
}
"#;

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    fn cand(script: &Path) -> FillCandidate {
        FillCandidate {
            script: script.to_path_buf(),
            weight: 1,
            when: None,
            min_gap_ms: 0,
            max_gap_ms: None,
            granted: vec![],
            config: None,
        }
    }

    fn plan(entries: Vec<FillCandidate>) -> FillPlan {
        FillPlan {
            every: Duration::from_secs(1800),
            min_fill: Duration::from_secs(2),
            entries,
        }
    }

    fn programme(id: &str) -> ResolvedItem {
        ResolvedItem {
            id: id.into(),
            block: 0,
            source: SourceConfig::Lavfi {
                params: "testsrc".into(),
            },
            in_point: None,
            out_point: None,
            program: None,
            catalog_duration: None,
            error_card: false,
            metadata: None,
            guide: None,
            guide_fields: Default::default(),
            tracks: None,
            selected: None,
            bumper: None,
        }
    }

    fn laid(spans: &[(&str, Duration)]) -> Laid {
        Laid {
            items: spans.iter().map(|(id, _)| programme(id)).collect(),
            durations: spans.iter().map(|(_, d)| *d).collect(),
            progress: spans.iter().map(|_| Progress::default()).collect(),
            bumper_records: Vec::new(),
        }
    }

    fn mins(m: u64) -> Duration {
        Duration::from_secs(m * 60)
    }

    fn run(
        plan: FillPlan,
        laid: Laid,
        start: OffsetDateTime,
        leading: bool,
        kind: &dyn Fn(&ResolvedItem) -> Map,
    ) -> Laid {
        let root = Path::new(".");
        let env = FillEnv {
            channel: ChannelInfo {
                name: "test".into(),
                ..Default::default()
            },
            tz: tz(),
            overlay_root: root,
            history: None,
            plans: vec![Some(plan)],
            item_map: kind,
        };
        fill_gaps(&env, laid, 0, start, leading, &Progress::default())
    }

    fn plain(item: &ResolvedItem) -> Map {
        let mut m = Map::new();
        m.insert("entry_id".into(), item.id.clone().into());
        m.insert("title".into(), item.id.clone().into());
        m.insert("type".into(), "movie".into());
        m
    }

    /// Wall-clock `(start, finish)` of every item, walked as `Sequential` does.
    fn spans(laid: &Laid, start: OffsetDateTime) -> Vec<(OffsetDateTime, OffsetDateTime)> {
        crate::rule::Sequential::new(&laid.items, &laid.durations)
            .item_spans(start)
            .collect()
    }

    /// The items strictly between programmes `from` and `to`.
    fn between<'a>(laid: &'a Laid, from: &str, to: &str) -> &'a [ResolvedItem] {
        let a = laid.items.iter().position(|i| i.id == from).unwrap();
        let b = laid.items.iter().position(|i| i.id == to).unwrap();
        &laid.items[a + 1..b]
    }

    fn gap_length(laid: &Laid, from: &str, to: &str) -> Duration {
        let a = laid.items.iter().position(|i| i.id == from).unwrap();
        let b = laid.items.iter().position(|i| i.id == to).unwrap();
        laid.durations[a + 1..b].iter().sum()
    }

    fn bumpers(laid: &Laid) -> usize {
        laid.items.iter().filter(|i| i.bumper.is_some()).count()
    }

    #[test]
    fn a_22_minute_programme_on_a_30m_grid_gets_8_minutes_ending_on_the_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let loopy = script(dir.path(), "loopy.rhai", LOOPY);
        let start = datetime!(2026-10-17 20:00 UTC);
        let out = run(
            plan(vec![cand(&loopy)]),
            laid(&[("a", mins(22)), ("b", mins(30))]),
            start,
            false,
            &plain,
        );

        let walk = spans(&out, start);
        let (b_at, _) = walk[out.items.iter().position(|i| i.id == "b").unwrap()];
        assert_eq!(
            b_at,
            datetime!(2026-10-17 20:30 UTC),
            "b starts on the grid"
        );
        assert_eq!(gap_length(&out, "a", "b"), mins(8));
        assert!(bumpers(&out) >= 1);
    }

    #[test]
    fn each_drawn_timeline_leaves_bumper_ledger_rows_at_its_gap_start() {
        let dir = tempfile::tempdir().unwrap();
        let loopy = script(dir.path(), "loopy.rhai", LOOPY);
        let start = datetime!(2026-10-17 20:00 UTC);
        let out = run(
            plan(vec![cand(&loopy)]),
            laid(&[("a", mins(22)), ("b", mins(30))]),
            start,
            false,
            &plain,
        );

        assert_eq!(out.bumper_records.len(), 1, "one segment, one draw");
        let row = &out.bumper_records[0];
        assert_eq!(row.kind, crate::history::AiringKind::Bumper);
        assert_eq!(row.start, datetime!(2026-10-17 20:22 UTC));
        assert_eq!(row.source.as_deref(), Some("color=c=black"));
        assert_eq!(
            row.produced_by.as_deref(),
            Some(loopy.display().to_string().as_str())
        );
    }

    #[test]
    fn a_one_second_gap_is_one_black_item_with_the_channel_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let loopy = script(dir.path(), "loopy.rhai", LOOPY);
        let start = datetime!(2026-10-17 20:00 UTC);
        let out = run(
            plan(vec![cand(&loopy)]),
            laid(&[("a", Duration::from_secs(30 * 60 - 1)), ("b", mins(30))]),
            start,
            false,
            &plain,
        );
        assert_eq!(out.items.len(), 3, "a, one black, b");
        let black = &out.items[1];
        assert_eq!(out.durations[1], Duration::from_secs(1));
        let stored = black.bumper.as_ref().unwrap();
        assert_eq!(stored["source"]["params"], "color=c=black");
        assert!(
            stored.get("overlay").is_none(),
            "no overlay key means the channel's own overlay, its logo"
        );
        assert_eq!(black.metadata.as_ref().unwrap()["why"], "under_min_fill");
    }

    #[test]
    fn a_28_minute_gap_chains_bumpers_and_the_last_draw_loops() {
        let dir = tempfile::tempdir().unwrap();
        let rigid = script(dir.path(), "rigid.rhai", RIGID);
        let loopy = script(dir.path(), "loopy.rhai", LOOPY);
        let mut short = cand(&rigid);
        short.max_gap_ms = Some(300_000);
        let mut long = cand(&loopy);
        long.max_gap_ms = Some(300_000);
        let start = datetime!(2026-10-17 20:00 UTC);
        let out = run(
            plan(vec![short, long]),
            laid(&[("a", mins(2)), ("b", mins(30))]),
            start,
            false,
            &plain,
        );
        let walk = spans(&out, start);
        let b = out.items.iter().position(|i| i.id == "b").unwrap();
        assert_eq!(walk[b].0, datetime!(2026-10-17 20:30 UTC));
        let gap = between(&out, "a", "b");
        assert!(gap.len() >= 6, "28 minutes under 5-minute draws chains");
        assert_eq!(gap_length(&out, "a", "b"), mins(28));
        let last_ms = out.durations[b - 1];
        assert!(
            last_ms < mins(5),
            "the last draw is the remainder, {last_ms:?}"
        );
    }

    #[test]
    fn the_last_draw_prefers_an_entry_that_can_stretch() {
        let dir = tempfile::tempdir().unwrap();
        let rigid = script(dir.path(), "rigid.rhai", RIGID);
        let loopy = script(dir.path(), "loopy.rhai", LOOPY);
        let start = datetime!(2026-10-17 20:00 UTC);
        // Whichever the weighted order tries first, a rigid 20s cannot reach
        // 8 minutes, so the loop-capable entry takes the draw.
        for weight in [1, 50] {
            let mut r = cand(&rigid);
            r.weight = weight;
            let out = run(
                plan(vec![r, cand(&loopy)]),
                laid(&[("a", mins(22)), ("b", mins(30))]),
                start,
                false,
                &plain,
            );
            let gap = between(&out, "a", "b");
            assert_eq!(
                gap.len(),
                1,
                "one draw reaches the boundary (weight {weight})"
            );
            assert_eq!(out.durations[1], mins(8));
        }
    }

    #[test]
    fn when_min_gap_and_max_gap_filter_the_draw() {
        let dir = tempfile::tempdir().unwrap();
        let a = script(dir.path(), "a.rhai", LOOPY);
        let b = script(
            dir.path(),
            "b.rhai",
            &LOOPY.replace("black()", "black().name(\"from-b\")"),
        );
        let start = datetime!(2026-10-17 20:00 UTC);
        let names = |entries: Vec<FillCandidate>| -> Vec<String> {
            let out = run(
                plan(entries),
                laid(&[("a", mins(22)), ("b", mins(30))]),
                start,
                false,
                &plain,
            );
            between(&out, "a", "b")
                .iter()
                .map(|i| {
                    i.metadata.as_ref().unwrap()["segment"]
                        .as_str()
                        .unwrap()
                        .to_string()
                })
                .collect()
        };

        // `when` on the following programme's type.
        let mut only_b = FillCandidate {
            weight: 100_000,
            ..cand(&b)
        };
        only_b.when = Some(r#"next.type == "episode""#.into());
        assert!(
            names(vec![cand(&a), only_b.clone()])
                .iter()
                .all(|n| n != "from-b")
        );
        only_b.when = Some(r#"next.type == "movie""#.into());
        assert!(names(vec![only_b]).iter().all(|n| n == "from-b"));

        // `min_gap` above the 8-minute gap excludes the entry.
        let mut high_min = FillCandidate {
            weight: 100_000,
            ..cand(&b)
        };
        high_min.min_gap_ms = 9 * 60_000;
        assert!(
            names(vec![cand(&a), high_min])
                .iter()
                .all(|n| n != "from-b")
        );

        // `max_gap` below the gap makes it a chain entry, not the last draw;
        // with an entry that covers the gap, the covering one is used alone.
        let mut short_b = FillCandidate {
            weight: 100_000,
            ..cand(&b)
        };
        short_b.max_gap_ms = Some(60_000);
        let got = names(vec![cand(&a), short_b]);
        assert!(got.iter().all(|n| n != "from-b"), "got {got:?}");
    }

    /// A trailer bumper whose film has no Plex id builds nothing, so the pass
    /// draws the entry after it instead of airing black.
    #[test]
    fn a_trailer_bumper_with_no_trailer_falls_through_to_the_next_entry() {
        let dir = tempfile::tempdir().unwrap();
        let loopy = script(
            dir.path(),
            "loopy.rhai",
            &LOOPY.replace("black()", "black().name(\"from-loopy\")"),
        );
        let trailer = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/plugins/trailer-bumper.rhai");
        let only_trailer = FillCandidate {
            weight: 100_000,
            granted: vec!["http".into()],
            config: Some(serde_json::json!({ "mode": "next" })),
            ..cand(&trailer)
        };
        let out = run(
            plan(vec![only_trailer, cand(&loopy)]),
            laid(&[("a", mins(22)), ("b", mins(30))]),
            datetime!(2026-10-17 20:00 UTC),
            false,
            &plain,
        );
        let names: Vec<String> = between(&out, "a", "b")
            .iter()
            .map(|i| {
                i.metadata.as_ref().unwrap()["segment"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert!(
            !names.is_empty() && names.iter().all(|n| n == "from-loopy"),
            "{names:?}"
        );
    }

    #[test]
    fn the_same_inputs_give_the_same_bumpers() {
        let dir = tempfile::tempdir().unwrap();
        let loopy = script(dir.path(), "loopy.rhai", LOOPY);
        let rigid = script(dir.path(), "rigid.rhai", RIGID);
        let start = datetime!(2026-10-17 20:00 UTC);
        let once = || {
            let mut r = cand(&rigid);
            r.max_gap_ms = Some(60_000);
            run(
                plan(vec![r, cand(&loopy)]),
                laid(&[("a", mins(2)), ("b", mins(30))]),
                start,
                false,
                &plain,
            )
        };
        let (x, y) = (once(), once());
        let key = |l: &Laid| -> Vec<(String, Duration, Option<serde_json::Value>)> {
            l.items
                .iter()
                .zip(&l.durations)
                .map(|(i, d)| (i.id.clone(), *d, i.bumper.clone()))
                .collect()
        };
        assert_eq!(key(&x), key(&y));
    }

    #[test]
    fn programmes_come_back_in_order_and_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let loopy = script(dir.path(), "loopy.rhai", LOOPY);
        let start = datetime!(2026-10-17 20:00 UTC);
        let out = run(
            plan(vec![cand(&loopy)]),
            laid(&[("a", mins(22)), ("b", mins(25)), ("c", mins(10))]),
            start,
            false,
            &plain,
        );
        let ids: Vec<&str> = out
            .items
            .iter()
            .filter(|i| i.bumper.is_none())
            .map(|i| i.id.as_str())
            .collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert_eq!(out.items.len(), out.durations.len());
        assert_eq!(out.items.len(), out.progress.len());
        // c's trailing gap fills too, so the generation ends on the grid.
        let walk = spans(&out, start);
        assert_eq!(walk.last().unwrap().1.minute() % 30, 0);
        assert_eq!(walk.last().unwrap().1.second(), 0);
    }

    #[test]
    fn no_aligned_block_leaves_the_schedule_alone() {
        let start = datetime!(2026-10-17 20:00 UTC);
        let env = FillEnv {
            channel: ChannelInfo::default(),
            tz: tz(),
            overlay_root: Path::new("."),
            history: None,
            plans: vec![None],
            item_map: &plain,
        };
        let out = fill_gaps(
            &env,
            laid(&[("a", mins(22)), ("b", mins(25))]),
            0,
            start,
            true,
            &Progress::default(),
        );
        assert_eq!(out.items.len(), 2);
    }

    /// A generation's start can sit nanoseconds past the line the last one
    /// landed on; it must not read as a 30-minute gap to the next line.
    #[test]
    fn a_start_a_few_nanoseconds_past_a_grid_line_is_on_the_grid() {
        let dir = tempfile::tempdir().unwrap();
        let loopy = script(dir.path(), "loopy.rhai", LOOPY);
        let start = datetime!(2026-10-17 20:00 UTC) + time::Duration::nanoseconds(7);
        let out = run(
            plan(vec![cand(&loopy)]),
            laid(&[("a", mins(30))]),
            start,
            true,
            &plain,
        );
        assert_eq!(out.items.len(), 1);
    }

    #[test]
    fn a_leading_gap_fills_up_to_the_grid() {
        let dir = tempfile::tempdir().unwrap();
        let loopy = script(dir.path(), "loopy.rhai", LOOPY);
        let start = datetime!(2026-10-17 20:12 UTC);
        let out = run(
            plan(vec![cand(&loopy)]),
            laid(&[("a", mins(30))]),
            start,
            true,
            &plain,
        );
        let walk = spans(&out, start);
        let a = out.items.iter().position(|i| i.id == "a").unwrap();
        assert_eq!(walk[a].0, datetime!(2026-10-17 20:30 UTC));
        assert!(a >= 1);
    }
}

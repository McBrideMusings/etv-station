//! `GET /api/channels/{ch}/chapters`: the chapters a channel's bumper plugin has
//! queued, with the plan each was rendered from.
//!
//! A plugin names a chapter's bumper segment `chapter:<job>|<entry>|<entry>...`,
//! where `<job>` is the id `etv-station enqueue` printed. The airings ledger
//! holds one bumper row per scheduled segment, so a chapter is in it from the
//! generation that laid it, and the renderer's queue holds its plan and state.
//! This joins the two; it writes nothing.

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::graphics::{Lookup, find_channel};
use super::*;
use crate::history::{AiringFilter, AiringKind};
use crate::renderer::CHAPTER_SEGMENT_PREFIX;

/// A chapter airing within this long ago is still listed, so the one on air
/// shows beside the ones queued behind it.
const LOOK_BACK: time::Duration = time::Duration::hours(1);
/// Most ledger rows read for one answer; `truncated` says when it was reached.
const ROW_LIMIT: usize = 1000;

/// `{channel, renderer, chapters: [{name, start, job, hooks, state, render, plan, notes}]}`
/// in air order. `state` is `failed` (at any time), `airing`, `aired`, `queued`,
/// `running`, `rendered`, `unqueued` (the plugin could not reach the renderer)
/// or `unknown` (the renderer is not configured here, or has no record of the
/// job). `stand_in` is true while the chapter's slot is airing before its
/// render is done, so the black stand-in is what viewers see.
pub(super) fn api_channel_chapters(state: &AppState, key: &str) -> Result<(u16, Value), String> {
    let (station, index) = match find_channel(state, key)? {
        Lookup::Missing(why) => return Ok((404, json!({ "error": why }))),
        Lookup::Dropped {
            config_path,
            reason,
        } => {
            return Ok((
                200,
                json!({
                    "channel": key,
                    "config_path": config_path.display().to_string(),
                    "load_error": reason,
                }),
            ));
        }
        Lookup::Loaded(station, index) => (station, index),
    };
    let channel = &station.channels[index].name;
    let now = OffsetDateTime::now_utc();
    let rows = match super::real::open_history(&station.station.output_base)? {
        Some(history) => history
            .airings(&AiringFilter {
                channel: Some(channel.clone()),
                kind: Some(AiringKind::Bumper),
                from: Some(now - LOOK_BACK),
                limit: ROW_LIMIT,
                ..Default::default()
            })
            .map_err(|e| e.to_string())?,
        None => Vec::new(),
    };
    let truncated = rows.len() >= ROW_LIMIT;
    let queue = state.options.health.renderer.as_ref().map(|r| r.queue());

    let mut chapters = Vec::new();
    for row in rows {
        let Some(rest) = row.record.entry_id.strip_prefix(CHAPTER_SEGMENT_PREFIX) else {
            continue;
        };
        let mut parts = rest.split('|');
        let job_id = parts.next().unwrap_or_default();
        let hooks: Vec<&str> = parts.collect();
        let start = row.record.start;
        let job = queue.as_ref().and_then(|q| q.read(job_id));
        let plan_text = queue
            .as_ref()
            .and_then(|q| std::fs::read_to_string(q.plan_path(job_id)).ok());
        let plan: Option<Value> = plan_text
            .as_deref()
            .and_then(|text| serde_json::from_str(text).ok());
        let length = job.as_ref().map(|j| j.duration_s).or_else(|| {
            plan_text
                .as_deref()
                .and_then(|text| etv_render::Plan::parse(text).ok())
                .map(|p| p.duration_s())
        });
        let ends = length.map(|s| start + time::Duration::seconds_f64(s));
        let state = match (&job, ends) {
            (Some(job), _) if job.status == etv_render::queue::Status::Failed => {
                "failed".to_string()
            }
            (Some(_), Some(end)) if end <= now => "aired".to_string(),
            (Some(_), Some(_)) if start <= now => "airing".to_string(),
            (Some(job), _) if job.status == etv_render::queue::Status::Done => {
                "rendered".to_string()
            }
            (Some(job), _) => job.status.as_str().to_string(),
            (None, _) if job_id.starts_with("unqueued-") => "unqueued".to_string(),
            (None, _) => "unknown".to_string(),
        };
        let on_air = start <= now && ends.is_none_or(|end| now < end);
        let stand_in = on_air
            && job
                .as_ref()
                .is_none_or(|j| j.status != etv_render::queue::Status::Done);
        let notes = job
            .as_ref()
            .map(|j| crate::audit_report::render_notes(&j.output))
            .unwrap_or_default();
        chapters.push(json!({
            "name": row.record.entry_id,
            "start": start.format(&Rfc3339).map_err(|e| e.to_string())?,
            "job": job_id,
            "hooks": hooks,
            "state": state,
            "stand_in": stand_in,
            "render": job,
            "plan": plan,
            "notes": notes,
        }));
    }
    tracing::info!(
        event = "ui.chapters",
        channel = %channel,
        chapters = chapters.len(),
        renderer = queue.is_some(),
        "listed a channel's chapters"
    );
    Ok((
        200,
        json!({
            "channel": key,
            "renderer": queue.is_some(),
            "truncated": truncated,
            "chapters": chapters,
        }),
    ))
}

//! The Edit page's preview: what applying the text in the editor would do.
//!
//! `POST /api/channels/{ch}/preview {yaml, schedule?, project?}` answers, for
//! one candidate `channel.yaml` text, in two shapes the page asks for apart
//! because the second is slow:
//!
//! * `schedule` absent or false — `valid` / `error` / `line` (the same check
//!   the apply route makes, `config::load_candidate`), `fingerprint`
//!   (`changed`, `same` or `unknown`: the daemon's regeneration fingerprint,
//!   `daemon::channel_config_fingerprint` and ADR 0010, of the channel as the
//!   candidate loads against the channel as the file loads now, both under the
//!   inspection rules, so a comment-only edit is `same`; `daemon` says whether
//!   the fingerprint the daemon last recorded matches the file's) and `rewind`
//!   (what a `changed` fingerprint costs: the published items the daemon wipes
//!   from the finish of the item on screen, `daemon::cut_after_airing`, to the
//!   end of what is published, read from the same chunk files the wipe works
//!   on; `unknown` when the fingerprint is).
//! * `schedule: true` — `valid` and `schedule`: the deployed review and the
//!   candidate's re-simulated over `project` days (default 3d), with each
//!   airing that differs. `schedule.pending` is true until both simulations
//!   land; ask again to read them.
//!
//! Text equal to the deployed file answers `changed: false` with nothing else:
//! no movement and no rewind. Nothing is written.

use std::sync::Arc;

use super::apply::channel_path;
use super::channel::{Loaded, channel_review};
use super::*;

type Answer = Result<(u16, Value), String>;

/// How many days a candidate's schedule projects when the request names none.
const DEFAULT_PROJECT: &str = "3d";

fn fingerprint(station: &config::Station, path: &Path) -> Option<String> {
    let lc = station.channels.iter().find(|c| c.config_path == path)?;
    crate::daemon::channel_config_fingerprint(lc, station.station.tracks.as_ref())
}

/// The fingerprint the daemon recorded when it last loaded this channel, from
/// the `.resume` sidecar beside its playout.
fn recorded_fingerprint(output_folder: &Path) -> Option<String> {
    let bytes = std::fs::read(crate::resume::sidecar_path(output_folder)).ok()?;
    let sidecar: Value = serde_json::from_slice(&bytes).ok()?;
    sidecar["config_id"].as_str().map(str::to_string)
}

/// `POST /api/channels/{ch}/preview`
pub(super) fn api_preview(state: &mut AppState, key: &str, body: &str) -> Answer {
    #[derive(serde::Deserialize)]
    struct Request {
        yaml: String,
        #[serde(default)]
        schedule: bool,
        project: Option<String>,
    }
    let req: Request = serde_json::from_str(body).map_err(|e| format!("bad request body: {e}"))?;
    let path = match channel_path(state, key)? {
        Ok(p) => p,
        Err(answer) => return answer,
    };
    let current = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if current == req.yaml {
        return Ok((200, json!({ "valid": true, "changed": false })));
    }
    let loaded = match config::load_candidate(&state.options.station, &path, &req.yaml) {
        Ok(l) => l,
        Err(reason) => {
            let line = super::edit::line_of(&reason);
            return Ok((
                200,
                json!({ "valid": false, "changed": true, "error": reason, "line": line }),
            ));
        }
    };
    if req.schedule {
        return schedule_answer(state, key, req.yaml, req.project, loaded);
    }
    let before_fp = fingerprint(&loaded.before, &path);
    let after_fp = fingerprint(&loaded.after, &path);
    let (fp_state, unknown_why) = match (&before_fp, &after_fp) {
        (Some(a), Some(b)) if a == b => ("same", None),
        (Some(_), Some(_)) => ("changed", None),
        (_, None) if !loaded.after.channels.iter().any(|c| c.config_path == path) => (
            "unknown",
            Some(
                "the edited channel needs an environment variable this process cannot see, so its fingerprint cannot be computed here",
            ),
        ),
        _ => ("unknown", Some("the channel could not be fingerprinted")),
    };
    let daemon = match (
        loaded
            .before
            .channels
            .iter()
            .find(|c| c.config_path == path)
            .and_then(|c| recorded_fingerprint(&c.output_folder)),
        &before_fp,
    ) {
        (Some(recorded), Some(file)) if recorded == *file => "matches",
        (Some(_), Some(_)) => "differs",
        _ => "unknown",
    };

    let now = time::OffsetDateTime::now_utc();
    let rewind = match fp_state {
        "same" => {
            json!({ "state": "none", "why": "the regeneration fingerprint is unchanged, so no published slot moves" })
        }
        "changed" => match published_rewind(&loaded.before, &path, now) {
            Ok(Some(w)) => json!({
                "state": "rewinds",
                "airings": w.airings,
                "bumpers": w.bumpers,
                "from": w.from.unix_timestamp(),
                "to": w.to.unix_timestamp(),
                "as_of": now.unix_timestamp(),
                "basis": "from the finish of the item on screen to the end of the published schedule, as the daemon's refresh_forward wipes it",
            }),
            Ok(None) => {
                json!({ "state": "none", "why": "nothing is published after the item on screen" })
            }
            Err(e) => {
                tracing::warn!(event = "ui.preview.failed", channel = key, stage = "rewind", error = %e, "preview: published schedule unreadable");
                json!({ "state": "unknown", "why": e })
            }
        },
        _ => json!({ "state": "unknown", "why": unknown_why }),
    };
    tracing::info!(
        event = "ui.preview",
        channel = key,
        fingerprint = fp_state,
        daemon,
        rewind = %rewind["state"],
        rewind_airings = %rewind["airings"],
        "edit previewed"
    );
    Ok((
        200,
        json!({
            "valid": true,
            "changed": true,
            "fingerprint": { "state": fp_state, "why": unknown_why, "daemon": daemon },
            "rewind": rewind,
        }),
    ))
}

/// The slow half: the deployed review and the candidate's over the same window.
fn schedule_answer(
    state: &mut AppState,
    key: &str,
    yaml: String,
    project: Option<String>,
    loaded: config::CandidateLoad,
) -> Answer {
    let project = project.unwrap_or_else(|| DEFAULT_PROJECT.to_string());
    let mut params = HashMap::new();
    params.insert("project".to_string(), project.clone());
    let deployed = channel_review(
        state,
        key,
        &params,
        None,
        Some(Loaded {
            station: loaded.before,
            candidate: None,
        }),
    )?;
    let candidate = channel_review(
        state,
        key,
        &params,
        None,
        Some(Loaded {
            station: loaded.after,
            candidate: Some(Arc::new(yaml)),
        }),
    )?;
    for (status, review) in [&deployed, &candidate] {
        if *status != 200 && *status != 202 {
            tracing::warn!(event = "ui.preview.failed", channel = key, stage = "schedule", status, error = %review, "preview: review failed");
            return Ok((
                200,
                json!({ "valid": true, "schedule": { "error": review["error"], "status": status } }),
            ));
        }
    }
    let pending = deployed.0 == 202 || candidate.0 == 202;
    let days: u32 = project.trim_end_matches('d').parse().unwrap_or(0);
    tracing::info!(
        event = "ui.preview",
        channel = key,
        schedule = true,
        pending,
        days,
        "edit schedule previewed"
    );
    Ok((
        200,
        json!({
            "valid": true,
            "schedule": {
                "pending": pending,
                "days": days,
                "changed_airings": if pending { Value::Null } else { changed_airings(&deployed.1, &candidate.1) },
            },
        }),
    ))
}

/// What a reload would wipe from a channel: the published items from the cut
/// on, split into programmes and bumpers, and the span they cover.
struct Wipe {
    from: time::OffsetDateTime,
    to: time::OffsetDateTime,
    airings: usize,
    bumpers: usize,
}

/// A published item as the wipe sees it: `(start, finish, is a bumper)`.
type Item = (time::OffsetDateTime, time::OffsetDateTime, bool);

fn published_rewind(
    station: &config::Station,
    path: &Path,
    now: time::OffsetDateTime,
) -> Result<Option<Wipe>, String> {
    let lc = station
        .channels
        .iter()
        .find(|c| c.config_path == path)
        .ok_or_else(|| "the channel is not loaded".to_string())?;
    let items: Vec<Item> = super::real::published_items(lc, now)?
        .iter()
        .map(|i| {
            (
                i.start,
                i.finish,
                ersatztv_playout::playout::is_bumper_id(&i.id),
            )
        })
        .collect();
    Ok(wipe(&items, now))
}

/// The wipe over published `items` when it is `now`: every item that starts at
/// or after the cut. The cut is the daemon's own rule, `cut_after_airing`.
fn wipe(items: &[Item], now: time::OffsetDateTime) -> Option<Wipe> {
    let from = crate::daemon::cut_after_airing(items.iter().map(|(s, f, _)| (*s, *f)), now);
    let wiped: Vec<&Item> = items.iter().filter(|(s, _, _)| *s >= from).collect();
    Some(Wipe {
        from: wiped.iter().map(|(s, _, _)| *s).min()?,
        to: wiped.iter().map(|(_, f, _)| *f).max()?,
        airings: wiped.iter().filter(|(_, _, bumper)| !bumper).count(),
        bumpers: wiped.iter().filter(|(_, _, bumper)| *bumper).count(),
    })
}

/// Each candidate airing that is not the deployed schedule's airing at the
/// same place in the sequence, paired with what the deployed schedule has
/// there. Once one airing differs, the ones after it usually shift in time
/// too, so airings pair by position from the first difference rather than by
/// instant. A pool rank shows only where a plugin ranks the pool.
fn changed_airings(deployed: &Value, candidate: &Value) -> Value {
    let key = |a: &Value| (a["start"].as_i64(), a["id"].as_str().map(str::to_string));
    let dep = deployed["airings"].as_array().cloned().unwrap_or_default();
    let cand = candidate["airings"].as_array().cloned().unwrap_or_default();
    let first = (0..cand.len())
        .find(|&i| dep.get(i).is_none_or(|d| key(d) != key(&cand[i])))
        .unwrap_or(cand.len());
    let rows: Vec<Value> = (first..cand.len())
        .filter(|&i| dep.get(i).is_none_or(|d| key(d) != key(&cand[i])))
        .map(|i| {
            let a = &cand[i];
            json!({
                "start": a["start"],
                "title": a["label"],
                "pool": a["pool"],
                "rank": a["ranking"]["rank"],
                "deployed": dep.get(i).map(|w| json!({
                    "start": w["start"],
                    "title": w["label"],
                    "pool": w["pool"],
                    "rank": w["ranking"]["rank"],
                })),
            })
        })
        .collect();
    Value::Array(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn t(h: i64, m: i64) -> time::OffsetDateTime {
        datetime!(2026-11-05 00:00 UTC) + time::Duration::minutes(h * 60 + m)
    }

    #[test]
    fn the_wipe_starts_after_the_item_on_screen_and_counts_bumpers_apart() {
        let items: Vec<Item> = vec![
            (t(8, 0), t(9, 0), false),
            (t(9, 0), t(10, 25), false),
            (t(10, 25), t(10, 30), true),
            (t(10, 30), t(12, 0), false),
            (t(12, 0), t(12, 1), true),
        ];
        // 9:30 is inside the second programme: it plays out; everything that
        // starts at or after its finish (10:25) is wiped, bumpers included.
        let w = wipe(&items, t(9, 30)).unwrap();
        assert_eq!(
            (w.from, w.to, w.airings, w.bumpers),
            (t(10, 25), t(12, 1), 1, 2)
        );
        // A bumper on screen is the item the cut waits for, as in the daemon.
        let w = wipe(&items, t(10, 27)).unwrap();
        assert_eq!(
            (w.from, w.to, w.airings, w.bumpers),
            (t(10, 30), t(12, 1), 1, 1)
        );
        // Between items the wipe starts now and takes every later item.
        let gap: Vec<Item> = vec![(t(8, 0), t(9, 0), false), (t(10, 0), t(11, 0), false)];
        let w = wipe(&gap, t(9, 30)).unwrap();
        assert_eq!((w.from, w.airings), (t(10, 0), 1));
        // Nothing after the item on screen: nothing to rewind.
        assert!(wipe(&items, t(12, 0) + time::Duration::seconds(30)).is_none());
        assert!(wipe(&[], t(11, 0)).is_none());
    }

    #[test]
    fn airings_pair_by_position_from_the_first_difference() {
        let a =
            |start: i64, id: &str, label: &str| json!({ "start": start, "id": id, "label": label });
        let deployed = json!({ "airings": [a(0, "x", "X"), a(10, "y", "Y"), a(20, "z", "Z")] });
        let candidate = json!({ "airings": [a(0, "x", "X"), a(12, "q", "Q"), a(30, "z", "Z")] });
        let rows = changed_airings(&deployed, &candidate);
        let rows = rows.as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            (
                rows[0]["title"].clone(),
                rows[0]["deployed"]["title"].clone()
            ),
            (json!("Q"), json!("Y"))
        );
        // Same title, shifted in time: still a difference.
        assert_eq!(
            (
                rows[1]["start"].clone(),
                rows[1]["deployed"]["start"].clone()
            ),
            (json!(30), json!(20))
        );
        // Identical schedules differ nowhere.
        assert!(
            changed_airings(&deployed, &deployed)
                .as_array()
                .unwrap()
                .is_empty()
        );
        // A longer candidate has airings the deployed schedule lacks.
        let longer = json!({ "airings": [a(0, "x", "X"), a(10, "y", "Y"), a(20, "z", "Z"), a(30, "w", "W")] });
        let rows = changed_airings(&deployed, &longer);
        assert!(rows[0]["deployed"].is_null());
    }
}

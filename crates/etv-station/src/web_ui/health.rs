//! `GET /api/health/channels`: one row per lineup channel answering "is it
//! encoding on the GPU, has it been failing, when did it last stall".
//!
//! Three sources, each read only:
//!
//! - **Encoder verdict** — the newest block of `ffmpeg-argv-ch<N>.log` in the
//!   diag folder, which `tools/ffmpeg-probe.sh` appends to on every transcode
//!   start, one argument per line between `argv_begin` and `argv_end` under a
//!   `=== <UTC time> pid=… channel=…` header. Only the newest block is judged: an
//!   older one could still carry the hardware encoder from before a regression.
//!   The rules, in order: `libx264` present is FAIL (software), the expected
//!   encoder absent is FAIL, a VAAPI render node absent is FAIL, a block with no
//!   `argv_end` is FAIL (still starting or cut off), a newest block older than
//!   three days is STALE (the GPU worked then, not necessarily now), else PASS.
//!   A channel with no argv log has verdict `NONE`: nobody has watched it.
//! - **Failure state** — ETV-next's `GET /health/channels.json`. When ETV-next
//!   cannot be reached the rows carry no failure state and the body names the
//!   URL and the error under `upstream`.
//! - **Last stall** — the newest `stall` row of `stream-events.log` for the
//!   channel, with whether a later `recovered` row closed it.
//!
//! What the encoder is expected to be comes from `ETV_ACCEL` and
//! `ETV_VAAPI_DEVICE`, the same variables the station renders ETV-next's config
//! from, so the verdict can never disagree with what the station asked for.
//!
//! Each verdict logs one `ui.health_verdict` event naming the argv log line it
//! was decided on.

use super::*;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// A newest transcode older than this is STALE rather than PASS.
const STALE_AFTER_SECS: i64 = 3 * 86_400;
/// The stall log is rotated by its writer; this much of its tail is plenty.
const STALL_LOG_TAIL_BYTES: u64 = 1 << 20;

/// Where the health endpoint reads from, fixed when the server starts.
#[derive(Clone, Default)]
pub struct HealthSources {
    /// The diag folder (`ETV_DIAG_DIR`, else `/data/diag`) holding `ffmpeg-argv-ch<N>.log` and `stream-events.log`.
    pub diag_dir: Option<PathBuf>,
    /// ETV-next's origin, for `/health/channels.json`.
    pub etv_next_url: String,
    /// `ETV_ACCEL`: the hardware backend channels are expected to encode with.
    pub accel: Option<String>,
    /// `ETV_VAAPI_DEVICE`: the render node expected on a VAAPI command line.
    pub vaapi_device: Option<String>,
}

impl HealthSources {
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let port = var("ETV_PORT").unwrap_or_else(|| "8409".into());
        Self {
            // The container's diag folder, which `tools/ffmpeg-probe.sh` and the entrypoint default to.
            diag_dir: Some(PathBuf::from(
                var("ETV_DIAG_DIR").unwrap_or_else(|| "/data/diag".into()),
            )),
            etv_next_url: var("ETV_NEXT_URL")
                .unwrap_or_else(|| format!("http://127.0.0.1:{port}"))
                .trim_end_matches('/')
                .to_string(),
            accel: var("ETV_ACCEL"),
            vaapi_device: var("ETV_VAAPI_DEVICE"),
        }
    }

    /// The encoder name on an ffmpeg command line for the configured backend.
    fn encoder(&self) -> Option<&'static str> {
        match self.accel.as_deref()? {
            "vaapi" => Some("h264_vaapi"),
            "cuda" => Some("h264_nvenc"),
            "qsv" => Some("h264_qsv"),
            "amf" => Some("h264_amf"),
            _ => None,
        }
    }
}

/// One channel's encoder judgement and the argv line it rests on.
#[derive(Debug, PartialEq)]
struct Verdict {
    verdict: &'static str,
    reason: String,
    /// The block's `=== ` time, as written.
    at: Option<String>,
    /// 1-based line in the argv log the verdict was decided on.
    line: Option<usize>,
}

/// Judge the newest block of an argv log. `now` is Unix seconds.
fn judge(log: &str, sources: &HealthSources, now: i64) -> Verdict {
    let lines: Vec<&str> = log.lines().collect();
    let Some(header) = lines.iter().rposition(|l| l.starts_with("=== ")) else {
        return Verdict {
            verdict: "FAIL",
            reason: "the argv log holds no `=== ` block".into(),
            at: None,
            line: None,
        };
    };
    let block = &lines[header..];
    let at = block[0]
        .split_whitespace()
        .nth(1)
        .map(str::to_string)
        .unwrap_or_default();
    let at = Some(at).filter(|a| !a.is_empty());
    let find = |needle: &str| {
        block
            .iter()
            .position(|l| *l == needle)
            .map(|i| header + i + 1)
    };
    let fail = |reason: String, line: usize| Verdict {
        verdict: "FAIL",
        reason,
        at: at.clone(),
        line: Some(line),
    };

    if find("argv_end").is_none() {
        return fail(
            "the newest argv block has no argv_end — the channel may still be starting".into(),
            header + 1,
        );
    }
    if let Some(line) = find("libx264") {
        return fail(
            "libx264 in the argv — this channel encoded in software".into(),
            line,
        );
    }
    let Some(encoder) = sources.encoder() else {
        return fail(
            "no hardware encoder is configured (ETV_ACCEL is empty or unrecognised) — software by request".into(),
            header + 1,
        );
    };
    let Some(encoder_line) = find(encoder) else {
        return fail(
            format!("{encoder} is not in the newest transcode's argv"),
            header + 1,
        );
    };
    if sources.accel.as_deref() == Some("vaapi")
        && let Some(device) = sources.vaapi_device.as_deref()
        && find(device).is_none()
    {
        return fail(
            format!("{encoder} is present but the render node {device} is not in the argv"),
            encoder_line,
        );
    }
    let age = at
        .as_deref()
        .and_then(|a| OffsetDateTime::parse(a, &Rfc3339).ok())
        .map(|t| now - t.unix_timestamp());
    match age {
        Some(age) if age > STALE_AFTER_SECS => Verdict {
            verdict: "STALE",
            reason: format!(
                "{encoder}, no libx264 — but the newest transcode is {} days old",
                age / 86_400
            ),
            at,
            line: Some(header + 1),
        },
        None => fail(
            "the newest argv block's time is missing or unreadable, so its age is unknown".into(),
            header + 1,
        ),
        _ => Verdict {
            verdict: "PASS",
            reason: format!("{encoder}, no libx264"),
            at,
            line: Some(encoder_line),
        },
    }
}

/// The newest `stall` of `channel` in a stall log, and whether a later
/// `recovered` closed it. Rows are `time<TAB>channel<TAB>event<TAB>detail`.
fn last_stall(log: &str, channel: i64) -> Option<Value> {
    let mut stall: Option<Value> = None;
    for row in log.lines() {
        let mut cols = row.splitn(4, '\t');
        let (Some(at), Some(ch), Some(event)) = (cols.next(), cols.next(), cols.next()) else {
            continue;
        };
        if ch.parse::<i64>().ok() != Some(channel) {
            continue;
        }
        match event {
            "stall" => {
                stall = Some(json!({
                    "at": at,
                    "detail": cols.next().unwrap_or(""),
                    "recovered": false,
                }));
            }
            "recovered" => {
                if let Some(s) = stall.as_mut() {
                    s["recovered"] = json!(true);
                }
            }
            _ => {}
        }
    }
    stall
}

fn read_tail(path: &Path, max: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len > max {
        file.seek(SeekFrom::Start(len - max)).ok()?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// ETV-next's per-channel failure rows by dial number, or the error that kept
/// us from reading them.
fn read_failures(base: &str) -> Result<HashMap<i64, Value>, String> {
    let url = format!("{base}/health/channels.json");
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(5))
        .build();
    let body: Value = match agent.get(&url).call() {
        Ok(r) => r.into_json().map_err(|e| format!("{url}: {e}"))?,
        Err(ureq::Error::Status(code, _)) => return Err(format!("{url}: HTTP {code}")),
        // ureq's transport errors already name the URL.
        Err(e) => return Err(e.to_string()),
    };
    let rows = body["channels"]
        .as_array()
        .ok_or_else(|| format!("{url}: no `channels` array in the answer"))?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let number = r["number"]
                .as_str()
                .and_then(|n| n.parse().ok())
                .or_else(|| r["number"].as_i64())?;
            Some((number, r.clone()))
        })
        .collect())
}

pub(super) fn api_health_channels(state: &AppState) -> Result<(u16, Value), String> {
    let sources = &state.options.health;
    let path = &state.options.station;
    let station =
        config::load_for_inspection(path).map_err(|e| format!("load {}: {e}", path.display()))?;
    let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let keys = lineup::lineup_keys(&station.lineup, &base);
    let now = OffsetDateTime::now_utc().unix_timestamp();

    let (failures, upstream) = match read_failures(&sources.etv_next_url) {
        Ok(f) => (f, None),
        Err(e) => {
            tracing::warn!(event = "ui.health_upstream", url = %sources.etv_next_url, error = %e, "ETV-next health unreadable");
            (
                HashMap::new(),
                Some(json!({ "url": sources.etv_next_url, "error": e })),
            )
        }
    };
    let diag_readable = sources
        .diag_dir
        .as_ref()
        .is_some_and(|d| std::fs::read_dir(d).is_ok());
    let stalls = sources
        .diag_dir
        .as_ref()
        .and_then(|d| read_tail(&d.join("stream-events.log"), STALL_LOG_TAIL_BYTES))
        .unwrap_or_default();

    let mut rows = Vec::new();
    for (config_path, key) in station.lineup.iter().zip(&keys) {
        let Some(lc) = station
            .channels
            .iter()
            .find(|c| c.config_path == *config_path)
        else {
            continue;
        };
        let number = lc.config.number;
        let argv_path = sources
            .diag_dir
            .as_ref()
            .map(|d| d.join(format!("ffmpeg-argv-ch{number}.log")));
        let argv = argv_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok());
        let mut argv_age = None;
        let encoder = match &argv {
            Some(text) => {
                let v = judge(text, sources, now);
                // Age of the newest transcode, which is what the log's age means to a reader.
                argv_age =
                    v.at.as_deref()
                        .and_then(|a| OffsetDateTime::parse(a, &Rfc3339).ok())
                        .map(|t| now - t.unix_timestamp());
                tracing::info!(
                    event = "ui.health_verdict",
                    channel = number,
                    verdict = v.verdict,
                    reason = %v.reason,
                    argv_log = ?argv_path,
                    line = v.line,
                    "encoder verdict"
                );
                json!({
                    "verdict": v.verdict,
                    "reason": v.reason,
                    "encoder": sources.encoder(),
                    "at": v.at,
                    "line": v.line,
                })
            }
            None => {
                tracing::info!(
                    event = "ui.health_verdict",
                    channel = number,
                    verdict = "NONE",
                    argv_log = ?argv_path,
                    "no argv log"
                );
                json!({
                    "verdict": "NONE",
                    "reason": "no ffmpeg argv log: no transcode has been recorded",
                    "encoder": sources.encoder(),
                    "at": null,
                    "line": null,
                })
            }
        };
        let fail = failures.get(&number);
        rows.push(json!({
            "channel": number,
            "key": key,
            "name": fail.and_then(|f| f["name"].as_str()).map(str::to_string)
                .or_else(|| lc.config.display_name.clone()),
            "encoder": encoder,
            "failures": fail.map(|f| f["consecutive_failures"].clone()),
            "failed": fail.map(|f| f["failed"].clone()),
            "retry_in_secs": fail.map(|f| f["retry_in_secs"].clone()),
            "last_failure": fail.map(|f| f["last_failure"].clone()),
            "last_stall": last_stall(&stalls, number),
            "argv_log_age_secs": argv_age,
        }));
    }
    Ok((
        200,
        json!({
            "tz": station.station.tz,
            "expected": { "accel": sources.accel, "encoder": sources.encoder(), "vaapi_device": sources.vaapi_device },
            "upstream": upstream,
            "diag": { "dir": sources.diag_dir, "readable": diag_readable },
            "channels": rows,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vaapi() -> HealthSources {
        HealthSources {
            accel: Some("vaapi".into()),
            vaapi_device: Some("/dev/dri/renderD128".into()),
            ..HealthSources::default()
        }
    }

    fn block(at: &str, args: &[&str]) -> String {
        let mut s = format!("=== {at} pid=1 channel=7\nargv_begin\n");
        for a in args {
            s += a;
            s += "\n";
        }
        s + "argv_end\n"
    }

    const GOOD: &[&str] = &["-c:v", "h264_vaapi", "-vaapi_device", "/dev/dri/renderD128"];

    fn iso(secs: i64) -> String {
        OffsetDateTime::from_unix_timestamp(secs)
            .unwrap()
            .format(&Rfc3339)
            .unwrap()
    }

    #[test]
    fn a_software_encoder_fails_naming_its_line() {
        let log = block(&iso(1_000), &["-c:v", "libx264"]);
        let v = judge(&log, &vaapi(), 1_000);
        assert_eq!(v.verdict, "FAIL");
        assert_eq!(v.line, Some(4));
    }

    #[test]
    fn a_missing_encoder_or_render_node_fails() {
        let no_encoder = block(&iso(1_000), &["-c:v", "copy"]);
        assert_eq!(judge(&no_encoder, &vaapi(), 1_000).verdict, "FAIL");
        let no_node = block(&iso(1_000), &["-c:v", "h264_vaapi"]);
        let v = judge(&no_node, &vaapi(), 1_000);
        assert_eq!((v.verdict, v.line), ("FAIL", Some(4)));
    }

    #[test]
    fn only_the_newest_block_counts_and_age_makes_it_stale() {
        let now = 10_000_000;
        let log = block(&iso(now - 100), &["-c:v", "libx264"])
            + block(&iso(now - 4 * 86_400), GOOD).as_str();
        let v = judge(&log, &vaapi(), now);
        assert_eq!(v.verdict, "STALE", "{}", v.reason);
        let fresh = block(&iso(now - 60), GOOD);
        assert_eq!(judge(&fresh, &vaapi(), now).verdict, "PASS");
        let garbled = block("not-a-time", GOOD);
        assert_eq!(judge(&garbled, &vaapi(), now).verdict, "FAIL");
    }

    #[test]
    fn the_last_stall_is_the_newest_and_notes_recovery() {
        let log = "t1\t3\tstall\tmedia_seq=1\nt2\t3\trecovered\tx\nt3\t4\tstall\ty\nt4\t3\tstall\tmedia_seq=9\n";
        let s = last_stall(log, 3).unwrap();
        assert_eq!(
            (s["at"].as_str(), s["recovered"].as_bool()),
            (Some("t4"), Some(false))
        );
        let first = last_stall("t1\t3\tstall\ta\nt2\t3\trecovered\tb\n", 3).unwrap();
        assert_eq!(first["recovered"], true);
        assert!(last_stall(log, 9).is_none());
    }
}

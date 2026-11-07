//! Channels › Edit's write path: replace a channel's `channel.yaml` with edited
//! text, and report what the daemon made of it.
//!
//! `POST /api/channels/{ch}/apply {yaml}` checks the text with the loader the
//! daemon uses (a channel that would be dropped from the lineup is refused and
//! nothing is written), copies the current file to a timestamped backup beside
//! it, writes the new text atomically, then sends the daemon SIGHUP through its
//! pid file. It answers at once; `GET /api/channels/{ch}/apply/status?after=N`
//! reads the daemon's own record of the reload (`reload_status`), where `N` is
//! the `seq_before` the apply answered with.

use super::pools::{Lookup, locate};
use super::*;
use crate::reload_status::{self, Outcome};
use crate::reset::{self, Reload};

type Answer = Result<(u16, Value), String>;

fn stamp() -> String {
    let t = time::OffsetDateTime::now_utc();
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

/// `<file>.bak-<UTC stamp>`, with `-2`, `-3`... when that name is taken, so a
/// second apply inside the same second never replaces the first one's backup.
fn unused_backup_name(path: &Path, file_name: &str) -> PathBuf {
    let stamp = stamp();
    let mut candidate = path.with_file_name(format!("{file_name}.bak-{stamp}"));
    let mut n = 2;
    while candidate.exists() {
        candidate = path.with_file_name(format!("{file_name}.bak-{stamp}-{n}"));
        n += 1;
    }
    candidate
}

/// Whether the daemon's recorded path and this process's path name one file:
/// equal as written, or the same file once resolved.
fn same_file(recorded: &str, mine: &Path) -> bool {
    if Path::new(recorded) == mine {
        return true;
    }
    matches!(
        (std::fs::canonicalize(recorded), std::fs::canonicalize(mine)),
        (Ok(a), Ok(b)) if a == b
    )
}

pub(super) fn channel_path(state: &AppState, key: &str) -> Result<Result<PathBuf, Answer>, String> {
    Ok(match locate(state, key)? {
        Lookup::Ready(l) => Ok(l.config_path),
        // A broken channel is exactly the one an edit has to be able to fix.
        Lookup::Broken { config_path, .. } => Ok(config_path),
        Lookup::Missing(m) => Err(Ok((404, json!({ "error": m })))),
    })
}

fn output_base(state: &AppState) -> Result<PathBuf, String> {
    let path = &state.options.station;
    config::load_for_inspection(path)
        .map(|s| s.station.output_base)
        .map_err(|e| format!("load {}: {e}", path.display()))
}

/// `POST /api/channels/{ch}/apply`
pub(super) fn api_apply(state: &mut AppState, key: &str, body: &str) -> Answer {
    #[derive(serde::Deserialize)]
    struct Request {
        yaml: String,
    }
    let req: Request = serde_json::from_str(body).map_err(|e| format!("bad request body: {e}"))?;
    let path = match channel_path(state, key)? {
        Ok(p) => p,
        Err(answer) => return answer,
    };
    let current = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if current == req.yaml {
        return Ok((200, json!({ "status": "unchanged", "channel": key })));
    }
    if let Err(reason) =
        config::validate_channel_candidate(&state.options.station, &path, &req.yaml)
    {
        tracing::info!(event = "ui.apply.refused", channel = key, reason = %reason, "apply refused");
        return Ok((422, json!({ "error": reason, "refused": true })));
    }
    let out = output_base(state)?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("channel.yaml");
    // Before the write: a reload that lands after it loads this edit, so it is
    // this edit's result, and one that landed before it is not.
    let seq_before = reload_status::read(&out).map_or(0, |s| s.seq);
    let backup = unused_backup_name(&path, file_name);
    std::fs::copy(&path, &backup).map_err(|e| format!("back up to {}: {e}", backup.display()))?;
    crate::atomic::atomic_write_bytes_sync(&path, req.yaml.as_bytes())
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    // The file is already replaced, so a signalling failure answers 200 with its
    // own status: an error body would tell the caller nothing was written.
    let (status, signal_error) = match reset::signal_reload(&out) {
        Ok(Reload::Signalled(_)) => ("signalled", None),
        Ok(Reload::NotRunning) => ("written_not_signalled", None),
        Err(e) => ("written_signal_failed", Some(e)),
    };
    tracing::info!(
        event = "ui.apply",
        channel = key,
        path = %path.display(),
        backup = %backup.display(),
        bytes = req.yaml.len(),
        status,
        seq_before,
        "channel config written"
    );
    Ok((
        200,
        json!({
            "status": status,
            "channel": key,
            "backup": backup.display().to_string(),
            "seq_before": seq_before,
            "signal_error": signal_error,
        }),
    ))
}

/// `GET /api/channels/{ch}/apply/status?after=N`
pub(super) fn api_apply_status(state: &mut AppState, key: &str, query: &str) -> Answer {
    let after: u64 = parse_query(query)
        .get("after")
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| "missing `after` (the seq_before an apply answered with)".to_string())?;
    let path = match channel_path(state, key)? {
        Ok(p) => p,
        Err(answer) => return answer,
    };
    let out = output_base(state)?;
    let Some(reload) = reload_status::read(&out).filter(|r| r.seq > after) else {
        return Ok((200, json!({ "status": "pending", "channel": key })));
    };
    let dropped = reload.dropped.iter().find(|d| same_file(&d.path, &path));
    let (status, reason) = match (reload.outcome, dropped) {
        (Outcome::Applied, None) => ("applied", None),
        (Outcome::Applied, Some(d)) => ("channel_dropped", Some(d.reason.clone())),
        (Outcome::Failed, _) => ("failed", reload.error.clone()),
        (Outcome::Reverted, _) => ("reverted", reload.error.clone()),
    };
    Ok((
        200,
        json!({
            "status": status,
            "channel": key,
            "reason": reason,
            "seq": reload.seq,
            "at_unix": reload.at_unix,
            "other_dropped": reload.dropped.iter().filter(|d| !same_file(&d.path, &path)).count(),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHANNEL: &str = "number: 1\nname: a\nwindow_days: 1\nroll_interval: \"1h\"\n\
        retention_days: 1\nchunk_hours: 6\nseed: 7\nrule:\n  blocks:\n    - mode: all\n      \
        pools:\n        - name: movies\n          expr: 'item.type == \"movie\"'\n          \
        order: random\n          advance: resume\n      pattern:\n        - pool: movies\n          take: 4\n";

    struct Fixture {
        dir: tempfile::TempDir,
        state: AppState,
    }

    impl Fixture {
        fn channel_file(&self) -> PathBuf {
            self.dir.path().join("channels/a/channel.yaml")
        }
        fn out(&self) -> PathBuf {
            self.dir.path().join("out")
        }
        fn post(&mut self, key: &str, yaml: &str) -> (u16, Value) {
            let body = json!({ "yaml": yaml }).to_string();
            match dispatch(
                &mut self.state,
                &Method::Post,
                &format!("/api/channels/{key}/apply"),
                "",
                &body,
            ) {
                Reply::Json(status, body) => (status, body),
                _ => panic!("apply did not answer JSON"),
            }
        }
        fn status(&mut self, after: u64) -> Value {
            match dispatch(
                &mut self.state,
                &Method::Get,
                "/api/channels/a/apply/status",
                &format!("after={after}"),
                "",
            ) {
                Reply::Json(200, body) => body,
                Reply::Json(status, body) => panic!("status {status}: {body}"),
                _ => panic!("apply status did not answer JSON"),
            }
        }
        fn backups(&self) -> Vec<PathBuf> {
            std::fs::read_dir(self.dir.path().join("channels/a"))
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| p.to_string_lossy().contains(".bak-"))
                .collect()
        }
    }

    fn fixture(channel_text: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("channels/a")).unwrap();
        std::fs::write(root.join("channels/a/channel.yaml"), channel_text).unwrap();
        std::fs::write(
            root.join("station.yaml"),
            format!(
                "tz: UTC\noutput_base: {out}\nchannels:\n  - channels/a/channel.yaml\n\
                 normalization:\n  audio:\n    format: aac\n    bitrate_kbps: 192\n    buffer_kbps: 384\n    \
                 channels: 2\n    sample_rate_hz: 48000\n    normalize_loudness: false\n  video:\n    \
                 format: h264\n    bit_depth: 8\n    width: 1280\n    height: 720\n    bitrate_kbps: 2000\n    \
                 buffer_kbps: 4000\n  subtitle:\n    mode: convert\n",
                out = root.join("out").display(),
            ),
        )
        .unwrap();
        let station = root.join("station.yaml");
        let state = AppState {
            catalog: Catalog::open_in_memory().expect("in-memory catalog"),
            pools: PoolState::default(),
            review: channel::ReviewState::new(station.clone(), PathBuf::from("/nonexistent/c.db")),
            mcp: None,
            arr: super::arr::ArrEnv::default(),
            plexdb: None,
            options: Options {
                station,
                catalog: PathBuf::from("/nonexistent/c.db"),
                plugins_dir: None,
                bind: IpAddr::from([127, 0, 0, 1]),
                port: 0,
                health: HealthSources::default(),
            },
        };
        Fixture { dir, state }
    }

    fn edited() -> String {
        CHANNEL.replace("take: 4", "take: 5")
    }

    #[test]
    fn a_valid_edit_is_written_and_the_old_file_is_kept_as_a_backup() {
        let mut f = fixture(CHANNEL);
        let (status, body) = f.post("a", &edited());
        assert_eq!(status, 200, "{body}");
        // No daemon is running under this temp folder, so nothing was signalled.
        assert_eq!(body["status"], json!("written_not_signalled"), "{body}");
        assert_eq!(body["seq_before"], json!(0));
        assert_eq!(std::fs::read_to_string(f.channel_file()).unwrap(), edited());
        let backups = f.backups();
        assert_eq!(backups.len(), 1, "{backups:?}");
        assert_eq!(std::fs::read_to_string(&backups[0]).unwrap(), CHANNEL);
    }

    #[test]
    fn text_the_loader_rejects_is_refused_and_nothing_is_written() {
        let mut f = fixture(CHANNEL);
        let bad = CHANNEL.replace("- pool: movies", "- pool: no-such-pool");
        let (status, body) = f.post("a", &bad);
        assert_eq!(status, 422, "{body}");
        assert_eq!(body["refused"], json!(true));
        assert!(
            body["error"].as_str().is_some_and(|e| !e.is_empty()),
            "{body}"
        );
        assert_eq!(std::fs::read_to_string(f.channel_file()).unwrap(), CHANNEL);
        assert!(f.backups().is_empty());
    }

    #[test]
    fn unchanged_text_writes_and_signals_nothing() {
        let mut f = fixture(CHANNEL);
        let (status, body) = f.post("a", CHANNEL);
        assert_eq!((status, &body["status"]), (200, &json!("unchanged")));
        assert!(f.backups().is_empty());
    }

    #[test]
    fn a_channel_that_fails_to_load_can_be_repaired_by_an_edit() {
        let mut f = fixture("number: 1\nname: a\nrule: [this is not a rule]\n");
        let (status, body) = f.post("a", CHANNEL);
        assert_eq!(status, 200, "{body}");
        assert_eq!(std::fs::read_to_string(f.channel_file()).unwrap(), CHANNEL);
    }

    #[test]
    fn a_broken_channel_edited_into_another_broken_text_is_refused() {
        let broken = "number: 1\nname: a\nrule: [this is not a rule]\n";
        let mut f = fixture(broken);
        let (status, body) = f.post("a", "number: 1\nname: a\nrule: [still not a rule]\n");
        assert_eq!(status, 422, "{body}");
        assert_eq!(std::fs::read_to_string(f.channel_file()).unwrap(), broken);
    }

    #[test]
    fn two_applies_in_one_second_keep_both_backups() {
        let mut f = fixture(CHANNEL);
        assert_eq!(f.post("a", &edited()).0, 200);
        assert_eq!(f.post("a", &CHANNEL.replace("take: 4", "take: 6")).0, 200);
        let backups = f.backups();
        assert_eq!(backups.len(), 2, "{backups:?}");
        let texts: Vec<String> = backups
            .iter()
            .map(|b| std::fs::read_to_string(b).unwrap())
            .collect();
        assert!(
            texts.contains(&CHANNEL.to_string()),
            "the original survived"
        );
    }

    #[test]
    fn an_unknown_channel_is_a_404() {
        let mut f = fixture(CHANNEL);
        assert_eq!(f.post("nope", &edited()).0, 404);
    }

    #[test]
    fn status_is_pending_until_the_daemon_records_a_newer_reload_then_names_this_channels_fate() {
        use crate::reload_status::{DroppedChannel, record};
        let mut f = fixture(CHANNEL);
        assert_eq!(f.status(0)["status"], json!("pending"));

        let first = record(&f.out(), Outcome::Applied, vec![], None)
            .unwrap()
            .seq;
        assert_eq!(
            f.status(first)["status"],
            json!("pending"),
            "a reload is not after itself"
        );
        assert_eq!(f.status(0)["status"], json!("applied"));

        let mine = f.channel_file().display().to_string();
        let dropped = vec![DroppedChannel {
            path: mine,
            reason: "unknown pool".into(),
        }];
        let second = record(&f.out(), Outcome::Applied, dropped, None)
            .unwrap()
            .seq;
        let s = f.status(first);
        assert_eq!(s["status"], json!("channel_dropped"), "{s}");
        assert_eq!(s["reason"], json!("unknown pool"));

        let third = record(&f.out(), Outcome::Reverted, vec![], Some("bad tz".into()))
            .unwrap()
            .seq;
        let s = f.status(second);
        assert_eq!(
            (&s["status"], &s["reason"]),
            (&json!("reverted"), &json!("bad tz"))
        );

        record(&f.out(), Outcome::Failed, vec![], Some("no parse".into())).unwrap();
        assert_eq!(f.status(third)["status"], json!("failed"));
    }

    fn call(f: &mut Fixture, method: Method, path: &str, body: Value) -> (u16, Value) {
        match dispatch(&mut f.state, &method, path, "", &body.to_string()) {
            Reply::Json(status, body) => (status, body),
            _ => panic!("{path} did not answer JSON"),
        }
    }

    #[test]
    fn the_edit_page_reads_the_deployed_text_and_checks_a_candidate_without_writing() {
        let mut f = fixture(CHANNEL);
        let (status, body) = call(&mut f, Method::Get, "/api/channels/a/file", json!({}));
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["yaml"], json!(CHANNEL));
        let (_, ok) = call(
            &mut f,
            Method::Post,
            "/api/channels/a/validate",
            json!({ "yaml": edited() }),
        );
        assert_eq!(ok["valid"], json!(true), "{ok}");
        let (status, bad) = call(
            &mut f,
            Method::Post,
            "/api/channels/a/validate",
            json!({ "yaml": CHANNEL.replace("- pool: movies", "- pool: no-such-pool") }),
        );
        assert_eq!(status, 200, "{bad}");
        assert_eq!(bad["valid"], json!(false), "{bad}");
        assert!(
            bad["error"].as_str().is_some_and(|e| !e.is_empty()),
            "{bad}"
        );
        let (_, syntax) = call(
            &mut f,
            Method::Post,
            "/api/channels/a/validate",
            json!({ "yaml": "number: 1\nname: [unclosed\n" }),
        );
        assert_eq!(syntax["valid"], json!(false), "{syntax}");
        assert!(syntax["line"].as_u64().is_some(), "{syntax}");
        assert_eq!(std::fs::read_to_string(f.channel_file()).unwrap(), CHANNEL);
        assert!(f.backups().is_empty());
    }

    #[test]
    fn a_profile_edit_rewrites_one_pools_block_and_keeps_the_rest_of_the_text() {
        let mut f = fixture(CHANNEL);
        let text = format!(
            "# keep me\n{}",
            CHANNEL.replace(
                "          order: random\n",
                "          plugin: p.rhai\n          order: random\n",
            )
        );
        let read = |f: &mut Fixture, yaml: &str| {
            call(
                f,
                Method::Post,
                "/api/channels/a/profile",
                json!({ "yaml": yaml, "op": "read" }),
            )
            .1
        };
        assert_eq!(read(&mut f, &text)["pools"][0]["entries"], json!([]));
        let (status, added) = call(
            &mut f,
            Method::Post,
            "/api/channels/a/profile",
            json!({ "yaml": text, "op": "add", "pool": "movies", "entry": { "keyword": "heist", "weight": 2.0 } }),
        );
        assert_eq!(status, 200, "{added}");
        let yaml = added["yaml"].as_str().unwrap();
        assert!(yaml.starts_with("# keep me\n"), "{yaml}");
        assert!(yaml.contains("          profile:\n"), "{yaml}");
        assert_eq!(added["pools"][0]["entries"][0]["keyword"], json!("heist"));
        let (_, removed) = call(
            &mut f,
            Method::Post,
            "/api/channels/a/profile",
            json!({ "yaml": yaml, "op": "remove", "pool": "movies", "entry": { "index": 0 } }),
        );
        assert_eq!(removed["yaml"], json!(text), "{removed}");
        // A pool no plugin scores has no profile to edit.
        let (status, refused) = call(
            &mut f,
            Method::Post,
            "/api/channels/a/profile",
            json!({ "yaml": CHANNEL, "op": "add", "pool": "movies", "entry": { "keyword": "x", "weight": 1.0 } }),
        );
        assert_eq!(status, 400, "{refused}");
    }

    #[test]
    fn the_preview_tells_a_comment_from_a_change_and_an_unchanged_text_from_both() {
        let mut f = fixture(CHANNEL);
        let preview = |f: &mut Fixture, yaml: &str| {
            call(
                f,
                Method::Post,
                "/api/channels/a/preview",
                json!({ "yaml": yaml }),
            )
        };
        let (status, same_text) = preview(&mut f, CHANNEL);
        assert_eq!(status, 200, "{same_text}");
        assert_eq!(same_text, json!({ "valid": true, "changed": false }));

        let (_, comment) = preview(&mut f, &format!("# a note\n{CHANNEL}"));
        assert_eq!(comment["fingerprint"]["state"], json!("same"), "{comment}");
        assert_eq!(comment["rewind"]["state"], json!("none"), "{comment}");

        let (_, edit) = preview(&mut f, &edited());
        assert_eq!(edit["fingerprint"]["state"], json!("changed"), "{edit}");
        // Nothing is published under the fixture's output folder.
        assert_eq!(edit["rewind"]["state"], json!("none"), "{edit}");

        let bad = CHANNEL.replace("- pool: movies", "- pool: no-such-pool");
        let (_, invalid) = preview(&mut f, &bad);
        assert_eq!(invalid["valid"], json!(false), "{invalid}");
        assert!(invalid["error"].as_str().is_some_and(|e| !e.is_empty()));
        assert_eq!(std::fs::read_to_string(f.channel_file()).unwrap(), CHANNEL);
        assert!(f.backups().is_empty());
    }

    use super::super::mcp::testing::Dashboard;

    fn serve_fixture(f: Fixture) -> (Dashboard, tempfile::TempDir) {
        let Fixture { dir, state } = f;
        (Dashboard::start(move || state), dir)
    }

    fn tool(d: &Dashboard, name: &str, args: Value) -> super::super::mcp::Outcome {
        let Value::Object(args) = args else { panic!() };
        d.mcp.call(name, &args).unwrap()
    }

    #[test]
    fn each_read_tool_returns_the_body_of_its_route() {
        let (d, _dir) = serve_fixture(fixture(CHANNEL));
        for (name, args, target) in [
            ("lineup", json!({}), "/api/lineup"),
            ("health", json!({}), "/api/health"),
            (
                "channel_file",
                json!({"channel": "a"}),
                "/api/channels/a/file",
            ),
            (
                "channel_pools",
                json!({"channel": "a"}),
                "/api/channels/a/pools",
            ),
            (
                "channel_review",
                json!({"channel": "a"}),
                "/api/channels/a/review",
            ),
            (
                "apply_status",
                json!({"channel": "a", "after": 0}),
                "/api/channels/a/apply/status?after=0",
            ),
        ] {
            let out = tool(&d, name, args);
            let (status, body) = d.get(target);
            assert_eq!(status, 200, "{name}: {body}");
            assert!(!out.is_error, "{name}: {}", out.text);
            assert_eq!(out.text, body, "{name}");
            assert!(serde_json::from_str::<Value>(&out.text).is_ok(), "{name}");
        }
    }

    #[test]
    fn a_route_error_is_a_tool_error_leading_with_its_status() {
        let (d, _dir) = serve_fixture(fixture(CHANNEL));
        let out = tool(&d, "airing_audit", json!({"airing": "not-an-id"}));
        let (status, body) = d.get("/api/airings/not-an-id/audit");
        assert_eq!(status, 400);
        assert!(out.is_error);
        assert_eq!(out.text, format!("HTTP 400: {body}"));
        let out = tool(&d, "channel_file", json!({"channel": "nope"}));
        assert!(
            out.is_error && out.text.starts_with("HTTP 404: "),
            "{}",
            out.text
        );
    }

    #[test]
    fn the_apply_tool_calls_the_apply_route_and_returns_its_status() {
        let (d, dir) = serve_fixture(fixture(CHANNEL));
        let file = dir.path().join("channels/a/channel.yaml");
        let out = tool(
            &d,
            "apply_channel",
            json!({"channel": "a", "yaml": edited()}),
        );
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(out.status, 200);
        let body: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(body["status"], json!("written_not_signalled"), "{body}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), edited());

        let refused = tool(
            &d,
            "apply_channel",
            json!({"channel": "a", "yaml": CHANNEL.replace("- pool: movies", "- pool: nope")}),
        );
        assert!(
            refused.is_error && refused.status == 422,
            "{}",
            refused.text
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), edited());

        let unchanged = tool(
            &d,
            "apply_channel",
            json!({"channel": "a", "yaml": edited()}),
        );
        assert!(unchanged.text.contains("unchanged"), "{}", unchanged.text);
    }

    /// The JSON-RPC a client sends, through the real accept loop and the
    /// thread `/mcp` is moved to.
    fn rpc(d: &Dashboard, body: Value) -> (u16, Value) {
        let r = ureq::post(&format!("{}/mcp", d.base))
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .send_string(&body.to_string());
        let r = match r {
            Ok(r) | Err(ureq::Error::Status(_, r)) => r,
            Err(e) => panic!("{e}"),
        };
        let status = r.status();
        let text = r.into_string().unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    #[test]
    fn mcp_initializes_lists_the_tools_and_calls_one_through_the_accept_loop() {
        let (d, _dir) = serve_fixture(fixture(CHANNEL));
        let (status, init) = rpc(
            &d,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "t", "version": "0"}}}),
        );
        assert_eq!(status, 200, "{init}");
        assert_eq!(init["result"]["serverInfo"]["name"], json!("etv-station"));
        assert!(
            init["result"]["capabilities"]["tools"].is_object(),
            "{init}"
        );

        let (status, list) = rpc(
            &d,
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        );
        assert_eq!(status, 200, "{list}");
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        let expected: Vec<&str> = super::super::mcp::TOOLS.iter().map(|t| t.name).collect();
        assert_eq!(names, expected);

        let (status, called) = rpc(
            &d,
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                   "params": {"name": "channel_file", "arguments": {"channel": "a"}}}),
        );
        assert_eq!(status, 200, "{called}");
        let (_, body) = d.get("/api/channels/a/file");
        assert_eq!(called["result"]["content"][0]["text"], json!(body));
        assert_ne!(called["result"]["isError"], json!(true));
        // The page is still served beside it.
        assert_eq!(d.get("/api/lineup").0, 200);
    }
}

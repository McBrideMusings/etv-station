//! Event hooks (ADR 0021): code the station runs at an event, which cannot
//! change the schedule. A hook's `run` is a Rhai script (`.rhai`, run on a
//! blocking thread with only the functions its `grants` name) or any other
//! executable (run as a child process, the event as JSON on its stdin).
//!
//! The station keeps no record of executions beyond the log line each run
//! writes (`event = "hook.run"`), so a script that starts outside work owns
//! its own deduplication.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rhai::{Array, Dynamic, Engine, Map, Scope};
use tokio::sync::Notify;

use crate::config::{HookDecl, HookGrant, Station};

/// Longest a `http()` grant waits for a response.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest one run may take — an executable, or a script together with every
/// program it `exec`s — before it is killed. A run that outlives its interval
/// already skips the next one; this bounds how long a hung run can do that.
const RUN_TIMEOUT: Duration = Duration::from_secs(600);

/// Most bytes of a child's stdout or stderr kept in memory.
const OUTPUT_CAP: usize = 1 << 20;

/// Bytes of stderr carried into a failed run's log line.
const STDERR_LOGGED: usize = 1000;

/// Rhai operation budget for one script run: a `loop {}` ends with an error
/// instead of holding a worker thread forever.
const MAX_OPERATIONS: u64 = 100_000_000;

/// Where a hook was declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookScope {
    Station,
    Channel(String),
}

/// One configured hook with the context it needs to run.
#[derive(Debug, Clone)]
pub struct HookSpec {
    pub scope: HookScope,
    pub index: usize,
    pub decl: HookDecl,
    /// Directory of the config file that declared it; a relative `run`
    /// resolves against it.
    pub base_dir: PathBuf,
}

impl HookSpec {
    pub fn run_path(&self) -> PathBuf {
        self.decl.resolved_run(&self.base_dir)
    }

    /// The name a log line and the listing use: `station#0` or `channel:<name>#1`.
    pub fn label(&self) -> String {
        match &self.scope {
            HookScope::Station => format!("station#{}", self.index),
            HookScope::Channel(n) => format!("channel:{n}#{}", self.index),
        }
    }

    pub fn kind(&self) -> &'static str {
        if self.decl.is_rhai() { "rhai" } else { "exec" }
    }
}

/// Every hook the loaded config declares: the station's, then each channel's.
pub fn collect(station: &Station) -> Vec<HookSpec> {
    let mut specs = Vec::new();
    let station_dir = station
        .config_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_default();
    for (index, decl) in station.station.hooks.iter().enumerate() {
        specs.push(HookSpec {
            scope: HookScope::Station,
            index,
            decl: decl.clone(),
            base_dir: station_dir.clone(),
        });
    }
    for ch in &station.channels {
        let dir = ch
            .config_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_default();
        for (index, decl) in ch.config.hooks.iter().enumerate() {
            specs.push(HookSpec {
                scope: HookScope::Channel(ch.name.clone()),
                index,
                decl: decl.clone(),
                base_dir: dir.clone(),
            });
        }
    }
    specs
}

/// One line per hook, for `--list-hooks`.
pub fn describe(specs: &[HookSpec]) -> String {
    let mut out = String::new();
    for s in specs {
        let every = s
            .decl
            .every
            .map(|d| humantime::format_duration(d).to_string())
            .unwrap_or_else(|| "-".into());
        let grants = if s.decl.grants.is_empty() {
            "-".to_string()
        } else {
            s.decl
                .grants
                .iter()
                .map(|g| g.as_str())
                .collect::<Vec<_>>()
                .join(",")
        };
        out.push_str(&format!(
            "{}\ton={}\tevery={}\t{}\tgrants={}\trun={}\n",
            s.label(),
            s.decl.on.as_str(),
            every,
            s.kind(),
            grants,
            s.run_path().display(),
        ));
    }
    out
}

/// What one run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Process exit code; `0` for a Rhai script that finished; `-1` when the
    /// run never produced one (spawn failure, killed by a signal, script error).
    pub exit: i32,
    /// Why it failed, when it did.
    pub error: Option<String>,
}

impl Outcome {
    fn ok() -> Self {
        Outcome {
            exit: 0,
            error: None,
        }
    }
    fn failed(exit: i32, error: impl Into<String>) -> Self {
        Outcome {
            exit,
            error: Some(error.into()),
        }
    }
}

/// The event JSON an executable reads on stdin and a script sees as `event`.
fn event_json(spec: &HookSpec, fired_at: &str) -> serde_json::Value {
    let (scope, channel) = match &spec.scope {
        HookScope::Station => ("station", serde_json::Value::Null),
        HookScope::Channel(n) => ("channel", serde_json::Value::String(n.clone())),
    };
    serde_json::json!({
        "hook": spec.label(),
        "trigger": spec.decl.on.as_str(),
        "scope": scope,
        "channel": channel,
        "fired_at": fired_at,
    })
}

/// Run a hook once, blocking, and log it. Callers on the async runtime go
/// through `spawn_blocking`.
pub fn execute(spec: &HookSpec, fired_at: &str) -> Outcome {
    let started = Instant::now();
    let event = event_json(spec, fired_at);
    let outcome = if spec.decl.is_rhai() {
        run_rhai(spec, &event)
    } else {
        run_exec(spec, &event)
    };
    let duration_ms = started.elapsed().as_millis() as u64;
    match &outcome.error {
        None => tracing::info!(
            event = "hook.run",
            hook = %spec.label(),
            trigger = spec.decl.on.as_str(),
            duration_ms,
            exit = outcome.exit,
            "hook finished",
        ),
        Some(err) => tracing::warn!(
            event = "hook.run",
            hook = %spec.label(),
            trigger = spec.decl.on.as_str(),
            duration_ms,
            exit = outcome.exit,
            error = %err,
            "hook failed",
        ),
    }
    outcome
}

fn run_exec(spec: &HookSpec, event: &serde_json::Value) -> Outcome {
    let path = spec.run_path();
    let mut cmd = Command::new(&path);
    match capture(&mut cmd, Some(event.to_string()), RUN_TIMEOUT) {
        Err(e) => Outcome::failed(-1, format!("spawn {}: {e}", path.display())),
        Ok(out) => {
            if !out.stdout.trim().is_empty() {
                tracing::debug!(event = "hook.stdout", hook = %spec.label(), stdout = %out.stdout.trim());
            }
            if out.timed_out {
                return Outcome::failed(-1, format!("killed after {}s", RUN_TIMEOUT.as_secs()));
            }
            match out.code {
                Some(0) => Outcome::ok(),
                Some(c) => Outcome::failed(c, format!("exited {c}: {}", tail(&out.stderr))),
                None => Outcome::failed(-1, format!("killed by a signal: {}", tail(&out.stderr))),
            }
        }
    }
}

/// The last `STDERR_LOGGED` bytes of a stream, trimmed, for an error message.
fn tail(text: &str) -> String {
    let t = text.trim();
    let start = t.len().saturating_sub(STDERR_LOGGED);
    let start = (start..=t.len())
        .find(|i| t.is_char_boundary(*i))
        .unwrap_or(t.len());
    t[start..].to_string()
}

struct Captured {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

/// Run `cmd` with optional stdin, collecting stdout and stderr (each capped at
/// `OUTPUT_CAP` bytes) on reader threads so a chatty child never blocks on a
/// full pipe, and kill it once `timeout` passes.
fn capture(
    cmd: &mut Command,
    stdin: Option<String>,
    timeout: Duration,
) -> std::io::Result<Captured> {
    use std::io::Read;
    let mut child = cmd
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // A hook that never reads its stdin closes the pipe early; that is not
        // a failure of the run.
        std::thread::spawn(move || {
            let _ = pipe.write_all(text.as_bytes());
        });
    }
    fn drain(mut r: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            while let Ok(n) = r.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                if buf.len() < OUTPUT_CAP {
                    buf.extend_from_slice(&chunk[..n]);
                }
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    }
    let out = child.stdout.take().map(drain);
    let err = child.stderr.take().map(drain);
    let deadline = Instant::now() + timeout;
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (Some(status), false);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break (None, true);
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    Ok(Captured {
        code: status.and_then(|s| s.code()),
        stdout: out
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default(),
        stderr: err
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default(),
        timed_out,
    })
}

fn run_rhai(spec: &HookSpec, event: &serde_json::Value) -> Outcome {
    let path = spec.run_path();
    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => return Outcome::failed(-1, format!("read {}: {e}", path.display())),
    };
    let engine = hook_engine(&spec.decl.grants, spec.label());
    let mut scope = Scope::new();
    match rhai::serde::to_dynamic(event) {
        Ok(d) => {
            scope.push_constant("event", d);
        }
        Err(e) => return Outcome::failed(-1, format!("event: {e}")),
    }
    match engine.run_with_scope(&mut scope, &source) {
        Ok(()) => Outcome::ok(),
        Err(e) => Outcome::failed(-1, format!("{}: {e}", path.display())),
    }
}

/// The engine a hook script runs in: `log(text)` always; `exec` and `http`
/// only when granted. A call to an ungranted function is a script error
/// ("function not found"), not a silent no-op.
fn hook_engine(grants: &[HookGrant], label: String) -> Engine {
    let mut engine = Engine::new();
    engine.set_max_expr_depths(128, 64);
    engine.set_max_operations(MAX_OPERATIONS);
    let log_label = label;
    engine.register_fn("log", move |text: &str| {
        tracing::info!(event = "hook.log", hook = %log_label, "{text}");
    });
    if grants.contains(&HookGrant::Exec) {
        engine.register_fn("exec", exec_grant);
    }
    if grants.contains(&HookGrant::Http) {
        engine.register_fn("http", http_grant);
    }
    engine
}

/// `exec(program, args)` -> `#{ code, stdout, stderr }`.
fn exec_grant(program: &str, args: Array) -> Result<Map, Box<rhai::EvalAltResult>> {
    let args: Vec<String> = args.into_iter().map(|a| a.to_string()).collect();
    let out = Command::new(program)
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("exec {program}: {e}"))?;
    let mut m = Map::new();
    m.insert(
        "code".into(),
        Dynamic::from(out.status.code().unwrap_or(-1) as i64),
    );
    m.insert(
        "stdout".into(),
        Dynamic::from(String::from_utf8_lossy(&out.stdout).into_owned()),
    );
    m.insert(
        "stderr".into(),
        Dynamic::from(String::from_utf8_lossy(&out.stderr).into_owned()),
    );
    Ok(m)
}

/// `http(url, method, body)` -> response body text. A 4xx/5xx is an error.
fn http_grant(url: &str, method: &str, body: &str) -> Result<String, Box<rhai::EvalAltResult>> {
    let agent = ureq::AgentBuilder::new().timeout(HTTP_TIMEOUT).build();
    let req = agent.request(&method.to_uppercase(), url);
    let resp = if body.is_empty() {
        req.call()
    } else {
        req.send_string(body)
    };
    match resp {
        Ok(r) => Ok(r.into_string().map_err(|e| format!("http body: {e}"))?),
        Err(ureq::Error::Status(code, _)) => Err(format!("http {method} -> status {code}").into()),
        // ureq puts the URL in its error's Display; a URL may carry a token.
        Err(_) => Err(format!("http {method} failed (transport error)").into()),
    }
}

/// The cron loop for one hook: sleep one `every`, run, repeat until `stop`.
/// A run still going when the next interval arrives makes that interval skip
/// (logged), so a slow hook never stacks up copies of itself.
pub async fn run_cron(spec: HookSpec, stop: Arc<Notify>) {
    let Some(every) = spec.decl.every else {
        return;
    };
    let spec = Arc::new(spec);
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut running: Option<tokio::task::JoinHandle<Outcome>> = None;
    loop {
        tokio::select! {
            biased;
            _ = stop.notified() => {
                // A reload starts a fresh loop for this hook; let the run in
                // flight finish first so the two never overlap.
                if let Some(h) = running.take() {
                    let _ = h.await;
                }
                break;
            }
            _ = interval.tick() => {
                if running.as_ref().is_some_and(|h| !h.is_finished()) {
                    tracing::warn!(
                        event = "hook.skipped",
                        hook = %spec.label(),
                        trigger = spec.decl.on.as_str(),
                        "previous run still going, skipping this interval",
                    );
                    continue;
                }
                let fired_at = time::OffsetDateTime::now_utc()
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default();
                let s = spec.clone();
                running = Some(tokio::task::spawn_blocking(move || execute(&s, &fired_at)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HookTrigger;

    fn spec(dir: &std::path::Path, run: &str, grants: Vec<HookGrant>) -> HookSpec {
        HookSpec {
            scope: HookScope::Channel("news".into()),
            index: 1,
            decl: HookDecl {
                on: HookTrigger::Cron,
                every: Some(Duration::from_secs(60)),
                run: run.into(),
                grants,
            },
            base_dir: dir.to_path_buf(),
        }
    }

    #[cfg(unix)]
    fn write_script(path: &std::path::Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// An executable gets the event JSON on stdin and its exit code is reported.
    #[cfg(unix)]
    #[test]
    fn an_executable_reads_the_event_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("seen.json");
        write_script(
            &dir.path().join("h.sh"),
            &format!("#!/bin/sh\ncat > {}\nexit 3\n", out.display()),
        );
        let o = execute(&spec(dir.path(), "h.sh", vec![]), "2026-01-01T00:00:00Z");
        assert_eq!(o.exit, 3);
        let seen: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(out).unwrap()).unwrap();
        assert_eq!(seen["hook"], "channel:news#1");
        assert_eq!(seen["trigger"], "cron");
        assert_eq!(seen["channel"], "news");
        assert_eq!(seen["fired_at"], "2026-01-01T00:00:00Z");
    }

    /// A Rhai script sees `event`, and `exec` runs only when granted.
    #[cfg(unix)]
    #[test]
    fn a_rhai_script_sees_the_event_and_exec_needs_its_grant() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("marker");
        std::fs::write(
            dir.path().join("h.rhai"),
            format!(
                "let r = exec(\"/bin/sh\", [\"-c\", \"printf '%s' \" + event.hook + \" > {}\"]);\nif r.code != 0 {{ throw \"bad\"; }}",
                out.display()
            ),
        )
        .unwrap();
        let denied = execute(&spec(dir.path(), "h.rhai", vec![]), "t");
        assert_eq!(denied.exit, -1);
        assert!(!out.exists());
        let granted = execute(&spec(dir.path(), "h.rhai", vec![HookGrant::Exec]), "t");
        assert_eq!(granted, Outcome::ok());
        assert_eq!(std::fs::read_to_string(out).unwrap(), "channel:news#1");
    }

    /// A run past the timeout is killed, and a failed run's stderr reaches the
    /// error that is logged.
    #[cfg(unix)]
    #[test]
    fn a_hung_program_is_killed_and_stderr_is_reported() {
        let mut hung = Command::new("/bin/sleep");
        hung.arg("30");
        let started = Instant::now();
        let out = capture(&mut hung, None, Duration::from_millis(200)).unwrap();
        assert!(out.timed_out);
        assert!(started.elapsed() < Duration::from_secs(5));

        let dir = tempfile::tempdir().unwrap();
        write_script(
            &dir.path().join("h.sh"),
            "#!/bin/sh\necho boom >&2\nexit 2\n",
        );
        let o = execute(&spec(dir.path(), "h.sh", vec![]), "t");
        assert_eq!(o.exit, 2);
        assert!(o.error.unwrap().contains("boom"));
    }

    /// An endless script ends at the operation budget instead of holding its
    /// worker thread.
    #[test]
    fn an_endless_script_is_stopped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("h.rhai"), "loop { }").unwrap();
        let o = execute(&spec(dir.path(), "h.rhai", vec![]), "t");
        assert_eq!(o.exit, -1);
        assert!(o.error.unwrap().contains("perations"));
    }

    #[test]
    fn describe_lists_every_hook() {
        let dir = std::path::Path::new("/cfg");
        let text = describe(&[spec(dir, "h.rhai", vec![HookGrant::Http])]);
        assert!(text.contains("channel:news#1"));
        assert!(text.contains("every=1m"));
        assert!(text.contains("rhai"));
        assert!(text.contains("grants=http"));
        assert!(text.contains("/cfg/h.rhai"));
    }
}

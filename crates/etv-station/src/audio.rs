//! Audio facts: the analyzer queue.
//!
//! `tools/audio-analyze.py` (shipped in the image as `audio-analyze`) prints one
//! JSON object of tempo, beat-grid, key and energy facts for a media file. This
//! module runs it once per catalog entry on its own thread, so a full-library
//! backfill never waits on, or delays, a generation, and stores the output in
//! `entry_audio` ([`crate::catalog::audio`]). The facts reach plugins as
//! `ctx.probe(path).audio` and the dashboard through `/api/health/audio` and the
//! item view.
//!
//! The analyzer is a host dependency, located through `ETV_STATION_AUDIO_ANALYZER`
//! (default `audio-analyze`, found on `PATH`). When it cannot be found the queue
//! analyzes nothing and [`analyzer_status`] says so; entries stay unanalyzed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rhai::Dynamic;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::cancel::Cancel;
use crate::catalog::Catalog;

/// Names the analyzer program; read once per queue pass.
pub const ANALYZER_ENV: &str = "ETV_STATION_AUDIO_ANALYZER";
/// The image-shipped analyzer, found on `PATH`.
pub const DEFAULT_ANALYZER: &str = "audio-analyze";
/// A track the analyzer cannot finish in this long is recorded as failed.
const ANALYZE_TIMEOUT: Duration = Duration::from_secs(600);
/// Entries with a longer catalog runtime are not analyzed: the analyzer decodes
/// the whole track to memory (about 10 MB a minute), which a feature film would
/// push past a gigabyte.
const MAX_DURATION_MS: i64 = 30 * 60 * 1000;
/// Entries fetched from the catalog per queue pass.
const BATCH: usize = 50;
/// How long the queue sleeps when it has nothing to do or no analyzer.
const IDLE: Duration = Duration::from_secs(60);

/// The stored audio facts for the entry that plays from `playback_path`, read
/// from the catalog file at `catalog`; `None` when the file has not been
/// analyzed or the catalog cannot be read (logged). `ctx.probe` returns it
/// under `audio`.
pub fn facts_for_path(catalog: &Path, playback_path: &str) -> Option<serde_json::Value> {
    let read = Catalog::open_readonly(catalog).and_then(|c| c.audio_facts_for_path(playback_path));
    match read {
        Ok(facts) => facts,
        Err(e) => {
            tracing::warn!(event = "audio.read_failed", path = playback_path, error = %e, "ctx.probe could not read audio facts");
            None
        }
    }
}

/// The configured analyzer program.
pub fn analyzer_program() -> String {
    std::env::var(ANALYZER_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_ANALYZER.to_string())
}

/// Where `program` resolves to, if it does: a path with a separator must be a
/// file; a bare name is searched on `PATH`.
pub fn locate(program: &str) -> Option<PathBuf> {
    if program.contains(std::path::MAIN_SEPARATOR) {
        let p = PathBuf::from(program);
        return p.is_file().then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// What `/api/health/audio` and `admin facts` report about the analyzer.
pub fn analyzer_status() -> serde_json::Value {
    let program = analyzer_program();
    let found = locate(&program);
    serde_json::json!({
        "program": program,
        "found": found.is_some(),
        "path": found.map(|p| p.display().to_string()),
        "env": ANALYZER_ENV,
    })
}

/// Run the analyzer on `file`: its stdout parsed as JSON. A nonzero exit is an
/// error carrying the last line the analyzer wrote to stderr.
pub fn analyze_file(
    program: &str,
    file: &str,
    cancel: &Cancel,
) -> Result<serde_json::Value, Failure> {
    let out = crate::hooks::exec_run(
        program,
        vec![Dynamic::from(file.to_string())],
        ANALYZE_TIMEOUT,
        cancel.flag(),
    )
    .map_err(|e| {
        // A timeout is the file's fault (it is recorded); a spawn failure or a
        // cancel is the host's (it is retried).
        if e.contains("killed after") {
            Failure::File(e)
        } else {
            Failure::Host(e)
        }
    })?;
    let code = out.get("code").and_then(|c| c.as_int().ok()).unwrap_or(-1);
    let text = |key: &str| {
        out.get(key)
            .and_then(|v| v.clone().into_string().ok())
            .unwrap_or_default()
    };
    if code != 0 {
        let stderr = text("stderr");
        let last = stderr.lines().rev().find(|l| !l.trim().is_empty());
        return Err(Failure::File(format!(
            "{program} exited {code}: {}",
            last.unwrap_or("no message")
        )));
    }
    serde_json::from_str(text("stdout").trim())
        .map_err(|e| Failure::File(format!("{program} printed invalid JSON: {e}")))
}

/// Why an analysis produced no facts.
#[derive(Debug)]
pub enum Failure {
    /// The analyzer ran and rejected or outlasted this file. Recorded, and not
    /// retried until the entry's path changes.
    File(String),
    /// The host could not run it (spawn failure, shutdown). Nothing is recorded.
    Host(String),
}

/// What one pass over the pending entries did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PassReport {
    pub analyzed: usize,
    pub failed: usize,
}

/// Analyze up to [`BATCH`] pending entries, storing each outcome. Stops early
/// when `cancel` fires.
pub fn run_pass(catalog: &Catalog, program: &str, cancel: &Cancel) -> Result<PassReport, String> {
    let pending = catalog
        .audio_pending(BATCH, MAX_DURATION_MS)
        .map_err(|e| e.to_string())?;
    let mut report = PassReport::default();
    for (entry_id, path) in pending {
        if cancel.is_cancelled() {
            break;
        }
        // A file that cannot be opened is a stale mount or a moved file, not a
        // bad track: leave it unrecorded and end the pass so the queue waits.
        if !Path::new(&path).exists() {
            tracing::warn!(event = "audio.file_unreachable", entry_id = %entry_id, path = %path, "audio file not reachable; pass ends, entry stays pending");
            break;
        }
        let started = std::time::Instant::now();
        let outcome = match analyze_file(program, &path, cancel) {
            Ok(facts) => Ok(facts),
            Err(Failure::File(e)) => Err(e),
            Err(Failure::Host(e)) => {
                tracing::warn!(event = "audio.host_error", entry_id = %entry_id, path = %path, error = %e, "analyzer could not run; pass ends, entry stays pending");
                break;
            }
        };
        let at = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_default();
        match &outcome {
            Ok(facts) => {
                report.analyzed += 1;
                tracing::info!(
                    event = "audio.analyzed",
                    entry_id = %entry_id,
                    path = %path,
                    bpm = ?facts.get("bpm"),
                    key = ?facts.get("key"),
                    beat_confidence = ?facts.get("beat_confidence"),
                    steady_beats = ?facts.pointer("/beat_grid/beats"),
                    took_ms = started.elapsed().as_millis() as u64,
                    "audio facts stored"
                );
            }
            Err(e) => {
                report.failed += 1;
                tracing::warn!(
                    event = "audio.failed",
                    entry_id = %entry_id,
                    path = %path,
                    error = %e,
                    "audio analysis failed; not retried until the entry's path changes"
                );
            }
        }
        catalog
            .set_audio_fact(
                &entry_id,
                &path,
                &at,
                outcome.as_ref().map_err(String::as_str),
            )
            .map_err(|e| e.to_string())?;
    }
    Ok(report)
}

/// Start the analyzer queue on its own thread against the catalog at `path`.
/// It runs until `cancel` fires.
pub fn spawn_queue(path: PathBuf, cancel: Arc<Cancel>) {
    let spawned = std::thread::Builder::new()
        .name("audio-queue".into())
        .spawn(move || queue_loop(&path, &cancel));
    if let Err(e) = spawned {
        tracing::error!(event = "audio.queue_spawn_failed", error = %e, "audio queue did not start");
    }
}

fn queue_loop(path: &Path, cancel: &Cancel) {
    let catalog = match Catalog::open(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(event = "audio.queue_open_failed", error = %e, "audio queue could not open the catalog");
            return;
        }
    };
    let mut warned_missing = false;
    while !cancel.is_cancelled() {
        let program = analyzer_program();
        let busy = if locate(&program).is_none() {
            if !warned_missing {
                tracing::warn!(
                    event = "audio.analyzer_missing",
                    program = %program,
                    env = ANALYZER_ENV,
                    "audio analyzer not found; entries stay unanalyzed"
                );
                warned_missing = true;
            }
            false
        } else {
            warned_missing = false;
            match run_pass(&catalog, &program, cancel) {
                Ok(r) => r.analyzed + r.failed > 0,
                Err(e) => {
                    tracing::error!(event = "audio.pass_failed", error = %e, "audio pass failed");
                    false
                }
            }
        };
        if !busy {
            sleep_unless_cancelled(cancel, IDLE);
        }
    }
}

fn sleep_unless_cancelled(cancel: &Cancel, total: Duration) {
    let step = Duration::from_millis(500);
    let mut slept = Duration::ZERO;
    while slept < total && !cancel.is_cancelled() {
        std::thread::sleep(step);
        slept += step;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::catalog::{Entry, EntrySource, Source};
    use std::os::unix::fs::PermissionsExt;

    fn fake_analyzer(dir: &Path, body: &str) -> String {
        let p = dir.join("fake-analyze");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.display().to_string()
    }

    fn catalog_with_one(path: &str) -> Catalog {
        let cat = Catalog::open_in_memory().unwrap();
        cat.upsert_entry(&Entry::new("a", "movie", "A", Source::LocalFs))
            .unwrap();
        cat.add_source(&EntrySource {
            source: Source::LocalFs,
            source_id: "a".into(),
            entry_id: "a".into(),
            playback_path: path.into(),
            last_seen: None,
            missing_since: None,
            unairable_reason: None,
        })
        .unwrap();
        cat
    }

    #[test]
    fn a_pass_stores_the_analyzers_json_and_a_failure_is_recorded_once() {
        let dir = tempfile::tempdir().unwrap();
        let ok = fake_analyze_ok(dir.path());
        let a = dir.path().join("a.mp4");
        let b = dir.path().join("b.mp4");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"x").unwrap();
        let (a, b) = (a.display().to_string(), b.display().to_string());
        let cat = catalog_with_one(&a);
        let report = run_pass(&cat, &ok, &Cancel::default()).unwrap();
        assert_eq!(
            report,
            PassReport {
                analyzed: 1,
                failed: 0
            }
        );
        let facts = cat.audio_facts_for_path(&a).unwrap().unwrap();
        assert_eq!(facts["bpm"], 120.0);
        assert_eq!(facts["args"], a.as_str());

        let cat = catalog_with_one(&b);
        let bad = fake_analyzer(dir.path(), "echo 'cannot decode' >&2; exit 1");
        let report = run_pass(&cat, &bad, &Cancel::default()).unwrap();
        assert_eq!(
            report,
            PassReport {
                analyzed: 0,
                failed: 1
            }
        );
        let row = cat.audio_fact("a").unwrap().unwrap();
        assert!(row.error.unwrap().contains("cannot decode"));
        assert_eq!(
            run_pass(&cat, &bad, &Cancel::default()).unwrap(),
            PassReport::default(),
            "a failed file is not retried"
        );
    }

    #[test]
    fn an_unreachable_file_is_left_pending_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let ok = fake_analyze_ok(dir.path());
        let cat = catalog_with_one("/nonexistent/stale-mount.mp4");
        assert_eq!(
            run_pass(&cat, &ok, &Cancel::default()).unwrap(),
            PassReport::default()
        );
        assert_eq!(cat.audio_pending(10, i64::MAX).unwrap().len(), 1);
    }

    fn fake_analyze_ok(dir: &Path) -> String {
        fake_analyzer(dir, r#"printf '{"bpm":120.0,"args":"%s"}\n' "$1""#)
    }

    #[test]
    fn locate_finds_a_file_path_and_rejects_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        let tool = fake_analyze_ok(dir.path());
        assert_eq!(locate(&tool), Some(PathBuf::from(&tool)));
        assert_eq!(locate("/nonexistent/audio-analyze"), None);
    }
}

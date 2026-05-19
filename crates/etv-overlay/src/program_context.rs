//! Per-channel program-context source.
//!
//! Reads the chunked playout JSON files the station daemon writes for a
//! channel (`{start}_{finish}.json`) and answers "what is airing at wallclock
//! T?" — title, sub_title, item_elapsed, item_remaining, and a one-item
//! lookahead (next_title / next_sub_title).
//!
//! No sidecar files. The playout JSON IS the schedule; we just consume it
//! read-only from the same folder station writes to.
//!
//! Reload triggers, in cost order:
//! 1. Stat the folder's mtime once per `MTIME_POLL`. Cheap.
//! 2. If mtime changed, re-read all chunk files in the folder.
//!
//! Per-frame `current_at` does a binary search against the loaded item list
//! (nanoseconds), so the renderer can call it at frame rate.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;
use time::OffsetDateTime;

const MTIME_POLL: Duration = Duration::from_secs(1);

/// Snapshot of "what's airing right now" on the channel the overlay is bound
/// to. All fields are best-effort: missing program metadata renders as empty
/// strings; an absent or out-of-range schedule renders as
/// [`ProgramContext::unknown`].
#[derive(Debug, Clone)]
pub struct ProgramContext {
    pub title: String,
    pub sub_title: String,
    pub next_title: String,
    pub next_sub_title: String,
    /// Seconds since the current item's `start`. `-1.0` when unknown so
    /// scripts can gate visibility on `item_elapsed >= 0.0 && item_elapsed < 10.0`.
    pub item_elapsed: f64,
    /// Seconds until the current item's `finish`. `-1.0` when unknown.
    pub item_remaining: f64,
}

impl ProgramContext {
    pub fn unknown() -> Self {
        Self {
            title: String::new(),
            sub_title: String::new(),
            next_title: String::new(),
            next_sub_title: String::new(),
            item_elapsed: -1.0,
            item_remaining: -1.0,
        }
    }
}

impl Default for ProgramContext {
    fn default() -> Self {
        Self::unknown()
    }
}

#[derive(Deserialize)]
struct Playout {
    #[serde(default)]
    items: Vec<ItemRow>,
}

#[derive(Deserialize, Clone)]
struct ItemRow {
    #[serde(with = "time::serde::rfc3339")]
    start: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    finish: OffsetDateTime,
    #[serde(default)]
    program: Option<ProgramRow>,
}

#[derive(Deserialize, Clone, Default)]
struct ProgramRow {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    sub_title: Option<String>,
}

/// Loads and caches the channel's chunked playout JSON. Call
/// [`Self::refresh`] each frame; it's rate-limited internally and only re-
/// reads disk when the folder mtime changes.
pub struct ProgramContextSource {
    folder: PathBuf,
    items: Vec<ItemRow>,
    folder_mtime: Option<SystemTime>,
    last_mtime_check: Option<Instant>,
}

impl ProgramContextSource {
    pub fn new(folder: PathBuf) -> Self {
        Self {
            folder,
            items: Vec::new(),
            folder_mtime: None,
            last_mtime_check: None,
        }
    }

    pub fn folder(&self) -> &Path {
        &self.folder
    }

    /// Reload schedule from disk if the folder's mtime has changed since the
    /// last successful refresh (or this is the first refresh). Rate-limited
    /// to one `stat` per `MTIME_POLL`.
    ///
    /// Returns `true` if items were reloaded this call.
    pub fn refresh(&mut self) -> std::io::Result<bool> {
        let now = Instant::now();
        if let Some(prev) = self.last_mtime_check
            && now.duration_since(prev) < MTIME_POLL
            && !self.items.is_empty()
        {
            return Ok(false);
        }
        self.last_mtime_check = Some(now);

        let meta = match std::fs::metadata(&self.folder) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        let mtime = meta.modified().ok();
        if mtime == self.folder_mtime && !self.items.is_empty() {
            return Ok(false);
        }
        self.folder_mtime = mtime;
        self.reload_items()?;
        Ok(true)
    }

    fn reload_items(&mut self) -> std::io::Result<()> {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&self.folder)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| is_chunk_file(p))
            .collect();
        entries.sort();

        let mut items = Vec::new();
        for path in &entries {
            let raw = match std::fs::read_to_string(path) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "program_context: failed to read chunk file; skipping",
                    );
                    continue;
                }
            };
            match serde_json::from_str::<Playout>(&raw) {
                Ok(p) => items.extend(p.items),
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "program_context: failed to parse chunk file; skipping",
                    );
                }
            }
        }
        items.sort_by_key(|i| i.start);
        self.items = items;
        Ok(())
    }

    /// Look up the item airing at `now`. Returns
    /// [`ProgramContext::unknown`] if no loaded item contains `now`.
    pub fn current_at(&self, now: OffsetDateTime) -> ProgramContext {
        if self.items.is_empty() {
            return ProgramContext::unknown();
        }
        // partition_point gives the first index whose `start > now`; the
        // candidate item is the one immediately before it.
        let after = self.items.partition_point(|i| i.start <= now);
        if after == 0 {
            return ProgramContext::unknown();
        }
        let idx = after - 1;
        let item = &self.items[idx];
        if now < item.start || now >= item.finish {
            return ProgramContext::unknown();
        }
        let next = self.items.get(idx + 1);

        let (title, sub_title) = program_strings(item.program.as_ref());
        let (next_title, next_sub_title) = program_strings(next.and_then(|n| n.program.as_ref()));
        let elapsed = (now - item.start).as_seconds_f64();
        let remaining = (item.finish - now).as_seconds_f64();

        ProgramContext {
            title,
            sub_title,
            next_title,
            next_sub_title,
            item_elapsed: elapsed,
            item_remaining: remaining,
        }
    }
}

fn program_strings(p: Option<&ProgramRow>) -> (String, String) {
    let Some(p) = p else {
        return (String::new(), String::new());
    };
    (
        p.title.clone().unwrap_or_default(),
        p.sub_title.clone().unwrap_or_default(),
    )
}

fn is_chunk_file(p: &Path) -> bool {
    if p.extension().and_then(|s| s.to_str()) != Some("json") {
        return false;
    }
    // Station chunk files are `{start}_{finish}.json`. The underscore is the
    // discriminator that lets us ignore any future sidecar (`now.json`,
    // `.heartbeat`, etc.) someone drops into the folder.
    p.file_name()
        .and_then(|s| s.to_str())
        .map(|name| name.contains('_'))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use time::macros::datetime;

    fn write_chunk(dir: &Path, name: &str, body: &str) {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    const TWO_ITEM_CHUNK: &str = r#"{
        "version": "test",
        "items": [
            {
                "id": "a",
                "start": "2026-04-13T00:00:00Z",
                "finish": "2026-04-13T00:10:00Z",
                "program": { "title": "Alpha", "sub_title": "Pilot" }
            },
            {
                "id": "b",
                "start": "2026-04-13T00:10:00Z",
                "finish": "2026-04-13T00:20:00Z",
                "program": { "title": "Beta" }
            }
        ]
    }"#;

    #[test]
    fn unknown_when_folder_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut src = ProgramContextSource::new(dir.path().to_path_buf());
        src.refresh().unwrap();
        let ctx = src.current_at(datetime!(2026-04-13 00:05 UTC));
        assert_eq!(ctx.title, "");
        assert_eq!(ctx.item_elapsed, -1.0);
    }

    #[test]
    fn finds_current_and_next() {
        let dir = tempfile::tempdir().unwrap();
        write_chunk(dir.path(), "chunk_a.json", TWO_ITEM_CHUNK);
        let mut src = ProgramContextSource::new(dir.path().to_path_buf());
        src.refresh().unwrap();

        let ctx = src.current_at(datetime!(2026-04-13 00:05 UTC));
        assert_eq!(ctx.title, "Alpha");
        assert_eq!(ctx.sub_title, "Pilot");
        assert_eq!(ctx.next_title, "Beta");
        assert!((ctx.item_elapsed - 300.0).abs() < 1e-3);
        assert!((ctx.item_remaining - 300.0).abs() < 1e-3);
    }

    #[test]
    fn spans_chunk_boundary_for_next_lookahead() {
        let dir = tempfile::tempdir().unwrap();
        write_chunk(
            dir.path(),
            "1_2.json",
            r#"{"version":"test","items":[
              {"id":"end","start":"2026-04-13T00:00:00Z","finish":"2026-04-13T01:00:00Z",
               "program":{"title":"Last of chunk 1"}}
            ]}"#,
        );
        write_chunk(
            dir.path(),
            "2_3.json",
            r#"{"version":"test","items":[
              {"id":"start","start":"2026-04-13T01:00:00Z","finish":"2026-04-13T02:00:00Z",
               "program":{"title":"First of chunk 2"}}
            ]}"#,
        );
        let mut src = ProgramContextSource::new(dir.path().to_path_buf());
        src.refresh().unwrap();

        let ctx = src.current_at(datetime!(2026-04-13 00:30 UTC));
        assert_eq!(ctx.title, "Last of chunk 1");
        assert_eq!(ctx.next_title, "First of chunk 2");
    }

    #[test]
    fn refresh_picks_up_newly_added_chunk_file() {
        let dir = tempfile::tempdir().unwrap();
        write_chunk(dir.path(), "1_2.json", TWO_ITEM_CHUNK);
        let mut src = ProgramContextSource::new(dir.path().to_path_buf());
        src.refresh().unwrap();
        assert_eq!(src.current_at(datetime!(2026-04-13 00:05 UTC)).title, "Alpha");
        assert_eq!(src.current_at(datetime!(2026-04-13 02:30 UTC)).title, "");

        // Station rolls a new chunk. Adding a file changes the directory's
        // mtime even on coarse-resolution filesystems.
        write_chunk(
            dir.path(),
            "2_3.json",
            r#"{"version":"test","items":[
              {"id":"c","start":"2026-04-13T02:00:00Z","finish":"2026-04-13T03:00:00Z",
               "program":{"title":"Gamma"}}
            ]}"#,
        );
        // Force the rate-limiter to consider another check.
        src.last_mtime_check = None;
        src.refresh().unwrap();
        assert_eq!(src.current_at(datetime!(2026-04-13 02:30 UTC)).title, "Gamma");
        // Original items still resolvable.
        assert_eq!(src.current_at(datetime!(2026-04-13 00:05 UTC)).title, "Alpha");
    }

    #[test]
    fn unknown_when_now_outside_loaded_items() {
        let dir = tempfile::tempdir().unwrap();
        write_chunk(dir.path(), "1_2.json", TWO_ITEM_CHUNK);
        let mut src = ProgramContextSource::new(dir.path().to_path_buf());
        src.refresh().unwrap();

        // Before any item
        assert_eq!(src.current_at(datetime!(2026-04-12 23:00 UTC)).title, "");
        // After the last item
        assert_eq!(src.current_at(datetime!(2026-04-13 03:00 UTC)).title, "");
    }
}

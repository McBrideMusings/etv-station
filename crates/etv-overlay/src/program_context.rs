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
    /// The current item's guide description. On a channel with
    /// `scoring.attribution` on, its last line names who has been watching
    /// (#113) — which is what a lower third would draw.
    pub description: String,
    /// Season number, or `-1` when the item carries none. Same absent-value
    /// convention as `item_elapsed`, and the discriminator a script uses to
    /// tell an episode from a film — the station writes `season`/`episode`
    /// only for `kind == "episode"` (see `resolve.rs:1456`), so `season >= 0`
    /// means "this is an episode" without needing a separate kind field.
    pub season: i64,
    /// Episode number, or `-1` when absent. See [`Self::season`].
    pub episode: i64,
    /// Release / air year, or `-1` when absent. Present on films and episodes
    /// alike.
    pub year: i64,
    pub next_title: String,
    pub next_sub_title: String,
    /// The one-item lookahead's counterparts to [`Self::season`],
    /// [`Self::episode`] and [`Self::year`], with the same `-1` sentinel.
    pub next_season: i64,
    pub next_episode: i64,
    pub next_year: i64,
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
            description: String::new(),
            season: -1,
            episode: -1,
            year: -1,
            next_title: String::new(),
            next_sub_title: String::new(),
            next_season: -1,
            next_episode: -1,
            next_year: -1,
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
    /// Carries the attribution line the station appends (#113). Read here and
    /// not in ETV-next because the overlay parses these chunk files itself.
    #[serde(default)]
    description: Option<String>,
    /// Written by the station for episodes only, so its presence is what a
    /// script reads as "this is an episode" rather than a film.
    #[serde(default)]
    season: Option<i64>,
    #[serde(default)]
    episode: Option<i64>,
    #[serde(default)]
    year: Option<i64>,
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

        let cur = program_fields(item.program.as_ref());
        let nxt = program_fields(next.and_then(|n| n.program.as_ref()));
        let elapsed = (now - item.start).as_seconds_f64();
        let remaining = (item.finish - now).as_seconds_f64();

        ProgramContext {
            title: cur.title,
            sub_title: cur.sub_title,
            description: item
                .program
                .as_ref()
                .and_then(|p| p.description.clone())
                .unwrap_or_default(),
            season: cur.season,
            episode: cur.episode,
            year: cur.year,
            next_title: nxt.title,
            next_sub_title: nxt.sub_title,
            next_season: nxt.season,
            next_episode: nxt.episode,
            next_year: nxt.year,
            item_elapsed: elapsed,
            item_remaining: remaining,
        }
    }
}

/// The per-item slice of [`ProgramContext`], extracted once so the current
/// item and the lookahead read the same way.
#[derive(Default)]
struct ProgramFields {
    title: String,
    sub_title: String,
    season: i64,
    episode: i64,
    year: i64,
}

fn program_fields(p: Option<&ProgramRow>) -> ProgramFields {
    let Some(p) = p else {
        return ProgramFields {
            season: -1,
            episode: -1,
            year: -1,
            ..ProgramFields::default()
        };
    };
    ProgramFields {
        title: p.title.clone().unwrap_or_default(),
        sub_title: p.sub_title.clone().unwrap_or_default(),
        season: p.season.unwrap_or(-1),
        episode: p.episode.unwrap_or(-1),
        year: p.year.unwrap_or(-1),
    }
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

    /// A chunk written by a channel with `scoring.attribution` on, shaped the
    /// way the station really writes it: the catalog synopsis, a blank line,
    /// then the credit.
    const ATTRIBUTED_CHUNK: &str = r#"{
        "version": "test",
        "items": [
            {
                "id": "a",
                "start": "2026-04-13T00:00:00Z",
                "finish": "2026-04-13T00:10:00Z",
                "program": {
                    "title": "Alpha",
                    "description": "A hobbit sets out.\n\nWatched recently by Bob Example and carol"
                }
            }
        ]
    }"#;

    /// The overlay reads the playout JSON itself, so the attribution line the
    /// station appends has to survive that parse to reach a script (#113).
    #[test]
    fn carries_the_attribution_description_through_to_the_context() {
        let dir = tempfile::tempdir().unwrap();
        write_chunk(dir.path(), "chunk_a.json", ATTRIBUTED_CHUNK);
        let mut src = ProgramContextSource::new(dir.path().to_path_buf());
        src.refresh().unwrap();

        let ctx = src.current_at(datetime!(2026-04-13 00:05 UTC));
        assert_eq!(
            ctx.description,
            "A hobbit sets out.\n\nWatched recently by Bob Example and carol",
        );
    }

    /// A channel that does not attribute writes no description, and that must
    /// arrive as an empty string rather than blowing up the parse.
    #[test]
    fn a_chunk_with_no_description_yields_an_empty_one() {
        let dir = tempfile::tempdir().unwrap();
        write_chunk(dir.path(), "chunk_a.json", TWO_ITEM_CHUNK);
        let mut src = ProgramContextSource::new(dir.path().to_path_buf());
        src.refresh().unwrap();

        assert_eq!(
            src.current_at(datetime!(2026-04-13 00:05 UTC)).description,
            ""
        );
    }

    /// One episode followed by one film, shaped the way the station really
    /// writes them: `season`/`episode` only on the episode (`resolve.rs:1456`),
    /// `year` on both.
    const MIXED_KINDS_CHUNK: &str = r#"{
        "version": "test",
        "items": [
            {
                "id": "a",
                "start": "2026-04-13T00:00:00Z",
                "finish": "2026-04-13T00:10:00Z",
                "program": {
                    "title": "Bob's Burgers",
                    "sub_title": "Manic Pixie Crap Show",
                    "season": 12,
                    "episode": 1,
                    "year": 2021
                }
            },
            {
                "id": "b",
                "start": "2026-04-13T00:10:00Z",
                "finish": "2026-04-13T00:20:00Z",
                "program": { "title": "Highest 2 Lowest", "year": 2025 }
            }
        ]
    }"#;

    /// An overlay script formats a film differently from an episode, and the
    /// only thing it can tell them apart by is whether a season number came
    /// through. Dropping these fields on the floor here is what made that
    /// impossible before.
    #[test]
    fn carries_season_episode_and_year_for_current_and_next() {
        let dir = tempfile::tempdir().unwrap();
        write_chunk(dir.path(), "1_2.json", MIXED_KINDS_CHUNK);
        let mut src = ProgramContextSource::new(dir.path().to_path_buf());
        src.refresh().unwrap();

        let ctx = src.current_at(datetime!(2026-04-13 00:05 UTC));
        assert_eq!(ctx.season, 12);
        assert_eq!(ctx.episode, 1);
        assert_eq!(ctx.year, 2021);
        // The lookahead is the film, so it has a year and no season/episode.
        assert_eq!(ctx.next_title, "Highest 2 Lowest");
        assert_eq!(ctx.next_year, 2025);
        assert_eq!(ctx.next_season, -1);
        assert_eq!(ctx.next_episode, -1);
    }

    /// A film must not read as season 0 episode 0 — `0` is a legal season per
    /// the playout schema, so absent has to be a value no real item can hold.
    #[test]
    fn an_absent_season_reads_as_minus_one_not_zero() {
        let dir = tempfile::tempdir().unwrap();
        write_chunk(dir.path(), "1_2.json", MIXED_KINDS_CHUNK);
        let mut src = ProgramContextSource::new(dir.path().to_path_buf());
        src.refresh().unwrap();

        let ctx = src.current_at(datetime!(2026-04-13 00:15 UTC));
        assert_eq!(ctx.title, "Highest 2 Lowest");
        assert_eq!(ctx.season, -1);
        assert_eq!(ctx.episode, -1);
        assert_eq!(ctx.year, 2025);
    }

    /// With no item airing there is nothing to format, and a script gating on
    /// `season >= 0` must not be told the unknown item is a film.
    #[test]
    fn unknown_context_reports_every_number_absent() {
        let ctx = ProgramContext::unknown();
        assert_eq!(ctx.season, -1);
        assert_eq!(ctx.episode, -1);
        assert_eq!(ctx.year, -1);
        assert_eq!(ctx.next_season, -1);
        assert_eq!(ctx.next_episode, -1);
        assert_eq!(ctx.next_year, -1);
    }

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
        assert_eq!(
            src.current_at(datetime!(2026-04-13 00:05 UTC)).title,
            "Alpha"
        );
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
        assert_eq!(
            src.current_at(datetime!(2026-04-13 02:30 UTC)).title,
            "Gamma"
        );
        // Original items still resolvable.
        assert_eq!(
            src.current_at(datetime!(2026-04-13 00:05 UTC)).title,
            "Alpha"
        );
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

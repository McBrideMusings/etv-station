use std::path::{Path, PathBuf};

use ersatztv_playout::playout::{DATE_FORMAT, Playout};
use time::OffsetDateTime;
use time_tz::Tz;

use crate::errors::StationError;

#[derive(Debug, Clone)]
pub struct DiscoveredFile {
    pub path: PathBuf,
    pub start: OffsetDateTime,
    pub finish: OffsetDateTime,
}

pub async fn scan_output_folder(folder: &Path) -> Result<Vec<DiscoveredFile>, StationError> {
    let io_err = |source: std::io::Error| StationError::Io {
        path: folder.to_path_buf(),
        source,
    };

    let mut out = Vec::new();
    let mut entries = match tokio::fs::read_dir(folder).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(source) => return Err(io_err(source)),
    };

    while let Some(entry) = entries.next_entry().await.map_err(io_err)? {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.ends_with(".json") {
            continue;
        }
        let stem = &name[..name.len() - ".json".len()];
        let Some((start_s, finish_s)) = split_start_finish(stem) else {
            continue;
        };
        let Ok(start) = OffsetDateTime::parse(start_s, &DATE_FORMAT) else {
            continue;
        };
        let Ok(finish) = OffsetDateTime::parse(finish_s, &DATE_FORMAT) else {
            continue;
        };
        out.push(DiscoveredFile {
            path,
            start,
            finish,
        });
    }
    out.sort_by_key(|f| f.start);
    Ok(out)
}

pub fn highest_finish(files: &[DiscoveredFile]) -> Option<OffsetDateTime> {
    files.iter().map(|f| f.finish).max()
}

/// What one sweep removed, split by which end of the window it fell off.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SweepCounts {
    /// Files whose window fully elapsed past the retention horizon.
    pub elapsed: usize,
    /// Files starting beyond anything the forward window can reach.
    pub unreachable: usize,
}

impl SweepCounts {
    pub fn total(self) -> usize {
        self.elapsed + self.unreachable
    }
}

/// The first instant a correct generation can never write a file at.
///
/// Derived from how [`crate::emit`] lays chunks, not picked: a generation runs
/// until the window target is covered, so the last chunk it opens is the one
/// containing `target`; an item straddling that chunk's end is emitted whole and
/// re-emitted into the next chunk, so one chunk of headroom past target's chunk
/// is reachable and the one after it is not. An item longer than a whole chunk
/// could in principle reach further; such a file is trimmed here and regenerated
/// on the same tick, which costs a rewrite and never leaves a hole.
pub fn unreachable_after(
    target: OffsetDateTime,
    chunk_hours: u32,
    tz: &'static Tz,
) -> OffsetDateTime {
    let target_chunk = crate::tz::chunk_boundary_at_or_before(target, chunk_hours, tz);
    let headroom = crate::tz::add_chunk(target_chunk, chunk_hours, tz);
    crate::tz::add_chunk(headroom, chunk_hours, tz)
}

/// Delete emitted playout files that fell off either end of the channel's
/// window: fully elapsed past the retention horizon (`finish < now -
/// retention_days`), or starting at/after `unreachable` — beyond anything the
/// forward window can produce.
///
/// The forward edge exists because forward materialization only ever moves the
/// frontier ahead (`highest_finish`). A file dated years out — however it got
/// there — makes the daemon read the window as already covered and stop
/// generating forever, so nothing but this sweep can ever remove it.
///
/// Cheap and idempotent: it parses the `{start}_{finish}.json` filename rather
/// than the file contents, so re-running with the same inputs removes nothing.
/// `scan_output_folder` only matches the window naming, so the `.anchor`
/// sidecar and `.durations.json` cache are never candidates — station state is
/// never swept. Housekeeping-grade: a scan failure or an individual delete
/// failure is logged and skipped, never propagated, so a single un-removable
/// file can't abort the sweep or tear down the daemon.
pub async fn sweep_window(
    folder: &Path,
    retention_days: u32,
    unreachable: OffsetDateTime,
    now: OffsetDateTime,
) -> SweepCounts {
    let cutoff = now - time::Duration::days(i64::from(retention_days));
    let files = match scan_output_folder(folder).await {
        Ok(files) => files,
        Err(e) => {
            tracing::warn!(
                event = "retention.scan_failed",
                folder = %folder.display(),
                error = %e,
                "window sweep: scan failed; skipping",
            );
            return SweepCounts::default();
        }
    };

    let mut counts = SweepCounts::default();
    for f in &files {
        let elapsed = f.finish < cutoff;
        let beyond = f.start >= unreachable;
        if !elapsed && !beyond {
            continue;
        }
        match tokio::fs::remove_file(&f.path).await {
            Ok(()) => {
                if elapsed {
                    tracing::info!(
                        event = "retention.delete",
                        file = %f.path.display(),
                        finish = %f.finish,
                        "pruned playout file past retention horizon",
                    );
                    counts.elapsed += 1;
                } else {
                    tracing::info!(
                        event = "unreachable.delete",
                        file = %f.path.display(),
                        start = %f.start,
                        unreachable = %unreachable,
                        "pruned playout file beyond the forward window",
                    );
                    counts.unreachable += 1;
                }
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                tracing::warn!(
                    event = "retention.delete_failed",
                    file = %f.path.display(),
                    error = %source,
                    "window sweep: failed to remove file",
                );
            }
        }
    }
    counts
}

/// The earliest instant in `[now, horizon)` that no scheduled item covers, when
/// a later item *does* cover something after it — i.e. an interior hole in the
/// timeline, the kind that airs as black. `None` means the window is covered
/// contiguously from `now` up to wherever content currently reaches.
///
/// Coverage is read from item spans, not filenames, so a file whose name
/// over-claims (the very defect this exists to catch) cannot hide a hole. A
/// purely trailing edge — content simply stops and nothing follows — is *not* a
/// gap: that is the un-materialized frontier the roll tick extends, not damage.
///
/// `horizon` bounds the scan: a small near-future horizon on a roll tick is
/// cheap and still catches a hole before the playhead reaches it, while a
/// full-window horizon at startup repairs damage already on disk in one pass.
pub async fn first_coverage_gap(
    folder: &Path,
    now: OffsetDateTime,
    horizon: OffsetDateTime,
) -> Result<Option<OffsetDateTime>, StationError> {
    if horizon <= now {
        return Ok(None);
    }
    let files = scan_output_folder(folder).await?;

    // Gather item spans overlapping [now, horizon]. The filename span only
    // decides which files are worth opening; the hole test uses item times.
    let mut spans: Vec<(OffsetDateTime, OffsetDateTime)> = Vec::new();
    for f in &files {
        if f.finish <= now || f.start >= horizon {
            continue;
        }
        let bytes = tokio::fs::read(&f.path)
            .await
            .map_err(|source| StationError::Io {
                path: f.path.clone(),
                source,
            })?;
        let playout: Playout =
            serde_json::from_slice(&bytes).map_err(|source| StationError::PlayoutCorrupt {
                path: f.path.clone(),
                source,
            })?;
        for item in playout.items {
            if item.finish > now && item.start < horizon {
                spans.push((item.start, item.finish));
            }
        }
    }
    spans.sort_by_key(|(start, _)| *start);

    let mut covered_to = now;
    for (start, finish) in spans {
        if start > covered_to {
            // A hole `[covered_to, start)`, and this span is content after it.
            return Ok(Some(covered_to));
        }
        if finish > covered_to {
            covered_to = finish;
        }
        if covered_to >= horizon {
            return Ok(None);
        }
    }
    Ok(None)
}

fn split_start_finish(stem: &str) -> Option<(&str, &str)> {
    // Filename is `{start}_{finish}` where each side may contain `-` or `+` from
    // the offset and `T`. Underscore appears once between them — our DATE_FORMAT
    // does not produce `_` itself.
    let idx = stem.find('_')?;
    Some((&stem[..idx], &stem[idx + 1..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use time::macros::datetime;

    async fn write_blank(path: &Path) {
        tokio::fs::write(path, b"{}").await.unwrap();
    }

    #[test]
    fn highest_finish_returns_none_for_empty() {
        assert!(highest_finish(&[]).is_none());
    }

    #[tokio::test]
    async fn ignores_non_json_and_garbage() {
        let dir = tempdir().unwrap();
        write_blank(&dir.path().join("garbage.txt")).await;
        write_blank(&dir.path().join("not_a_window.json")).await;
        let files = scan_output_folder(dir.path()).await.unwrap();
        assert!(files.is_empty());
    }

    fn window_name(start: OffsetDateTime, finish: OffsetDateTime) -> String {
        let s = start.format(&DATE_FORMAT).unwrap();
        let f = finish.format(&DATE_FORMAT).unwrap();
        format!("{s}_{f}.json")
    }

    /// Write a playout file named `[file_start, file_finish]` holding items with
    /// the given `[start, finish]` spans. Names and contents are supplied
    /// independently so a test can build an over-claiming file on purpose.
    async fn write_playout(
        dir: &Path,
        file_start: OffsetDateTime,
        file_finish: OffsetDateTime,
        item_spans: &[(OffsetDateTime, OffsetDateTime)],
    ) {
        let items: Vec<String> = item_spans
            .iter()
            .enumerate()
            .map(|(i, (s, f))| {
                format!(
                    r#"{{"id":"i{i}","start":"{}","finish":"{}"}}"#,
                    s.format(&time::format_description::well_known::Rfc3339)
                        .unwrap(),
                    f.format(&time::format_description::well_known::Rfc3339)
                        .unwrap(),
                )
            })
            .collect();
        let body = format!(
            r#"{{"version":"https://ersatztv.org/playout/version/0.0.1","items":[{}]}}"#,
            items.join(",")
        );
        tokio::fs::write(dir.join(window_name(file_start, file_finish)), body)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn no_gap_when_coverage_is_contiguous() {
        let dir = tempdir().unwrap();
        let now = datetime!(2026-04-20 12:00 UTC);
        write_playout(
            dir.path(),
            datetime!(2026-04-20 12:00 UTC),
            datetime!(2026-04-20 13:00 UTC),
            &[
                (
                    datetime!(2026-04-20 12:00 UTC),
                    datetime!(2026-04-20 12:30 UTC),
                ),
                (
                    datetime!(2026-04-20 12:30 UTC),
                    datetime!(2026-04-20 13:00 UTC),
                ),
            ],
        )
        .await;
        let gap = first_coverage_gap(dir.path(), now, datetime!(2026-04-20 13:00 UTC))
            .await
            .unwrap();
        assert_eq!(gap, None);
    }

    #[tokio::test]
    async fn trailing_frontier_is_not_a_gap() {
        // Content stops at 12:30 and nothing follows: that is the un-materialized
        // frontier, not a hole, even though the horizon extends past it.
        let dir = tempdir().unwrap();
        let now = datetime!(2026-04-20 12:00 UTC);
        write_playout(
            dir.path(),
            datetime!(2026-04-20 12:00 UTC),
            datetime!(2026-04-20 12:30 UTC),
            &[(
                datetime!(2026-04-20 12:00 UTC),
                datetime!(2026-04-20 12:30 UTC),
            )],
        )
        .await;
        let gap = first_coverage_gap(dir.path(), now, datetime!(2026-04-20 18:00 UTC))
            .await
            .unwrap();
        assert_eq!(gap, None);
    }

    #[tokio::test]
    async fn interior_hole_is_reported_at_its_start() {
        // Covered 12:00–12:30, nothing 12:30–13:00, covered again 13:00–13:30.
        // The hole begins at 12:30.
        let dir = tempdir().unwrap();
        let now = datetime!(2026-04-20 12:00 UTC);
        write_playout(
            dir.path(),
            datetime!(2026-04-20 12:00 UTC),
            datetime!(2026-04-20 12:30 UTC),
            &[(
                datetime!(2026-04-20 12:00 UTC),
                datetime!(2026-04-20 12:30 UTC),
            )],
        )
        .await;
        write_playout(
            dir.path(),
            datetime!(2026-04-20 13:00 UTC),
            datetime!(2026-04-20 13:30 UTC),
            &[(
                datetime!(2026-04-20 13:00 UTC),
                datetime!(2026-04-20 13:30 UTC),
            )],
        )
        .await;
        let gap = first_coverage_gap(dir.path(), now, datetime!(2026-04-20 14:00 UTC))
            .await
            .unwrap();
        assert_eq!(gap, Some(datetime!(2026-04-20 12:30 UTC)));
    }

    #[tokio::test]
    async fn an_over_claiming_name_cannot_hide_a_hole() {
        // The exact defect that aired black: a file NAMED [12:00,18:00] but
        // holding only 3 minutes. Coverage is read from items, so the hole after
        // 12:03 is still found despite the name reaching 18:00.
        let dir = tempdir().unwrap();
        let now = datetime!(2026-04-20 12:00 UTC);
        write_playout(
            dir.path(),
            datetime!(2026-04-20 12:00 UTC),
            datetime!(2026-04-20 18:00 UTC), // name over-claims 6h
            &[(
                datetime!(2026-04-20 12:00 UTC),
                datetime!(2026-04-20 12:03 UTC),
            )],
        )
        .await;
        write_playout(
            dir.path(),
            datetime!(2026-04-20 18:00 UTC),
            datetime!(2026-04-20 18:30 UTC),
            &[(
                datetime!(2026-04-20 18:00 UTC),
                datetime!(2026-04-20 18:30 UTC),
            )],
        )
        .await;
        let gap = first_coverage_gap(dir.path(), now, datetime!(2026-04-20 19:00 UTC))
            .await
            .unwrap();
        assert_eq!(gap, Some(datetime!(2026-04-20 12:03 UTC)));
    }

    #[tokio::test]
    async fn a_horizon_before_the_hole_reports_nothing() {
        // The near-future look-ahead only heals what is about to air; a hole past
        // the horizon waits for a later tick.
        let dir = tempdir().unwrap();
        let now = datetime!(2026-04-20 12:00 UTC);
        write_playout(
            dir.path(),
            datetime!(2026-04-20 12:00 UTC),
            datetime!(2026-04-20 12:30 UTC),
            &[(
                datetime!(2026-04-20 12:00 UTC),
                datetime!(2026-04-20 12:30 UTC),
            )],
        )
        .await;
        write_playout(
            dir.path(),
            datetime!(2026-04-20 13:00 UTC),
            datetime!(2026-04-20 13:30 UTC),
            &[(
                datetime!(2026-04-20 13:00 UTC),
                datetime!(2026-04-20 13:30 UTC),
            )],
        )
        .await;
        // Horizon 12:20 — before the 12:30 hole — so nothing to report yet.
        let gap = first_coverage_gap(dir.path(), now, datetime!(2026-04-20 12:20 UTC))
            .await
            .unwrap();
        assert_eq!(gap, None);
    }

    #[tokio::test]
    async fn sweep_removes_only_past_retention_files() {
        let dir = tempdir().unwrap();
        let now = datetime!(2026-04-20 00:00 UTC);

        // Old: finished 10 days ago — outside a 7-day horizon.
        let old = window_name(
            datetime!(2026-04-09 00:00 UTC),
            datetime!(2026-04-10 00:00 UTC),
        );
        // Recent: finished 2 days ago — inside the horizon, kept.
        let recent = window_name(
            datetime!(2026-04-17 00:00 UTC),
            datetime!(2026-04-18 00:00 UTC),
        );
        // Future: not yet finished, kept.
        let future = window_name(
            datetime!(2026-04-20 00:00 UTC),
            datetime!(2026-04-21 00:00 UTC),
        );
        // State sidecars: never parsed as windows, so never swept.
        write_blank(&dir.path().join(&old)).await;
        write_blank(&dir.path().join(&recent)).await;
        write_blank(&dir.path().join(&future)).await;
        write_blank(&dir.path().join(".anchor")).await;
        write_blank(&dir.path().join(".durations.json")).await;

        // Far enough out that nothing here trips the forward edge.
        let unreachable = datetime!(2027-01-01 00:00 UTC);
        let swept = sweep_window(dir.path(), 7, unreachable, now).await;
        assert_eq!(swept.elapsed, 1);
        assert_eq!(swept.unreachable, 0);
        assert!(!dir.path().join(&old).exists(), "old file should be pruned");
        assert!(dir.path().join(&recent).exists(), "recent file kept");
        assert!(dir.path().join(&future).exists(), "future file kept");
        assert!(dir.path().join(".anchor").exists(), "anchor sidecar kept");
        assert!(
            dir.path().join(".durations.json").exists(),
            "duration cache kept",
        );

        // Idempotent: a second sweep removes nothing.
        assert_eq!(
            sweep_window(dir.path(), 7, unreachable, now).await.total(),
            0
        );
    }

    // The 2037 case: a run from before the generation bound left files dated
    // years out. Forward materialization can never revisit them — `highest_finish`
    // only moves ahead, so the daemon reads the window as covered and stops — so
    // the sweep is the only thing that can remove them.
    #[tokio::test]
    async fn sweep_removes_files_beyond_the_forward_window() {
        let dir = tempdir().unwrap();
        let now = datetime!(2026-08-08 00:00 UTC);
        let unreachable = datetime!(2026-08-10 00:00 UTC);

        // Inside the window: kept.
        let today = window_name(
            datetime!(2026-08-08 00:00 UTC),
            datetime!(2026-08-08 06:00 UTC),
        );
        // Straddles the edge — starts before it, so a generation could have
        // written it. Kept.
        let straddling = window_name(
            datetime!(2026-08-09 18:00 UTC),
            datetime!(2026-08-10 06:00 UTC),
        );
        // Starts at the edge, and years past it: unreachable, both pruned.
        let at_edge = window_name(
            datetime!(2026-08-10 00:00 UTC),
            datetime!(2026-08-10 06:00 UTC),
        );
        let runaway = window_name(
            datetime!(2037-06-20 17:00 UTC),
            datetime!(2037-06-20 23:00 UTC),
        );
        for name in [&today, &straddling, &at_edge, &runaway] {
            write_blank(&dir.path().join(name)).await;
        }

        let swept = sweep_window(dir.path(), 7, unreachable, now).await;
        assert_eq!(swept.unreachable, 2);
        assert_eq!(swept.elapsed, 0);
        assert!(dir.path().join(&today).exists(), "in-window file kept");
        assert!(
            dir.path().join(&straddling).exists(),
            "a file starting before the edge is reachable and kept",
        );
        assert!(!dir.path().join(&at_edge).exists(), "edge file pruned");
        assert!(!dir.path().join(&runaway).exists(), "2037 file pruned");

        // The point of sweeping first: the frontier the daemon resumes from is
        // back inside the window, so this tick generates instead of reading the
        // window as covered through 2037 and doing nothing.
        let files = scan_output_folder(dir.path()).await.unwrap();
        assert_eq!(
            highest_finish(&files),
            Some(datetime!(2026-08-10 06:00 UTC)),
        );
    }

    // Headroom is derived from how chunks are laid, not picked: the chunk
    // holding the target is reachable, so is the one after it (an item
    // straddling the boundary is emitted whole), and the one after that is not.
    #[test]
    fn unreachable_edge_sits_two_chunks_past_the_target() {
        let tz = crate::tz::parse("UTC").unwrap();
        let target = datetime!(2026-08-09 07:30 UTC);
        // Target sits in the 06:00 chunk; two 6h chunks on gives 18:00.
        assert_eq!(
            unreachable_after(target, 6, tz),
            datetime!(2026-08-09 18:00 UTC),
        );
    }

    #[tokio::test]
    async fn parses_well_formed_filenames() {
        let dir = tempdir().unwrap();
        let start = datetime!(2026-04-13 00:00 UTC);
        let finish = datetime!(2026-04-14 00:00 UTC);
        let s = start.format(&DATE_FORMAT).unwrap();
        let f = finish.format(&DATE_FORMAT).unwrap();
        let name = format!("{s}_{f}.json");
        write_blank(&dir.path().join(&name)).await;
        let files = scan_output_folder(dir.path()).await.unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].start, start);
        assert_eq!(files[0].finish, finish);
        assert_eq!(highest_finish(&files), Some(finish));
    }
}

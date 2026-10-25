//! Keeping rendered chapters and their stems under a size cap.
//!
//! The cache is every `done` job's output file plus every stem directory under
//! `<cache>/stems`. Over the cap, stems no live job refers to go first, then
//! chapters in air-time order, oldest first. Only a chapter that has finished
//! airing is removed: one that has not aired yet is needed, and one with no air
//! time is never removed, since nothing says it is safe. A chapter is also
//! protected from thirty minutes before it airs until it has finished.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use time::OffsetDateTime;

use crate::queue::{JobState, Queue, Status};
use crate::settings::Settings;

/// How long before air a chapter is off limits to eviction.
pub const PROTECT_BEFORE_AIR: Duration = Duration::from_secs(30 * 60);

fn dir_bytes(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_bytes(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

/// Every `<entry>/<region>` directory under `<cache>/stems`, as a path
/// relative to it.
fn stem_dirs(settings: &Settings) -> Vec<String> {
    let root = settings.cache_dir.join("stems");
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(&root) else {
        return out;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !entry.path().is_dir() {
            continue;
        }
        let Ok(regions) = fs::read_dir(entry.path()) else {
            continue;
        };
        for region in regions.filter_map(Result::ok) {
            let r = region.file_name().to_string_lossy().into_owned();
            if !r.starts_with('.') && region.path().is_dir() {
                out.push(format!("{name}/{r}"));
            }
        }
    }
    out.sort();
    out
}

/// Whether `job` may not be removed at `now`.
pub fn protected(job: &JobState, now: OffsetDateTime) -> bool {
    match job.air_at {
        None => true,
        Some(air) => {
            let from = air - time::Duration::try_from(PROTECT_BEFORE_AIR).expect("30 minutes fits");
            let until = air + time::Duration::seconds_f64(job.duration_s);
            now >= from && now < until
        }
    }
}

/// Whether `job`'s chapter has finished airing at `now`. A job with no air time has not.
fn has_aired(job: &JobState, now: OffsetDateTime) -> bool {
    match job.air_at {
        None => false,
        Some(air) => !protected(job, now) && now >= air,
    }
}

/// Bytes the cache holds now: every `done` chapter's file and every stem directory.
pub fn used_bytes(queue: &Queue, settings: &Settings) -> u64 {
    let stems_root = settings.cache_dir.join("stems");
    queue.list().iter().map(output_size).sum::<u64>()
        + stem_dirs(settings)
            .iter()
            .map(|d| dir_bytes(&stems_root.join(d)))
            .sum::<u64>()
}

fn output_size(job: &JobState) -> u64 {
    if job.status == Status::Done {
        fs::metadata(&job.output).map(|m| m.len()).unwrap_or(0)
    } else {
        0
    }
}

/// Bring the cache under `cap_bytes`, returning the ids of the chapters removed.
pub fn enforce_cap(
    queue: &Queue,
    settings: &Settings,
    cap_bytes: u64,
    now: OffsetDateTime,
) -> Vec<String> {
    let jobs = queue.list();
    let stems_root = settings.cache_dir.join("stems");
    let mut total = used_bytes(queue, settings);
    if total <= cap_bytes {
        return Vec::new();
    }

    let referenced = |jobs: &[JobState]| -> BTreeSet<String> {
        jobs.iter()
            .filter(|j| matches!(j.status, Status::Queued | Status::Running | Status::Done))
            .flat_map(|j| j.stems.iter().cloned())
            .collect()
    };
    let remove_stems = |dirs: Vec<String>, total: &mut u64| {
        for d in dirs {
            let path: PathBuf = stems_root.join(&d);
            let bytes = dir_bytes(&path);
            if fs::remove_dir_all(&path).is_ok() {
                *total = total.saturating_sub(bytes);
                tracing::info!(event = "render.stems_evicted", dir = %d, bytes, "removed unreferenced stems");
            }
        }
    };

    // Stems nothing refers to cost the least to lose: one re-separation.
    // A queued or running job records its stems only when it finishes, so while
    // one exists no stem directory can be called unreferenced.
    let in_flight = |jobs: &[JobState]| {
        jobs.iter()
            .any(|j| matches!(j.status, Status::Queued | Status::Running))
    };
    let orphans_of = |jobs: &[JobState]| -> Vec<String> {
        if in_flight(jobs) {
            return Vec::new();
        }
        let keep = referenced(jobs);
        stem_dirs(settings)
            .into_iter()
            .filter(|d| !keep.contains(d))
            .collect()
    };
    remove_stems(orphans_of(&jobs), &mut total);

    let mut jobs = jobs;
    let mut evicted = Vec::new();
    let mut candidates: Vec<usize> = (0..jobs.len())
        .filter(|&i| jobs[i].status == Status::Done && has_aired(&jobs[i], now))
        .collect();
    candidates.sort_by_key(|&i| jobs[i].air_at);
    for i in candidates {
        if total <= cap_bytes {
            break;
        }
        let bytes = output_size(&jobs[i]);
        if fs::remove_file(&jobs[i].output).is_err() && jobs[i].output.exists() {
            continue;
        }
        total = total.saturating_sub(bytes);
        jobs[i].status = Status::Evicted;
        jobs[i].output_bytes = None;
        let _ = queue.write(&jobs[i]);
        evicted.push(jobs[i].id.clone());
        remove_stems(orphans_of(&jobs), &mut total);
    }
    if total > cap_bytes {
        tracing::warn!(
            event = "render.over_cap",
            total_bytes = total,
            cap_bytes,
            "cache is over its cap and nothing more can be evicted: the rest is queued, running, within 30 minutes of air, or has no air time"
        );
    }
    evicted
}

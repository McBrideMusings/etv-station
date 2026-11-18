//! The queue follows the schedule: `Queue::sync` queues the plan a scheduled
//! pending file names, supersedes a queued job the schedule stopped airing, and
//! never loops on a plan that cannot be queued.

mod common;

use std::path::{Path, PathBuf};

use common::*;
use etv_render::Plan;
use etv_render::queue::{Queue, Status, SyncCounts, UnreadablePlans, Wanted};
use time::{Duration, OffsetDateTime};

/// Writes the fixture plan, retargeted at `<dir>/<name>.mp4`, to
/// `<dir>/<name>.plan.json` and returns that `Wanted` airing at `air_at`.
fn scheduled(dir: &Path, name: &str, air_at: OffsetDateTime) -> Wanted {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/chapter.plan.json");
    let mut plan = Plan::parse(&std::fs::read_to_string(fixture).unwrap()).unwrap();
    plan.output = dir.join(format!("{name}.mp4"));
    let plan_path = dir.join(format!("{name}.plan.json"));
    std::fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();
    Wanted {
        output: plan.output,
        plan: Some(plan_path),
        air_at,
    }
}

fn status_of(queue: &Queue, output: &Path) -> Vec<Status> {
    queue
        .list()
        .into_iter()
        .filter(|j| j.output == output)
        .map(|j| j.status)
        .collect()
}

fn setup() -> (impl Drop, PathBuf, Queue, OffsetDateTime) {
    let (guard, dir) = temp();
    let queue = Queue::new(dir.join("queue"));
    (guard, dir, queue, OffsetDateTime::now_utc())
}

#[test]
fn the_queue_gains_what_the_schedule_airs_and_drops_what_it_replaced() {
    let (_guard, dir, queue, now) = setup();
    let old = scheduled(&dir, "old", now + Duration::minutes(40));
    let new = scheduled(&dir, "new", now + Duration::minutes(40));
    let mut unreadable = UnreadablePlans::new();

    let first = queue.sync(std::slice::from_ref(&old), now, &mut unreadable);
    assert_eq!(first.enqueued, 1);
    assert_eq!(status_of(&queue, &old.output), [Status::Queued]);

    // A newer generation laid `new` where `old` was.
    let second = queue.sync(std::slice::from_ref(&new), now, &mut unreadable);
    assert_eq!(
        second,
        SyncCounts {
            wanted: 1,
            enqueued: 1,
            superseded: 1,
            removed: 0,
            unreadable_plans: 0,
        }
    );
    assert_eq!(status_of(&queue, &old.output), [Status::Superseded]);
    assert_eq!(status_of(&queue, &new.output), [Status::Queued]);

    // The same schedule again changes nothing.
    let third = queue.sync(std::slice::from_ref(&new), now, &mut unreadable);
    assert_eq!((third.enqueued, third.superseded), (0, 0));

    // The schedule names `old` again: it is queued again.
    let fourth = queue.sync(&[old.clone(), new.clone()], now, &mut unreadable);
    assert_eq!(fourth.enqueued, 1);
    assert_eq!(status_of(&queue, &old.output), [Status::Queued]);
}

#[test]
fn a_running_job_stays_and_a_failed_one_is_not_retried() {
    let (_guard, dir, queue, now) = setup();
    let running = scheduled(&dir, "running", now + Duration::minutes(10));
    let failed = scheduled(&dir, "failed", now + Duration::minutes(20));
    let mut unreadable = UnreadablePlans::new();
    queue.sync(&[running.clone(), failed.clone()], now, &mut unreadable);
    for mut job in queue.list() {
        job.status = if job.output == running.output {
            Status::Running
        } else {
            Status::Failed
        };
        queue.write(&job).unwrap();
    }

    let counts = queue.sync(std::slice::from_ref(&failed), now, &mut unreadable);
    assert_eq!((counts.enqueued, counts.superseded), (0, 0));
    assert_eq!(status_of(&queue, &running.output), [Status::Running]);
    assert_eq!(status_of(&queue, &failed.output), [Status::Failed]);
}

#[test]
fn a_plan_that_cannot_be_queued_is_read_again_only_after_it_changes() {
    let (_guard, dir, queue, now) = setup();
    let good = scheduled(&dir, "chapter", now + Duration::minutes(30));
    let plan_path = good.plan.clone().unwrap();
    let text = std::fs::read(&plan_path).unwrap();
    std::fs::write(&plan_path, "not a plan").unwrap();
    let mut unreadable = UnreadablePlans::new();

    let first = queue.sync(std::slice::from_ref(&good), now, &mut unreadable);
    assert_eq!((first.enqueued, first.unreadable_plans), (0, 1));
    let again = queue.sync(std::slice::from_ref(&good), now, &mut unreadable);
    assert_eq!(
        (again.enqueued, again.unreadable_plans),
        (0, 0),
        "an unchanged bad plan is not read again"
    );

    // The plugin rewrites the plan; its modification time moves on.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&plan_path, text).unwrap();
    let fixed = queue.sync(std::slice::from_ref(&good), now, &mut unreadable);
    assert_eq!((fixed.enqueued, fixed.unreadable_plans), (1, 0));
}

#[test]
fn a_plan_that_makes_another_file_is_not_queued_for_this_one() {
    let (_guard, dir, queue, now) = setup();
    let other = scheduled(&dir, "other", now + Duration::minutes(30));
    let wanted = Wanted {
        output: dir.join("this.mp4"),
        plan: other.plan.clone(),
        air_at: other.air_at,
    };
    let counts = queue.sync(&[wanted], now, &mut UnreadablePlans::new());
    assert_eq!((counts.enqueued, counts.unreadable_plans), (0, 1));
    assert!(queue.list().is_empty());
}

#[test]
fn a_pending_file_with_no_plan_keeps_its_job_and_queues_nothing() {
    let (_guard, dir, queue, now) = setup();
    let chapter = scheduled(&dir, "chapter", now + Duration::minutes(30));
    let mut unreadable = UnreadablePlans::new();
    queue.sync(std::slice::from_ref(&chapter), now, &mut unreadable);
    let planless = Wanted {
        plan: None,
        ..chapter.clone()
    };
    let counts = queue.sync(&[planless], now, &mut unreadable);
    assert_eq!((counts.enqueued, counts.superseded), (0, 0));
    assert_eq!(status_of(&queue, &chapter.output), [Status::Queued]);
}

/// A queued job whose air time has come is the sidecar being late, not the
/// schedule replacing it: sync leaves it queued for `mark_missed`.
#[test]
fn a_job_already_on_air_is_not_superseded() {
    let (_guard, dir, queue, now) = setup();
    let late = scheduled(&dir, "late", now + Duration::minutes(5));
    let mut unreadable = UnreadablePlans::new();
    queue.sync(std::slice::from_ref(&late), now, &mut unreadable);

    let after_air = now + Duration::minutes(6);
    let counts = queue.sync(&[], after_air, &mut unreadable);
    assert_eq!(counts.superseded, 0);
    assert_eq!(status_of(&queue, &late.output), [Status::Queued]);
}

/// A superseded job stays while its air time is ahead, since the schedule may
/// name it again, and is deleted with its plan once that time has passed, so
/// the queue does not grow with every generation that replaced a chapter.
#[test]
fn a_superseded_job_is_deleted_once_its_air_time_passes() {
    let (_guard, dir, queue, now) = setup();
    let old = scheduled(&dir, "old", now + Duration::minutes(40));
    let mut unreadable = UnreadablePlans::new();
    queue.sync(std::slice::from_ref(&old), now, &mut unreadable);
    queue.sync(&[], now, &mut unreadable);
    assert_eq!(status_of(&queue, &old.output), [Status::Superseded]);

    let before_air = queue.sync(&[], now + Duration::minutes(39), &mut unreadable);
    assert_eq!(before_air.removed, 0);
    assert_eq!(status_of(&queue, &old.output), [Status::Superseded]);

    let after_air = queue.sync(&[], now + Duration::minutes(41), &mut unreadable);
    assert_eq!(after_air.removed, 1);
    assert!(queue.list().is_empty());
    let left: Vec<_> = std::fs::read_dir(&queue.dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(left.is_empty(), "plan and state both gone, found {left:?}");
}

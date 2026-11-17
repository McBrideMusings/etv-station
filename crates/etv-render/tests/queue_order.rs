//! The order the sidecar takes queued jobs in: soonest air first, a job whose
//! air time has come never. No media tools involved: each plan file fails to
//! parse, so `process_next` finishes a job the moment it takes it.

mod common;

use std::cell::Cell;

use common::*;
use etv_render::queue::{JobState, Queue, Status, process_next};
use time::{Duration, OffsetDateTime};

fn queued(queue: &Queue, id: &str, air: Option<OffsetDateTime>, enqueued_at: OffsetDateTime) {
    std::fs::create_dir_all(&queue.dir).unwrap();
    std::fs::write(queue.plan_path(id), "not a plan").unwrap();
    queue
        .write(&JobState {
            id: id.to_string(),
            status: Status::Queued,
            output: queue.dir.join(format!("{id}.mp4")),
            duration_s: 100.0,
            enqueued_at,
            air_at: air,
            started_at: None,
            finished_at: None,
            failed_layer: None,
            last_error: None,
            timings: None,
            stems: vec![],
            output_bytes: None,
            attempts: 0,
        })
        .unwrap();
}

#[test]
fn the_sidecar_takes_the_soonest_airing_job_and_never_one_already_aired() {
    let (_guard, dir) = temp();
    let queue = Queue::new(dir.join("queue"));
    let settings = settings(&dir);
    let now = OffsetDateTime::now_utc();
    queued(
        &queue,
        "tomorrow",
        Some(now + Duration::hours(31)),
        now - Duration::hours(5),
    );
    queued(
        &queue,
        "soon",
        Some(now + Duration::hours(1)),
        now - Duration::hours(4),
    );
    queued(
        &queue,
        "aired",
        Some(now - Duration::hours(1)),
        now - Duration::hours(3),
    );
    queued(&queue, "whenever", None, now - Duration::hours(6));

    let tick = Cell::new(0);
    let clock = || {
        tick.set(tick.get() + 1);
        now + Duration::milliseconds(tick.get())
    };
    let mut order = Vec::new();
    while let Some(job) = process_next(&queue, &settings, u64::MAX, clock) {
        order.push(job.id);
    }

    assert_eq!(order, ["soon", "tomorrow", "whenever"]);
    let aired = queue.read("aired").unwrap();
    assert_eq!(aired.status, Status::Missed);
    assert_eq!(aired.attempts, 0);
}

#[test]
fn a_plan_enqueued_after_its_air_time_is_missed_until_given_a_later_one() {
    let (_guard, dir) = temp();
    let queue = Queue::new(dir.join("queue"));
    let plan = fixture_plan(&dir);
    let now = OffsetDateTime::now_utc();

    let late = queue
        .enqueue(&plan, Some(now - Duration::minutes(5)), now)
        .unwrap();
    assert_eq!(late.status, Status::Missed);

    let again = queue
        .enqueue(&plan, Some(now - Duration::minutes(5)), now)
        .unwrap();
    assert_eq!(again, late);

    let later = queue
        .enqueue(&plan, Some(now + Duration::hours(2)), now)
        .unwrap();
    assert_eq!(later.status, Status::Queued);
}

//! Cache eviction at the queue seam: no media tools involved.

mod common;

use common::*;
use etv_render::cache::enforce_cap;
use etv_render::queue::{JobState, Queue, Status};
use time::{Duration, OffsetDateTime};

const KB: usize = 1024;

fn done_job(
    queue: &Queue,
    dir: &std::path::Path,
    name: &str,
    air: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> JobState {
    let output = dir.join(format!("{name}.mp4"));
    std::fs::write(&output, vec![0u8; 10 * KB]).unwrap();
    let job = JobState {
        id: name.to_string(),
        status: Status::Done,
        output,
        duration_s: 100.0,
        enqueued_at: now,
        air_at: air,
        started_at: None,
        finished_at: None,
        failed_layer: None,
        last_error: None,
        timings: None,
        stems: vec![],
        output_bytes: Some(10 * KB as u64),
        attempts: 1,
    };
    queue.write(&job).unwrap();
    job
}

#[test]
fn over_the_cap_the_oldest_aired_chapter_goes_and_one_near_air_stays() {
    let (_guard, dir) = temp();
    let settings = settings(&dir);
    let queue = Queue::new(dir.join("queue"));
    std::fs::create_dir_all(&queue.dir).unwrap();
    let now = OffsetDateTime::now_utc();

    let aired_long_ago = done_job(
        &queue,
        &dir,
        "aired-long-ago",
        Some(now - Duration::hours(5)),
        now,
    );
    let aired_recently = done_job(
        &queue,
        &dir,
        "aired-recently",
        Some(now - Duration::hours(1)),
        now,
    );
    let airing_now = done_job(
        &queue,
        &dir,
        "airing-now",
        Some(now - Duration::seconds(30)),
        now,
    );
    let in_ten_minutes = done_job(
        &queue,
        &dir,
        "in-ten-minutes",
        Some(now + Duration::minutes(10)),
        now,
    );
    let tonight = done_job(&queue, &dir, "tonight", Some(now + Duration::hours(6)), now);

    // Five 10 KB chapters against a 45 KB cap: one must go.
    let evicted = enforce_cap(&queue, &settings, 45 * KB as u64, now);
    assert_eq!(evicted, vec!["aired-long-ago".to_string()]);
    assert!(!aired_long_ago.output.exists());
    assert_eq!(
        queue.read("aired-long-ago").unwrap().status,
        Status::Evicted
    );

    // A cap that needs three gone takes the next aired chapter, and never one
    // that is airing now, within 30 minutes of air, or has not aired yet.
    let evicted = enforce_cap(&queue, &settings, 20 * KB as u64, now);
    assert_eq!(evicted, vec!["aired-recently".to_string()]);
    assert!(!aired_recently.output.exists());
    assert!(
        airing_now.output.exists(),
        "a chapter on air is never removed"
    );
    assert!(
        in_ten_minutes.output.exists(),
        "a chapter within 30 minutes of air is never removed"
    );
    assert!(
        tonight.output.exists(),
        "a chapter that has not aired yet is never removed"
    );
}

#[test]
fn a_cap_nothing_can_satisfy_removes_only_what_is_allowed() {
    let (_guard, dir) = temp();
    let settings = settings(&dir);
    let queue = Queue::new(dir.join("queue"));
    std::fs::create_dir_all(&queue.dir).unwrap();
    let now = OffsetDateTime::now_utc();
    let soon = done_job(&queue, &dir, "soon", Some(now + Duration::minutes(5)), now);
    let unknown = done_job(&queue, &dir, "no-air-time", None, now);

    let evicted = enforce_cap(&queue, &settings, 1, now);

    assert!(evicted.is_empty());
    assert!(soon.output.exists() && unknown.output.exists());
}

#[test]
fn stems_no_live_job_refers_to_go_before_any_chapter() {
    let (_guard, dir) = temp();
    let settings = settings(&dir);
    let queue = Queue::new(dir.join("queue"));
    std::fs::create_dir_all(&queue.dir).unwrap();
    let now = OffsetDateTime::now_utc();
    let old = done_job(&queue, &dir, "old", Some(now - Duration::hours(3)), now);
    let orphan = settings.cache_dir.join("stems/song/1000-2000");
    std::fs::create_dir_all(&orphan).unwrap();
    std::fs::write(orphan.join("vocals.wav"), vec![0u8; 8 * KB]).unwrap();

    // 18 KB total against a 12 KB cap: dropping the 8 KB orphan is enough.
    let evicted = enforce_cap(&queue, &settings, 12 * KB as u64, now);

    assert!(evicted.is_empty());
    assert!(!orphan.exists());
    assert!(old.output.exists());
}

#[test]
fn enqueue_returns_at_once_and_the_same_plan_is_one_job() {
    let (_guard, dir) = temp();
    let queue = Queue::new(dir.join("queue"));
    let plan = etv_render::Document::from(fixture_plan(&dir));
    let now = OffsetDateTime::now_utc();

    let first = queue.enqueue(&plan, None, now).unwrap();
    let again = queue
        .enqueue(&plan, Some(now + Duration::hours(1)), now)
        .unwrap();

    assert_eq!(first.id, again.id);
    assert_eq!(first.status, Status::Queued);
    assert_eq!(queue.list().len(), 1);
    assert_eq!(
        queue.read(&first.id).unwrap().air_at,
        Some(now + Duration::hours(1))
    );
}

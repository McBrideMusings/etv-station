//! The renderer at its seam: a plan plus pre-separated stems in, a file out.

mod common;

use common::*;
use etv_render::queue::{Queue, Status, process_next};
use etv_render::render::render;
use time::OffsetDateTime;

#[test]
fn a_fixture_plan_renders_to_its_planned_length_with_a_steady_beat_across_the_seam() {
    let (_guard, dir) = temp();
    make_source_video(&dir.join("source.mp4"));
    let settings = settings(&dir);
    let plan = fixture_plan(&dir);
    for hook in &plan.hooks {
        seed_stems(&settings, hook);
    }

    render(&plan, &settings).unwrap_or_else(|e| panic!("render failed: {e}"));

    // 10 bars x 2 s.
    let length = duration_s(&plan.output);
    assert!(
        (length - 20.0).abs() < 0.05,
        "chapter lasts {length}s, planned 20s"
    );
    assert!(
        !plan.output.with_extension("part").exists(),
        "the partial file is gone"
    );

    // The vocal-over seam is bars 4 and 5: 8 s to 12 s. One beat either side.
    let beats = onsets(&decode_mono(&plan.output));
    let seam: Vec<f64> = beats
        .iter()
        .copied()
        .filter(|t| (7.5..12.5).contains(t))
        .collect();
    assert!(
        seam.len() >= 8,
        "found {} beats near the seam: {beats:?}",
        seam.len()
    );
    let intervals: Vec<f64> = seam.windows(2).map(|w| w[1] - w[0]).collect();
    let spread = stdev(&intervals);
    assert!(
        spread < 0.015,
        "beat intervals across the seam have stdev {:.1} ms: {intervals:?}",
        spread * 1000.0
    );
    // And the beat is the chapter's 120 bpm.
    let mean = intervals.iter().sum::<f64>() / intervals.len() as f64;
    assert!((mean - 0.5).abs() < 0.005, "mean beat interval {mean}");
}

#[test]
fn the_render_is_1080p() {
    let (_guard, dir) = temp();
    make_source_video(&dir.join("source.mp4"));
    let settings = settings(&dir);
    let plan = fixture_plan(&dir);
    for hook in &plan.hooks {
        seed_stems(&settings, hook);
    }
    render(&plan, &settings).unwrap();
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0",
        ])
        .arg(&plan.output)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "1920,1080");
}

#[test]
fn a_plan_naming_a_missing_file_fails_with_that_path_in_the_job_state() {
    let (_guard, dir) = temp();
    let settings = settings(&dir);
    std::fs::write(dir.join("source.mp4"), b"").unwrap();
    let mut plan = fixture_plan(&dir);
    plan.hooks[1].path = dir.join("gone/song-b.mp4");
    let queue = Queue::new(dir.join("queue"));
    queue
        .enqueue(&plan.into(), None, OffsetDateTime::now_utc())
        .unwrap();

    let job = process_next(&queue, &settings, u64::MAX, OffsetDateTime::now_utc).unwrap();

    let missing = dir.join("gone/song-b.mp4").display().to_string();
    assert_eq!(job.status, Status::Failed);
    assert!(
        job.last_error.as_deref().unwrap().contains(&missing),
        "{:?}",
        job.last_error
    );
    assert_eq!(job.failed_layer.as_deref(), Some("hook 2 (fixture-b)"));
    let stored = queue.read(&job.id).unwrap();
    assert!(stored.last_error.unwrap().contains(&missing));
}

#[test]
fn with_demucs_absent_the_render_fails_naming_it() {
    let (_guard, dir) = temp();
    make_source_video(&dir.join("source.mp4"));
    let mut settings = settings(&dir);
    settings.python = dir.join("no-such-python").display().to_string();
    let plan = fixture_plan(&dir);

    let err = render(&plan, &settings).unwrap_err();

    assert_eq!(err.layer, "dependency demucs");
    assert!(err.message.contains("demucs is missing"), "{err}");
}

#[test]
fn a_song_that_ends_inside_its_hook_is_refused() {
    let (_guard, dir) = temp();
    make_source_video(&dir.join("source.mp4"));
    let settings = settings(&dir);
    let plan = fixture_plan(&dir);
    for hook in &plan.hooks {
        seed_stems(&settings, hook);
    }
    // Replace hook b's stems with 10 s of audio where 24 s are needed.
    let dir_b = etv_render::stems::stem_dir(&settings, &plan.hooks[1]);
    write_click_wav(&dir_b.join("vocals.wav"), 130.0, 10.0, 1500.0);

    let err = render(&plan, &settings).unwrap_err();

    assert_eq!(err.layer, "hook 2 (fixture-b) stems");
    assert!(
        err.message.contains("the song ends inside the hook"),
        "{err}"
    );
}

//! The renderer at its seam for a set plan: whole songs in, one file out.

mod common;

use std::path::Path;
use std::process::Command;

use common::*;
use etv_render::Document;
use etv_render::queue::{Queue, Status, process_next};
use etv_render::set::{Recipe, Seam, SetKind, SetPlan, Track};
use time::OffsetDateTime;

/// A 40-second song at 120 bpm (2 s bars): a click on every beat and a test picture.
fn make_song(path: &Path, freq: &str) {
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=25",
        ])
        .args(["-f", "lavfi", "-i"])
        .arg(format!("sine=frequency={freq}:sample_rate=44100"))
        .args(["-t", "40", "-pix_fmt", "yuv420p", "-c:a", "aac"])
        .arg(path)
        .status()
        .expect("ffmpeg must be installed to run the renderer tests");
    assert!(status.success());
}

fn plan(dir: &Path, recipe: Recipe, bars: u32, b_enter: f64) -> SetPlan {
    for (name, freq) in [("a", "300"), ("b", "500")] {
        make_song(&dir.join(format!("{name}.mp4")), freq);
    }
    let track = |id: &str, enter_s: f64| Track {
        entry_id: id.into(),
        path: dir.join(format!("{id}.mp4")),
        native_bpm: 120.0,
        enter_s,
        ratio: 1.0,
        ride_bars: 0,
    };
    SetPlan {
        kind: SetKind::Set,
        tracks: vec![track("a", 4.0), track("b", b_enter)],
        seams: vec![Seam {
            recipe,
            out_s: 20.0,
            bars,
            sweep: false,
        }],
        end_s: 30.0,
        cards: vec![],
        output: dir.join("out/set.mp4"),
    }
}

fn has_audio_and_video(path: &Path) -> (bool, bool) {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    (text.contains("audio"), text.contains("video"))
}

#[test]
fn every_recipe_renders_to_the_planned_length_with_picture_and_sound() {
    for (recipe, bars, b_enter) in [
        (Recipe::BassSwap, 2, 8.0),
        (Recipe::EchoOut, 0, 0.0),
        (Recipe::DropMix, 0, 0.0),
    ] {
        let (_guard, dir) = temp();
        let plan = plan(&dir, recipe, bars, b_enter);
        let settings = settings(&dir);
        let planned = plan.duration_s();
        etv_render::set_render::render(&plan, &settings)
            .unwrap_or_else(|e| panic!("{recipe:?} failed: {e}"));
        let length = duration_s(&plan.output);
        assert!(
            (length - planned).abs() < 0.1,
            "{recipe:?}: rendered {length}s, planned {planned}s"
        );
        assert_eq!(
            has_audio_and_video(&plan.output),
            (true, true),
            "{recipe:?}"
        );
        assert!(!plan.output.with_extension("part").exists());
    }
}

#[test]
fn a_song_that_ends_inside_its_seam_fails_naming_the_song() {
    let (_guard, dir) = temp();
    let mut plan = plan(&dir, Recipe::EchoOut, 0, 0.0);
    // The echo rings two bars (4 s) past out_s, beyond the song's 40 s.
    plan.seams[0].out_s = 38.0;
    let err = etv_render::set_render::render(&plan, &settings(&dir)).unwrap_err();
    assert!(err.to_string().contains("track 1 (a)"), "{err}");
}

#[test]
fn a_queued_set_renders_through_the_sidecar_path() {
    let (_guard, dir) = temp();
    let plan = plan(&dir, Recipe::DropMix, 0, 0.0);
    let settings = settings(&dir);
    let queue = Queue::new(dir.join("queue"));
    let doc = Document::from(plan.clone());
    let job = queue
        .enqueue(&doc, None, OffsetDateTime::now_utc())
        .unwrap();
    assert!((job.duration_s - plan.duration_s()).abs() < 1e-9);
    let done = process_next(&queue, &settings, u64::MAX, OffsetDateTime::now_utc).unwrap();
    assert_eq!(done.status, Status::Done, "{:?}", done.last_error);
    assert!(plan.output.exists());
}

#[test]
fn a_song_riding_back_to_its_own_tempo_renders_to_the_planned_length() {
    let (_guard, dir) = temp();
    let mut plan = plan(&dir, Recipe::BassSwap, 2, 8.0);
    plan.tracks[1].native_bpm = 108.0;
    plan.tracks[1].ratio = 120.0 / 108.0;
    plan.tracks[1].ride_bars = 2;
    let planned = plan.duration_s();
    etv_render::set_render::render(&plan, &settings(&dir)).unwrap();
    let length = duration_s(&plan.output);
    assert!(
        (length - planned).abs() < 0.1,
        "rendered {length}s, planned {planned}s"
    );
    assert_eq!(has_audio_and_video(&plan.output), (true, true));
}

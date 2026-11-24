//! The renderer at its seam for whole songs read unseparated: full-source
//! layers with a tempo ride, filters and fades in, one file out.

mod common;

use std::path::Path;
use std::process::Command;

use common::*;
use etv_render::plan::{Curve, Cut, Fade, FadeKind, Filter, Layer, Plan, Ride, Source, Stem};
use etv_render::queue::{Queue, Status, process_next};
use time::OffsetDateTime;

/// A 40-second song: a steady tone and a test picture.
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

fn layer(name: &str, source: &str, src: f64, out: f64, len: f64) -> Layer {
    Layer {
        name: name.into(),
        source: source.into(),
        stem: Stem::Full,
        src_start_s: src,
        out_start_s: out,
        out_len_s: len,
        ratio: 1.0,
        ride: None,
        pitch: 1.0,
        level_db: None,
        gain_db: 0.0,
        fades: vec![],
        filters: vec![],
    }
}

/// `a` plays 0-20 s; `b` enters at 16 s riding from 0.9 to its own speed,
/// filtered and faded in, and plays to 30 s; `a` rings out in an echo.
fn plan(dir: &Path) -> Plan {
    for (name, freq) in [("a", "300"), ("b", "500")] {
        make_song(&dir.join(format!("{name}.mp4")), freq);
    }
    let mut a_body = layer("a body", "a", 4.0, 0.0, 18.0);
    a_body.fades.push(Fade {
        kind: FadeKind::Out,
        start_s: 16.0,
        len_s: 2.0,
        curve: Curve::EqualPower,
    });
    let mut a_echo = layer("a echo", "a", 22.0, 18.0, 2.0);
    a_echo.filters.push(Filter::Echo {
        in_gain: 0.8,
        out_gain: 0.7,
        delay_ms: 375.0,
        decay: 0.55,
    });
    let mut b = layer("b", "b", 8.0, 16.0, 14.0);
    b.ride = Some(Ride {
        step_src_s: 2.0,
        ratios: vec![0.9, 0.95, 1.0],
    });
    b.filters.push(Filter::Highpass { hz: 250.0 });
    b.fades.push(Fade {
        kind: FadeKind::In,
        start_s: 0.0,
        len_s: 2.0,
        curve: Curve::EqualPower,
    });
    Plan {
        sources: vec![
            Source {
                id: "a".into(),
                path: dir.join("a.mp4"),
                separate: None,
            },
            Source {
                id: "b".into(),
                path: dir.join("b.mp4"),
                separate: None,
            },
        ],
        layers: vec![a_body, a_echo, b],
        cuts: vec![
            Cut {
                source: "a".into(),
                src_start_s: 4.0,
                out_start_s: 0.0,
                out_len_s: 16.0,
                ratio: 1.0,
            },
            Cut {
                source: "b".into(),
                src_start_s: 8.0,
                out_start_s: 16.0,
                out_len_s: 14.0,
                ratio: 1.0,
            },
        ],
        duration_s: 30.0,
        cards: vec![],
        output: dir.join("out/whole.mp4"),
        notes: serde_json::Value::Null,
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
fn full_layers_with_a_ride_filters_and_fades_render_to_the_planned_length() {
    let (_guard, dir) = temp();
    let plan = plan(&dir);
    etv_render::render::render(&plan, &settings(&dir)).unwrap_or_else(|e| panic!("{e}"));
    let length = duration_s(&plan.output);
    assert!(
        (length - 30.0).abs() < 0.1,
        "rendered {length}s, planned 30s"
    );
    assert_eq!(has_audio_and_video(&plan.output), (true, true));
    assert!(!plan.output.with_extension("part").exists());
}

#[test]
fn a_source_that_ends_inside_a_layer_fails_naming_the_layer() {
    let (_guard, dir) = temp();
    let mut plan = plan(&dir);
    // a's echo now reads from 39 s: past the end of a 40 s song.
    plan.layers[1].src_start_s = 39.0;
    let err = etv_render::render::render(&plan, &settings(&dir)).unwrap_err();
    assert_eq!(err.layer, "a echo");
    assert!(
        err.message.contains("the source ends inside the layer"),
        "{err}"
    );
}

#[test]
fn a_queued_plan_renders_through_the_sidecar_path() {
    let (_guard, dir) = temp();
    let plan = plan(&dir);
    let settings = settings(&dir);
    let queue = Queue::new(dir.join("queue"));
    let job = queue
        .enqueue(&plan, None, OffsetDateTime::now_utc())
        .unwrap();
    assert!((job.duration_s - 30.0).abs() < 1e-9);
    let done = process_next(&queue, &settings, u64::MAX, None, OffsetDateTime::now_utc).unwrap();
    assert_eq!(done.status, Status::Done, "{:?}", done.last_error);
    assert!(plan.output.exists());
}

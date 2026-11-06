//! Acceptance tests for the generated-ident bumper
//! (`examples/plugins/bumpers/generated_ident.rhai`): it derives render
//! parameters, reuses a finished render it finds by its `lg_params` tag, and
//! otherwise submits to a broker and airs a stand-in as a pending segment.
//!
//! The broker is `examples/plugins/bumpers/test/stub_broker.sh`, which lands a
//! clip carrying ComfyUI-style tags after a delay; no ComfyUI runs here.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use etv_station::bumper::hook::{BumperInput, ChannelInfo, build};
use etv_station::bumper::timeline::SegmentSource;
use serde_json::json;

fn plugin_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/plugins/bumpers/generated_ident.rhai")
}

fn stub_broker() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/plugins/bumpers/test/stub_broker.sh")
        .canonicalize()
        .unwrap()
}

struct Fixture {
    dir: tempfile::TempDir,
    stand_in: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let stand_in = dir.path().join("stand-in.mp4");
        let ok = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=320x180:r=25",
                "-t",
                "2",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&stand_in)
            .status()
            .unwrap()
            .success();
        assert!(ok, "ffmpeg made the stand-in");
        Fixture { dir, stand_in }
    }

    fn input(&self, extra: serde_json::Value, granted: &[&str]) -> BumperInput {
        let mut config = json!({
            "dir": self.dir.path(),
            "prompts": ["slow drifting colour fields"],
            "variants": 1,
            "stand_in": self.stand_in,
            "broker": stub_broker(),
            "broker_args": ["0"],
            "seconds": 2,
        });
        for (k, v) in extra.as_object().unwrap() {
            config[k] = v.clone();
        }
        BumperInput {
            gap_ms: 30_000,
            next: None,
            prev: None,
            upcoming: Vec::new(),
            pass: Vec::new(),
            config: Some(config),
            channel: ChannelInfo {
                name: "idents".into(),
                number: 7,
                logo: None,
                vocabulary: Vec::new(),
                tz: None,
            },
            air_date: "2027-01-15".into(),
            draw: 0,
            granted: granted.iter().map(|s| s.to_string()).collect(),
            history: None,
            catalog_path: None,
        }
    }

    fn renders(&self) -> Vec<PathBuf> {
        let dir = self.dir.path().join("idents");
        let Ok(rd) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        rd.map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "mp4"))
            .collect()
    }

    fn pending_markers(&self) -> Vec<PathBuf> {
        let dir = self.dir.path().join("idents");
        let Ok(rd) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        rd.map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "pending"))
            .collect()
    }

    fn wait_for_render(&self) {
        let until = Instant::now() + Duration::from_secs(20);
        while self
            .renders()
            .iter()
            .all(|p| p.to_string_lossy().contains(".part"))
        {
            assert!(
                Instant::now() < until,
                "the stub broker never landed a render"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

const BOTH: &[&str] = &["probe", "exec"];

#[test]
fn first_gap_airs_the_stand_in_pending_then_the_landed_render_airs() {
    let f = Fixture::new();
    let first = build(&plugin_path(), &f.input(json!({}), BOTH)).unwrap();
    let seg = &first.timeline.segments[0];
    let SegmentSource::Pending { path, stand_in } = &seg.source else {
        panic!("expected pending, got {:?}", seg.source);
    };
    assert_eq!(stand_in, &f.stand_in);
    assert_eq!(
        f.pending_markers().len(),
        1,
        "a .pending marker beside the render path"
    );

    f.wait_for_render();
    assert!(path.exists());
    let second = build(&plugin_path(), &f.input(json!({}), BOTH)).unwrap();
    let SegmentSource::Render { path: aired, .. } = &second.timeline.segments[0].source else {
        panic!(
            "expected the render, got {:?}",
            second.timeline.segments[0].source
        );
    };
    assert_eq!(aired, path);
}

#[test]
fn a_matching_render_is_reused_without_calling_exec() {
    let f = Fixture::new();
    build(&plugin_path(), &f.input(json!({}), BOTH)).unwrap();
    f.wait_for_render();
    // Only probe is granted: any ctx.exec would error and fall to the stand-in.
    let built = build(&plugin_path(), &f.input(json!({}), &["probe"])).unwrap();
    let SegmentSource::Render { path, .. } = &built.timeline.segments[0].source else {
        panic!(
            "expected the render, got {:?}",
            built.timeline.segments[0].source
        );
    };
    assert_ne!(path, &f.stand_in, "the render, not the stand-in");
    assert!(path.starts_with(f.dir.path().join("idents")));
}

#[test]
fn a_render_from_other_params_is_not_reused() {
    let f = Fixture::new();
    build(&plugin_path(), &f.input(json!({}), BOTH)).unwrap();
    f.wait_for_render();
    // A new epoch derives a new seed, so a new path and a new submit.
    let built = build(&plugin_path(), &f.input(json!({ "epoch": 1 }), BOTH)).unwrap();
    assert!(matches!(
        built.timeline.segments[0].source,
        SegmentSource::Pending { .. }
    ));
}

#[test]
fn a_pending_marker_over_an_hour_old_is_abandoned_and_resubmitted() {
    let f = Fixture::new();
    // The broker is slow, so the marker is the only trace of the first submit.
    let slow = json!({ "broker_args": ["30"] });
    build(&plugin_path(), &f.input(slow.clone(), BOTH)).unwrap();
    let marker = f.pending_markers().pop().expect("marker");

    // A fresh marker: no second submit, still pending.
    std::fs::write(&marker, "marked").unwrap();
    let fresh = build(&plugin_path(), &f.input(slow.clone(), BOTH)).unwrap();
    assert!(matches!(
        fresh.timeline.segments[0].source,
        SegmentSource::Pending { .. }
    ));
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "marked",
        "fresh marker untouched"
    );

    // Age it 2 hours: abandoned, and the submit rewrites the marker.
    let ok = Command::new("touch")
        .args(["-t", "202001010000"])
        .arg(&marker)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let stale = build(&plugin_path(), &f.input(slow, BOTH)).unwrap();
    assert!(matches!(
        stale.timeline.segments[0].source,
        SegmentSource::Pending { .. }
    ));
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "submitted",
        "a new submit rewrote the marker"
    );
}

#[test]
fn with_exec_ungranted_and_no_render_the_stand_in_airs() {
    let f = Fixture::new();
    let built = build(&plugin_path(), &f.input(json!({}), &["probe"])).unwrap();
    let seg = &built.timeline.segments[0];
    let SegmentSource::Render { path, .. } = &seg.source else {
        panic!(
            "expected the stand-in as a plain render, got {:?}",
            seg.source
        );
    };
    assert_eq!(path, &f.stand_in);
    assert!(f.pending_markers().is_empty());
}

#[test]
fn render_audio_is_muted_by_default_and_a_bed_when_asked() {
    let f = Fixture::new();
    build(&plugin_path(), &f.input(json!({}), BOTH)).unwrap();
    f.wait_for_render();
    let muted = build(&plugin_path(), &f.input(json!({}), BOTH)).unwrap();
    let bed = build(&plugin_path(), &f.input(json!({ "audio": "bed" }), BOTH)).unwrap();
    assert_eq!(muted.usage.get("mute"), Some(&1));
    assert_eq!(bed.usage.get("bed_audio"), Some(&1));
    // "own" on a clip with no audio stream still mutes.
    let own = build(&plugin_path(), &f.input(json!({ "audio": "own" }), BOTH)).unwrap();
    assert_eq!(own.usage.get("mute"), Some(&1));
    assert_eq!(own.usage.get("own_audio"), None);
}

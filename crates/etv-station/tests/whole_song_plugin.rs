//! Acceptance tests for the whole-song bumper (`examples/plugins/bumpers/whole_song.rhai`):
//! it reads audio facts through `ctx.catalog`, walks a set of whole songs, picks a
//! seam recipe per pair by rule, writes a set plan the renderer accepts, queues it
//! and airs it as a pending segment.
//!
//! The catalog and facts are written fresh by this file; `enqueue` and `ffmpeg`
//! are shell stubs, so the test proves the plugin's plan and arguments rather than
//! a real render.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use etv_render::Document;
use etv_render::set::Recipe;
use etv_station::bumper::hook::{BumperInput, ChannelInfo, build};
use etv_station::bumper::timeline::SegmentSource;
use etv_station::catalog::{Catalog, Entry, EntrySource, Source};
use rhai::Map;
use serde_json::json;

const PLUGIN: &str = "../../examples/plugins/bumpers/whole_song.rhai";
const SONG_S: f64 = 240.0;

fn plugin_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(PLUGIN)
}

/// A song's analyzer row: a steady grid from 0.3 s, `energy` per bar for every bar
/// of a 240 s file, landing on bar 16.
fn facts(bpm: f64, confidence: f64, energy: impl Fn(usize) -> f64) -> serde_json::Value {
    let bar = 240.0 / bpm;
    let bars = ((SONG_S - 0.3) / bar) as usize;
    json!({
        "bpm": bpm,
        "beat_grid": { "slope": 60.0 / bpm, "intercept": 0.3, "start_s": 0.3, "end_s": SONG_S - 1.0,
                       "mean_residual_s": 0.004, "beats": (bars * 4) as i64 },
        "downbeat_phase": 0,
        "landing_bar": 16,
        "landing_bar_s": 16.0 * bar + 0.3,
        "key": "A", "scale": "minor", "camelot": "8A", "key_strength": 0.8,
        "beat_confidence": confidence,
        "bar_energy": (0..bars).map(energy).collect::<Vec<_>>(),
        "rms_mean": 0.1,
    })
}

struct Song {
    id: &'static str,
    bpm: f64,
    confidence: f64,
    /// Energy of the bars before the landing bar, then from it on.
    intro: f64,
    body: f64,
}

fn flat(id: &'static str, bpm: f64) -> Song {
    Song {
        id,
        bpm,
        confidence: 4.0,
        intro: 0.04,
        body: 0.04,
    }
}

fn catalog_file(dir: &Path, songs: &[Song]) -> PathBuf {
    let path = dir.join("catalog.db");
    let cat = Catalog::open(&path).unwrap();
    for s in songs {
        let mut e = Entry::new(s.id, "movie", format!("Song {}", s.id), Source::LocalFs);
        e.show = Some(format!("Artist {}", s.id));
        e.year = Some(1999);
        e.duration_ms = Some((SONG_S * 1000.0) as i64);
        e.library = Some("Music Videos".into());
        cat.upsert_entry(&e).unwrap();
        let playback = format!("/media/{}.mp4", s.id);
        cat.add_source(&EntrySource {
            source: Source::LocalFs,
            source_id: playback.clone(),
            entry_id: s.id.into(),
            playback_path: playback.clone(),
            last_seen: None,
            missing_since: None,
            unairable_reason: None,
        })
        .unwrap();
        let (intro, body) = (s.intro, s.body);
        cat.set_audio_fact(
            s.id,
            &playback,
            "2026-10-25T00:00:00Z",
            Ok(&facts(s.bpm, s.confidence, |i| {
                if i < 16 { intro } else { body }
            })),
        )
        .unwrap();
    }
    path
}

fn stub(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A script just written is "text file busy" if another test forks while its
/// write handle is open, so the tests take turns.
static STUBS: Mutex<()> = Mutex::new(());

struct Fixture {
    dir: tempfile::TempDir,
    work: PathBuf,
    _turn: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new() -> Self {
        let turn = STUBS.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("sets");
        std::fs::create_dir_all(&work).unwrap();
        Fixture {
            dir,
            work,
            _turn: turn,
        }
    }

    fn input(&self, songs: &[Song], entry: &str, gap_ms: u64) -> BumperInput {
        let enqueue = stub(self.dir.path(), "enqueue", "echo job-9");
        let ffmpeg = stub(
            self.dir.path(),
            "ffmpeg",
            "for a; do last=$a; done\n: > \"$last\"",
        );
        let mut next = Map::new();
        next.insert("entry_id".into(), "mv-next".to_string().into());
        next.insert("start".into(), 1_800_000_000_i64.into());
        BumperInput {
            gap_ms,
            next: Some(next),
            prev: None,
            upcoming: Vec::new(),
            pass: Vec::new(),
            config: Some(json!({
                "expr": "item.library == \"Music Videos\"",
                "dir": self.work,
                "enqueue": enqueue,
                "ffmpeg": ffmpeg,
                "entry": entry,
            })),
            channel: ChannelInfo {
                name: "songs".into(),
                number: 78,
                logo: None,
                vocabulary: Vec::new(),
                tz: None,
            },
            air_date: "2027-01-15".into(),
            draw: 0,
            granted: vec!["probe".into(), "catalog_read".into(), "exec".into()],
            history: None,
            catalog_path: Some(catalog_file(self.dir.path(), songs)),
        }
    }

    fn plan(&self) -> etv_render::set::SetPlan {
        let mut found: Vec<PathBuf> = std::fs::read_dir(&self.work)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().ends_with(".plan.json"))
            .collect();
        assert_eq!(found.len(), 1, "one plan beside the render path");
        found.sort();
        match Document::parse(&std::fs::read_to_string(&found[0]).unwrap())
            .expect("the renderer accepts the plan")
        {
            Document::Set(plan) => plan,
            Document::Chapter(_) => panic!("a chapter plan, not a set plan"),
        }
    }
}

fn four_songs() -> Vec<Song> {
    vec![
        flat("s-a", 118.0),
        flat("s-b", 120.0),
        flat("s-c", 122.0),
        flat("s-d", 121.0),
    ]
}

#[test]
fn a_set_plan_fills_the_gap_with_songs_entering_at_their_landing_bars() {
    let f = Fixture::new();
    let built = build(&plugin_path(), &f.input(&four_songs(), "landing", 600_000)).unwrap();
    let plan = f.plan();

    assert!(plan.tracks.len() >= 2, "{} songs", plan.tracks.len());
    assert_eq!(plan.seams.len(), plan.tracks.len() - 1);
    for track in &plan.tracks {
        let bar = 240.0 / track.native_bpm;
        assert!(
            (track.enter_s - (16.0 * bar + 0.3)).abs() < 1e-6,
            "{} enters at {}s, not its landing bar",
            track.entry_id,
            track.enter_s
        );
    }
    // Flat energy and close tempos: every seam is a bass swap, and a swap that
    // arrives at the song's peak is two bars with a sweep.
    for seam in &plan.seams {
        assert_eq!(seam.recipe, Recipe::BassSwap, "{seam:?}");
        assert_eq!((seam.bars, seam.sweep), (2, true));
    }
    assert!(plan.duration_s() <= 600.0 + 1e-6, "{}s", plan.duration_s());
    assert!(
        plan.duration_s() > 590.0,
        "the set fills the gap: {}s",
        plan.duration_s()
    );
    assert_eq!(plan.cards.len(), plan.tracks.len());

    let segment = &built.timeline.segments[0];
    assert!(
        segment.name.starts_with("chapter:job-9|"),
        "{}",
        segment.name
    );
    let SegmentSource::Pending { path, stand_in } = &segment.source else {
        panic!("expected a pending segment, got {:?}", segment.source);
    };
    assert_eq!(path, &plan.output);
    assert!(stand_in.exists());
    let etv_station::bumper::timeline::Duration::Fixed(aired_ms) = segment.duration else {
        panic!("expected a fixed slot, got {:?}", segment.duration);
    };
    assert!(
        (aired_ms as f64 - plan.duration_s() * 1000.0).abs() < 2.0,
        "the slot is {aired_ms} ms, the plan lasts {} s",
        plan.duration_s()
    );
}

#[test]
fn songs_far_apart_in_tempo_join_with_an_echo_out() {
    let f = Fixture::new();
    let songs = vec![flat("s-slow", 100.0), flat("s-fast", 120.0)];
    build(&plugin_path(), &f.input(&songs, "landing", 900_000)).unwrap();
    let plan = f.plan();
    assert_eq!(plan.seams.len(), 1);
    assert_eq!(plan.seams[0].recipe, Recipe::EchoOut);
    assert_eq!(plan.tracks[1].ratio, 1.0);
}

#[test]
fn an_arrival_louder_than_the_exit_is_a_drop_mix() {
    let f = Fixture::new();
    let loud = |id| Song {
        intro: 0.01,
        body: 0.01,
        ..flat(id, 120.0)
    };
    let mut songs = vec![loud("s-quiet")];
    // The second song's landing bar is 10x the power of the first's exit.
    songs.push(Song {
        intro: 0.01,
        body: 0.1,
        ..flat("s-loud", 120.0)
    });
    build(&plugin_path(), &f.input(&songs, "landing", 900_000)).unwrap();
    let plan = f.plan();
    let order: Vec<&str> = plan.tracks.iter().map(|t| t.entry_id.as_str()).collect();
    if order[0] == "s-quiet" {
        assert_eq!(plan.seams[0].recipe, Recipe::DropMix, "{order:?}");
    } else {
        assert_ne!(plan.seams[0].recipe, Recipe::DropMix, "{order:?}");
    }
}

#[test]
fn a_start_entry_plays_each_song_from_its_first_second_and_never_bass_swaps() {
    let f = Fixture::new();
    build(&plugin_path(), &f.input(&four_songs(), "start", 600_000)).unwrap();
    let plan = f.plan();
    assert!(plan.tracks.len() >= 2);
    for track in &plan.tracks {
        assert_eq!(track.enter_s, 0.0, "{}", track.entry_id);
    }
    for seam in &plan.seams {
        assert_ne!(seam.recipe, Recipe::BassSwap, "{seam:?}");
    }
}

#[test]
fn a_build_with_the_renderer_absent_still_emits_a_timeline() {
    let f = Fixture::new();
    let mut input = f.input(&four_songs(), "landing", 600_000);
    input.config.as_mut().unwrap()["enqueue"] = json!("/nonexistent/etv-station");
    let built = build(&plugin_path(), &input).unwrap();
    let segment = &built.timeline.segments[0];
    assert!(
        segment.name.starts_with("chapter:unqueued-"),
        "{}",
        segment.name
    );
    assert!(matches!(segment.source, SegmentSource::Pending { .. }));
    f.plan();
}

#[test]
fn a_song_under_the_confidence_floor_is_never_played() {
    let f = Fixture::new();
    let mut songs = four_songs();
    songs.push(Song {
        confidence: 1.0,
        ..flat("s-weak", 120.0)
    });
    build(&plugin_path(), &f.input(&songs, "landing", 900_000)).unwrap();
    assert!(
        !f.plan().tracks.iter().any(|t| t.entry_id == "s-weak"),
        "s-weak is under the floor"
    );
}

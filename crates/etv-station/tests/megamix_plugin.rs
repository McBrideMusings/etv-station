//! Acceptance tests for the megamix bumper (`examples/plugins/bumpers/megamix.rhai`):
//! it reads audio facts through `ctx.catalog`, picks a tempo and key cluster,
//! writes a chapter plan, queues it with the renderer and airs it as a pending
//! segment.
//!
//! The catalog, the facts, the renderer's `enqueue` and `ffmpeg` are all written
//! fresh by this file: the two programs are shell stubs, so the test proves the
//! plugin's arguments and the plan it wrote rather than a real render.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use etv_render::Plan;
use etv_station::bumper::hook::{BumperInput, ChannelInfo, build};
use etv_station::bumper::timeline::SegmentSource;
use etv_station::catalog::{Catalog, Entry, EntrySource, Source};
use etv_station::history::{AiringKind, PlayRecord};
use rhai::Map;
use serde_json::json;
use time::OffsetDateTime;

const PLUGIN: &str = "../../examples/plugins/bumpers/megamix.rhai";

fn plugin_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(PLUGIN)
}

/// One analyzer row: a 40-beat steady run unless `beats` says otherwise.
fn facts(
    bpm: f64,
    camelot: &str,
    key: &str,
    confidence: f64,
    beats: i64,
    landing_bar: usize,
) -> serde_json::Value {
    let bar = 240.0 / bpm;
    json!({
        "bpm": bpm,
        "beat_grid": { "slope": 60.0 / bpm, "intercept": 0.3, "start_s": 0.3, "end_s": 0.3 + beats as f64 * 60.0 / bpm,
                       "mean_residual_s": 0.004, "beats": beats },
        "downbeat_phase": 0,
        "landing_bar": landing_bar,
        "landing_bar_s": landing_bar as f64 * bar + 0.3,
        "key": key,
        "scale": if camelot.ends_with('A') { "minor" } else { "major" },
        "camelot": camelot,
        "key_strength": 0.8,
        "beat_confidence": confidence,
        "bar_energy": (0..80).map(|i| 0.01 * (1 + i % 7) as f64).collect::<Vec<_>>(),
        "rms_mean": 0.1,
    })
}

/// (entry, bpm, camelot, key, confidence, beats): five songs that cluster, one
/// with an unsteady beat and one with a low-confidence beat.
const SONGS: &[(&str, f64, &str, &str, f64, i64)] = &[
    ("mv-a", 118.0, "8A", "A", 4.1, 48),
    ("mv-b", 120.0, "8A", "A", 3.9, 64),
    ("mv-c", 121.0, "9A", "E", 3.2, 40),
    ("mv-d", 122.0, "7A", "D", 3.6, 56),
    ("mv-e", 119.0, "8B", "C", 3.0, 36),
    ("mv-unsteady", 120.0, "8A", "A", 4.5, 20),
    ("mv-weak", 120.0, "8A", "A", 1.2, 64),
];

fn catalog_file(dir: &Path) -> PathBuf {
    let path = dir.join("catalog.db");
    let cat = Catalog::open(&path).unwrap();
    for (i, (id, bpm, camelot, key, confidence, beats)) in SONGS.iter().enumerate() {
        let mut e = Entry::new(*id, "movie", format!("Song {id}"), Source::LocalFs);
        e.show = Some(format!("Artist {id}"));
        e.year = Some(1990 + i as i64);
        e.duration_ms = Some(240_000);
        e.library = Some("Music Videos".into());
        cat.upsert_entry(&e).unwrap();
        let playback = format!("/media/{id}.mp4");
        cat.add_source(&EntrySource {
            source: Source::LocalFs,
            source_id: playback.clone(),
            entry_id: (*id).into(),
            playback_path: playback.clone(),
            last_seen: None,
            missing_since: None,
            unairable_reason: None,
        })
        .unwrap();
        cat.add_source(&EntrySource {
            source: Source::Plex,
            source_id: format!("{}", 9000 + i),
            entry_id: (*id).into(),
            playback_path: format!("/library/parts/{}", 9000 + i),
            last_seen: None,
            missing_since: None,
            unairable_reason: None,
        })
        .unwrap();
        let landing = 8 * (1 + i % 3);
        cat.set_audio_fact(
            id,
            &playback,
            "2026-10-25T00:00:00Z",
            Ok(&facts(*bpm, camelot, key, *confidence, *beats, landing)),
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
        let work = dir.path().join("chapters");
        std::fs::create_dir_all(&work).unwrap();
        Fixture {
            dir,
            work,
            _turn: turn,
        }
    }

    fn input(&self, enqueue: &str, gap_ms: u64) -> BumperInput {
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
            config: Some(json!({
                "expr": "item.library == \"Music Videos\"",
                "dir": self.work,
                "enqueue": enqueue,
                "ffmpeg": ffmpeg,
                "floor": 2.5,
                "card_script": "overlays/hook_card.rhai",
            })),
            channel: ChannelInfo {
                name: "megamix".into(),
                number: 77,
                logo: None,
                vocabulary: Vec::new(),
                tz: None,
            },
            air_date: "2027-01-15".into(),
            draw: 0,
            granted: vec![
                "probe".into(),
                "catalog_read".into(),
                "exec".into(),
                "http".into(),
            ],
            history: None,
            pass: Vec::new(),
            catalog_path: Some(catalog_file(self.dir.path())),
        }
    }

    fn plans(&self) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(&self.work)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().ends_with(".plan.json"))
            .collect();
        found.sort();
        found
    }
}

/// Steps between two Camelot codes, as the plugin counts them.
fn camelot_dist(a: &str, b: &str) -> i64 {
    let split = |c: &str| {
        (
            c[..c.len() - 1].parse::<i64>().unwrap(),
            c[c.len() - 1..].to_string(),
        )
    };
    let (x, xl) = split(a);
    let (y, yl) = split(b);
    let mut d = (x - y).abs();
    if d > 6 {
        d = 12 - d;
    }
    d + i64::from(xl != yl)
}

fn shifted(code: &str, semitones: i64) -> String {
    let n: i64 = code[..code.len() - 1].parse().unwrap();
    format!(
        "{}{}",
        (n - 1 + 7 * semitones).rem_euclid(12) + 1,
        &code[code.len() - 1..]
    )
}

#[test]
fn a_chapter_plan_passes_the_floor_semitone_camelot_and_bar_rules() {
    let f = Fixture::new();
    let args_log = f.dir.path().join("enqueue.args");
    let job = stub(
        f.dir.path(),
        "enqueue",
        &format!(
            "printf '%s\\n' \"$@\" > {}\necho job-77",
            args_log.display()
        ),
    );
    let built = build(&plugin_path(), &f.input(job.to_str().unwrap(), 600_000)).unwrap();

    let plans = f.plans();
    assert_eq!(plans.len(), 1, "one plan beside the render path");
    let plan = Plan::parse(&std::fs::read_to_string(&plans[0]).unwrap())
        .expect("the renderer accepts the plan");

    assert!(
        (3..=5).contains(&plan.hooks.len()),
        "{} hooks",
        plan.hooks.len()
    );
    let mut bars = 0;
    assert!(
        !plan
            .hooks
            .iter()
            .any(|h| h.entry_id == "mv-unsteady" || h.entry_id == "mv-weak")
    );
    for hook in &plan.hooks {
        let song = SONGS
            .iter()
            .find(|s| s.0 == hook.entry_id)
            .expect("a pool song");
        assert!(song.4 >= 2.5, "{} is under the confidence floor", song.0);
        assert!(song.5 >= 32, "{} has a steady run under 32 beats", song.0);
        assert!(
            hook.semitones.abs() <= 3,
            "{} moves {} semitones",
            song.0,
            hook.semitones
        );
        let landed = shifted(song.2, i64::from(hook.semitones));
        assert!(
            camelot_dist(&landed, &plan.camelot) <= 1,
            "{} lands on {landed}, chapter is {}",
            song.0,
            plan.camelot
        );
        assert_eq!(
            bars % 8,
            0,
            "{} starts at chapter bar {bars}, not an 8-bar boundary",
            song.0
        );
        bars += hook.vocal_over_bars + hook.full_bars;
        assert_eq!(hook.native_bpm, song.1);
    }
    // The chapter tempo is the median of the cluster, 119.5-121 here.
    assert!(
        (118.0..=122.0).contains(&plan.bpm),
        "chapter bpm {}",
        plan.bpm
    );
    assert_eq!(plan.cards.len(), plan.hooks.len());
    assert_eq!(
        plan.cards[0].artist,
        format!("Artist {}", plan.hooks[0].entry_id)
    );

    // The renderer's queue was handed this plan, with the chapter's air time:
    // the next programme's start (1_800_000_000) less the 600 s gap.
    let args: Vec<String> = std::fs::read_to_string(args_log)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    assert_eq!(args[0], "enqueue");
    assert_eq!(Path::new(&args[1]), plans[0]);
    assert_eq!(&args[2..], ["--air-at", "2027-01-15T07:50:00Z"]);

    // One pending segment named for the queued job and its hooks.
    let segment = &built.timeline.segments[0];
    assert!(
        segment.name.starts_with("chapter:job-77|"),
        "{}",
        segment.name
    );
    let SegmentSource::Pending { path, stand_in } = &segment.source else {
        panic!("expected a pending segment, got {:?}", segment.source);
    };
    assert_eq!(path, &plan.output);
    assert!(stand_in.exists(), "the stand-in file was made");
    assert_eq!(
        segment.overlay.as_ref().unwrap().vars["cards"]
            .as_array()
            .unwrap()
            .len(),
        plan.hooks.len()
    );
}

#[test]
fn a_chapter_laid_earlier_in_the_pass_keeps_its_songs_out_of_the_next() {
    let f = Fixture::new();
    let mut input = f.input("/nonexistent/etv-station", 600_000);
    input.config.as_mut().unwrap()["hooks_min"] = json!(2);
    input.config.as_mut().unwrap()["hooks_max"] = json!(2);

    let first = build(&plugin_path(), &input).unwrap();
    let first_name = first.timeline.segments[0].name.clone();
    // The same inputs draw the same songs, so only the pass's rows can differ.
    let again = build(&plugin_path(), &input).unwrap();
    assert_eq!(again.timeline.segments[0].name, first_name);

    input.pass = vec![PlayRecord {
        entry_id: first_name.clone(),
        start: OffsetDateTime::from_unix_timestamp(1_799_999_000).unwrap(),
        kind: AiringKind::Bumper,
        ..Default::default()
    }];
    let second = build(&plugin_path(), &input).unwrap();
    let songs = |name: &str| -> Vec<String> { name.split('|').skip(1).map(String::from).collect() };
    let used = songs(&first_name);
    let next = songs(&second.timeline.segments[0].name);
    assert_eq!(next.len(), 2, "{next:?}");
    assert!(
        next.iter().all(|s| !used.contains(s)),
        "{next:?} reuses a song from {used:?}"
    );
}

#[test]
fn a_build_with_the_renderer_absent_still_emits_a_timeline() {
    let f = Fixture::new();
    let built = build(
        &plugin_path(),
        &f.input("/nonexistent/etv-station", 600_000),
    )
    .unwrap();
    let segment = &built.timeline.segments[0];
    assert!(
        segment.name.starts_with("chapter:unqueued-"),
        "{}",
        segment.name
    );
    assert!(matches!(segment.source, SegmentSource::Pending { .. }));
    assert_eq!(f.plans().len(), 1, "the plan is still written");
}

/// A stand-in for the plex-db-ex explorer: answers every request with `reply`
/// and sends each request's first line and body down the channel.
fn explorer(reply: &'static str) -> (String, std::sync::mpsc::Receiver<String>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { return };
            // Read through the body: closing on an unread body resets the client.
            let mut head = String::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = sock.read(&mut buf).unwrap_or(0);
                head.push_str(&String::from_utf8_lossy(&buf[..n]));
                let Some((top, body)) = head.split_once("\r\n\r\n") else {
                    if n == 0 {
                        break;
                    }
                    continue;
                };
                let want = top
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                if body.len() >= want || n == 0 {
                    break;
                }
            }
            let body = head.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
            let _ = tx.send(format!("{}\n{body}", head.lines().next().unwrap_or("")));
            let _ = write!(
                sock,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    (url, rx)
}

fn only_plan(f: &Fixture) -> (PathBuf, Plan) {
    let plans = f.plans();
    assert_eq!(plans.len(), 1);
    let plan = Plan::parse(&std::fs::read_to_string(&plans[0]).unwrap())
        .expect("the renderer accepts the plan");
    (plans[0].clone(), plan)
}

#[test]
fn frame_index_shots_replace_the_other_hooks_pictures() {
    let f = Fixture::new();
    // mv-unsteady and mv-weak never become hooks, so they are always valid B-roll.
    let (url, requests) = explorer(
        r#"{"shots":[
            {"item_id":"9005","in_ms":10000,"out_ms":30000,"drawn":1},
            {"item_id":"9006","in_ms":50000,"out_ms":70000,"drawn":1},
            {"item_id":"9005","in_ms":1000,"out_ms":2000},
            {"item_id":"9999","in_ms":0,"out_ms":20000,"drawn":1}
        ]}"#,
    );
    let mut input = f.input("/nonexistent/etv-station", 600_000);
    input.config.as_mut().unwrap()["plexdb_url"] = json!(url);
    input.config.as_mut().unwrap()["phrase"] = json!("a neon city at night");
    build(&plugin_path(), &input).unwrap();

    let request = requests.recv().unwrap();
    let (line, body) = request.split_once('\n').unwrap();
    assert!(line.starts_with("POST /api/search/run"), "{line}");
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(body["phrase"], "a neon city at night");
    let (plan_path, plan) = only_plan(&f);
    let pool: Vec<String> = body["pool"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(pool.len(), SONGS.len() - plan.hooks.len(), "{pool:?}");
    for hook in &plan.hooks {
        let i = SONGS.iter().position(|s| s.0 == hook.entry_id).unwrap();
        assert!(
            !pool.contains(&format!("{}", 9000 + i)),
            "{} is a hook and in the pool",
            hook.entry_id
        );
    }
    assert_eq!(body["draw"]["n"], plan.hooks.len() * 2);

    let ids: Vec<&str> = plan.broll.iter().map(|b| b.entry_id.as_str()).collect();
    assert_eq!(ids, ["mv-unsteady", "mv-weak"]);
    let shots: Vec<_> = plan
        .cuts
        .iter()
        .filter(|c| ids.contains(&c.entry_id.as_str()))
        .collect();
    assert_eq!(
        shots.len(),
        2,
        "the unknown item and the short shot are dropped"
    );
    assert_eq!((shots[0].native_start_s, shots[0].ratio), (10.0, 1.0));
    assert_eq!(shots[1].native_start_s, 50.0);
    let audit_path = plan_path
        .to_string_lossy()
        .replace(".plan.json", ".audit.txt");
    let audit = std::fs::read_to_string(audit_path).unwrap();
    assert_eq!(audit.matches("broll at ").count(), 2, "{audit}");
}

#[test]
fn an_unreachable_explorer_leaves_the_hooks_own_pictures() {
    let f = Fixture::new();
    let mut input = f.input("/nonexistent/etv-station", 600_000);
    input.config.as_mut().unwrap()["plexdb_url"] = json!("http://127.0.0.1:9");
    input.config.as_mut().unwrap()["phrase"] = json!("anything");
    build(&plugin_path(), &input).unwrap();
    let (_, plan) = only_plan(&f);
    assert!(plan.broll.is_empty());
}

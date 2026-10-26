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

/// Serves one OpenAI-style chat completion whose message text is `content`,
/// returning the url and a handle that yields the request it received.
fn model_server(content: &str) -> (String, std::thread::JoinHandle<String>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let reply = json!({ "choices": [{ "message": { "role": "assistant", "content": content } }] })
        .to_string();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = stream.read(&mut buf).unwrap();
            got.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&got).to_string();
            if let Some(split) = text.find("\r\n\r\n") {
                let length = text[..split]
                    .to_lowercase()
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("content-length: ")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if got.len() >= split + 4 + length {
                    break;
                }
            }
            if n == 0 {
                break;
            }
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        String::from_utf8_lossy(&got).to_string()
    });
    (url, handle)
}

fn model_input(f: &Fixture, url: &str) -> BumperInput {
    let mut input = f.input("/nonexistent/etv-station", 600_000);
    let cfg = input.config.as_mut().unwrap();
    cfg["ranker"] = json!("model");
    cfg["model_url"] = json!(url);
    cfg["model"] = json!("ranker-1");
    cfg["brief"] = json!("slow-burn eighties, nothing sad");
    cfg["model_headers"] = json!({ "Authorization": "Bearer test-key" });
    input
}

fn hooks_of(name: &str) -> Vec<String> {
    name.split('|').skip(1).map(String::from).collect()
}

#[test]
fn a_model_pick_sets_the_chapters_hooks_in_its_order() {
    let f = Fixture::new();
    let (url, request) = model_server(
        "Here you go:\n{\"cluster\": \"c0\", \"hooks\": [\"mv-d\", \"mv-a\", \"mv-b\"]}\n",
    );
    let built = build(&plugin_path(), &model_input(&f, &url)).unwrap();
    let sent = request.join().unwrap();

    assert_eq!(
        hooks_of(&built.timeline.segments[0].name),
        ["mv-d", "mv-a", "mv-b"]
    );
    let plan = Plan::parse(&std::fs::read_to_string(&f.plans()[0]).unwrap()).unwrap();
    let ids: Vec<&str> = plan.hooks.iter().map(|h| h.entry_id.as_str()).collect();
    assert_eq!(ids, ["mv-d", "mv-a", "mv-b"]);
    assert!(sent.contains("slow-burn eighties, nothing sad"), "{sent}");
    assert!(
        sent.to_lowercase()
            .contains("authorization: bearer test-key"),
        "{sent}"
    );
    assert!(sent.contains("ranker-1"), "{sent}");
    assert!(
        sent.contains("mv-e"),
        "the cluster's facts are in the prompt: {sent}"
    );
}

#[test]
fn a_rejected_model_pick_falls_back_to_the_rule() {
    // Each reply breaks one rule: an unfit song, a repeat, too few hooks, an
    // unoffered cluster, text with no JSON.
    for reply in [
        "{\"cluster\": \"c0\", \"hooks\": [\"mv-a\", \"mv-b\", \"mv-weak\"]}",
        "{\"cluster\": \"c0\", \"hooks\": [\"mv-a\", \"mv-b\", \"mv-a\"]}",
        "{\"cluster\": \"c0\", \"hooks\": [\"mv-a\", \"mv-b\"]}",
        "{\"cluster\": \"c9\", \"hooks\": [\"mv-a\", \"mv-b\", \"mv-c\"]}",
        "I would open with mv-a.",
    ] {
        let f = Fixture::new();
        let (url, request) = model_server(reply);
        let built = build(&plugin_path(), &model_input(&f, &url)).unwrap();
        request.join().unwrap();
        let hooks = hooks_of(&built.timeline.segments[0].name);
        assert!((3..=5).contains(&hooks.len()), "{reply}: {hooks:?}");
        assert!(
            !hooks.iter().any(|h| h == "mv-weak" || h == "mv-unsteady"),
            "{reply}: {hooks:?}"
        );
        let rule = build(
            &plugin_path(),
            &f.input("/nonexistent/etv-station", 600_000),
        )
        .unwrap();
        assert_eq!(
            built.timeline.segments[0].name, rule.timeline.segments[0].name,
            "{reply}: the fallback is the rule's chapter"
        );
    }
}

#[test]
fn an_unreachable_model_falls_back_to_the_rule() {
    let f = Fixture::new();
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://{}/v1/chat/completions",
        closed.local_addr().unwrap()
    );
    drop(closed);
    let built = build(&plugin_path(), &model_input(&f, &url)).unwrap();
    assert!((3..=5).contains(&hooks_of(&built.timeline.segments[0].name).len()));
}

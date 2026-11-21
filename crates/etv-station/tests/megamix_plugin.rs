//! Acceptance tests for the megamix bumper (`examples/plugins/bumpers/megamix.rhai`):
//! it reads audio facts through `ctx.catalog`, picks a tempo and key cluster,
//! writes a render plan and airs it as a pending segment that names the plan,
//! which the render sidecar queues as the chapter's air time nears.
//!
//! The catalog, the facts and `ffmpeg` are all written fresh by this file:
//! `ffmpeg` is a shell stub, so the test proves the plan the plugin wrote and
//! the segment it laid rather than a real render.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use etv_render::Plan;
use etv_render::plan::{Curve, FadeKind, Stem};
use etv_station::bumper::hook::{BumperInput, ChannelInfo, build};
use etv_station::bumper::timeline::SegmentSource;
use etv_station::catalog::{Catalog, Entry, EntrySource, Source};
use etv_station::history::{AiringKind, PlayRecord};
use rhai::Map;
use serde::Deserialize;
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

#[path = "support/plexdb_ex_fake.rs"]
mod fake;

/// A fake plex-db-ex explorer holding the songs' audio facts.
fn audio_explorer() -> fake::Server {
    let mut store = fake::Store::default();
    for (i, (id, bpm, camelot, key, confidence, beats)) in SONGS.iter().enumerate() {
        let landing = 8 * (1 + i % 3);
        store.audio(id, facts(*bpm, camelot, key, *confidence, *beats, landing));
    }
    fake::serve(store)
}

fn catalog_file(dir: &Path) -> PathBuf {
    let path = dir.join("catalog.db");
    let cat = Catalog::open(&path).unwrap();
    for (i, (id, ..)) in SONGS.iter().enumerate() {
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
    explorer: fake::Server,
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
            explorer: audio_explorer(),
            _turn: turn,
        }
    }

    fn input(&self, gap_ms: u64) -> BumperInput {
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
                "ffmpeg": ffmpeg,
                "floor": 2.5,
                "blend": "vocal_over",
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
            plexdb: Some(etv_station::plexdb_ex::Explorer::new(&self.explorer.url)),
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

/// What the plugin records in a plan's `notes` about the chapter.
#[derive(Debug, Deserialize)]
struct Notes {
    bpm: f64,
    camelot: String,
    bar_seconds: f64,
    hooks: Vec<NoteHook>,
}

#[derive(Debug, Deserialize)]
struct NoteHook {
    entry_id: String,
    landing_s: f64,
    native_bpm: f64,
    semitones: i32,
    bars: u32,
}

fn notes(plan: &Plan) -> Notes {
    serde_json::from_value(plan.notes.clone()).expect("the plan's notes name the chapter")
}

/// The plan's sources that are not hooks: the Frame index shots.
fn broll(plan: &Plan) -> Vec<String> {
    let hooks = notes(plan).hooks;
    plan.sources
        .iter()
        .filter(|s| !hooks.iter().any(|h| h.entry_id == s.id))
        .map(|s| s.id.clone())
        .collect()
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
    let built = build(&plugin_path(), &f.input(600_000)).unwrap();

    let plans = f.plans();
    assert_eq!(plans.len(), 1, "one plan beside the render path");
    let plan = Plan::parse(&std::fs::read_to_string(&plans[0]).unwrap())
        .expect("the renderer accepts the plan");
    let chapter = notes(&plan);

    assert!(
        (3..=5).contains(&chapter.hooks.len()),
        "{} hooks",
        chapter.hooks.len()
    );
    let mut bars = 0;
    assert!(
        !chapter
            .hooks
            .iter()
            .any(|h| h.entry_id == "mv-unsteady" || h.entry_id == "mv-weak")
    );
    for hook in &chapter.hooks {
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
            camelot_dist(&landed, &chapter.camelot) <= 1,
            "{} lands on {landed}, chapter is {}",
            song.0,
            chapter.camelot
        );
        assert_eq!(
            bars % 8,
            0,
            "{} starts at chapter bar {bars}, not an 8-bar boundary",
            song.0
        );
        bars += hook.bars;
        assert_eq!(hook.native_bpm, song.1);
    }
    // The chapter tempo is the median of the cluster, 119.5-121 here.
    assert!(
        (118.0..=122.0).contains(&chapter.bpm),
        "chapter bpm {}",
        chapter.bpm
    );
    assert_eq!(plan.cards.len(), chapter.hooks.len());
    assert_eq!(
        plan.cards[0].artist,
        format!("Artist {}", chapter.hooks[0].entry_id)
    );

    // One pending segment named for the render's stem and its hooks, naming
    // the plan the render sidecar queues as its air time nears.
    let segment = &built.timeline.segments[0];
    let stem = plan.output.file_stem().unwrap().to_str().unwrap();
    assert!(
        segment.name.starts_with(&format!("chapter:{stem}|")),
        "{}",
        segment.name
    );
    let SegmentSource::Pending {
        path,
        stand_in,
        plan: named,
    } = &segment.source
    else {
        panic!("expected a pending segment, got {:?}", segment.source);
    };
    assert_eq!(path, &plan.output);
    assert_eq!(named.as_deref(), Some(plans[0].as_path()));
    assert!(stand_in.exists(), "the stand-in file was made");
    assert_eq!(
        segment.overlay.as_ref().unwrap().vars["cards"]
            .as_array()
            .unwrap()
            .len(),
        chapter.hooks.len()
    );
}

/// One layer as the plugin should lay it: (name, source, stem, src_start_s,
/// out_start_s, out_len_s, ratio, pitch, level_db, fade in, fade out).
type Expected = (String, String, Stem, f64, f64, f64, f64, f64, f64, f64, f64);

/// The vocal-over seam, worked out from the chapter's notes: the first hook's
/// vocal and instrumental; for each later hook, the previous instrumental
/// carrying on under its vocal, then its own vocal and instrumental.
fn expected_layers(c: &Notes) -> Vec<Expected> {
    let bar = c.bar_seconds;
    let mut out = Vec::new();
    let mut at = 0u32;
    for (i, h) in c.hooks.iter().enumerate() {
        let lay = |name: String,
                   k: &NoteHook,
                   stem,
                   from: u32,
                   bars: u32,
                   at_bar: u32,
                   db,
                   fi: f64,
                   fo: f64| {
            let len = f64::from(bars) * bar;
            (
                name,
                k.entry_id.clone(),
                stem,
                k.landing_s + f64::from(from) * 240.0 / k.native_bpm,
                f64::from(at_bar) * bar,
                len,
                c.bpm / k.native_bpm,
                2f64.powf(f64::from(k.semitones) / 12.0),
                db,
                fi.min(len / 2.0),
                fo.min(len / 2.0),
            )
        };
        let label = format!("hook {} ({}) ", i + 1, h.entry_id);
        // `vocal_over_bars` is left at its default, 2.
        let over = if i == 0 { 0 } else { 2 };
        let full = h.bars - over;
        if i > 0 && over > 0 {
            let prev = &c.hooks[i - 1];
            out.push(lay(
                format!(
                    "hook {i} ({}) instrumental under the next vocal",
                    prev.entry_id
                ),
                prev,
                Stem::Instrumental,
                prev.bars,
                over,
                at,
                -17.0,
                0.02,
                bar,
            ));
            out.push(lay(
                format!("{label}vocals over the previous song"),
                h,
                Stem::Vocals,
                0,
                over,
                at,
                -19.0,
                0.5,
                0.02,
            ));
        }
        out.push(lay(
            format!("{label}vocals"),
            h,
            Stem::Vocals,
            over,
            full,
            at + over,
            -20.0,
            0.02,
            0.02,
        ));
        let inst_in = if i == 0 { 1.0 } else { 0.3 };
        out.push(lay(
            format!("{label}instrumental"),
            h,
            Stem::Instrumental,
            over,
            full,
            at + over,
            -17.0,
            inst_in,
            0.02,
        ));
        at += h.bars;
    }
    out
}

#[test]
fn the_layers_are_the_vocal_over_seam_and_the_picture_runs_at_the_audios_speed() {
    let f = Fixture::new();
    build(&plugin_path(), &f.input(600_000)).unwrap();
    let (_, plan) = only_plan(&f);
    let chapter = notes(&plan);
    assert!(chapter.hooks.len() >= 3);

    let close = |a: f64, b: f64| (a - b).abs() < 1e-6;
    let want = expected_layers(&chapter);
    assert_eq!(plan.layers.len(), want.len(), "{:#?}", plan.layers);
    for (l, w) in plan.layers.iter().zip(&want) {
        assert_eq!((&l.name, &l.source, l.stem), (&w.0, &w.1, w.2));
        assert!(
            close(l.src_start_s, w.3) && close(l.out_start_s, w.4) && close(l.out_len_s, w.5),
            "{}: read {} at {} for {}, want {w:?}",
            l.name,
            l.src_start_s,
            l.out_start_s,
            l.out_len_s
        );
        assert!(
            close(l.ratio, w.6) && close(l.pitch, w.7),
            "{}: ratio {} pitch {}",
            l.name,
            l.ratio,
            l.pitch
        );
        assert!(
            l.ride.is_none() && l.filters.is_empty() && l.gain_db == 0.0,
            "{}",
            l.name
        );
        assert_eq!(l.level_db, Some(w.8), "{}", l.name);
        let [fade_in, fade_out] = &l.fades[..] else {
            panic!("{}: {:?}", l.name, l.fades);
        };
        assert_eq!((fade_in.kind, fade_out.kind), (FadeKind::In, FadeKind::Out));
        assert_eq!(
            (fade_in.curve, fade_out.curve),
            (Curve::Linear, Curve::Linear)
        );
        assert!(
            close(fade_in.start_s, 0.0) && close(fade_in.len_s, w.9),
            "{}",
            l.name
        );
        assert!(
            close(fade_out.len_s, w.10) && close(fade_out.start_s, w.5 - w.10),
            "{}",
            l.name
        );
    }

    let bars: u32 = chapter.hooks.iter().map(|h| h.bars).sum();
    assert!(close(
        plan.duration_s,
        f64::from(bars) * chapter.bar_seconds
    ));
    assert_cuts_tile(&plan);
    assert_eq!(unheard_on_screen_s(&plan), 0.0);
    // Every hook's stems are one span, 4 bars before its landing to 18 after,
    // whichever part of it this chapter reads.
    for h in &chapter.hooks {
        let s = plan.sources.iter().find(|s| s.id == h.entry_id).unwrap();
        let sep = s.separate.expect("a hook source names its stem span");
        let native_bar = 240.0 / h.native_bpm;
        assert!(close(sep.start_s, h.landing_s - 4.0 * native_bar), "{s:?}");
        assert!(close(sep.len_s, 22.0 * native_bar), "{s:?}");
    }
}

/// The cuts cover the chapter end to end with no gap or overlap, and every cut
/// of a chapter song starts where one of its layers plays, at that layer's
/// source second and speed. A cut of any other source is a Frame index shot.
fn assert_cuts_tile(plan: &Plan) {
    let close = |a: f64, b: f64| (a - b).abs() < 1e-6;
    let mut covered = 0.0;
    for cut in &plan.cuts {
        assert!(
            close(cut.out_start_s, covered),
            "{cut:?} leaves a gap or overlaps"
        );
        covered += cut.out_len_s;
        if !plan.layers.iter().any(|l| l.source == cut.source) {
            continue;
        }
        let l = plan
            .layers
            .iter()
            .find(|l| {
                l.source == cut.source
                    && l.out_start_s <= cut.out_start_s + 1e-9
                    && cut.out_start_s < l.out_start_s + l.out_len_s
            })
            .unwrap_or_else(|| panic!("{cut:?} shows a song with no layer playing"));
        assert!(close(cut.ratio, l.ratio), "{cut:?}");
        assert!(
            close(
                cut.src_start_s,
                l.src_start_s + (cut.out_start_s - l.out_start_s) * l.ratio
            ),
            "{cut:?} vs {l:?}"
        );
    }
    assert!(
        close(covered, plan.duration_s),
        "cuts cover {covered} of {}",
        plan.duration_s
    );
}

/// Seconds of picture showing a song with no vocal layer playing, sampled every
/// 10 ms: the number `.claude/skills/project/verify.md` § Verifying the megamix
/// prints per chapter.
fn unheard_on_screen_s(plan: &Plan) -> f64 {
    let mut bad = 0.0;
    let mut t = 0.0;
    while t < plan.duration_s {
        let shown = plan
            .cuts
            .iter()
            .find(|k| k.out_start_s <= t && t < k.out_start_s + k.out_len_s);
        let heard = |src: &str| {
            plan.layers.iter().any(|l| {
                l.source == src
                    && l.stem == Stem::Vocals
                    && l.out_start_s <= t
                    && t < l.out_start_s + l.out_len_s
            })
        };
        if shown.is_some_and(|k| !heard(&k.source)) {
            bad += 0.01;
        }
        t += 0.01;
    }
    bad
}

/// The `vocal_over` module's `plan`, called directly with the two hooks the
/// renderer's own layer tests pinned: `a` at 100 bpm landing at 30 s with 4
/// bars, `b` at 120 bpm two semitones down with 6 bars, 2 of them its vocal
/// over `a`, in a 120 bpm chapter.
/// Every number below is the one those tests pinned, read in source seconds.
#[test]
fn vocal_over_puts_each_stem_where_the_renderer_used_to() {
    let mut engine = rhai::Engine::new();
    // The host's limits (hooks.rs): the plugin nests deeper than rhai's default.
    engine.set_max_expr_depths(128, 64);
    let ast = engine
        .compile_file(plugin_path().with_file_name("megamix/vocal_over.rhai"))
        .unwrap();
    let hook = |id: &str, bpm: f64, semis: i64, bars: i64| {
        let mut m = Map::new();
        m.insert("entry_id".into(), id.to_string().into());
        m.insert("landing_s".into(), 30.0_f64.into());
        m.insert("native_bpm".into(), bpm.into());
        m.insert("bars".into(), bars.into());
        m.insert("ratio".into(), (120.0 / bpm).into());
        m.insert("pitch".into(), 2f64.powf(semis as f64 / 12.0).into());
        rhai::Dynamic::from_map(m)
    };
    let hooks: rhai::Array = vec![hook("a", 100.0, 0, 4), hook("b", 120.0, -2, 6)];
    let mut cfg = Map::new();
    cfg.insert("bar_s".into(), 2.0_f64.into());
    cfg.insert("region_bars".into(), 18_i64.into());
    let out: rhai::Array = engine
        .call_fn(&mut rhai::Scope::new(), &ast, "plan", (hooks, cfg))
        .unwrap();
    let layers: Vec<serde_json::Value> = out
        .into_iter()
        .map(|d| rhai::serde::from_dynamic(&d).unwrap())
        .collect();
    let row = |l: &serde_json::Value| {
        (
            l["name"].as_str().unwrap().to_string(),
            l["stem"].as_str().unwrap().to_string(),
            l["src_start_s"].as_f64().unwrap(),
            l["out_start_s"].as_f64().unwrap(),
            l["out_len_s"].as_f64().unwrap(),
            l["level_db"].as_f64().unwrap(),
        )
    };
    let got: Vec<_> = layers.iter().map(row).collect();
    let want = [
        ("hook 1 (a) vocals", "vocals", 30.0, 0.0, 8.0, -20.0),
        (
            "hook 1 (a) instrumental",
            "instrumental",
            30.0,
            0.0,
            8.0,
            -17.0,
        ),
        // After a's 4 bars of 2.4 s: 30 + 9.6.
        (
            "hook 1 (a) instrumental under the next vocal",
            "instrumental",
            39.6,
            8.0,
            4.0,
            -17.0,
        ),
        (
            "hook 2 (b) vocals over the previous song",
            "vocals",
            30.0,
            8.0,
            4.0,
            -19.0,
        ),
        // b runs at the chapter tempo: 2 seam bars of 2 s in.
        ("hook 2 (b) vocals", "vocals", 34.0, 12.0, 8.0, -20.0),
        (
            "hook 2 (b) instrumental",
            "instrumental",
            34.0,
            12.0,
            8.0,
            -17.0,
        ),
    ];
    assert_eq!(got.len(), want.len(), "{got:#?}");
    for (g, w) in got.iter().zip(want) {
        assert_eq!((g.0.as_str(), g.1.as_str()), (w.0, w.1));
        for (x, y) in [(g.2, w.2), (g.3, w.3), (g.4, w.4), (g.5, w.5)] {
            assert!((x - y).abs() < 1e-9, "{g:?} vs {w:?}");
        }
    }
    // a is stretched 100 -> 120 bpm; b is two semitones down: 2^(-2/12).
    assert!((layers[0]["ratio"].as_f64().unwrap() - 1.2).abs() < 1e-9);
    assert!((layers[2]["ratio"].as_f64().unwrap() - 1.2).abs() < 1e-9);
    assert!((layers[4]["pitch"].as_f64().unwrap() - 0.890899).abs() < 1e-6);
    // The under layer fades out over one chapter bar; the seam vocal fades in over 0.5 s.
    assert_eq!(
        layers[2]["fades"][1],
        json!({"kind": "out", "start_s": 2.0, "len_s": 2.0})
    );
    assert_eq!(
        layers[3]["fades"][0],
        json!({"kind": "in", "start_s": 0.0, "len_s": 0.5})
    );
}

/// `picture_runs`, called directly on hand-made layers in a 2-second bar: `a`
/// plays instrumental at -20 dB for 16 s, `b` comes in at -10 dB from 4 s, and
/// `c`'s vocal sings at -30 dB from 8 s to 12 s. With a 4-beat hold and a 3 dB
/// margin the picture is `a`, then `b` once it is louder, then `c` while its
/// vocal is the one heard, then `b` again.
#[test]
fn the_picture_is_the_heard_vocal_else_the_loudest_song() {
    let mut engine = rhai::Engine::new();
    engine.set_max_expr_depths(128, 64);
    let ast = engine.compile_file(plugin_path()).unwrap();
    let layer = |src: &str, stem: &str, from: f64, len: f64, db: f64| {
        rhai::serde::to_dynamic(json!({
            "source": src, "stem": stem, "src_start_s": 100.0, "out_start_s": from,
            "out_len_s": len, "ratio": 1.0, "level_db": db, "fades": [],
        }))
        .unwrap()
    };
    let layers: rhai::Array = vec![
        layer("a", "instrumental", 0.0, 16.0, -20.0),
        layer("b", "instrumental", 4.0, 12.0, -10.0),
        layer("c", "vocals", 8.0, 4.0, -30.0),
    ];
    let mut cfg = Map::new();
    cfg.insert("picture_hold_beats".into(), 4_i64.into());
    cfg.insert("picture_margin_db".into(), 3.0_f64.into());
    let out: rhai::Array = engine
        .call_fn(
            &mut rhai::Scope::new(),
            &ast,
            "picture_runs",
            (layers, 16.0_f64, 2.0_f64, cfg),
        )
        .unwrap();
    let runs: Vec<(String, i64, i64, f64)> = out
        .into_iter()
        .map(|d| {
            let r: serde_json::Value = rhai::serde::from_dynamic(&d).unwrap();
            (
                r["source"].as_str().unwrap().to_string(),
                r["from"].as_i64().unwrap(),
                r["to"].as_i64().unwrap(),
                r["offset"].as_f64().unwrap(),
            )
        })
        .collect();
    // In sixteenths of a 2 s bar: 4 s is 32, 8 s is 64, 12 s is 96, 16 s is 128.
    // Each layer reads source second 100 at its own start, so its offset is
    // 100 minus that start.
    assert_eq!(
        runs,
        [
            ("a".to_string(), 0, 32, 100.0),
            ("b".to_string(), 32, 64, 96.0),
            ("c".to_string(), 64, 96, 92.0),
            ("b".to_string(), 96, 128, 96.0),
        ]
    );
}

/// The same rule where the hold and the margin each keep `a` on screen: `b`
/// arriving 10 dB louder at 1 s waits for the 4-beat hold to end at 2 s, and
/// `b` arriving only 2 dB louder never takes the picture.
#[test]
fn the_hold_and_the_margin_keep_the_song_on_screen() {
    let mut engine = rhai::Engine::new();
    engine.set_max_expr_depths(128, 64);
    let ast = engine.compile_file(plugin_path()).unwrap();
    let runs = |b_from: f64, b_db: f64| -> Vec<(String, i64, i64)> {
        let layer = |src: &str, from: f64, len: f64, db: f64| {
            rhai::serde::to_dynamic(json!({
                "source": src, "stem": "instrumental", "src_start_s": 100.0, "out_start_s": from,
                "out_len_s": len, "ratio": 1.0, "level_db": db, "fades": [],
            }))
            .unwrap()
        };
        let layers: rhai::Array = vec![
            layer("a", 0.0, 16.0, -20.0),
            layer("b", b_from, 16.0 - b_from, b_db),
        ];
        let mut cfg = Map::new();
        cfg.insert("picture_hold_beats".into(), 4_i64.into());
        cfg.insert("picture_margin_db".into(), 3.0_f64.into());
        let out: rhai::Array = engine
            .call_fn(
                &mut rhai::Scope::new(),
                &ast,
                "picture_runs",
                (layers, 16.0_f64, 2.0_f64, cfg),
            )
            .unwrap();
        out.into_iter()
            .map(|d| {
                let r: serde_json::Value = rhai::serde::from_dynamic(&d).unwrap();
                (
                    r["source"].as_str().unwrap().to_string(),
                    r["from"].as_i64().unwrap(),
                    r["to"].as_i64().unwrap(),
                )
            })
            .collect()
    };
    // 1 s is sixteenth 8; the hold of 4 beats is 16 sixteenths.
    assert_eq!(
        runs(1.0, -10.0),
        [("a".to_string(), 0, 16), ("b".to_string(), 16, 128)]
    );
    assert_eq!(runs(4.0, -18.0), [("a".to_string(), 0, 128)]);
}

#[test]
fn a_fill_entry_without_a_blend_fails_naming_the_key() {
    let f = Fixture::new();
    let mut input = f.input(600_000);
    input.config.as_mut().unwrap()["blend"] = json!(null);
    let err = build(&plugin_path(), &input).unwrap_err().to_string();
    assert!(err.contains("config.blend"), "{err}");
}

#[test]
fn a_chapter_laid_earlier_in_the_pass_keeps_its_songs_out_of_the_next() {
    let f = Fixture::new();
    let mut input = f.input(600_000);
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
    let mut input = f.input(600_000);
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
    let hooks = notes(&plan).hooks;
    assert_eq!(pool.len(), SONGS.len() - hooks.len(), "{pool:?}");
    for hook in &hooks {
        let i = SONGS.iter().position(|s| s.0 == hook.entry_id).unwrap();
        assert!(
            !pool.contains(&format!("{}", 9000 + i)),
            "{} is a hook and in the pool",
            hook.entry_id
        );
    }
    assert_eq!(body["draw"]["n"], hooks.len() * 2);

    let ids = broll(&plan);
    assert_eq!(ids, ["mv-unsteady", "mv-weak"]);
    let shots: Vec<_> = plan
        .cuts
        .iter()
        .filter(|c| ids.contains(&c.source))
        .collect();
    assert_eq!(
        shots.len(),
        2,
        "the unknown item and the short shot are dropped"
    );
    assert_eq!((shots[0].src_start_s, shots[0].ratio), (10.0, 1.0));
    assert_eq!(shots[1].src_start_s, 50.0);
    // broll_share 0.5 of 16-beat windows: the first song's second and fourth
    // four-bar windows, each one window long, its own picture around them.
    let window = 4.0 * notes(&plan).bar_seconds;
    for (shot, at) in shots.iter().zip([1.0, 3.0]) {
        assert!((shot.out_start_s - at * window).abs() < 1e-6, "{shot:?}");
        assert!((shot.out_len_s - window).abs() < 1e-6, "{shot:?}");
    }
    assert_cuts_tile(&plan);
    let audit_path = plan_path
        .to_string_lossy()
        .replace(".plan.json", ".audit.txt");
    let audit = std::fs::read_to_string(audit_path).unwrap();
    assert_eq!(audit.matches("broll at ").count(), 2, "{audit}");
}

/// The explorer answers `/api/extras/fields` with an artist for every id asked,
/// named `Extra <id>`, and `/api/search/run` with no shots; each request's first
/// line and body go down the channel.
fn extras_explorer() -> (String, std::sync::mpsc::Receiver<String>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { return };
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
            let line = head.lines().next().unwrap_or("").to_string();
            let body = head.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
            let reply = if line.starts_with("POST /api/extras/fields") {
                let asked: serde_json::Value = serde_json::from_str(&body).unwrap();
                let items: serde_json::Map<String, serde_json::Value> = asked["ids"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|id| {
                        let id = id.as_str().unwrap();
                        (
                            id.to_string(),
                            json!({"fields": [
                                {"name": "year", "values": [{"value": "1999", "source": "person"}]},
                                {"name": "artist", "values": [
                                    {"value": format!("Extra {id}"), "source": "person"},
                                    {"value": "Scraped", "source": "scraper"}]}
                            ]}),
                        )
                    })
                    .collect();
                json!({ "items": items }).to_string()
            } else {
                r#"{"shots":[]}"#.to_string()
            };
            let _ = tx.send(format!("{line}\n{body}"));
            let _ = write!(
                sock,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    (url, rx)
}

#[test]
fn a_card_without_the_artist_field_reads_it_from_the_extras_fields() {
    let f = Fixture::new();
    let (url, requests) = extras_explorer();
    let mut input = f.input(600_000);
    input.config.as_mut().unwrap()["plexdb_url"] = json!(url);
    input.config.as_mut().unwrap()["artist_field"] = json!("artist");
    build(&plugin_path(), &input).unwrap();

    let (_, plan) = only_plan(&f);
    let request = requests.recv().unwrap();
    let (line, body) = request.split_once('\n').unwrap();
    assert!(line.starts_with("POST /api/extras/fields"), "{line}");
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    let asked: Vec<&str> = body["ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let chapter = notes(&plan);
    let hooks: Vec<&str> = chapter.hooks.iter().map(|h| h.entry_id.as_str()).collect();
    assert_eq!(asked, hooks, "one call, every hook's entry id");
    for (card, hook) in plan.cards.iter().zip(&chapter.hooks) {
        assert_eq!(card.artist, format!("Extra {}", hook.entry_id));
    }
}

#[test]
fn an_extra_is_searched_by_its_entry_id_and_found_by_it() {
    let f = Fixture::new();
    // mv-unsteady and mv-weak are never hooks; make them extras, which carry no
    // Plex source and whose item_id in the explorer is their entry id.
    let (url, requests) = explorer(
        r#"{"shots":[
            {"item_id":"mv-unsteady","in_ms":10000,"out_ms":30000,"drawn":1},
            {"item_id":"mv-weak","in_ms":50000,"out_ms":70000,"drawn":1}
        ]}"#,
    );
    let mut input = f.input(600_000);
    input.config.as_mut().unwrap()["plexdb_url"] = json!(url);
    let conn = rusqlite::Connection::open(f.dir.path().join("catalog.db")).unwrap();
    conn.execute(
        "DELETE FROM entry_sources WHERE source = 'plex' AND entry_id IN ('mv-unsteady', 'mv-weak')",
        [],
    )
    .unwrap();
    drop(conn);
    input.config.as_mut().unwrap()["phrase"] = json!("a neon city at night");
    build(&plugin_path(), &input).unwrap();

    let request = requests.recv().unwrap();
    let (_, body) = request.split_once('\n').unwrap();
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    let pool: Vec<&str> = body["pool"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        pool.contains(&"mv-unsteady") && pool.contains(&"mv-weak"),
        "{pool:?}"
    );
    let (_, plan) = only_plan(&f);
    assert_eq!(broll(&plan), ["mv-unsteady", "mv-weak"]);
}

#[test]
fn an_unreachable_explorer_leaves_the_hooks_own_pictures() {
    let f = Fixture::new();
    let mut input = f.input(600_000);
    input.config.as_mut().unwrap()["plexdb_url"] = json!("http://127.0.0.1:9");
    input.config.as_mut().unwrap()["phrase"] = json!("anything");
    build(&plugin_path(), &input).unwrap();
    let (_, plan) = only_plan(&f);
    assert!(broll(&plan).is_empty());
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
    let mut input = f.input(600_000);
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
    let ids: Vec<String> = notes(&plan).hooks.into_iter().map(|h| h.entry_id).collect();
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
        let rule = build(&plugin_path(), &f.input(600_000)).unwrap();
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

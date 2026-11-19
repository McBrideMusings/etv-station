//! Acceptance tests for the whole-song bumper (`examples/plugins/bumpers/whole_song.rhai`):
//! it reads audio facts through `ctx.catalog`, walks a set of whole songs, picks a
//! seam recipe per pair by rule, writes a render plan the renderer accepts (the
//! layers and cuts, with the songs and seams in `notes`) and airs it as a pending
//! segment naming that plan, which the render sidecar queues as air time nears.
//!
//! The catalog and facts are written fresh by this file; `ffmpeg` is a shell
//! stub, so the test proves the plugin's plan and segment rather than a real
//! render.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use etv_render::plan::{Curve, Filter, Plan};
use etv_station::bumper::hook::{BumperInput, ChannelInfo, build};
use etv_station::bumper::timeline::SegmentSource;
use etv_station::catalog::{Catalog, Entry, EntrySource, Source};
use rhai::Map;
use serde::Deserialize;
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

#[path = "support/plexdb_ex_fake.rs"]
mod fake;

/// The fake plex-db-ex explorer's store, holding `songs`' audio facts.
fn audio_store(songs: &[Song]) -> fake::Store {
    let mut store = fake::Store::default();
    for s in songs {
        let (intro, body) = (s.intro, s.body);
        store.audio(
            s.id,
            facts(s.bpm, s.confidence, |i| if i < 16 { intro } else { body }),
        );
    }
    store
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
        let work = dir.path().join("sets");
        std::fs::create_dir_all(&work).unwrap();
        Fixture {
            dir,
            work,
            explorer: fake::serve(fake::Store::default()),
            _turn: turn,
        }
    }

    fn input(&self, songs: &[Song], entry: &str, gap_ms: u64) -> BumperInput {
        *self.explorer.store.lock().unwrap() = audio_store(songs);
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
            plexdb: Some(etv_station::plexdb_ex::Explorer::new(&self.explorer.url)),
        }
    }

    fn plan(&self) -> Plan {
        let mut found: Vec<PathBuf> = std::fs::read_dir(&self.work)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().ends_with(".plan.json"))
            .collect();
        assert_eq!(found.len(), 1, "one plan beside the render path");
        found.sort();
        Plan::parse(&std::fs::read_to_string(&found[0]).unwrap())
            .expect("the renderer accepts the plan")
    }
}

/// What the plugin records in a plan's `notes`: the songs and the seams.
#[derive(Debug, Deserialize)]
struct Notes {
    end_s: f64,
    songs: Vec<NoteSong>,
    seams: Vec<NoteSeam>,
}

#[derive(Debug, Deserialize)]
struct NoteSong {
    entry_id: String,
    native_bpm: f64,
    enter_s: f64,
    ratio: f64,
    ride_bars: u32,
}

#[derive(Debug, Deserialize)]
struct NoteSeam {
    recipe: String,
    out_s: f64,
    bars: u32,
    sweep: bool,
}

fn notes(plan: &Plan) -> Notes {
    serde_json::from_value(plan.notes.clone()).expect("the plan's notes name the songs and seams")
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
    let n = notes(&plan);

    assert!(n.songs.len() >= 2, "{} songs", n.songs.len());
    assert_eq!(n.seams.len(), n.songs.len() - 1);
    for track in &n.songs {
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
    for seam in &n.seams {
        assert_eq!(seam.recipe, "bass_swap", "{seam:?}");
        assert_eq!((seam.bars, seam.sweep), (2, true));
    }
    assert!(plan.duration_s <= 600.0 + 1e-6, "{}s", plan.duration_s);
    assert!(
        plan.duration_s > 590.0,
        "the set fills the gap: {}s",
        plan.duration_s
    );
    assert_eq!(plan.cards.len(), n.songs.len());

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
    assert_eq!(
        named.as_deref(),
        Some(plan.output.with_extension("plan.json").as_path())
    );
    assert!(stand_in.exists());
    let etv_station::bumper::timeline::Duration::Fixed(aired_ms) = segment.duration else {
        panic!("expected a fixed slot, got {:?}", segment.duration);
    };
    assert!(
        (aired_ms as f64 - plan.duration_s * 1000.0).abs() < 2.0,
        "the slot is {aired_ms} ms, the plan lasts {} s",
        plan.duration_s
    );
}

#[test]
fn songs_far_apart_in_tempo_join_with_an_echo_out() {
    let f = Fixture::new();
    let songs = vec![flat("s-slow", 100.0), flat("s-fast", 120.0)];
    build(&plugin_path(), &f.input(&songs, "landing", 900_000)).unwrap();
    let n = notes(&f.plan());
    assert_eq!(n.seams.len(), 1);
    assert_eq!(n.seams[0].recipe, "echo_out");
    assert_eq!(n.songs[1].ratio, 1.0);
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
    let n = notes(&f.plan());
    let order: Vec<&str> = n.songs.iter().map(|t| t.entry_id.as_str()).collect();
    if order[0] == "s-quiet" {
        assert_eq!(n.seams[0].recipe, "drop_mix", "{order:?}");
    } else {
        assert_ne!(n.seams[0].recipe, "drop_mix", "{order:?}");
    }
}

#[test]
fn a_start_entry_plays_each_song_from_its_first_second_and_never_bass_swaps() {
    let f = Fixture::new();
    build(&plugin_path(), &f.input(&four_songs(), "start", 600_000)).unwrap();
    let n = notes(&f.plan());
    assert!(n.songs.len() >= 2);
    for track in &n.songs {
        assert_eq!(track.enter_s, 0.0, "{}", track.entry_id);
    }
    for seam in &n.seams {
        assert_ne!(seam.recipe, "bass_swap", "{seam:?}");
    }
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
        !notes(&f.plan())
            .songs
            .iter()
            .any(|t| t.entry_id == "s-weak"),
        "s-weak is under the floor"
    );
}

#[test]
fn a_song_entered_by_a_bass_swap_rides_back_to_its_own_tempo() {
    let f = Fixture::new();
    let built = build(&plugin_path(), &f.input(&four_songs(), "landing", 600_000)).unwrap();
    let plan = f.plan();
    let n = notes(&plan);

    assert!(n.songs.len() >= 2, "{} songs", n.songs.len());
    assert_eq!(n.songs[0].ride_bars, 0);
    // Every swapped song that is not the last has room for the ride; the last
    // may have lost it to the end of the gap.
    let last = n.songs.len() - 1;
    for (i, track) in n
        .songs
        .iter()
        .enumerate()
        .skip(1)
        .take(last.saturating_sub(1))
    {
        if (track.ratio - 1.0).abs() > 1e-6 {
            assert_eq!(track.ride_bars, 16, "{} (track {i})", track.entry_id);
        }
    }
    assert!(
        n.songs.iter().skip(1).any(|t| t.ride_bars == 16),
        "tempos 118 to 122 differ, so at least one swap rides"
    );
    // The slot the plugin airs is the length the renderer lays the ride out to.
    let etv_station::bumper::timeline::Duration::Fixed(aired_ms) =
        built.timeline.segments[0].duration
    else {
        panic!("expected a fixed slot");
    };
    assert!(
        (aired_ms as f64 - plan.duration_s * 1000.0).abs() < 2.0,
        "the slot is {aired_ms} ms, the plan lasts {} s",
        plan.duration_s
    );
}

#[test]
fn ride_bars_zero_keeps_the_swaps_tempo_for_the_whole_song() {
    let f = Fixture::new();
    let mut input = f.input(&four_songs(), "landing", 600_000);
    input.config.as_mut().unwrap()["ride_bars"] = json!(0);
    build(&plugin_path(), &input).unwrap();
    let n = notes(&f.plan());
    assert!(n.songs.iter().all(|t| t.ride_bars == 0));
    assert!(n.songs.iter().skip(1).any(|t| (t.ratio - 1.0).abs() > 1e-6));
}

fn close(a: f64, b: f64, what: &str) {
    assert!((a - b).abs() < 1e-6, "{what}: {a} != {b}");
}

/// Two 120 bpm songs (2 s bars) joined by a 2-bar sweeping bass swap lay out as
/// the set arithmetic says: A's body through the phrase and half the swap, A's
/// tail without bass, B's bass in before its entry, B alone to the closing fade,
/// and the picture cut once a bar between the songs.
#[test]
fn a_two_song_set_lays_out_the_layers_and_cuts_of_a_bass_swap() {
    let f = Fixture::new();
    let songs = vec![flat("s-a", 120.0), flat("s-b", 120.0)];
    build(&plugin_path(), &f.input(&songs, "landing", 900_000)).unwrap();
    let plan = f.plan();
    let n = notes(&plan);
    assert_eq!(n.songs.len(), 2);
    assert_eq!(
        (
            n.seams[0].recipe.as_str(),
            n.seams[0].bars,
            n.seams[0].sweep
        ),
        ("bass_swap", 2, true)
    );
    let (a, b) = (&n.songs[0].entry_id, &n.songs[1].entry_id);
    let (ea, eb, p) = (n.songs[0].enter_s, n.songs[1].enter_s, n.seams[0].out_s);
    let bl = 240.0 / n.songs[0].native_bpm;
    let half = bl;
    let body = p - ea;
    let tp = body;
    let swap = tp + half;
    let last_len = n.end_s - eb;

    let names: Vec<&str> = plan.layers.iter().map(|l| l.name.as_str()).collect();
    assert_eq!(
        names,
        [
            format!("seam 1 ({a}) body"),
            format!("seam 1 ({a}) tail without bass"),
            format!("seam 1 ({b}) entering without bass"),
            format!("last song ({b})"),
        ]
    );
    let src = |i: usize| plan.layers[i].source.as_str();
    assert_eq!([src(0), src(1), src(2), src(3)], [a.as_str(), a, b, b]);
    let at = |i: usize, s: f64, o: f64, l: f64| {
        let layer = &plan.layers[i];
        close(layer.src_start_s, s, &layer.name);
        close(layer.out_start_s, o, &layer.name);
        close(layer.out_len_s, l, &layer.name);
        close(layer.ratio, 1.0, &layer.name);
        assert!(layer.ride.is_none(), "{}", layer.name);
    };
    at(0, ea, 0.0, body + half);
    at(1, p + half, swap, half);
    at(2, eb - half, tp, half);
    at(3, eb, swap, last_len);

    let fade = |i: usize, k: usize, kind: &str, start: f64, len: f64| {
        let x = &plan.layers[i].fades[k];
        assert_eq!(format!("{:?}", x.kind).to_lowercase(), kind);
        close(x.start_s, start, &plan.layers[i].name);
        close(x.len_s, len, &plan.layers[i].name);
        assert_eq!(x.curve, Curve::Linear);
    };
    assert_eq!(plan.layers[0].fades.len(), 1);
    fade(0, 0, "out", body + half - 0.02, 0.02);
    assert_eq!(plan.layers[1].fades.len(), 2);
    fade(1, 0, "in", 0.0, 0.02);
    fade(1, 1, "out", half / 2.0, half / 2.0);
    assert_eq!(plan.layers[2].fades.len(), 2);
    fade(2, 0, "in", 0.0, bl.min(half));
    fade(2, 1, "out", half - 0.02, 0.02);
    assert_eq!(plan.layers[3].fades.len(), 1);
    fade(3, 0, "out", last_len - 2.0, 2.0);

    assert!(plan.layers[0].filters.is_empty() && plan.layers[3].filters.is_empty());
    assert_eq!(
        plan.layers[1].filters,
        [
            Filter::Highpass { hz: 250.0 },
            Filter::Lowpass { hz: 1200.0 }
        ]
    );
    assert_eq!(plan.layers[2].filters, [Filter::Highpass { hz: 250.0 }]);

    // One bar of the swap: A for one beat, B for three.
    let beat = bl / 4.0;
    let cuts: Vec<(&str, f64, f64, f64)> = plan
        .cuts
        .iter()
        .map(|c| (c.source.as_str(), c.src_start_s, c.out_start_s, c.out_len_s))
        .collect();
    let want = [
        (a.as_str(), ea, 0.0, body),
        (a.as_str(), p, tp, beat),
        (b.as_str(), eb - half + beat, tp + beat, 3.0 * beat),
        (b.as_str(), eb, swap, last_len),
    ];
    assert_eq!(cuts.len(), want.len(), "{cuts:?}");
    for (got, want) in cuts.iter().zip(want) {
        assert_eq!(got.0, want.0);
        close(got.1, want.1, "cut src");
        close(got.2, want.2, "cut out");
        close(got.3, want.3, "cut len");
    }
    assert!(plan.cuts.iter().all(|c| (c.ratio - 1.0).abs() < 1e-9));
    close(plan.duration_s, swap + last_len, "duration");
}

// The set arithmetic, called on the plugin's own `layout` and `validate_set`
// with a hand-built set. 120 bpm songs have 2 s bars, so the numbers are easy to
// check; the expected values below are written out, not derived.

use rhai::{Dynamic, Engine, Scope};
use serde_json::Value;

fn call(name: &str, args: (Value, Value, f64)) -> Result<Dynamic, String> {
    let mut engine = Engine::new();
    // The host's limits (`hooks.rs`), so the script compiles as it does there.
    engine.set_max_expr_depths(128, 64);
    let ast = engine.compile_file(plugin_path()).unwrap();
    let arg = |v: Value| rhai::serde::to_dynamic(v).unwrap();
    engine
        .call_fn::<Dynamic>(
            &mut Scope::new(),
            &ast,
            name,
            (arg(args.0), arg(args.1), args.2),
        )
        .map_err(|e| e.to_string())
}

fn track(id: &str, bpm: f64, enter: f64, ratio: f64, ride: i64) -> Value {
    json!({ "item": { "entry_id": id, "audio": { "bpm": bpm } },
            "enter": enter, "ratio": ratio, "ride": ride, "at": 0.0 })
}

fn seam(recipe: &str, out_s: f64, bars: i64, sweep: bool) -> Value {
    json!({ "recipe": recipe, "out_s": out_s, "bars": bars, "sweep": sweep })
}

/// Song a (120 bpm, enters at 16 s) into song b (120 bpm, enters at 32 s), the
/// seam out of a at 96 s and the set ending at 80 s of b's file.
fn pair(recipe: &str, bars: i64, sweep: bool) -> (Value, Value, f64) {
    (
        json!([
            track("a", 120.0, 16.0, 1.0, 0),
            track("b", 120.0, 32.0, 1.0, 0)
        ]),
        json!([seam(recipe, 96.0, bars, sweep)]),
        80.0,
    )
}

fn laid(set: (Value, Value, f64)) -> Value {
    let out = call("layout", set).unwrap();
    rhai::serde::from_dynamic(&out).unwrap()
}

fn num(v: &Value) -> f64 {
    v.as_f64().unwrap_or_else(|| panic!("{v} is not a number"))
}

fn same(got: &Value, want: f64, what: &str) {
    close(num(got), want, what);
}

/// A layer's name, source start, result start and length.
fn layer(l: &Value, i: usize, name: &str, src: f64, out: f64, len: f64) {
    let x = &l["layers"][i];
    assert_eq!(x["name"], name);
    same(&x["src_start_s"], src, name);
    same(&x["out_start_s"], out, name);
    same(&x["out_len_s"], len, name);
}

fn fade(l: &Value, i: usize, k: usize, kind: &str, start: f64, len: f64) {
    let x = &l["layers"][i]["fades"][k];
    assert_eq!(x["kind"], kind, "layer {i} fade {k}");
    same(&x["start_s"], start, "fade start");
    same(&x["len_s"], len, "fade len");
    assert_eq!(x["curve"], "linear");
}

/// The picture cuts as (source, src start, result start, length).
fn cuts(l: &Value, want: &[(&str, f64, f64, f64)]) {
    let got = l["cuts"].as_array().unwrap();
    assert_eq!(got.len(), want.len(), "{got:?}");
    for (g, w) in got.iter().zip(want) {
        assert_eq!(g["source"], w.0);
        same(&g["src_start_s"], w.1, "cut src");
        same(&g["out_start_s"], w.2, "cut out");
        same(&g["out_len_s"], w.3, "cut len");
    }
}

#[test]
fn a_drop_mix_cuts_a_at_its_phrase_and_starts_b_on_the_beat() {
    let l = laid(pair("drop_mix", 0, false));
    // a plays 16 s to 96 s: 80 s. b plays 32 s to 80 s: 48 s.
    assert_eq!(l["layers"].as_array().unwrap().len(), 2);
    layer(&l, 0, "seam 1 (a) body", 16.0, 0.0, 80.0);
    fade(&l, 0, 0, "out", 79.99, 0.01);
    layer(&l, 1, "last song (b)", 32.0, 80.0, 48.0);
    fade(&l, 1, 0, "out", 46.0, 2.0);
    same(&l["duration_s"], 128.0, "duration");
    cuts(&l, &[("a", 16.0, 0.0, 80.0), ("b", 32.0, 80.0, 48.0)]);
}

#[test]
fn a_bass_swap_overlaps_the_two_songs_for_its_bars_and_keeps_their_tempos_locked() {
    let l = laid(pair("bass_swap", 4, false));
    // Four bars of 2 s: the swap lasts 8 s, half of it (4 s) before B takes over
    // alone at 80 s + 4 s.
    let names: Vec<&str> = l["layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "seam 1 (a) body",
            "seam 1 (a) tail without bass",
            "seam 1 (b) entering without bass",
            "last song (b)"
        ]
    );
    // a's body runs through the phrase and the half; its tail follows.
    layer(&l, 0, "seam 1 (a) body", 16.0, 0.0, 84.0);
    layer(&l, 1, "seam 1 (a) tail without bass", 100.0, 84.0, 4.0);
    // b's bass comes in 4 s of source before it enters at 32 s, at 80 s.
    layer(&l, 2, "seam 1 (b) entering without bass", 28.0, 80.0, 4.0);
    layer(&l, 3, "last song (b)", 32.0, 84.0, 48.0);
    assert_eq!(
        l["layers"][1]["filters"],
        json!([{ "kind": "highpass", "hz": 250.0 }])
    );
    assert_eq!(
        l["layers"][2]["filters"],
        json!([{ "kind": "highpass", "hz": 250.0 }])
    );
    same(&l["duration_s"], 132.0, "duration");
    // Two swap bars: the picture cuts a 2/2 bar, then a 1/3 bar.
    cuts(
        &l,
        &[
            ("a", 16.0, 0.0, 80.0),
            ("a", 96.0, 80.0, 1.0),
            ("b", 29.0, 81.0, 1.0),
            ("a", 98.0, 82.0, 0.5),
            ("b", 30.5, 82.5, 1.5),
            ("b", 32.0, 84.0, 48.0),
        ],
    );
}

#[test]
fn a_sweeping_bass_swap_narrows_the_tail_to_a_band() {
    let l = laid(pair("bass_swap", 2, true));
    assert_eq!(
        l["layers"][1]["filters"],
        json!([{ "kind": "highpass", "hz": 250.0 }, { "kind": "lowpass", "hz": 1200.0 }])
    );
}

#[test]
fn an_echo_out_rings_the_last_bar_into_the_next_song() {
    let l = laid(pair("echo_out", 0, false));
    // The body stops one bar (2 s) early; the echo covers that bar and two more.
    layer(&l, 0, "seam 1 (a) body", 16.0, 0.0, 78.0);
    fade(&l, 0, 0, "out", 77.98, 0.02);
    layer(&l, 1, "seam 1 (a) echo", 94.0, 78.0, 6.0);
    fade(&l, 1, 0, "in", 0.0, 0.02);
    fade(&l, 1, 1, "out", 2.0, 4.0);
    // A 2 s bar's beat is 0.5 s; the echo delay is three quarters of it.
    let echo = &l["layers"][1]["filters"][0];
    assert_eq!(echo["kind"], "echo");
    same(&echo["in_gain"], 0.8, "in_gain");
    same(&echo["out_gain"], 0.7, "out_gain");
    same(&echo["delay_ms"], 375.0, "delay_ms");
    same(&echo["decay"], 0.55, "decay");
    layer(&l, 2, "last song (b)", 32.0, 80.0, 48.0);
    same(&l["duration_s"], 128.0, "duration");
}

#[test]
fn a_songs_ratio_stretches_its_body_in_the_set() {
    let (mut tracks, seams, end) = pair("drop_mix", 0, false);
    tracks[0]["ratio"] = json!(1.25);
    let l = laid((tracks, seams, end));
    // 80 s of source at 1.25x is 64 s of set.
    layer(&l, 0, "seam 1 (a) body", 16.0, 0.0, 64.0);
    same(&l["layers"][0]["ratio"], 1.25, "ratio");
    layer(&l, 1, "last song (b)", 32.0, 64.0, 48.0);
    same(&l["duration_s"], 112.0, "duration");
    cuts(&l, &[("a", 16.0, 0.0, 64.0), ("b", 32.0, 64.0, 48.0)]);
    same(&l["cuts"][0]["ratio"], 1.25, "cut ratio");
}

/// The picture has no gap and no overlap: each cut starts where the last ended
/// and the last ends with the set.
fn assert_picture_whole(l: &Value, what: &str) {
    let mut end = 0.0;
    for c in l["cuts"].as_array().unwrap() {
        close(num(&c["out_start_s"]), end, what);
        assert!(num(&c["out_len_s"]) > 0.0, "{what}: empty cut {c}");
        end = num(&c["out_start_s"]) + num(&c["out_len_s"]);
    }
    close(end, num(&l["duration_s"]), what);
}

#[test]
fn the_picture_has_no_gap_and_no_overlap_in_any_recipe() {
    for (recipe, bars, sweep) in [
        ("bass_swap", 2, false),
        ("bass_swap", 2, true),
        ("bass_swap", 4, false),
        ("echo_out", 0, false),
        ("drop_mix", 0, false),
    ] {
        assert_picture_whole(&laid(pair(recipe, bars, sweep)), recipe);
    }
    assert_picture_whole(&laid(riding()), "riding");
}

/// Song b is 108 bpm (2.222 s bars) playing at a's 120 bpm after a bass swap,
/// then riding back to 108 over 4 bars.
fn riding() -> (Value, Value, f64) {
    (
        json!([
            track("a", 120.0, 16.0, 1.0, 0),
            track("b", 108.0, 32.0, 120.0 / 108.0, 4)
        ]),
        json!([seam("bass_swap", 96.0, 2, false)]),
        80.0,
    )
}

#[test]
fn a_bass_swap_into_a_song_at_another_tempo_reads_b_at_the_running_tempo() {
    let l = laid(riding());
    // The swap is one 2 s half: b enters at 120/108 = 1.1111x, and its bass comes
    // in 2 s of set, 2.2222 s of file, before its entry at 32 s.
    layer(&l, 1, "seam 1 (a) tail without bass", 98.0, 82.0, 2.0);
    layer(
        &l,
        2,
        "seam 1 (b) entering without bass",
        29.77777777777778,
        80.0,
        2.0,
    );
    same(
        &l["layers"][2]["ratio"],
        1.1111111111111112,
        "bass-in ratio",
    );
    same(&l["layers"][1]["ratio"], 1.0, "tail ratio");
    assert!(l["layers"][2]["ride"].is_null());
    // B's third of the bar (3 beats of 0.5 s) is read at the same ratio.
    cuts_at(
        &l,
        &[
            (0, "a", 16.0, 0.0, 80.0),
            (1, "a", 96.0, 80.0, 0.5),
            (2, "b", 30.333333333333336, 80.5, 1.5),
        ],
    );
    same(&l["cuts"][2]["ratio"], 1.1111111111111112, "cut ratio");
}

/// The cuts at the given indexes only.
fn cuts_at(l: &Value, want: &[(usize, &str, f64, f64, f64)]) {
    for (i, source, src, out, len) in want {
        let c = &l["cuts"][*i];
        assert_eq!(c["source"], *source, "cut {i}");
        same(&c["src_start_s"], *src, "cut src");
        same(&c["out_start_s"], *out, "cut out");
        same(&c["out_len_s"], *len, "cut len");
    }
}

#[test]
fn a_rides_last_song_reads_the_file_the_picture_and_sound_agree_on() {
    let l = laid(riding());
    let last = &l["layers"][3];
    assert_eq!(last["name"], "last song (b)");
    let ride = &last["ride"];
    let ratios: Vec<f64> = ride["ratios"].as_array().unwrap().iter().map(num).collect();
    let want = [
        1.0833333333333335,
        1.0555555555555556,
        1.0277777777777777,
        1.0,
    ];
    assert_eq!(ratios.len(), 4);
    for (g, w) in ratios.iter().zip(want) {
        close(*g, w, "ride ratio");
    }
    same(&ride["step_src_s"], 2.2222222222222223, "ride step");
    // end_s 80 s minus the 32 s entry, 4 bars of which ride: 48 s of file read in
    // 47.652 s of set.
    same(&last["out_len_s"], 47.65204070467229, "last song length");
    let step = num(&ride["step_src_s"]);
    let ride_out: f64 = ratios.iter().map(|r| step / r).sum();
    close(ride_out, 8.540929593561172, "ride out");
    close(
        4.0 * step + (num(&last["out_len_s"]) - ride_out),
        48.0,
        "file read",
    );
    same(&l["duration_s"], 129.6520407046723, "duration");
    // One picture cut per ride bar, each reading one file bar, then one for the
    // rest at native tempo.
    let all = l["cuts"].as_array().unwrap();
    let pics = &all[3..];
    assert_eq!(pics.len(), 5);
    for (c, r) in pics.iter().zip(want) {
        same(&c["ratio"], r, "cut ratio");
        close(
            num(&c["out_len_s"]) * num(&c["ratio"]),
            2.2222222222222223,
            "bar",
        );
    }
    same(&pics[4]["ratio"], 1.0, "native tempo");
    same(&pics[4]["src_start_s"], 40.888888888888886, "rest src");
    same(&pics[4]["out_len_s"], 39.111111111111114, "rest len");
    assert_picture_whole(&l, "riding");
}

#[test]
fn a_valid_set_passes_the_checks() {
    call("validate_set", pair("bass_swap", 4, false))
        .map(drop)
        .expect("a valid set passes");
    call("validate_set", pair("echo_out", 0, false))
        .map(drop)
        .expect("a valid set passes");
    call("validate_set", riding())
        .map(drop)
        .expect("a valid set passes");
}

fn refused(set: (Value, Value, f64), needle: &str) {
    let err = call("validate_set", set).unwrap_err();
    assert!(err.contains("whole_song: "), "{err}");
    assert!(err.contains(needle), "{err:?} lacks {needle:?}");
}

#[test]
fn a_song_without_a_tempo_is_refused() {
    let (mut tracks, seams, end) = pair("drop_mix", 0, false);
    tracks[1]["item"]["audio"]["bpm"] = json!(0.0);
    refused((tracks, seams, end), "native_bpm must be positive");
}

#[test]
fn a_song_entered_before_the_start_of_its_file_is_refused() {
    let (mut tracks, seams, end) = pair("drop_mix", 0, false);
    tracks[0]["enter"] = json!(-1.0);
    refused((tracks, seams, end), "enter_s must not be negative");
}

#[test]
fn a_bass_swap_tail_longer_than_the_closing_body_is_refused() {
    // 4 bars of 2 s: a 4 s tail. b plays 32 s to 35 s at the closing seam: 3 s.
    let (tracks, seams, _) = pair("bass_swap", 4, false);
    refused((tracks, seams, 35.0), "the set ends");
}

#[test]
fn a_bass_swap_tail_longer_than_the_next_body_is_refused() {
    let tracks = json!([
        track("a", 120.0, 16.0, 1.0, 0),
        track("b", 120.0, 32.0, 1.0, 0),
        track("c", 120.0, 32.0, 1.0, 0)
    ]);
    let with = |out: f64| {
        (
            tracks.clone(),
            json!([
                seam("bass_swap", 96.0, 4, false),
                seam("drop_mix", out, 0, false)
            ]),
            80.0,
        )
    };
    refused(with(32.0 + 3.0), "the next seam");
    call("validate_set", with(32.0 + 4.0))
        .map(drop)
        .expect("a valid set passes");
}

#[test]
fn the_last_song_needs_room_for_its_closing_fade() {
    let (tracks, seams, _) = pair("drop_mix", 0, false);
    refused((tracks, seams, 32.5), "closing fade");
}

#[test]
fn a_ride_that_does_not_finish_before_the_closing_fade_is_refused() {
    let (tracks, seams, _) = riding();
    refused((tracks, seams, 32.0 + 9.0), "tempo ride");
}

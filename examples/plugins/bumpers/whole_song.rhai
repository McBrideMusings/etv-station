// A whole-song bumper: fills a gap with music videos played whole, one after
// another, each pair joined by a seam recipe chosen by rule, and hands the set
// plan to the renderer's queue (docs/schema.md § Set plan).
//
// config (the block's `fill:` entry):
//   expr:            CEL over the catalog naming the pool, as a pool's `expr` does (required)
//   dir:             folder for plans, renders and the stand-in (required)
//   entry:           where a song enters the set: "landing" (its landing bar, default) or
//                    "start" (the start of the file, so the song plays whole)
//   floor:           lowest `beat_confidence` a song may have, default 2.5
//   steady_beats:    fewest beats in a song's steady run, default 32
//   tail:            recent sets whose songs are not reused, default 12
//   tempo_gap_pct:   a gap wider than this between the running tempo and the next
//                    song's, in percent, falls back to echo_out, default 8
//   drop_step_db:    an arrival this many dB louder than the exit is a drop_mix, default 3
//   artist_field:    item field holding the artist for the card, default "show"
//   card_script:     overlay script that draws the card copy, relative to the
//                    channel config's directory; unset carries no overlay
//   enqueue:         program run as `<enqueue> <enqueue_args...> enqueue <plan> --air-at <t>`,
//                    default "etv-station"
//   enqueue_args:    extra arguments before `enqueue`, default []
//   ffmpeg:          program that makes the stand-in, default "ffmpeg"
//
// The recipe per seam, as the autodj spike's round 4 settled it: echo_out when the
// tempos are more than `tempo_gap_pct` apart or either beat is unsure; drop_mix
// when the arrival is a step up in energy; otherwise bass_swap, 2 bars with a
// sweep when the arrival is near the song's peak, 4 bars without. A song that
// enters at its start has no lead-in for a bass swap, so that seam is a drop_mix
// on a step up and an echo_out otherwise.
//
// Beside each render it writes <stem>.plan.json and <stem>.audit.txt (one line
// per seam). The segment is named `chapter:<job>|<entry>|...` so `GET
// /api/channels/<n>/chapters`, `admin audit` and the `tail` check read it the
// way they read a megamix chapter.
fn hooks() { ["bumper"] }
fn capabilities() { ["probe", "catalog_read", "exec"] }

// The fewest bars of a song's body before a seam out of it.
fn min_body_bars() { 4 }
// Seconds a last song needs between its entry and the end of the set.
fn min_last_s() { 6.0 }
fn end_margin_s() { 0.5 }

fn num(v) { v * 1.0 }

fn conf(cfg, key, fallback) {
    let v = cfg[key];
    if v == () { fallback } else { v }
}

fn db(x) { 20.0 * (num(x) + 0.000000001).log() }

fn fnv(text) {
    let h = 2166136261;
    for ch in text.chars() {
        h = ((h ^ ch.to_int()) * 16777619) & 0xFFFFFFFF;
    }
    h
}

fn pad2(n) { if n < 10 { "0" + n } else { n.to_string() } }

// Unix seconds as an RFC 3339 UTC time, the form `enqueue --air-at` reads.
fn rfc3339(secs) {
    let days = secs / 86400;
    let rem = secs % 86400;
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 { y += 1; }
    y.to_string() + "-" + pad2(m) + "-" + pad2(d) + "T" + pad2(rem / 3600) + ":" + pad2((rem % 3600) / 60) + ":" + pad2(rem % 60) + "Z"
}

// The song's bar grid: seconds of its first bar and of each bar after it, as the
// analyzer numbered them (bar 0 is the first downbeat at or after 0 s).
fn bar_seconds(item) { 4.0 * num(item.audio.beat_grid.slope) }

fn first_bar(item) {
    let g = item.audio.beat_grid;
    let slope = num(g.slope);
    let k = item.audio.downbeat_phase;
    while num(g.intercept) + slope * k < 0.0 { k += 4; }
    while num(g.intercept) + slope * (k - 4) >= 0.0 { k -= 4; }
    num(g.intercept) + slope * k
}

fn bar_time(item, i) { first_bar(item) + i * bar_seconds(item) }

// The latest 8-bar boundary after `after_s` with `room_s` of song left behind it
// and music in the four bars before it, or () when there is none.
fn phrase_out(item, after_s, room_s) {
    let energy = item.audio.bar_energy;
    let duration = num(item.duration_ms) / 1000.0;
    let best = ();
    let i = 8;
    while i < energy.len() {
        let t = bar_time(item, i);
        if t > after_s && t + room_s <= duration {
            let live = true;
            for j in (i - 4)..i { if num(energy[j]) <= 0.0 { live = false; } }
            if live { best = i; }
        }
        i += 8;
    }
    best
}

fn entry_of(item, cfg) {
    if conf(cfg, "entry", "landing") == "start" { 0.0 } else { num(item.audio.landing_bar_s) }
}

fn eligible(item, cfg) {
    let a = item.audio;
    if a == () || item.path == () || item.duration_ms == () { return false; }
    if a.bpm == () || a.beat_grid == () || a.downbeat_phase == () || a.bar_energy == () { return false; }
    if a.beat_confidence == () || num(a.beat_confidence) < num(conf(cfg, "floor", 2.5)) { return false; }
    if a.beat_grid.beats == () || a.beat_grid.beats < conf(cfg, "steady_beats", 32) { return false; }
    if conf(cfg, "entry", "landing") != "start" && a.landing_bar_s == () { return false; }
    let enter = entry_of(item, cfg);
    // A body, then room for a seam out of it.
    phrase_out(item, enter + min_body_bars() * bar_seconds(item), 5.0 * bar_seconds(item)) != ()
}

fn recent_entries(ctx, tail) {
    let seen = #{};
    let sets = 0;
    for row in ctx.airings(#{ kind: "bumper", order: "desc", limit: 200 }) {
        if !row.entry_id.starts_with("chapter:") { continue; }
        let parts = row.entry_id.split("|");
        for i in 1..parts.len() { seen[parts[i]] = true; }
        sets += 1;
        if sets >= tail { break; }
    }
    seen
}

// The recipe for the seam from `a` (playing at `ratio`, entering at `enter`) into
// `b` (entering at `b_enter`), with the numbers behind the choice.
fn choose(a, ratio, b, b_enter, cfg) {
    let running = num(a.audio.bpm) * ratio;
    let gap = (running - num(b.audio.bpm)).abs() / running;
    let a_bar = bar_seconds(a);
    let po = phrase_out(a, 0.0, 5.0 * a_bar);
    let exit_e = 0.0;
    for j in (po - 4)..po { exit_e += num(a.audio.bar_energy[j]) / 4.0; }
    let b_bar = if conf(cfg, "entry", "landing") == "start" { 0 } else { b.audio.landing_bar };
    let arrive_e = num(b.audio.bar_energy[b_bar]);
    let peak = 0.0;
    let i = 8;
    while i < b.audio.bar_energy.len() {
        if num(b.audio.bar_energy[i]) > peak { peak = num(b.audio.bar_energy[i]); }
        i += 4;
    }
    let step = db(arrive_e) - db(exit_e);
    let recipe = "bass_swap";
    let bars = 4;
    let sweep = false;
    if gap > num(conf(cfg, "tempo_gap_pct", 8.0)) / 100.0
        || num(a.audio.beat_confidence) < 1.5 || num(b.audio.beat_confidence) < 1.5 {
        recipe = "echo_out";
    } else if step > num(conf(cfg, "drop_step_db", 3.0)) {
        recipe = "drop_mix";
    } else if arrive_e >= 0.9 * peak {
        bars = 2;
        sweep = true;
    }
    if recipe == "bass_swap" {
        // The swap brings B's bass in before its entry: it needs that much file.
        let half = bars / 2.0 * (a_bar / ratio);
        let rb = running / num(b.audio.bpm);
        if b_enter < half * rb - 0.000001 {
            recipe = if step > num(conf(cfg, "drop_step_db", 3.0)) { "drop_mix" } else { "echo_out" };
        }
    }
    #{
        recipe: recipe, bars: if recipe == "bass_swap" { bars } else { 0 }, sweep: recipe == "bass_swap" && sweep,
        gap_pct: (gap * 1000.0).round() / 10.0, step_db: (step * 10.0).round() / 10.0, running_bpm: (running * 10.0).round() / 10.0,
    }
}

fn cfg_dir_of(path) {
    let at = path.len() - 1;
    while at > 0 && path.sub_string(at, 1) != "/" { at -= 1; }
    if at == 0 { "." } else { path.sub_string(0, at) }
}

fn write_file(ctx, path, text) {
    let made = ctx.exec("mkdir", ["-p", cfg_dir_of(path)]);
    if made.code != 0 { throw "whole_song: could not make the folder for " + path + ": " + made.stderr; }
    let r = ctx.exec("sh", ["-c", "printf %s \"$1\" > \"$2\"", "sh", text, path]);
    if r.code != 0 { throw "whole_song: could not write " + path + ": " + r.stderr; }
}

// The id the renderer filed the plan under, or () when the renderer is absent.
fn enqueue(ctx, cfg, plan_path, air_at) {
    let program = conf(cfg, "enqueue", "etv-station");
    let args = [];
    for a in conf(cfg, "enqueue_args", []) { args.push(a); }
    args.push("enqueue");
    args.push(plan_path);
    if air_at != () {
        args.push("--air-at");
        args.push(rfc3339(air_at));
    }
    try {
        let r = ctx.exec(program, args);
        if r.code == 0 {
            // trim() works in place
            let id = r.stdout;
            id.trim();
            return id;
        }
        log("whole_song: enqueue failed code=" + r.code + " stderr=" + r.stderr);
    } catch (e) {
        log("whole_song: enqueue unavailable: " + e);
    }
    ()
}

// A ten-second black clip with a brown-noise bed; a pending set loops it.
fn stand_in(ctx, cfg) {
    let path = cfg.dir + "/stand-in.mp4";
    if ctx.probe(path) != () { return path; }
    try {
        let r = ctx.exec(conf(cfg, "ffmpeg", "ffmpeg"), [
            "-hide_banner", "-loglevel", "error", "-n",
            "-f", "lavfi", "-i", "color=c=black:s=1280x720:r=24",
            "-f", "lavfi", "-i", "anoisesrc=color=brown:amplitude=0.05,lowpass=f=180",
            "-t", "10", "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p",
            "-c:a", "aac", "-shortest", path,
        ]);
        if r.code == 0 { return path; }
        log("whole_song: stand-in not made code=" + r.code + " stderr=" + r.stderr);
    } catch (e) {
        log("whole_song: stand-in not made: " + e);
    }
    ()
}

fn idle(ctx, why) {
    log("whole_song: idle " + why);
    let t = timeline();
    t.push(segment().black().looped().fixed_ms(ctx.gap_ms).name("whole-song-idle"));
    t
}

fn without(items, entry_id) {
    let kept = [];
    for it in items { if it.entry_id != entry_id { kept.push(it); } }
    kept
}

// The song after `a`: one of the three nearest in tempo to the running tempo.
fn next_song(ctx, rest, running) {
    let ranked = rest;
    ranked.sort(|x, y| {
        let dx = (num(x.audio.bpm) - running).abs();
        let dy = (num(y.audio.bpm) - running).abs();
        if dx < dy { -1 } else if dx > dy { 1 } else if x.entry_id < y.entry_id { -1 } else { 1 }
    });
    let n = if ranked.len() < 3 { ranked.len() } else { 3 };
    ranked[ctx.rng.int(0, n - 1)]
}

fn build(ctx) {
    let cfg = ctx.config;
    if cfg.expr == () { throw "whole_song: config.expr (the pool's CEL) is required"; }
    if cfg.dir == () { throw "whole_song: config.dir (where plans and renders go) is required"; }
    let entry = conf(cfg, "entry", "landing");
    if entry != "landing" && entry != "start" {
        throw "whole_song: config.entry must be \"landing\" or \"start\", got " + entry;
    }

    let recent = recent_entries(ctx, conf(cfg, "tail", 12));
    let cands = [];
    let pool = ctx.catalog(cfg.expr);
    for item in pool {
        if !eligible(item, cfg) { continue; }
        if recent.contains(item.entry_id) { continue; }
        cands.push(item);
    }
    log("whole_song: pool=" + pool.len() + " candidates=" + cands.len());
    if cands.len() == 0 { return idle(ctx, "no candidates"); }

    let gap_s = ctx.gap_ms / 1000.0;
    let first = cands.remove(ctx.rng.int(0, cands.len() - 1));

    // Walk the set song by song, tracking where it is on the timeline exactly as
    // the renderer lays it out (crates/etv-render/src/set.rs).
    let songs = [first];
    let tracks = [#{ item: first, enter: entry_of(first, cfg), ratio: 1.0, at: 0.0 }];
    let seams = [];
    let notes = [];
    let t = 0.0;
    let end_s = ();
    while end_s == () {
        let cur = tracks[tracks.len() - 1];
        let a = cur.item;
        let room = (num(a.duration_ms) / 1000.0 - end_margin_s() - cur.enter) / cur.ratio;
        if t + room >= gap_s || cands.len() == 0 {
            let play = if t + room >= gap_s { gap_s - t } else { room };
            end_s = cur.enter + play * cur.ratio;
            break;
        }
        let b = next_song(ctx, cands, num(a.audio.bpm) * cur.ratio);
        let b_enter = entry_of(b, cfg);
        let pick = choose(a, cur.ratio, b, b_enter, cfg);
        let a_bar = bar_seconds(a);
        let room_bars = if pick.recipe == "bass_swap" { pick.bars + 1 } else if pick.recipe == "echo_out" { 2 } else { 4 };
        let po = phrase_out(a, cur.enter + min_body_bars() * a_bar, room_bars * a_bar);
        if po == () {
            // This song cannot be left from here; end the set on it.
            end_s = cur.enter + room * cur.ratio;
            break;
        }
        let out_s = bar_time(a, po);
        let bl = a_bar / cur.ratio;
        let body = (out_s - cur.enter) / cur.ratio;
        let half = pick.bars / 2.0 * bl;
        let landed = t + body + (if pick.recipe == "bass_swap" { half } else { 0.0 });
        if landed > gap_s - min_last_s() {
            end_s = cur.enter + room * cur.ratio;
            break;
        }
        let b_ratio = if pick.recipe == "bass_swap" { num(a.audio.bpm) * cur.ratio / num(b.audio.bpm) } else { 1.0 };
        seams.push(#{ recipe: pick.recipe, out_s: out_s });
        if pick.recipe == "bass_swap" {
            seams[seams.len() - 1].bars = pick.bars;
            seams[seams.len() - 1].sweep = pick.sweep;
        }
        notes.push("seam " + seams.len() + " at " + (t + body) + "s: " + pick.recipe
            + (if pick.recipe == "bass_swap" { " " + pick.bars + " bars" + (if pick.sweep { " sweep" } else { "" }) } else { "" })
            + ", " + a.title + " (" + a.entry_id + ") out at source bar " + po + " -> " + b.title + " (" + b.entry_id + ") enters at "
            + b_enter + "s, tempo gap " + pick.gap_pct + "%, energy step " + pick.step_db + " dB, running " + pick.running_bpm + " bpm\n");
        songs.push(b);
        cands = without(cands, b.entry_id);
        tracks.push(#{ item: b, enter: b_enter, ratio: b_ratio, at: t + body });
        t = landed;
    }
    let last = tracks[tracks.len() - 1];
    let set_s = t + (end_s - last.enter) / last.ratio;

    let ids = "";
    for s in songs { ids += s.entry_id + "|"; }
    let stem = "ch" + ctx.channel.number + "-set-" + fnv(ids + entry).to_hex();
    let plan = #{
        kind: "set",
        tracks: [],
        seams: seams,
        end_s: end_s,
        cards: [],
        output: cfg.dir + "/" + stem + ".mp4",
    };
    let fields = conf(cfg, "artist_field", "show");
    for tr in tracks {
        let it = tr.item;
        plan.tracks.push(#{ entry_id: it.entry_id, path: it.path, native_bpm: num(it.audio.bpm), enter_s: tr.enter, ratio: tr.ratio });
        let card = #{
            at_s: tr.at,
            artist: (it[fields] ?? it.show ?? it.studio ?? "").to_string(),
            title: (it.title ?? "").to_string(),
        };
        if it.year != () { card.year = it.year; }
        plan.cards.push(card);
    }
    let plan_path = cfg.dir + "/" + stem + ".plan.json";
    write_file(ctx, plan_path, to_json(plan));
    let text = "";
    for n in notes { text += n; }
    if text == "" { text = "no seams: " + songs[0].title + " (" + songs[0].entry_id + ") fills the gap\n"; }
    write_file(ctx, cfg.dir + "/" + stem + ".audit.txt", text);

    let air_at = if ctx.next == () { () } else { ctx.next.start - ctx.gap_ms / 1000 };
    let job = enqueue(ctx, cfg, plan_path, air_at);
    let set_ms = (set_s * 1000.0).round().to_int();

    let names = "chapter:" + (if job == () { "unqueued-" + stem } else { job });
    for s in songs { names += "|" + s.entry_id; }
    log("whole_song: set=" + names + " songs=" + songs.len() + " seconds=" + set_s + " queued=" + (job != ()));
    for n in notes { log("whole_song: " + n); }

    let t_line = timeline();
    let standing = stand_in(ctx, cfg);
    let seg = segment();
    if standing == () {
        seg = seg.black().looped();
    } else {
        seg = seg.pending(plan.output, standing).looped().own_audio();
    }
    seg = seg.fixed_ms(set_ms).name(names);
    if cfg.card_script != () {
        seg = seg.overlay(cfg.card_script, #{ cards: plan.cards });
    }
    t_line.push(seg);
    // A set that ends before the gap does leaves the rest black.
    if ctx.gap_ms - set_ms > 1000 {
        t_line.push(segment().black().looped().fixed_ms(ctx.gap_ms - set_ms).name("whole-song-rest"));
    }
    t_line
}

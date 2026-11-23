// A whole-song bumper: fills a gap with music videos played whole, one after
// another, each pair joined by a mixing strategy, and hands the render plan (the
// audio layers and picture cuts, docs/schema.md § Render plan) to the renderer's
// queue. The strategies are megamix/strategies.rhai's, drawn per seam from
// `config.strategies`; the picture is megamix/picture.rhai's, cut from the
// layers. The plan's `notes` keep the songs and seams for audit.
//
// config (the block's `fill:` entry):
//   expr:            CEL over the catalog naming the pool, as a pool's `expr` does (required)
//   dir:             folder for plans, renders and the stand-in (required)
//   entry:           where a song enters the set: "landing" (its landing bar, default) or
//                    "start" (the start of the file, so the song plays whole)
//   floor:           lowest `beat_confidence` a song may have, default 2.5
//   steady_beats:    fewest beats in a song's steady run, default 32
//   tail:            recent sets whose songs are not reused, default 12
//   strategies:      [{ name, weight, when }] drawn per seam among bass_swap, echo_out,
//                    drop_mix, blend and whole (megamix/strategies.rhai); default
//                    default_strategies() below
//   ride_bars:       bars over which a song that entered at the outgoing song's tempo
//                    (bass_swap, blend) steps back to its own, default 16; 0 keeps the
//                    entry tempo for the rest of the song
//   picture_margin_db, picture_hold_beats: as megamix.rhai's, one bar being the
//                    first song's
//   artist_field:    item field holding the artist for the card, default "show"
//   card_script:     overlay script that draws the card copy, relative to the
//                    channel config's directory; unset carries no overlay
//   ffmpeg:          program that makes the stand-in, default "ffmpeg"
//
// A song leaves at an 8-bar phrase boundary with room for its seam, or, joined by
// `whole`, at the end of its file. After a bass swap or a blend the incoming song
// plays at the outgoing song's tempo and rides back to its own over `ride_bars`
// bars, so the next seam starts from the song's native tempo; a song with no room
// for the ride before its seam or the end of the set keeps the entry tempo.
//
// Beside each render it writes <stem>.plan.json and <stem>.audit.txt (one line
// per seam). The segment is pending on <stem>.mp4 with <stem>.plan.json
// as its plan, which the render sidecar queues as the set's air time nears. It is
// named `chapter:<stem>|<entry>|...` so `GET
// /api/channels/<n>/chapters`, `admin audit` and the `tail` check read it the
// way they read a megamix chapter.
fn hooks() { ["bumper"] }
fn capabilities() { ["probe", "catalog_read", "exec"] }

// The autodj spike's rule: echo_out when the tempos are more than 8 % apart, a
// beat is unsure, or a bass swap has no lead-in to play; drop_mix when the
// arrival is more than 3 dB louder than the exit; otherwise bass_swap.
fn default_strategies() {
    let unsure = "seam.tempo_gap_pct > 8.0 || a.audio.beat_confidence < 1.5 || b.audio.beat_confidence < 1.5";
    [
        #{ name: "echo_out", when: unsure + " || (seam.energy_step_db <= 3.0 && !seam.fits.bass_swap)" },
        #{ name: "drop_mix", when: "!(" + unsure + ") && seam.energy_step_db > 3.0" },
        #{ name: "bass_swap", when: "!(" + unsure + ") && seam.energy_step_db <= 3.0" },
    ]
}

fn allowed() { ["bass_swap", "echo_out", "drop_mix", "blend", "whole"] }

// Strategies after which the incoming song plays at the outgoing song's tempo.
fn locks_tempo(name) { name == "bass_swap" || name == "blend" }

// Bars of A's file the walk keeps after A's exit point for each strategy.
fn room_bars(name, joined) {
    switch name {
        "bass_swap" => joined.bars + 1,
        "echo_out" => 2,
        "drop_mix" => 4,
        _ => 1,
    }
}

// The fewest bars of a song's body before a seam out of it.
fn min_body_bars() { 4 }
// Seconds a last song needs between its entry and the end of the set.
fn min_last_s() { 6.0 }
fn end_margin_s() { 0.5 }
fn last_fade_s() { 2.0 }
fn max_ride_bars() { 64 }

fn num(v) { v * 1.0 }

fn conf(cfg, key, fallback) {
    let v = cfg[key];
    if v == () { fallback } else { v }
}

// Config `ride_bars` as a whole number of bars, 0 (no ride) to 64; a ride is one
// step per bar and the plan takes 1 to 64 steps.
fn ride_bars_of(cfg) {
    let n = conf(cfg, "ride_bars", 16).to_int();
    if n < 0 { 0 } else if n > max_ride_bars() { max_ride_bars() } else { n }
}

// Seconds in one bar of a song's file at its native tempo (240 / bpm).
fn file_bar(item) { 240.0 / num(item.audio.bpm) }

// The ratio of each bar of a track's tempo ride: a straight line from the entry
// ratio to 1.0, the last step native.
fn ride_ratios(tr) {
    let out = [];
    for i in 1..=tr.ride { out.push(tr.ratio + (1.0 - tr.ratio) * i / tr.ride); }
    out
}

fn ride_src(tr) { tr.ride * file_bar(tr.item) }

fn ride_out(tr) {
    let total = 0.0;
    if tr.ride > 0 { for r in ride_ratios(tr) { total += file_bar(tr.item) / r; } }
    total
}

// The ratio a track plays at once its ride is over.
fn exit_ratio(tr) { if tr.ride > 0 { 1.0 } else { tr.ratio } }

// Set seconds taken to play `src_s` seconds of the file from where the song enters.
fn play_out(tr, src_s) {
    if tr.ride > 0 { ride_out(tr) + (src_s - ride_src(tr)) } else { src_s / tr.ratio }
}

// Where the set stops in a last song that has `play` set seconds. A gap that ends
// before the ride has finished and left a closing fade drops the ride.
fn finish(tr, play) {
    if tr.ride > 0 && play >= ride_out(tr) + 3.0 {
        #{ end_s: tr.enter + ride_src(tr) + (play - ride_out(tr)), ride: tr.ride }
    } else {
        #{ end_s: tr.enter + play * tr.ratio, ride: 0 }
    }
}

fn rides(tr) { tr.ride > 0 && (tr.ratio - 1.0).abs() > 0.000000001 }

// Seconds in one bar as the set plays it when the song leaves.
fn bar_out(tr) { file_bar(tr.item) / exit_ratio(tr) }

fn ride_of(tr) {
    if rides(tr) { #{ step_src_s: file_bar(tr.item), ratios: ride_ratios(tr) } } else { () }
}

fn fade(kind, start, len) {
    #{ kind: kind, start_s: if start < 0.0 { 0.0 } else { start }, len_s: len, curve: "linear" }
}

fn mk_layer(name, tr, src, out, len, ratio) {
    #{ name: name, source: tr.item.entry_id, src_start_s: src, out_start_s: out, out_len_s: len,
       ratio: ratio, fades: [], filters: [] }
}

fn bad(msg) { throw "whole_song: " + msg; }

// What a strategy is told about the seam from track `a`, leaving its file at
// `out_s`, into track `b` (docs in megamix/strategies.rhai). A's `src_s` is its
// source second at the arrival, `exit_to_frame_s` after it leaves.
fn join_input(k, a, b, out_s, exit_to_frame_s, before_s, facts, cfg) {
    let ra = exit_ratio(a);
    #{
        a: #{ label: "seam " + (k + 1) + " (" + a.item.entry_id + ")", id: a.item.entry_id,
              src_s: out_s + exit_to_frame_s * ra, ratio: ra, before_s: before_s },
        b: #{ label: "seam " + (k + 1) + " (" + b.item.entry_id + ")", id: b.item.entry_id,
              src_s: b.enter, ratio: b.ratio, before_s: 0.0 },
        bar_s: bar_out(a), full_db: (), stems: false, seam: facts, cfg: cfg,
    }
}

// Strategy `name` joining `a` (leaving at `out_s`) into `b`, placed in its own
// frame: it is called once to learn how long after A's exit its arrival falls,
// then with A read from there.
fn join_at(name, k, a, b, out_s, before_s, facts, cfg) {
    import "megamix/strategies" as strategies;
    let first = strategies::join(name, join_input(k, a, b, out_s, 0.0, before_s, facts, cfg));
    if first == () { return (); }
    strategies::join(name, join_input(k, a, b, out_s, num(first.exit_to_frame_s), before_s, facts, cfg))
}

// Whether every layer of a join reads inside its file.
fn reads_inside(joined, files) {
    for l in joined.layers {
        let src = num(l.src_start_s);
        if src < 0.0 || src + num(l.out_len_s) * num(l.ratio ?? 1.0) > files[l.source] + 0.000001 { return false; }
    }
    true
}

// The checks on a set that the walk in `build` does not settle by construction:
// the facts a song's row supplies (tempo, entry time, length), that the song
// after each seam plays on its own for as long as the outgoing song's seam layers
// sound over it, and the room the last song has.
fn validate_set(tracks, seams, end_s, cfg) {
    import "megamix/strategies" as strategies;
    for i in 0..tracks.len() {
        let t = tracks[i];
        let at = "tracks[" + i + "] (" + t.item.entry_id + ")";
        if !(num(t.item.audio.bpm) > 0.0) { bad(at + ": native_bpm must be positive"); }
        if !(t.enter >= 0.0) { bad(at + ": enter_s must not be negative"); }
    }
    for k in 0..seams.len() {
        let seam = seams[k];
        let a = tracks[k];
        let b = tracks[k + 1];
        let joined = join_at(seam.strategy, k, a, b, seam.out_s, play_out(a, seam.out_s - a.enter), seam.facts, cfg);
        if joined == () { bad("seams[" + k + "]: " + seam.strategy + " cannot join " + a.item.entry_id + " -> " + b.item.entry_id); }
        let tail = strategies::a_tail_s(joined, a.item.entry_id);
        let b_stops_at = if k + 1 < seams.len() { seams[k + 1].out_s } else { end_s };
        let b_alone = play_out(b, b_stops_at - b.enter);
        if tail > 0.0 && b_stops_at >= b.enter && b_alone + 0.000000001 < tail {
            bad("seams[" + k + "] (" + a.item.entry_id + " -> " + b.item.entry_id + "): " + b.item.entry_id + " plays alone for " + b_alone + "s before "
                + (if k + 1 < seams.len() { "the next seam" } else { "the set ends" })
                + ", shorter than the " + tail + "s tail of " + a.item.entry_id + " the " + seam.strategy + " lays over it");
        }
    }
    let last = tracks[tracks.len() - 1];
    if !(end_s >= last.enter + ride_src(last) + last_fade_s() * exit_ratio(last)) {
        bad("end_s " + end_s + " leaves less than the " + last_fade_s() + "s closing fade after "
            + last.item.entry_id + " enters at " + last.enter + "s"
            + (if rides(last) { " and its " + last.ride + "-bar tempo ride" } else { "" }));
    }
}

// The audio layers and picture cuts of a set, and how long it lasts: each song's
// body from its arrival to where its seam's strategy stops it, the strategy's own
// layers, and the last song to `end_s` with a closing fade. The picture is cut
// from the layers, one bar being the first song's.
fn layout(tracks, seams, end_s, cfg) {
    import "megamix/picture" as picture;
    let layers = [];
    let t = 0.0;
    let open_fade = 0.0;
    for k in 0..seams.len() {
        let seam = seams[k];
        let a = tracks[k];
        let b = tracks[k + 1];
        let body = play_out(a, seam.out_s - a.enter);
        let joined = join_at(seam.strategy, k, a, b, seam.out_s, body, seam.facts, cfg);
        let frame = t + body + num(joined.exit_to_frame_s);
        let len = frame + num(joined.a_until_s) - t;
        let l = mk_layer("seam " + (k + 1) + " (" + a.item.entry_id + ") body", a, a.enter, t, len, a.ratio);
        l.ride = ride_of(a);
        if open_fade > 0.0 { l.fades.push(fade("in", 0.0, open_fade)); }
        l.fades.push(fade("out", len - num(joined.a_fade_s), num(joined.a_fade_s)));
        layers.push(l);
        for s in joined.layers {
            let placed = s;
            placed.out_start_s = frame + num(s.out_start_s);
            layers.push(placed);
        }
        open_fade = num(joined.b_fade_s);
        t = frame + num(joined.b_from_s);
    }
    let last = tracks[tracks.len() - 1];
    let n = play_out(last, end_s - last.enter);
    let l = mk_layer("last song (" + last.item.entry_id + ")", last, last.enter, t, n, last.ratio);
    l.ride = ride_of(last);
    if open_fade > 0.0 { l.fades.push(fade("in", 0.0, open_fade)); }
    l.fades.push(fade("out", n - last_fade_s(), last_fade_s()));
    layers.push(l);
    let duration_s = t + n;
    #{ layers: layers, cuts: picture::cuts_of(layers, duration_s, file_bar(tracks[0].item), cfg), duration_s: duration_s }
}

fn fnv(text) {
    let h = 2166136261;
    for ch in text.chars() {
        h = ((h ^ ch.to_int()) * 16777619) & 0xFFFFFFFF;
    }
    h
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

// Where track `cur` leaves its file for strategy `name`: its file's end for an
// `exit: "end"` strategy, else the latest phrase boundary with the strategy's
// room after it; () when the song cannot be left that way.
fn exit_point(cur, name, joined, cfg) {
    let a = cur.item;
    let a_bar = bar_seconds(a);
    let earliest = cur.enter + ride_src(cur) + min_body_bars() * a_bar;
    if joined.exit == "end" {
        let end = num(a.duration_ms) / 1000.0 - end_margin_s();
        return if end > earliest { end } else { () };
    }
    let po = phrase_out(a, earliest, room_bars(name, joined) * a_bar);
    if po == () { () } else { bar_time(a, po) }
}

fn build(ctx) {
    import "megamix/strategies" as strategies;
    let cfg = ctx.config;
    if cfg.expr == () { throw "whole_song: config.expr (the pool's CEL) is required"; }
    if cfg.dir == () { throw "whole_song: config.dir (where plans and renders go) is required"; }
    let entry = conf(cfg, "entry", "landing");
    if entry != "landing" && entry != "start" {
        throw "whole_song: config.entry must be \"landing\" or \"start\", got " + entry;
    }
    let list = conf(cfg, "strategies", default_strategies());
    strategies::validate(list, allowed(), "whole_song");

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
    // `layout` lays it out.
    let songs = [first];
    let tracks = [#{ item: first, enter: entry_of(first, cfg), ratio: 1.0, ride: 0, at: 0.0 }];
    let files = #{};
    files[first.entry_id] = num(first.duration_ms) / 1000.0;
    let seams = [];
    let notes = [];
    let t = 0.0;
    let end_s = ();
    while end_s == () {
        let cur = tracks[tracks.len() - 1];
        let a = cur.item;
        let a_bar = bar_seconds(a);
        // The ride needs the file to last past it and a seam out beyond it.
        if cur.ride > 0 {
            let file_room = num(a.duration_ms) / 1000.0 - end_margin_s() - cur.enter;
            if file_room < ride_src(cur) + 3.0 * file_bar(a)
                || phrase_out(a, cur.enter + (cur.ride + min_body_bars()) * a_bar, 5.0 * a_bar) == () {
                cur.ride = 0;
                tracks[tracks.len() - 1].ride = 0;
            }
        }
        let ra = exit_ratio(cur);
        let room = play_out(cur, num(a.duration_ms) / 1000.0 - end_margin_s() - cur.enter);
        if t + room >= gap_s || cands.len() == 0 {
            let play = if t + room >= gap_s { gap_s - t } else { room };
            let fin = finish(cur, play);
            end_s = fin.end_s;
            tracks[tracks.len() - 1].ride = fin.ride;
            break;
        }
        let running = num(a.audio.bpm) * ra;
        let b = next_song(ctx, cands, running);
        let b_enter = entry_of(b, cfg);
        files[b.entry_id] = num(b.duration_ms) / 1000.0;
        let k = seams.len();
        let facts = strategies::seam_facts(a, b, phrase_out(a, 0.0, 5.0 * a_bar),
            if entry == "start" { 0 } else { b.audio.landing_bar }, running);
        // Each listed strategy laid where it would leave A, to learn which fit.
        let fits = #{};
        let tried = #{};
        for e in list {
            if tried.contains(e.name) { continue; }
            let b_ratio = if locks_tempo(e.name) { running / num(b.audio.bpm) } else { 1.0 };
            let bt = #{ item: b, enter: b_enter, ratio: b_ratio, ride: 0, at: 0.0 };
            let probe = join_at(e.name, k, cur, bt, cur.enter + 1e6, 1e9, facts, cfg);
            let out_s = if probe == () { () } else { exit_point(cur, e.name, probe, cfg) };
            let joined = if out_s == () { () } else { join_at(e.name, k, cur, bt, out_s, play_out(cur, out_s - cur.enter), facts, cfg) };
            tried[e.name] = #{ out_s: out_s, joined: joined, b_ratio: b_ratio };
            fits[e.name] = joined != () && reads_inside(joined, files);
        }
        facts.fits = fits;
        let pick = strategies::draw(ctx, list, #{ a: a, b: b, seam: facts }, fits, "whole_song");
        let chosen = tried[pick];
        if chosen == () || !(fits[pick] ?? false) {
            // `whole` drawn as the fallback, or nothing that can leave this song: end the set on it.
            let bt = #{ item: b, enter: b_enter, ratio: 1.0, ride: 0, at: 0.0 };
            let probe = join_at("whole", k, cur, bt, cur.enter + 1e6, 1e9, facts, cfg);
            let out_s = exit_point(cur, "whole", probe, cfg);
            chosen = if out_s == () { () } else { #{ out_s: out_s, b_ratio: 1.0,
                joined: join_at("whole", k, cur, bt, out_s, play_out(cur, out_s - cur.enter), facts, cfg) } };
        }
        if chosen == () || chosen.joined == () {
            let fin = finish(cur, room);
            end_s = fin.end_s;
            tracks[tracks.len() - 1].ride = fin.ride;
            break;
        }
        let joined = chosen.joined;
        let out_s = chosen.out_s;
        let body = play_out(cur, out_s - cur.enter);
        let landed = t + body + num(joined.exit_to_frame_s) + num(joined.b_from_s);
        let tail = strategies::a_tail_s(joined, a.entry_id);
        let room_after = if tail > min_last_s() { tail } else { min_last_s() };
        if landed > gap_s - room_after {
            let fin = finish(cur, room);
            end_s = fin.end_s;
            tracks[tracks.len() - 1].ride = fin.ride;
            break;
        }
        let b_ratio = chosen.b_ratio;
        let b_ride = if (b_ratio - 1.0).abs() > 0.000001 { ride_bars_of(cfg) } else { 0 };
        seams.push(#{ strategy: pick, out_s: out_s, bars: joined.bars ?? 0, sweep: joined.sweep ?? false, facts: facts });
        notes.push("seam " + seams.len() + " at " + (t + body) + "s: " + pick
            + (if pick == "bass_swap" { " " + joined.bars + " bars" + (if joined.sweep { " sweep" } else { "" }) } else { "" })
            + ", " + a.title + " (" + a.entry_id + ") out at " + out_s + "s -> " + b.title + " (" + b.entry_id + ") enters at "
            + b_enter + "s, tempo gap " + facts.tempo_gap_pct + "%, energy step " + facts.energy_step_db + " dB, running " + facts.running_bpm + " bpm\n");
        songs.push(b);
        cands = without(cands, b.entry_id);
        tracks.push(#{ item: b, enter: b_enter, ratio: b_ratio, ride: b_ride, at: t + body });
        t = landed;
    }
    validate_set(tracks, seams, end_s, cfg);
    let lay = layout(tracks, seams, end_s, cfg);
    let set_s = lay.duration_s;

    let ids = "";
    for s in songs { ids += s.entry_id + "|"; }
    for s in seams { ids += s.strategy + "|"; }
    let stem = "ch" + ctx.channel.number + "-set-" + fnv(ids + entry).to_hex();
    let fields = conf(cfg, "artist_field", "show");
    let sources = [];
    let cards = [];
    let song_notes = [];
    for tr in tracks {
        let it = tr.item;
        sources.push(#{ id: it.entry_id, path: it.path });
        let card = #{
            at_s: tr.at,
            artist: (it[fields] ?? it.show ?? it.studio ?? "").to_string(),
            title: (it.title ?? "").to_string(),
        };
        if it.year != () { card.year = it.year; }
        cards.push(card);
        song_notes.push(#{ entry_id: it.entry_id, title: (it.title ?? "").to_string(), native_bpm: num(it.audio.bpm),
                           enter_s: tr.enter, ratio: tr.ratio, ride_bars: tr.ride, at_s: tr.at });
    }
    let seam_notes = [];
    for s in seams { seam_notes.push(#{ strategy: s.strategy, out_s: s.out_s, bars: s.bars, sweep: s.sweep }); }
    let plan = #{
        sources: sources,
        layers: lay.layers,
        cuts: lay.cuts,
        duration_s: set_s,
        cards: cards,
        output: cfg.dir + "/" + stem + ".mp4",
        notes: #{ entry: entry, end_s: end_s, songs: song_notes, seams: seam_notes },
    };
    let plan_path = cfg.dir + "/" + stem + ".plan.json";
    write_file(ctx, plan_path, to_json(plan));
    let text = "";
    for n in notes { text += n; }
    if text == "" { text = "no seams: " + songs[0].title + " (" + songs[0].entry_id + ") fills the gap\n"; }
    write_file(ctx, cfg.dir + "/" + stem + ".audit.txt", text);

    let set_ms = (set_s * 1000.0).round().to_int();

    let names = "chapter:" + stem;
    for s in songs { names += "|" + s.entry_id; }
    log("whole_song: set=" + names + " songs=" + songs.len() + " seconds=" + set_s);
    for n in notes { log("whole_song: " + n); }

    let t_line = timeline();
    let standing = stand_in(ctx, cfg);
    let seg = segment();
    if standing == () {
        seg = seg.black().looped();
    } else {
        seg = seg.pending(plan.output, standing, plan_path).looped().own_audio();
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

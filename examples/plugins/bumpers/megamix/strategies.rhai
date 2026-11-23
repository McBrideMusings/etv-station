// How two songs join: the mixing strategies megamix.rhai and whole_song.rhai
// choose among for every seam, and the draw that picks one.
//
// A strategy is `join(name, j)`. Its frame is the seam's arrival, t = 0, where
// the incoming song B starts playing on its own. `j` is
//   #{ a: song, b: song, bar_s, full_db, stems, seam, cfg }
// where a song is #{ label, id, src_s, ratio, pitch, before_s }: `src_s` is its
// source second at t = 0 (A's as if it kept playing at `ratio`), `before_s` how
// long A plays before t = 0 in this chapter or set. `bar_s` is one bar of output
// at the seam, `full_db` the level of a `full` layer (() leaves the source's),
// `stems` whether vocals and instrumental are separated, `seam` the facts below,
// `cfg` the fill config. It returns () when it cannot join these songs, else
//   #{ layers, a_until_s, a_fade_s, b_from_s, b_fade_s, exit_to_frame_s, exit }
// with each layer's out_start_s relative to t = 0: the caller plays A's own
// body until `a_until_s` (closing over `a_fade_s`) and B's from `b_from_s`
// (opening over `b_fade_s`, 0 for none). `exit_to_frame_s` is how long after
// A's exit point t = 0 falls; `exit` is "phrase" (A leaves at a phrase
// boundary) or "end" (A plays to the end of its file).
//
// config (in the fill entry's config):
//   strategies:      [{ name, weight, when }], drawn per seam: among the entries
//                    whose `when` (CEL over a, b and seam) holds and that fit the
//                    pair, one by weight (default 1); none leaves `whole`
//   vocal_over_bars: bars B's vocal sings over A's instrumental, default 2
//   blend_bars:      bars of a blend's crossfade, default 8, at most half of A's
//                    part and as much of B's file as precedes its arrival
//
// Seam facts (`seam` in a `when`): tempo_gap_pct (B's tempo against A's running
// tempo, percent), energy_step_db (B's arrival bar over A's last four bars
// before it leaves), arrival_peak_share (B's arrival bar over its loudest
// downbeat bar), running_bpm, and fits (strategy name -> whether it fits).

fn names() { ["vocal_over", "bass_swap", "echo_out", "drop_mix", "blend", "whole"] }

fn short_fade_s() { 0.02 }
fn cut_fade_s() { 0.01 }
fn bass_crossover_hz() { 250.0 }
fn sweep_ceiling_hz() { 1200.0 }

fn num(v) { v * 1.0 }

fn smaller(x, y) { if x < y { x } else { y } }

fn fade(kind, start, len, curve) {
    let f = #{ kind: kind, start_s: if start < 0.0 { 0.0 } else { start }, len_s: len };
    if curve != "linear" { f.curve = curve; }
    f
}

// A fade in at the start and a fade out at the end of a `len_s`-second layer,
// each at most half the layer.
fn edge_fades(len_s, fade_in, fade_out) {
    let fi = smaller(fade_in, len_s / 2.0);
    let fo = smaller(fade_out, len_s / 2.0);
    [fade("in", 0.0, fi, "linear"), fade("out", len_s - fo, fo, "linear")]
}

// `len` seconds of song `s`'s `stem` placed at `out` (relative to the frame),
// read where the song plays at that moment.
fn layer(name, s, stem, out, len, level_db) {
    let l = #{
        name: name,
        source: s.id,
        stem: stem,
        src_start_s: num(s.src_s) + out * num(s.ratio),
        out_start_s: out,
        out_len_s: len,
        ratio: s.ratio,
        fades: [],
        filters: [],
    };
    if s.pitch != () { l.pitch = s.pitch; }
    if level_db != () { l.level_db = level_db; }
    l
}

fn joined(layers, a_until, a_fade, b_from, b_fade) {
    #{ layers: layers, a_until_s: a_until, a_fade_s: a_fade, b_from_s: b_from, b_fade_s: b_fade,
       exit_to_frame_s: 0.0, exit: "phrase" }
}

// B's vocal over A's instrumental for `vocal_over_bars`, then B's own body.
fn vocal_over(j) {
    if !j.stems { return (); }
    let over = j.cfg.vocal_over_bars ?? 2;
    if over <= 0 { return (); }
    let len = over * j.bar_s;
    let under = layer(j.a.label + " instrumental under the next vocal", j.a, "instrumental", 0.0, len, -17.0);
    under.fades = edge_fades(len, 0.02, j.bar_s);
    let sung = layer(j.b.label + " vocals over the previous song", j.b, "vocals", 0.0, len, -19.0);
    sung.fades = edge_fades(len, 0.5, 0.02);
    joined([under, sung], 0.0, short_fade_s(), len, 0.3)
}

// A's bass gives way to B's: B enters without its bass for half the swap, both
// basslines change hands at t = 0, then A rings out without its bass. Two bars
// with a band-narrowing sweep when B arrives near its peak, else four.
fn bass_swap(j) {
    let sweep = num(j.seam.arrival_peak_share ?? 0.0) >= 0.9;
    let bars = if sweep { 2 } else { 4 };
    let bl = j.bar_s;
    let half = bars / 2.0 * bl;
    let tail = layer(j.a.label + " tail without bass", j.a, "full", 0.0, half, j.full_db);
    tail.filters.push(#{ kind: "highpass", hz: bass_crossover_hz() });
    if sweep { tail.filters.push(#{ kind: "lowpass", hz: sweep_ceiling_hz() }); }
    tail.fades = [fade("in", 0.0, short_fade_s(), "linear"), fade("out", half / 2.0, half / 2.0, "linear")];
    let lead = layer(j.b.label + " entering without bass", j.b, "full", -half, half, j.full_db);
    lead.filters.push(#{ kind: "highpass", hz: bass_crossover_hz() });
    lead.fades = [fade("in", 0.0, smaller(bl, half), "linear"), fade("out", half - short_fade_s(), short_fade_s(), "linear")];
    let out = joined([tail, lead], 0.0, short_fade_s(), 0.0, 0.0);
    out.exit_to_frame_s = half;
    out.bars = bars;
    out.sweep = sweep;
    out
}

// A stops a bar early and its last bar rings out as echoes under B.
fn echo_out(j) {
    let bl = j.bar_s;
    let echo = layer(j.a.label + " echo", j.a, "full", -bl, 3.0 * bl, j.full_db);
    echo.filters.push(#{ kind: "echo", in_gain: 0.8, out_gain: 0.7,
        delay_ms: (bl / 4.0 * 0.75 * 1000.0).to_int().to_float(), decay: 0.55 });
    echo.fades = [fade("in", 0.0, short_fade_s(), "linear"), fade("out", bl, 2.0 * bl, "linear")];
    joined([echo], -bl, short_fade_s(), 0.0, 0.0)
}

// A cuts on the downbeat and B starts on it.
fn drop_mix(j) {
    joined([], 0.0, cut_fade_s(), 0.0, 0.0)
}

// Both full songs crossfade on equal-power curves for `blend_bars` bars before
// B's arrival, so B arrives at full level as A's fade ends.
fn blend(j) {
    let bl = j.bar_s;
    let n = j.cfg.blend_bars ?? 8;
    let room_a = (num(j.a.before_s) / bl / 2.0).floor().to_int();
    let room_b = (num(j.b.src_s) / (bl * num(j.b.ratio))).floor().to_int();
    n = smaller(n, smaller(room_a, room_b));
    if n < 1 { return (); }
    let len = n * bl;
    let x = cut_fade_s();
    let out_a = layer(j.a.label + " fading out", j.a, "full", -len, len, j.full_db);
    out_a.fades = [fade("in", 0.0, x, "linear"), fade("out", 0.0, len, "equal_power")];
    let in_b = layer(j.b.label + " fading in", j.b, "full", -len, len + x, j.full_db);
    in_b.fades = [fade("in", 0.0, len, "equal_power"), fade("out", len, x, "linear")];
    let out = joined([out_a, in_b], -len + x, x, 0.0, x);
    out.bars = n;
    out
}

// No overlap: A ends and B starts, each with a short fade.
fn whole(j) {
    let out = joined([], 0.0, short_fade_s(), 0.0, short_fade_s());
    out.exit = "end";
    out
}

fn join(name, j) {
    switch name {
        "vocal_over" => vocal_over(j),
        "bass_swap" => bass_swap(j),
        "echo_out" => echo_out(j),
        "drop_mix" => drop_mix(j),
        "blend" => blend(j),
        "whole" => whole(j),
        _ => throw "megamix/strategies: no strategy named " + name,
    }
}

// The seconds after t = 0 that A's seam layers still sound.
fn a_tail_s(joined, a_id) {
    let tail = 0.0;
    for l in joined.layers {
        if l.source != a_id { continue; }
        let end = num(l.out_start_s) + num(l.out_len_s);
        if end > tail { tail = end; }
    }
    tail
}

// Checks `config.strategies` against the names `allowed` and throws naming the
// first entry it refuses: a known name, a positive weight, a `when` that is CEL.
fn validate(list, allowed, who) {
    if type_of(list) != "array" || list.len() == 0 {
        throw who + ": config.strategies must list at least one of " + allowed;
    }
    for i in 0..list.len() {
        let e = list[i];
        let at = who + ": config.strategies[" + i + "]";
        if type_of(e) != "map" || type_of(e.name) != "string" { throw at + " needs a name"; }
        if !allowed.contains(e.name) { throw at + ": " + e.name + " is not one of " + allowed; }
        if e.weight != () && !(num(e.weight) > 0.0) { throw at + " (" + e.name + "): weight must be positive"; }
        if e.when != () { cel_check(e.when); }
    }
}

// The strategy for one seam: a weighted pick among the entries of `list` whose
// `when` holds over `facts` (#{ a, b, seam }) and whose name `fits`; `whole`
// when none does. A `when` that fails to run counts as not holding.
fn draw(ctx, list, facts, fits, who) {
    let open = [];
    let total = 0.0;
    for e in list {
        if !(fits[e.name] ?? false) { continue; }
        if e.when != () {
            let holds = false;
            try {
                holds = cel(e.when, facts) == true;
            } catch (err) {
                log(who + ": when_failed strategy=" + e.name + " reason=" + err);
            }
            if !holds { continue; }
        }
        let w = num(e.weight ?? 1.0);
        open.push(#{ name: e.name, w: w });
        total += w;
    }
    if open.len() == 0 {
        log(who + ": no strategy holds and fits " + facts.a.entry_id + " -> " + facts.b.entry_id + "; whole");
        return "whole";
    }
    // One open entry leaves the rng untouched, so a single-strategy list draws as before.
    if open.len() == 1 { return open[0].name; }
    let r =ctx.rng.float() * total;
    for o in open {
        if r < o.w { return o.name; }
        r -= o.w;
    }
    open[open.len() - 1].name
}

fn db(x) { 20.0 * (num(x) + 0.000000001).log() }

// The seam facts a `when` reads, from both songs' audio facts: A leaves after
// source bar `exit_bar` playing at `running_bpm`; B arrives on source bar
// `arrival_bar`.
fn seam_facts(a, b, exit_bar, arrival_bar, running_bpm) {
    let gap = (running_bpm - num(b.audio.bpm)).abs() / running_bpm;
    let exit_e = 0.0;
    let count = 0;
    let energy = a.audio.bar_energy ?? [];
    for k in (exit_bar - 4)..exit_bar {
        if k >= 0 && k < energy.len() { exit_e += num(energy[k]); count += 1; }
    }
    if count > 0 { exit_e /= count; }
    let b_energy = b.audio.bar_energy ?? [];
    let arrive_e = if arrival_bar < b_energy.len() { num(b_energy[arrival_bar]) } else { 0.0 };
    let peak = 0.0;
    let i = 8;
    while i < b_energy.len() {
        if num(b_energy[i]) > peak { peak = num(b_energy[i]); }
        i += 4;
    }
    #{
        tempo_gap_pct: (gap * 1000.0).round() / 10.0,
        energy_step_db: ((db(arrive_e) - db(exit_e)) * 10.0).round() / 10.0,
        arrival_peak_share: if peak > 0.0 { arrive_e / peak } else { 0.0 },
        running_bpm: (running_bpm * 10.0).round() / 10.0,
    }
}

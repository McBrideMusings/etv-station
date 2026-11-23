// The picture of a render plan, cut from its audio layers (docs/schema.md §
// Render plan), for megamix.rhai and whole_song.rhai alike, whatever strategy
// laid the audio:
//   - only a song with an audible layer may be on screen;
//   - the song whose voice is heard (a `vocals` or `full` layer) wins, the
//     loudest of them, else the loudest song heard;
//   - the song on screen keeps the picture until it leaves the mix, or until
//     another candidate is `picture_margin_db` (default 2) louder after
//     `picture_hold_beats` (default 4, one bar) on screen.
// A song heard only as its instrumental counts as not singing: the plan has no
// facts about where its singer's lines fall.
//
// Levels are the plan's own: each layer's level_db plus gain_db and its fades,
// every fade read as a linear ramp whatever its curve.

fn num(v) { v * 1.0 }

// The level layer `l` plays at output second `t` in dB, or () when it is silent.
fn layer_db(l, t) {
    let at = t - num(l.out_start_s);
    if at < 0.0 || at >= num(l.out_len_s) { return (); }
    let g = 1.0;
    for f in l.fades ?? [] {
        let from = num(f.start_s);
        let len = num(f.len_s);
        if f.kind == "in" {
            if at < from { return (); }
            if len > 0.0 && at < from + len { g *= (at - from) / len; }
        } else {
            if at >= from + len { return (); }
            if len > 0.0 && at > from { g *= 1.0 - (at - from) / len; }
        }
    }
    if g <= 0.0 { return (); }
    num(l.level_db ?? 0.0) + num(l.gain_db ?? 0.0) + 20.0 * g.log()
}

// Where layer `l` reads at output second `t`, as the piece it is in:
// #{ offset, ratio } with source second = offset + t * ratio. A layer with a
// `ride` steps through its ratios, `step_src_s` source seconds each, then
// keeps the last.
fn layer_place(l, t) {
    let ride = l.ride;
    if ride == () {
        let ratio = num(l.ratio ?? 1.0);
        return #{ offset: num(l.src_start_s) - num(l.out_start_s) * ratio, ratio: ratio };
    }
    let out = num(l.out_start_s);
    let src = num(l.src_start_s);
    let step = num(ride.step_src_s);
    let last = 1.0;
    for r in ride.ratios {
        let r = num(r);
        let len = step / r;
        last = r;
        if t < out + len { return #{ offset: src - out * r, ratio: r }; }
        out += len;
        src += step;
    }
    #{ offset: src - out * last, ratio: last }
}

// Every song audible at output second `t`: source -> #{ db, voice, offset,
// ratio }. `db` sums its layers' power; `voice` is true when one of them carries
// its vocal; `offset` and `ratio` place its picture where its loudest layer reads.
fn heard_at(layers, t) {
    let heard = #{};
    for l in layers {
        let db = layer_db(l, t);
        if db == () { continue; }
        let place = layer_place(l, t);
        let s = heard[l.source];
        if s == () {
            s = #{ power: 0.0, voice: false, top_db: db, offset: place.offset, ratio: place.ratio };
        } else if db > s.top_db {
            s.top_db = db;
            s.offset = place.offset;
            s.ratio = place.ratio;
        }
        s.power += 10.0 ** (db / 10.0);
        let stem = l.stem ?? "full";
        if stem == "vocals" || stem == "full" { s.voice = true; }
        heard[l.source] = s;
    }
    for id in heard.keys() {
        let s = heard[id];
        s.db = 10.0 * s.power.log();
        heard[id] = s;
    }
    heard
}

// The picture in sixteenths of a `bar_s` bar, as runs #{ source, from, to,
// offset, ratio } covering 0 to `duration_s` end to end; a silent sixteenth
// keeps the song before it.
fn picture_runs(layers, duration_s, bar_s, cfg) {
    let margin = num(cfg.picture_margin_db ?? 2.0);
    let hold = (cfg.picture_hold_beats ?? 4) * 4;
    let step = bar_s / 16.0;
    let n = (duration_s / step).ceiling().to_int();
    let runs = [];
    let cur = ();
    let since = 0;
    for k in 0..n {
        let mid = (k + 0.5) * step;
        if mid > duration_s { mid = (k * step + duration_s) / 2.0; }
        let heard = heard_at(layers, mid);
        let pool = #{};
        for id in heard.keys() { if heard[id].voice { pool[id] = heard[id]; } }
        if pool.len() == 0 { pool = heard; }
        let best = ();
        for id in pool.keys() {
            if best == () || pool[id].db > pool[best].db { best = id; }
        }
        if best != () {
            if cur == () || !pool.contains(cur) {
                cur = best;
                since = k;
            } else if best != cur && k - since >= hold && pool[best].db > pool[cur].db + margin {
                cur = best;
                since = k;
            }
        }
        if cur == () { continue; }
        let at = if pool.contains(cur) { pool[cur] } else { () };
        let last = runs.len() - 1;
        let same = last >= 0 && runs[last].source == cur
            && (at == () || ((runs[last].offset - at.offset).abs() < 1e-6 && (runs[last].ratio - at.ratio).abs() < 1e-9));
        if same {
            runs[last].to = k + 1;
        } else {
            runs.push(#{ source: cur, from: if runs.len() == 0 { 0 } else { k }, to: k + 1, offset: at.offset, ratio: at.ratio });
        }
    }
    runs
}

// Run `run`'s song from sixteenth `from` to `to`, read where its audio plays.
fn own_cut(run, from, to, step) {
    #{
        source: run.source,
        src_start_s: run.offset + from * step * run.ratio,
        out_start_s: from * step,
        out_len_s: (to - from) * step,
        ratio: run.ratio,
    }
}

// The picture cuts: each run is its song read where its audio plays, broken by a
// shot in each `window`-sixteenth window `windows` lists as [run, window] and
// `shots` fills, in order. The last cut ends at `duration_s`.
fn picture_cuts(runs, windows, shots, bar_s, window, duration_s) {
    let step = bar_s / 16.0;
    let shot_at = #{};
    for i in 0..windows.len() {
        if i >= shots.len() { break; }
        shot_at["" + windows[i][0] + "/" + windows[i][1]] = shots[i];
    }
    let cuts = [];
    for r in 0..runs.len() {
        let run = runs[r];
        let whole = if window > 0 { (run.to - run.from) / window } else { 0 };
        let own_from = run.from;
        for j in 0..whole {
            let shot = shot_at["" + r + "/" + j];
            if shot == () { continue; }
            let at = run.from + j * window;
            if at > own_from { cuts.push(own_cut(run, own_from, at, step)); }
            cuts.push(#{ source: shot.entry_id, src_start_s: shot.in_s, out_start_s: at * step, out_len_s: window * step });
            own_from = at + window;
        }
        if run.to > own_from { cuts.push(own_cut(run, own_from, run.to, step)); }
    }
    if cuts.len() > 0 {
        let last = cuts.len() - 1;
        cuts[last].out_len_s = duration_s - num(cuts[last].out_start_s);
    }
    cuts
}

// The plan's picture from its layers, with no shots.
fn cuts_of(layers, duration_s, bar_s, cfg) {
    picture_cuts(picture_runs(layers, duration_s, bar_s, cfg), [], [], bar_s, 0, duration_s)
}

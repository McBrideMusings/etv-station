// An Adult Swim-style text card: white lines on black, a few at a time, over a
// low synthesized rumble. Each screen is one black segment; the card's words
// come from text_cards.txt (read with ctx.read, so the script declares `read`)
// and the drawing from card.rhai beside it.
//
// config (the block's `fill:` entry):
//   font      family name for every line and the credit (default: the overlay's
//             OS sans-serif fallback; a family from the station's `fonts:` list
//             may be named instead)
//   size      text size multiplier (default 1.0; 1.0 is 5% of the frame height)
//   max_secs  longest the card runs (default 15): a static screen reads as
//             stalled past that, so a longer gap is left to the next draw
//   card      draw this card id instead of a random one
//   overlay   the drawing script, relative to the channel folder
//             (default ../plugins/bumpers/card.rhai)
//
// Timing, per screen: lines appear 0.5s apart, the screen holds for
// max(1.4s, 0.32s per word), then cuts to black for 0.3s (0.4s after the last).
// Given more time than that, the card keeps its @optional screens, then slows
// the reveal up to 2x, then stretches the holds up to 2.5x, then gives what is
// left to the last hold. Given less, it drops the @optional screens, then
// squeezes the reveal and the holds toward a floor of 0.6x beats and 1.0s holds.
fn hooks() { ["bumper"] }
fn capabilities() { ["read"] }

fn beat() { 0.5 }
fn min_hold() { 1.4 }
fn per_word() { 0.32 }
fn cut() { 0.3 }
fn end_black() { 0.4 }

// The bed: brown noise low-passed at 180 Hz. bed_gain_db() puts the bumper's
// output at -36 LUFS integrated (ffmpeg ebur128, 6 seeds, 8s and 15s: -35.8 to
// -36.2), 5 to 16 LU under the programme levels sampled from the library.
fn bed_graph() { "anoisesrc=color=brown:sample_rate=48000,lowpass=f=180,aformat=channel_layouts=stereo" }
fn bed_gain_db() { -15.9 }

fn fmax(a, b) { if a > b { a } else { b } }
fn fmin(a, b) { if a < b { a } else { b } }

// ---- reading the corpus ----------------------------------------------------

fn parse_corpus(text) {
    let cards = [];
    let card = ();
    let screen = #{ optional: false, lines: [], pause: 0.0 };
    for raw in text.split('\n') {
        let s = raw;
        s.trim();
        if s == "" || s.starts_with("#") { continue; }
        if s.starts_with("== ") {
            if card != () {
                card.screens.push(screen);
                cards.push(card);
            }
            let id = s.sub_string(3);
            id.trim();
            card = #{ id: id, screens: [] };
            screen = #{ optional: false, lines: [], pause: 0.0 };
        } else if card == () {
            continue;
        } else if s == "---" {
            card.screens.push(screen);
            screen = #{ optional: false, lines: [], pause: 0.0 };
        } else if s == "@optional" {
            screen.optional = true;
        } else if s.starts_with("~") {
            let n = s.sub_string(1);
            n.trim();
            screen.pause = n.parse_float();
        } else if s.starts_with("> ") {
            let who = s.sub_string(2);
            who.trim();
            screen.lines.push(#{ kind: "credit", text: who, pause: screen.pause });
            screen.pause = 0.0;
        } else {
            screen.lines.push(#{ kind: "line", text: s, pause: screen.pause });
            screen.pause = 0.0;
        }
    }
    if card != () {
        card.screens.push(screen);
        cards.push(card);
    }
    cards
}

// `text` with every {slot} and {slot|fallback} replaced, or () when a slot has
// neither a value nor a fallback. A slot never reaches the screen unfilled.
fn fill_slots(text, slots) {
    let out = "";
    let rest = text;
    loop {
        let i = rest.index_of("{");
        let j = rest.index_of("}");
        if i < 0 || j < i {
            out += rest;
            return out;
        }
        out += rest.sub_string(0, i);
        let body = rest.sub_string(i + 1, j - i - 1);
        rest = rest.sub_string(j + 1);
        let bar = body.index_of("|");
        let name = if bar >= 0 { body.sub_string(0, bar) } else { body };
        let fallback = if bar >= 0 { body.sub_string(bar + 1) } else { () };
        let value = slots[name];
        if value == () || value == "" { value = fallback; }
        if value == () { return (); }
        out += value;
    }
}

// The card with its slots filled, or () when any of its lines cannot be filled.
fn fill_card(card, slots) {
    let screens = [];
    for sc in card.screens {
        let lines = [];
        let words = 0;
        for line in sc.lines {
            let text = fill_slots(line.text, slots);
            if text == () { return (); }
            words += text.split(' ').len();
            lines.push(#{ kind: line.kind, text: text, pause: line.pause });
        }
        if lines.len() > 0 {
            screens.push(#{ optional: sc.optional, lines: lines, words: words });
        }
    }
    if screens.len() == 0 { return (); }
    #{ id: card.id, screens: screens }
}

// ---- laying the card out on a length ----------------------------------------

// Each screen's reveal times and length with the given reveal and hold factors.
fn nat(screens, beat_k, hold_k, hold_min) {
    let out = [];
    let total = 0.0;
    let n = screens.len();
    for i in 0..n {
        let sc = screens[i];
        let at = 0.0;
        let reveal = [];
        let j = 0;
        for line in sc.lines {
            if j > 0 { at += beat() * beat_k + line.pause; } else { at += line.pause; }
            reveal.push(at);
            j += 1;
        }
        let hold_end = at + fmax(hold_min, per_word() * sc.words) * hold_k;
        let black = if i < n - 1 { cut() } else { end_black() };
        out.push(#{ screen: sc, reveal: reveal, hold_end: hold_end, length: hold_end + black, last_reveal: at });
        total += hold_end + black;
    }
    #{ screens: out, total: total }
}

fn required_only(screens) {
    let out = [];
    for sc in screens { if !sc.optional { out.push(sc); } }
    out
}

fn included(screens, keep) {
    let out = [];
    for i in 0..screens.len() { if keep[i] { out.push(screens[i]); } }
    out
}

// The shortest the required screens can run.
fn floor_secs(screens) {
    nat(required_only(screens), 0.6, 0.7, 1.0).total
}

// `target` seconds of screens: #{ screens, total, mode }.
fn layout(screens, target) {
    let req = required_only(screens);
    let natural = nat(req, 1.0, 1.0, min_hold());
    if natural.total > target {
        // Squeeze: bisect one factor between the natural timing and the floor.
        let lo = 0.0;
        let hi = 1.0;
        for step in 0..30 {
            let m = (lo + hi) / 2.0;
            let t = nat(req, 0.6 + 0.4 * m, 0.7 + 0.3 * m, 1.0 + (min_hold() - 1.0) * m).total;
            if t > target { hi = m; } else { lo = m; }
        }
        let squeezed = nat(req, 0.6 + 0.4 * lo, 0.7 + 0.3 * lo, 1.0 + (min_hold() - 1.0) * lo);
        squeezed.mode = "squeezed";
        return squeezed;
    }

    // Keep optional screens, in order, while the natural timing still fits.
    let keep = [];
    for sc in screens { keep.push(!sc.optional); }
    for i in 0..screens.len() {
        if screens[i].optional {
            keep[i] = true;
            if nat(included(screens, keep), 1.0, 1.0, min_hold()).total > target { keep[i] = false; }
        }
    }
    let chosen = included(screens, keep);

    // Slow the reveal, at most 2x, with up to half the spare time.
    let spare = target - nat(chosen, 1.0, 1.0, min_hold()).total;
    let steps = 0;
    for sc in chosen { if sc.lines.len() > 1 { steps += sc.lines.len() - 1; } }
    let beat_k = 1.0;
    if steps > 0 && spare > 0.0 {
        beat_k = 1.0 + fmin(spare * 0.5, steps * beat()) / (steps * beat());
    }

    // Stretch the holds, at most 2.5x, with the time that is left.
    let after_beats = nat(chosen, beat_k, 1.0, min_hold());
    spare = target - after_beats.total;
    let holds = 0.0;
    for ev in after_beats.screens { holds += ev.hold_end - ev.last_reveal; }
    let hold_k = 1.0;
    if holds > 0.0 && spare > 0.0 { hold_k = fmin(2.5, 1.0 + spare / holds); }

    // Whatever is still left goes to the last hold.
    let result = nat(chosen, beat_k, hold_k, min_hold());
    let left = target - result.total;
    if left > 0.0 {
        let last = result.screens.len() - 1;
        result.screens[last].hold_end += left;
        result.screens[last].length += left;
        result.total += left;
    }
    result.mode = "stretched beat x" + beat_k + " hold x" + hold_k;
    result
}

// ---- the timeline ------------------------------------------------------------

fn clean(title) {
    let out = if title == () { "" } else { title };
    out.replace("*", "");
    out
}

fn build(ctx) {
    let config = ctx.config;
    let target_max = (config.max_secs ?? 15.0).to_float();
    let gap = ctx.gap_ms.to_float() / 1000.0;
    // A gap longer than the cap runs the cap and leaves the rest to the next
    // draw, but a remainder too short for another card is absorbed by this one
    // (up to 6s past the cap) rather than left as black.
    let target = gap;
    if gap > target_max {
        target = target_max;
        if gap - target < 6.0 { target = gap; }
    }

    let slots = #{
        next_title: if ctx.next == () { "" } else { clean(ctx.next.title) },
        prev_title: if ctx.prev == () { "" } else { clean(ctx.prev.title) },
        channel: clean(ctx.channel.name),
    };

    let eligible = [];
    for card in parse_corpus(ctx.read("text_cards.txt") ?? "") {
        if config.card != () && config.card != card.id { continue; }
        let filled = fill_card(card, slots);
        if filled != () && floor_secs(filled.screens) <= target { eligible.push(filled); }
    }
    let t = timeline();
    if eligible.len() == 0 {
        log("text_card: no card fits gap=" + gap + "s");
        return t;
    }
    let card = eligible.random(ctx.rng);
    let laid = layout(card.screens, target);

    let longest = 0;
    for sc in card.screens {
        for line in sc.lines { longest = if line.text.len() > longest { line.text.len() } else { longest }; }
    }

    let n = laid.screens.len();
    for i in 0..n {
        let ev = laid.screens[i];
        let lines = [];
        let reveal = [];
        let credit = "";
        let credit_at = -1.0;
        for j in 0..ev.screen.lines.len() {
            let line = ev.screen.lines[j];
            if line.kind == "credit" {
                credit = line.text;
                credit_at = ev.reveal[j];
            } else {
                lines.push(line.text);
                reveal.push(ev.reveal[j]);
            }
        }
        let seg = segment().black().name(card.id + "-" + i)
            .overlay(config.overlay ?? "../plugins/bumpers/card.rhai", #{
                lines: lines,
                reveal: reveal,
                credit: credit,
                credit_at: credit_at,
                show_until: ev.hold_end,
                longest: longest,
                size: config.size ?? 1.0,
                font_family: config.font ?? "sans-serif",
            })
            .fixed(ev.length)
            .bed_audio();
        if ev.screen.optional { seg = seg.optional(); }
        t.push(seg);
    }
    t.audio_bed(#{ lavfi: bed_graph() }, #{ gain_db: bed_gain_db(), fade_in: 0.0, fade_out: 0.0 });
    log("text_card: card=" + card.id + " gap=" + gap + "s target=" + target + "s screens=" + n + " " + laid.mode);
    t
}

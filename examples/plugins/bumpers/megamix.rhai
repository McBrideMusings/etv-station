// A megamix bumper: plans one chapter of 3 to 5 song hooks from a pool of music
// videos, hands the plan to the chapter renderer's queue, and airs the render
// when it lands (a stand-in until then).
//
// config (the block's `fill:` entry):
//   expr:            CEL over the catalog naming the pool, as a pool's `expr` does (required)
//   dir:             folder for plans, renders and the stand-in (required)
//   floor:           lowest `beat_confidence` a song may have, default 2.5
//   steady_beats:    fewest beats in a song's steady run, default 32
//   ranker:          how the cluster and hooks are picked: "rule" (the cluster with
//                    the most candidates, hooks drawn at random) or "model"
//   brief:           what the chapter should be, sent to the model (ranker "model")
//   model_url:       OpenAI-compatible chat-completions endpoint (ranker "model")
//   model:           model name sent in the request (ranker "model")
//   model_headers:   extra request headers, e.g. Authorization, default {}
//   model_clusters:  clusters offered to the model, default 6
//   model_songs:     songs listed per cluster, default 40
//                    A model pick that names an unoffered cluster or song, repeats a
//                    song, or has too few or too many hooks for the gap, and any
//                    http or JSON failure, falls back to the rule.
//   hooks_min/max:   hooks per chapter, default 3 and 5
//   tempo_band_pct:  a cluster's tempo window around its seed, +/- percent, default 4
//   max_shift:       most semitones a hook may move, default 3
//   vocal_over_bars: bars a hook's vocal sings over the previous song, default 2
//   tail:            recent chapters whose hooks are not reused, default 12
//   seeds:           most cluster seeds tried per build, default 60
//   artist_field:    item field holding the artist for the card, default "show"
//   card_script:     overlay script that draws the card copy, relative to the
//                    channel config's directory; unset carries no overlay
//   enqueue:         program run as `<enqueue> <enqueue_args...> enqueue <plan> --air-at <t>`,
//                    default "etv-station"
//   enqueue_args:    extra arguments before `enqueue`, default []
//   ffmpeg:          program that makes the stand-in, default "ffmpeg"
//
// Beside each render it writes <stem>.plan.json (the renderer's plan) and
// <stem>.audit.txt (one seam line per hook, which `admin audit` and
// `GET /api/channels/<n>/chapters` show).
//
// The segment is named `chapter:<job>|<entry>|<entry>...`: `GET
// /api/channels/<n>/chapters` joins the job id to the renderer's queue, and this
// script reads the entries back from `ctx.airings` to keep a song out of the
// next `tail` chapters; that includes chapters laid earlier in the same
// generation pass, before they reach the ledger.
//
// A hook is 16 bars, or 8 when 16 bars would pass 40 seconds. Hooks start on
// 8-bar boundaries of the chapter. Every hook but the first begins with
// `vocal_over_bars` bars of its vocal over the previous song's instrumental.
fn hooks() { ["bumper"] }
fn capabilities() { ["probe", "catalog_read", "exec", "http"] }

// Bars after the landing bar that the renderer separates from a song.
fn region_bars() { 18 }

fn num(v) { v * 1.0 }

fn conf(cfg, key, fallback) {
    let v = cfg[key];
    if v == () { fallback } else { v }
}

fn camelot_parts(code) {
    let n = code.sub_string(0, code.len() - 1).parse_int();
    #{ n: n, letter: code.sub_string(code.len() - 1, 1) }
}

// Steps between two Camelot codes: 0 same, 1 neighbour or relative, 2+ otherwise.
fn camelot_dist(a, b) {
    let x = camelot_parts(a);
    let y = camelot_parts(b);
    let d = (x.n - y.n).abs();
    if d > 6 { d = 12 - d; }
    if x.letter != y.letter { d += 1; }
    d
}

// The code a song lands on when moved `semitones`: a semitone up is +7 on the wheel.
fn shifted_camelot(code, semitones) {
    let x = camelot_parts(code);
    let n = ((x.n - 1 + 7 * semitones) % 12 + 12) % 12 + 1;
    n.to_string() + x.letter
}

// The shift within +/- max_shift that lands closest to the chapter key, the
// smallest move winning a tie.
fn best_shift(code, chapter_code, max_shift) {
    let best = 0;
    let best_dist = camelot_dist(code, chapter_code);
    for s in 1..=max_shift {
        for signed in [s, -s] {
            let d = camelot_dist(shifted_camelot(code, signed), chapter_code);
            if d < best_dist { best = signed; best_dist = d; }
        }
    }
    #{ semitones: best, dist: best_dist }
}

fn eligible(item, cfg) {
    let a = item.audio;
    if a == () || item.path == () { return false; }
    if a.bpm == () || a.camelot == () || a.key == () || a.landing_bar == () || a.landing_bar_s == () { return false; }
    if a.beat_confidence == () || num(a.beat_confidence) < num(conf(cfg, "floor", 2.5)) { return false; }
    let grid = a.beat_grid;
    if grid == () || grid.beats == () || grid.beats < conf(cfg, "steady_beats", 32) { return false; }
    let native_bar = 240.0 / num(a.bpm);
    if item.duration_ms == () { return false; }
    // The renderer separates 18 bars from the landing bar on.
    num(item.duration_ms) / 1000.0 - num(a.landing_bar_s) >= region_bars() * native_bar
}

fn recent_entries(ctx, tail) {
    let seen = #{};
    let chapters = 0;
    for row in ctx.airings(#{ kind: "bumper", order: "desc", limit: 200 }) {
        if !row.entry_id.starts_with("chapter:") { continue; }
        let parts = row.entry_id.split("|");
        for i in 1..parts.len() { seen[parts[i]] = true; }
        chapters += 1;
        if chapters >= tail { break; }
    }
    seen
}

fn median(values) {
    let v = values;
    v.sort(|a, b| if a < b { -1 } else if a > b { 1 } else { 0 });
    let n = v.len();
    if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 }
}

fn cluster_of(seed, cands, band_pct) {
    let members = [];
    let lo = num(seed.audio.bpm) * (1.0 - band_pct / 100.0);
    let hi = num(seed.audio.bpm) * (1.0 + band_pct / 100.0);
    for c in cands {
        let bpm = num(c.audio.bpm);
        if bpm < lo || bpm > hi { continue; }
        if camelot_dist(c.audio.camelot, seed.audio.camelot) > 1 { continue; }
        members.push(c);
    }
    members
}

// Mean energy over the four bars from the landing bar, the song's loudest stretch.
fn hook_energy(item) {
    let bars = item.audio.bar_energy;
    let from = item.audio.landing_bar;
    let sum = 0.0;
    let count = 0;
    for i in from..(from + 4) {
        if i < bars.len() { sum += num(bars[i]); count += 1; }
    }
    if count == 0 { 0.0 } else { sum / count }
}

// The cluster seeds one build tries: every candidate, or an evenly strided
// `seeds` of them from a start the rng picks.
fn tried_seeds(ctx, cands, cfg) {
    let seeds = cands;
    seeds.sort(|a, b| if a.entry_id < b.entry_id { -1 } else if a.entry_id > b.entry_id { 1 } else { 0 });
    let limit = conf(cfg, "seeds", 60);
    if seeds.len() <= limit { return seeds; }
    let tried = [];
    let stride = seeds.len() / limit;
    let start = ctx.rng.int(0, stride - 1);
    for i in 0..limit { tried.push(seeds[start + i * stride]); }
    tried
}

// The picked cluster: the seed with the most members, a tie broken by the rng.
fn pick_cluster(ctx, cands, cfg) {
    let band = num(conf(cfg, "tempo_band_pct", 4.0));
    let tried = tried_seeds(ctx, cands, cfg);
    let best = [];
    let ties = [];
    for seed in tried {
        let members = cluster_of(seed, cands, band);
        if members.len() > best.len() {
            best = members;
            ties = [#{ seed: seed, members: members }];
        } else if members.len() == best.len() && members.len() > 0 {
            ties.push(#{ seed: seed, members: members });
        }
    }
    if ties.len() == 0 { return (); }
    ties.random(ctx.rng)
}

// The clusters a model is offered: the `model_clusters` largest distinct ones
// around the tried seeds, each with at most `model_songs` of its songs listed.
fn offered_clusters(ctx, cands, cfg) {
    let band = num(conf(cfg, "tempo_band_pct", 4.0));
    let seen = #{};
    let found = [];
    for seed in tried_seeds(ctx, cands, cfg) {
        let members = cluster_of(seed, cands, band);
        let ids = [];
        for m in members { ids.push(m.entry_id); }
        ids.sort();
        let key = "";
        for id in ids { key += id + "|"; }
        if seen.contains(key) { continue; }
        seen[key] = true;
        found.push(#{ seed: seed, members: members });
    }
    found.sort(|a, b| {
        if a.members.len() != b.members.len() { return b.members.len() - a.members.len(); }
        if a.seed.entry_id < b.seed.entry_id { -1 } else if a.seed.entry_id > b.seed.entry_id { 1 } else { 0 }
    });
    found.truncate(conf(cfg, "model_clusters", 6));
    let cap = conf(cfg, "model_songs", 40);
    let offered = [];
    for i in 0..found.len() {
        let listed = [];
        let left = [];
        for m in found[i].members { left.push(m); }
        while listed.len() < cap && left.len() > 0 {
            listed.push(left.remove(ctx.rng.int(0, left.len() - 1)));
        }
        offered.push(#{ id: "c" + i, seed: found[i].seed, members: found[i].members, listed: listed });
    }
    offered
}

fn model_prompt(cfg, offered, gap_s) {
    let text = "Brief: " + conf(cfg, "brief", "a good mix") + "\n\nClusters (songs in one cluster share a tempo and key):\n";
    for c in offered {
        let bpms = [];
        for m in c.members { bpms.push(num(m.audio.bpm)); }
        let fit = shape(240.0 / median(bpms), gap_s, cfg, c.members.len());
        let takes = if fit == () { "does not fit the gap, do not pick it" } else { "the gap takes " + conf(cfg, "hooks_min", 3) + " to " + fit.hooks + " hooks" };
        text += "\n" + c.id + " (" + c.members.len() + " songs; " + takes + "):\n";
        for m in c.listed {
            let a = m.audio;
            text += "- " + m.entry_id + " | " + (m.title ?? "") + " | " + (m[conf(cfg, "artist_field", "show")] ?? m.show ?? "") + " | " + (m.year ?? "") + " | "
                + a.bpm + " bpm | " + a.camelot + " | energy " + hook_energy(m) + "\n";
        }
    }
    text
}

// The JSON object in a chat-completion reply's text.
fn reply_object(raw) {
    let text = parse_json(raw).choices[0].message.content;
    let start = text.index_of("{");
    let end = text.len() - 1;
    while end > 0 && text.sub_string(end, 1) != "}" { end -= 1; }
    if start < 0 || end <= start { throw "no JSON object in the reply"; }
    parse_json(text.sub_string(start, end - start + 1))
}

// What the model picked, held to the rule ranker's limits: one offered cluster,
// distinct songs from its list, between hooks_min and as many as the gap fits.
// Throws the reason when the pick breaks one.
fn check_model_pick(reply, offered, cfg, gap_s) {
    let cluster = ();
    for c in offered { if c.id == reply.cluster { cluster = c; } }
    if cluster == () { throw "cluster " + reply.cluster + " was not offered"; }
    if type_of(reply.hooks) != "array" { throw "hooks is not a list"; }
    let by_id = #{};
    for m in cluster.listed { by_id[m.entry_id] = m; }
    let drawn = [];
    let used = #{};
    for id in reply.hooks {
        if type_of(id) != "string" || !by_id.contains(id) { throw "hook " + id + " is not in " + cluster.id; }
        if used.contains(id) { throw "hook " + id + " is named twice"; }
        used[id] = true;
        drawn.push(by_id[id]);
    }
    let bpms = [];
    for m in cluster.members { bpms.push(num(m.audio.bpm)); }
    let fitted = shape(240.0 / median(bpms), gap_s, cfg, cluster.members.len());
    if fitted == () { throw "cluster " + cluster.id + " does not fit the gap"; }
    if drawn.len() < conf(cfg, "hooks_min", 3) || drawn.len() > fitted.hooks {
        throw drawn.len() + " hooks, the gap takes " + conf(cfg, "hooks_min", 3) + " to " + fitted.hooks;
    }
    #{ seed: cluster.seed, members: cluster.members, drawn: drawn, fitted: #{ bars: fitted.bars, hooks: drawn.len() } }
}

// The model's chapter, or () after logging why the rule must pick instead.
fn model_rank(ctx, cfg, cands) {
    if cfg.model_url == () || cfg.model == () {
        throw "megamix: config.model_url and config.model are required when ranker is \"model\"";
    }
    let offered = offered_clusters(ctx, cands, cfg);
    if offered.len() == 0 { return (); }
    let gap_s = ctx.gap_ms / 1000.0;
    let min_hooks = conf(cfg, "hooks_min", 3);
    let max_hooks = conf(cfg, "hooks_max", 5);
    let system = "You plan one chapter of a music-video megamix. Reply with one JSON object and nothing else: "
        + "{\"cluster\": \"<cluster id>\", \"hooks\": [\"<entry id>\", ...]} naming one cluster and between "
        + min_hooks + " and " + max_hooks + " of its listed songs (never more than the cluster's own limit) in the order they play, fitting the brief.";
    let headers = #{ "Content-Type": "application/json" };
    let extra = conf(cfg, "model_headers", #{});
    for k in extra.keys() { headers[k] = extra[k]; }
    let body = to_json(#{
        model: cfg.model,
        temperature: 0,
        messages: [
            #{ role: "system", content: system },
            #{ role: "user", content: model_prompt(cfg, offered, gap_s) },
        ],
    });
    try {
        let raw = ctx.http(cfg.model_url, "POST", body, headers);
        let picked = check_model_pick(reply_object(raw), offered, cfg, gap_s);
        let ids = "";
        for d in picked.drawn { ids += d.entry_id + " "; }
        log("megamix: model_ranked hooks=" + ids);
        return picked;
    } catch (e) {
        log("megamix: model_rejected, the rule picks: " + e);
    }
    ()
}

fn hex(n) { n.to_hex() }

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

// Bars per hook and the hook count that fit `gap_s`, or () when fewer than
// hooks_min fit.
fn shape(bar_s, gap_s, cfg, available) {
    let min_hooks = conf(cfg, "hooks_min", 3);
    let max_hooks = conf(cfg, "hooks_max", 5);
    for bars in [16, 8] {
        if bars == 16 && 16.0 * bar_s > 40.0 { continue; }
        let fit = (gap_s / (bars * bar_s)).floor().to_int();
        let n = [max_hooks, available, fit].reduce(|best, v| if v < best { v } else { best }, 1000);
        if n >= min_hooks { return #{ bars: bars, hooks: n }; }
    }
    ()
}

// The plan the renderer reads (docs/schema.md § Chapter plan), and the card copy.
fn plan_for(ctx, cfg, chosen, chapter_bpm, chapter, shape, stem) {
    let bar_s = 240.0 / chapter_bpm;
    let over = conf(cfg, "vocal_over_bars", 2);
    let max_shift = conf(cfg, "max_shift", 3);
    let dir = cfg.dir;
    if over < 0 || shape.bars + over > region_bars() {
        throw "megamix: config.vocal_over_bars must be between 0 and " + (region_bars() - shape.bars) + " for " + shape.bars + "-bar hooks";
    }
    let hooks = [];
    let cards = [];
    let cuts = [];
    let fields = conf(cfg, "artist_field", "show");
    for i in 0..chosen.len() {
        let item = chosen[i];
        let a = item.audio;
        let move_by = best_shift(a.camelot, chapter.audio.camelot, max_shift);
        let vocal_over = if i == 0 { 0 } else { over };
        hooks.push(#{
            entry_id: item.entry_id,
            path: item.path,
            landing_s: num(a.landing_bar_s),
            native_bpm: num(a.bpm),
            semitones: move_by.semitones,
            vocal_over_bars: vocal_over,
            full_bars: shape.bars - vocal_over,
        });
        let card = #{
            at_s: i * shape.bars * bar_s,
            artist: (item[fields] ?? item.show ?? item.studio ?? "").to_string(),
            title: (item.title ?? "").to_string(),
        };
        if item.year != () { card.year = item.year; }
        cards.push(card);
    }
    // Four-bar cuts: the hook's own picture, then another song's, alternating.
    let piece_bars = 4;
    let piece_s = piece_bars * bar_s;
    for i in 0..chosen.len() {
        let pieces = shape.bars / piece_bars;
        for p in 0..pieces {
            let other = (i + 1 + p / 2) % chosen.len();
            let src = if p % 2 == 0 || other == i { i } else { other };
            let h = hooks[src];
            let ratio = h.native_bpm / chapter_bpm;
            cuts.push(#{
                entry_id: h.entry_id,
                native_start_s: h.landing_s + p * piece_s * ratio,
                out_start_s: (i * shape.bars + p * piece_bars) * bar_s,
                length_s: piece_s,
                ratio: ratio,
            });
        }
    }
    #{
        bpm: chapter_bpm,
        key: chapter.audio.key + " " + chapter.audio.scale,
        camelot: chapter.audio.camelot,
        bar_seconds: bar_s,
        hooks: hooks,
        cuts: cuts,
        cards: cards,
        output: dir + "/" + stem + ".mp4",
    }
}

fn cfg_dir_of(path) {
    let at = path.len() - 1;
    while at > 0 && path.sub_string(at, 1) != "/" { at -= 1; }
    if at == 0 { "." } else { path.sub_string(0, at) }
}

// One line per hook for `admin audit`, written beside the render as <stem>.audit.txt.
fn seam_notes(plan, chosen, bars) {
    let text = "";
    for i in 0..plan.hooks.len() {
        let h = plan.hooks[i];
        let item = chosen[i];
        let shift = if h.semitones > 0 { "+" + h.semitones } else { h.semitones.to_string() };
        text += "seam " + (i + 1) + " at bar " + (i * bars) + ": " + item.title + " (" + h.entry_id + ") lands source bar "
            + item.audio.landing_bar + " at " + h.landing_s + "s, " + shift + " semitones, vocal over " + h.vocal_over_bars + " bars\n";
    }
    text
}

fn write_file(ctx, path, text) {
    let made = ctx.exec("mkdir", ["-p", cfg_dir_of(path)]);
    if made.code != 0 { throw "megamix: could not make the folder for " + path + ": " + made.stderr; }
    let r = ctx.exec("sh", ["-c", "printf %s \"$1\" > \"$2\"", "sh", text, path]);
    if r.code != 0 { throw "megamix: could not write " + path + ": " + r.stderr; }
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
        log("megamix: enqueue failed code=" + r.code + " stderr=" + r.stderr);
    } catch (e) {
        log("megamix: enqueue unavailable: " + e);
    }
    ()
}

// A ten-second black clip with a brown-noise bed; a pending chapter loops it.
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
        log("megamix: stand-in not made code=" + r.code + " stderr=" + r.stderr);
    } catch (e) {
        log("megamix: stand-in not made: " + e);
    }
    ()
}

fn idle(ctx, why) {
    log("megamix: idle " + why);
    let t = timeline();
    t.push(segment().black().looped().fixed_ms(ctx.gap_ms).name("megamix-idle"));
    t
}

fn build(ctx) {
    let cfg = ctx.config;
    if cfg.expr == () { throw "megamix: config.expr (the pool's CEL) is required"; }
    if cfg.dir == () { throw "megamix: config.dir (where plans and renders go) is required"; }
    let ranker = conf(cfg, "ranker", "rule");
    if ranker != "rule" && ranker != "model" {
        throw "megamix: config.ranker must be \"rule\" or \"model\", got " + ranker;
    }

    let recent = recent_entries(ctx, conf(cfg, "tail", 12));
    let cands = [];
    let pool = ctx.catalog(cfg.expr);
    for item in pool {
        if !eligible(item, cfg) { continue; }
        if recent.contains(item.entry_id) { continue; }
        cands.push(item);
    }
    log("megamix: pool=" + pool.len() + " candidates=" + cands.len());

    let ranked = ();
    if ranker == "model" && cands.len() > 0 { ranked = model_rank(ctx, cfg, cands); }
    let members = ();
    let seed = ();
    let fitted = ();
    let drawn = [];
    if ranked != () {
        members = ranked.members;
        seed = ranked.seed;
        fitted = ranked.fitted;
        drawn = ranked.drawn;
    } else {
        let picked = pick_cluster(ctx, cands, cfg);
        if picked == () { return idle(ctx, "no candidates"); }
        members = picked.members;
        seed = picked.seed;
    }

    let bpms = [];
    for m in members { bpms.push(num(m.audio.bpm)); }
    let chapter_bpm = median(bpms);
    let bar_s = 240.0 / chapter_bpm;
    if ranked == () {
        fitted = shape(bar_s, ctx.gap_ms / 1000.0, cfg, members.len());
        if fitted == () { return idle(ctx, "cluster of " + members.len() + " does not fit a gap of " + ctx.gap_ms + "ms"); }

        // The hooks are drawn at random from the cluster, then laid out by rising energy.
        let pool_left = members;
        while drawn.len() < fitted.hooks {
            let i = ctx.rng.int(0, pool_left.len() - 1);
            drawn.push(pool_left.remove(i));
        }
        drawn.sort(|a, b| {
            let x = hook_energy(a);
            let y = hook_energy(b);
            if x < y { -1 } else if x > y { 1 } else { 0 }
        });
    }

    let ids = "";
    for d in drawn { ids += d.entry_id + "|"; }
    let stem = "ch" + ctx.channel.number + "-" + hex(fnv(ids + chapter_bpm));
    let plan = plan_for(ctx, cfg, drawn, chapter_bpm, seed, fitted, stem);
    let plan_path = cfg.dir + "/" + stem + ".plan.json";
    write_file(ctx, plan_path, to_json(plan));
    let notes = seam_notes(plan, drawn, fitted.bars);
    write_file(ctx, cfg.dir + "/" + stem + ".audit.txt", notes);

    let air_at = if ctx.next == () { () } else { ctx.next.start - ctx.gap_ms / 1000 };
    let job = enqueue(ctx, cfg, plan_path, air_at);
    let chapter_ms = (fitted.hooks * fitted.bars * bar_s * 1000.0).round().to_int();

    let names = "chapter:" + (if job == () { "unqueued-" + stem } else { job });
    for d in drawn { names += "|" + d.entry_id; }
    log("megamix: chapter=" + names + " bpm=" + chapter_bpm + " key=" + plan.key + " bars=" + (fitted.hooks * fitted.bars) + " queued=" + (job != ()));
    for i in 0..plan.hooks.len() {
        let h = plan.hooks[i];
        log("megamix: seam=" + i + " bar=" + (i * fitted.bars) + " entry=" + h.entry_id + " landing_s=" + h.landing_s + " semitones=" + h.semitones);
    }

    let t = timeline();
    let standing = stand_in(ctx, cfg);
    let seg = segment();
    if standing == () {
        seg = seg.black().looped();
    } else {
        seg = seg.pending(plan.output, standing).looped().own_audio();
    }
    seg = seg.fixed_ms(chapter_ms).name(names);
    if cfg.card_script != () {
        seg = seg.overlay(cfg.card_script, #{ cards: plan.cards, bar_s: plan.bar_seconds });
    }
    t.push(seg);
    t
}

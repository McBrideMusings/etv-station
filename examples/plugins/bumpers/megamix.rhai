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
//   strategies:      how each seam joins two hooks (required): [{ name, weight, when }],
//                    drawn per seam; names, keys and seam facts in megamix/strategies.rhai
//   picture_margin_db:  dB another song must be louder than the one on screen to
//                    take the picture, default 2
//   picture_hold_beats: fewest beats a song keeps the picture before a louder one
//                    takes it, default 4
//   broll_share:     share of each song's four-bar picture windows given to Frame
//                    index shots when `phrase` finds them, default 0.5
//   tail:           recent chapters whose hooks are not reused, default 12
//   seeds:           most cluster seeds tried per build, default 60
//   artist_field:    item field holding the artist for the card, default "show"; an
//                    item without it (an extra) is looked up by entry id in the
//                    explorer's extras fields under that name (needs plexdb_url
//                    and the `http` grant)
//   card_script:     overlay script that draws the card copy, relative to the
//                    channel config's directory; unset carries no overlay
//   ffmpeg:          program that makes the stand-in, default "ffmpeg"
//   plexdb_url:      base URL of the plex-db-ex explorer, e.g. "http://host:8765"; with
//                    `phrase`, `broll_share` of the picture is drawn from the Frame
//                    index (needs the `http` grant)
//   phrase:          text the Frame index search looks for; the shots come from the
//                    pool's songs that are not hooks of this chapter. Unset, or an
//                    explorer that is ungranted, unreachable or finds nothing, leaves
//                    the audible songs' own pictures
//
// Beside each render it writes <stem>.plan.json (the renderer's plan: audio
// layers and picture cuts, with the chapter's tempo, key and hooks in its
// `notes`) and <stem>.audit.txt (one seam line per hook, which `just audit` and
// `GET /api/channels/<n>/chapters` show).
//
// The segment is pending on <stem>.mp4 and names <stem>.plan.json as the plan
// that makes it; the render sidecar queues that plan as the chapter's air time
// nears. The segment is named `chapter:<stem>|<entry>|<entry>...`: `GET
// /api/channels/<n>/chapters` joins the stem to the renderer job whose output
// it names, and this
// script reads the entries back from `ctx.airings` to keep a song out of the
// next `tail` chapters; that includes chapters laid earlier in the same
// generation pass, before they reach the ledger.
//
// A hook is 16 bars, or 8 when 16 bars would pass 40 seconds. Hooks start on
// 8-bar boundaries of the chapter. This script picks the songs and lays each
// one's body (vocal and instrumental stems); the strategy drawn for each seam
// (megamix/strategies.rhai) lays the join; megamix/picture.rhai cuts the
// picture from the result.
fn hooks() { ["bumper"] }
fn capabilities() { ["probe", "catalog_read", "exec", "http"] }

// Bars after the landing bar a chapter may read from a song: its own bars plus
// the next hook's vocal-over, which plays over this song's instrumental.
fn region_bars() { 18 }

// Bars before the landing bar that are split into stems with the rest, so the
// separator has context: a song's stems are one span, landing - 4 bars to
// landing + region_bars(), whatever part of it a chapter reads, so every
// chapter that uses the song shares one separation.
fn lead_bars() { 4 }

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
    // A chapter reads up to region_bars() bars from the landing bar on.
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

// Shots of the pool's other songs that the explorer's Frame index finds for
// config.phrase, at most `want`, each #{ entry_id, path, in_s } with room for
// `need_s` seconds from in_s. Empty when the search is unset, ungranted,
// unreachable or finds nothing, so the caller keeps the hooks' own pictures.
fn broll_shots(ctx, cfg, others, want, need_s) {
    if cfg.plexdb_url == () || cfg.phrase == () || want <= 0 { return []; }
    // The explorer's item_id: a Plex entry's rating key, an extra's entry id.
    let by_item = #{};
    for item in others {
        let ids = item.source_ids;
        if item.path == () || item.duration_ms == () { continue; }
        if ids != () && ids.plex != () {
            by_item["" + ids.plex] = item;
        } else {
            by_item[item.entry_id] = item;
        }
    }
    if by_item.len() == 0 { return []; }
    let body = to_json(#{
        phrase: cfg.phrase,
        pool: by_item.keys(),
        draw: #{ n: want, seed: ctx.rng.int(0, 1000000000) },
    });
    let reply = ();
    try {
        reply = ctx.http(cfg.plexdb_url + "/api/search/run", "POST", body,
                         #{ "Content-Type": "application/json" });
    } catch (e) {
        log("megamix: broll search=skipped reason=" + e);
        return [];
    }
    let found = ();
    try {
        found = parse_json(reply).shots;
    } catch (e) {
        log("megamix: broll search=skipped reason=unreadable reply: " + e);
        return [];
    }
    if found == () { found = []; }
    // The drawn shots first, then the rest in rank order.
    let ordered = [];
    for s in found { if s.drawn == 1 { ordered.push(s); } }
    for s in found { if s.drawn != 1 { ordered.push(s); } }
    let shots = [];
    for s in ordered {
        if shots.len() >= want { break; }
        let item = by_item["" + s.item_id];
        if item == () { continue; }
        let in_s = num(s.in_ms) / 1000.0;
        if num(s.out_ms) / 1000.0 - in_s < need_s || in_s + need_s > num(item.duration_ms) / 1000.0 { continue; }
        shots.push(#{ entry_id: item.entry_id, path: item.path, in_s: in_s });
    }
    log("megamix: broll search=" + (if shots.len() == 0 { "empty" } else { "hit" }) + " found=" + found.len() + " used=" + shots.len() + " phrase=" + cfg.phrase);
    shots
}

// The artist of each chosen item that lacks `field`, read from the explorer's
// `POST /api/extras/fields` (the first value, person first), as entry_id -> artist.
// Empty when plexdb_url is unset, `http` is ungranted, the explorer is unreachable
// or names no such field, so the card falls back to show/studio.
fn extras_artists(ctx, cfg, chosen, field) {
    let missing = [];
    for item in chosen { if item[field] == () { missing.push(item.entry_id); } }
    if cfg.plexdb_url == () || missing.len() == 0 { return #{}; }
    let reply = ();
    try {
        reply = ctx.http(cfg.plexdb_url + "/api/extras/fields", "POST", to_json(#{ ids: missing }),
                         #{ "Content-Type": "application/json" });
    } catch (e) {
        log("megamix: extras fields=skipped reason=" + e);
        return #{};
    }
    let items = ();
    try {
        items = parse_json(reply).items;
    } catch (e) {
        log("megamix: extras fields=skipped reason=unreadable reply: " + e);
        return #{};
    }
    let artists = #{};
    if items == () { return artists; }
    for id in items.keys() {
        for f in items[id].fields ?? [] {
            if f.name == field && f.values != () && f.values.len() > 0 {
                artists[id] = f.values[0].value.to_string();
            }
        }
    }
    log("megamix: extras fields=" + (if artists.len() == 0 { "empty" } else { "hit" }) + " asked=" + missing.len() + " found=" + artists.len());
    artists
}

// What each song does in the chapter, in play order: its landing, tempo and the
// shift to the chapter key, and the `bars` it is given. The arrangement module
// reads these; the plan's `notes` keep them for the audit.
fn song_records(chosen, chapter_bpm, chapter, shape, cfg) {
    let max_shift = conf(cfg, "max_shift", 3);
    let songs = [];
    for item in chosen {
        let a = item.audio;
        let move_by = best_shift(a.camelot, chapter.audio.camelot, max_shift);
        let native_bpm = num(a.bpm);
        songs.push(#{
            entry_id: item.entry_id,
            landing_s: num(a.landing_bar_s),
            landing_bar: a.landing_bar,
            native_bpm: native_bpm,
            semitones: move_by.semitones,
            bars: shape.bars,
            // Source seconds per chapter second, and the pitch ratio of the shift.
            ratio: chapter_bpm / native_bpm,
            pitch: 2.0 ** (move_by.semitones / 12.0),
        });
    }
    songs
}

// Sixteenths in a b-roll window: four bars.
fn broll_window() { 64 }

// The four-bar windows of each run that show a Frame index shot instead, as
// [run index, window index] pairs: `broll_share` of each run's whole windows,
// spread evenly, its first window always its own picture.
fn broll_windows(runs, cfg) {
    let share = num(conf(cfg, "broll_share", 0.5));
    let window = broll_window();
    let picked = [];
    if share <= 0.0 { return picked; }
    for r in 0..runs.len() {
        let windows = (runs[r].to - runs[r].from) / window;
        for j in 0..windows {
            if ((j + 1) * share).floor() > (j * share).floor() { picked.push([r, j]); }
        }
    }
    picked
}

// The plan the renderer reads (docs/schema.md § Render plan), and the card copy.
// The chapter's tempo, key, songs and seams go in `notes`, for the audit.
fn plan_for(ctx, cfg, chosen, songs, seams, layers, cuts, shots, chapter_bpm, chapter, shape, stem) {
    let bar_s = 240.0 / chapter_bpm;
    let dir = cfg.dir;
    let cards = [];
    let sources = [];
    let fields = conf(cfg, "artist_field", "show");
    let extra_artists = extras_artists(ctx, cfg, chosen, fields);
    for i in 0..chosen.len() {
        let item = chosen[i];
        let a = item.audio;
        let native_bar = 240.0 / num(a.bpm);
        let sep_start = num(a.landing_bar_s) - lead_bars() * native_bar;
        if sep_start < 0.0 { sep_start = 0.0; }
        sources.push(#{
            id: item.entry_id,
            path: item.path,
            separate: #{
                start_s: sep_start,
                len_s: num(a.landing_bar_s) + region_bars() * native_bar - sep_start,
            },
        });
        let card = #{
            at_s: i * shape.bars * bar_s,
            artist: (item[fields] ?? extra_artists[item.entry_id] ?? item.show ?? item.studio ?? "").to_string(),
            title: (item.title ?? "").to_string(),
        };
        if item.year != () { card.year = item.year; }
        cards.push(card);
    }
    let broll = #{};
    for s in shots { broll[s.entry_id] = s.path; }
    let shown = #{};
    for c in cuts { shown[c.source] = true; }
    let shot_ids = broll.keys();
    shot_ids.sort(|a, b| if a < b { -1 } else if a > b { 1 } else { 0 });
    for id in shot_ids { if shown.contains(id) { sources.push(#{ id: id, path: broll[id] }); } }
    #{
        sources: sources,
        layers: layers,
        cuts: cuts,
        duration_s: chosen.len() * shape.bars * bar_s,
        cards: cards,
        output: dir + "/" + stem + ".mp4",
        notes: #{
            bpm: chapter_bpm,
            key: chapter.audio.key + " " + chapter.audio.scale,
            camelot: chapter.audio.camelot,
            bar_seconds: bar_s,
            hooks: songs,
            seams: seams,
        },
    }
}

fn cfg_dir_of(path) {
    let at = path.len() - 1;
    while at > 0 && path.sub_string(at, 1) != "/" { at -= 1; }
    if at == 0 { "." } else { path.sub_string(0, at) }
}

// One line per hook for `just audit`, written beside the render as <stem>.audit.txt.
fn seam_notes(plan, chosen, bars) {
    let text = "";
    let hooks = plan.notes.hooks;
    for i in 0..hooks.len() {
        let h = hooks[i];
        let item = chosen[i];
        let shift = if h.semitones > 0 { "+" + h.semitones } else { h.semitones.to_string() };
        text += "seam " + (i + 1) + " at bar " + (i * bars) + ": " + item.title + " (" + h.entry_id + ") lands source bar "
            + item.audio.landing_bar + " at " + h.landing_s + "s, " + shift + " semitones, " + h.bars + " bars, "
            + (if i == 0 { "opens the chapter" } else { "joined by " + plan.notes.seams[i - 1].strategy }) + "\n";
    }
    let hooked = #{};
    for h in hooks { hooked[h.entry_id] = true; }
    for cut in plan.cuts {
        if hooked.contains(cut.source) { continue; }
        text += "broll at " + cut.out_start_s + "s: " + cut.source + " from " + cut.src_start_s + "s, " + cut.out_len_s + "s\n";
    }
    text
}

fn write_file(ctx, path, text) {
    let made = ctx.exec("mkdir", ["-p", cfg_dir_of(path)]);
    if made.code != 0 { throw "megamix: could not make the folder for " + path + ": " + made.stderr; }
    let r = ctx.exec("sh", ["-c", "printf %s \"$1\" > \"$2\"", "sh", text, path]);
    if r.code != 0 { throw "megamix: could not write " + path + ": " + r.stderr; }
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

// The vocal and instrumental of song `h` from output second `from` to `until_s`,
// at the chapter tempo and key, read where its hook places them.
fn body_layers(label, h, slot_s, from, until_s, fade_in, fade_out, bar_s) {
    import "megamix/strategies" as strategies;
    let len = until_s - from;
    if len <= 0.0 { return []; }
    let native_bar = 240.0 / h.native_bpm;
    let src = h.landing_s + (from - slot_s) / bar_s * native_bar;
    let out = [];
    for part in [["vocals", -20.0, 0.02], ["instrumental", -17.0, fade_in]] {
        out.push(#{
            name: label + part[0],
            source: h.entry_id,
            stem: part[0],
            src_start_s: src,
            out_start_s: from,
            out_len_s: len,
            ratio: h.ratio,
            pitch: h.pitch,
            level_db: part[1],
            fades: strategies::edge_fades(len, part[2], fade_out),
        });
    }
    out
}

// Whether every layer of `j`'s join reads inside its file, a stem inside the
// song's separated span, and leaves A at least a bar of its own body.
fn join_fits(joined, j, files, spans, frame_s) {
    if joined == () { return false; }
    if -num(joined.a_until_s) > num(j.a.before_s) - j.bar_s { return false; }
    for l in joined.layers {
        let src = num(l.src_start_s);
        let end = src + num(l.out_len_s) * num(l.ratio ?? 1.0);
        if src < 0.0 || end > files[l.source] { return false; }
        if l.stem == "vocals" || l.stem == "instrumental" {
            let span = spans[l.source];
            if src < span.start_s - 1e-6 || end > span.start_s + span.len_s + 1e-6 { return false; }
        }
        if frame_s + num(l.out_start_s) < -1e-6 { return false; }
    }
    true
}

// The chapter's audio: each hook's body in its slot, and each seam laid by the
// strategy `config.strategies` draws for it. Returns #{ layers, seams }, a seam
// being #{ at_bar, strategy, facts }.
fn arrange(ctx, cfg, drawn, songs, bar_s) {
    import "megamix/strategies" as strategies;
    let files = #{};
    let spans = #{};
    for i in 0..drawn.len() {
        let item = drawn[i];
        let a = item.audio;
        let native_bar = 240.0 / num(a.bpm);
        files[item.entry_id] = num(item.duration_ms) / 1000.0;
        let start = num(a.landing_bar_s) - lead_bars() * native_bar;
        if start < 0.0 { start = 0.0; }
        spans[item.entry_id] = #{ start_s: start, len_s: num(a.landing_bar_s) + region_bars() * native_bar - start };
    }
    let slots = [0.0];
    for h in songs { slots.push(slots[slots.len() - 1] + h.bars * bar_s); }
    let joins = [()];
    let seams = [];
    let b_from = 0.0;
    for i in 1..songs.len() {
        let a = songs[i - 1];
        let b = songs[i];
        let frame_s = slots[i];
        let song = |h, n, src, before| #{ label: "hook " + (n + 1) + " (" + h.entry_id + ")", id: h.entry_id,
            src_s: src, ratio: h.ratio, pitch: h.pitch, before_s: before };
        let j = #{
            a: song.call(a, i - 1, a.landing_s + a.bars * 240.0 / a.native_bpm, a.bars * bar_s - b_from),
            b: song.call(b, i, b.landing_s, b.bars * bar_s),
            // -15.2 dB is the power sum of a body's -20 dB vocal and -17 dB instrumental.
            bar_s: bar_s, full_db: -15.2, stems: true, cfg: cfg,
        };
        let facts = strategies::seam_facts(drawn[i - 1], drawn[i], drawn[i - 1].audio.landing_bar + a.bars,
            drawn[i].audio.landing_bar, num(drawn[i - 1].audio.bpm));
        let fits = #{};
        let laid = #{};
        for e in cfg.strategies {
            if laid.contains(e.name) { continue; }
            let j2 = j;
            j2.seam = facts;
            let got = strategies::join(e.name, j2);
            laid[e.name] = got;
            fits[e.name] = join_fits(got, j, files, spans, frame_s);
        }
        facts.fits = fits;
        let pick = strategies::draw(ctx, cfg.strategies, #{ a: drawn[i - 1], b: drawn[i], seam: facts }, fits, "megamix");
        let joined = if laid.contains(pick) && fits[pick] { laid[pick] } else { j.seam = facts; strategies::join(pick, j) };
        joins.push(joined);
        seams.push(#{ at_bar: (slots[i] / bar_s).round().to_int(), strategy: pick, facts: facts });
        b_from = num(joined.b_from_s);
    }
    let layers = [];
    for i in 0..songs.len() {
        let h = songs[i];
        let slot_s = slots[i];
        let from = slot_s;
        let fade_in = 1.0;
        if i > 0 {
            let joined = joins[i];
            for l in joined.layers {
                let placed = l;
                placed.out_start_s = slot_s + num(l.out_start_s);
                layers.push(placed);
            }
            from = slot_s + num(joined.b_from_s);
            fade_in = if joined.b_fade_s > 0.0 { joined.b_fade_s } else { 0.02 };
        }
        let until_s = slot_s + h.bars * bar_s;
        let fade_out = 0.02;
        if i + 1 < songs.len() {
            until_s += num(joins[i + 1].a_until_s);
            fade_out = num(joins[i + 1].a_fade_s);
        }
        layers.append(body_layers("hook " + (i + 1) + " (" + h.entry_id + ") ", h, slot_s, from, until_s, fade_in, fade_out, bar_s));
    }
    #{ layers: layers, seams: seams }
}

fn build(ctx) {
    let cfg = ctx.config;
    if cfg.expr == () { throw "megamix: config.expr (the pool's CEL) is required"; }
    if cfg.dir == () { throw "megamix: config.dir (where plans and renders go) is required"; }
    import "megamix/strategies" as strategies;
    strategies::validate(cfg.strategies, strategies::names(), "megamix");
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
    let hooked = #{};
    for d in drawn { hooked[d.entry_id] = true; }
    let others = [];
    for item in pool { if !hooked.contains(item.entry_id) { others.push(item); } }

    let songs = song_records(drawn, chapter_bpm, seed, fitted, cfg);
    let arranged = arrange(ctx, cfg, drawn, songs, bar_s);
    let layers = arranged.layers;
    let duration_s = drawn.len() * fitted.bars * bar_s;
    import "megamix/picture" as picture;
    let runs = picture::picture_runs(layers, duration_s, bar_s, cfg);
    let windows = broll_windows(runs, cfg);
    let shots = broll_shots(ctx, cfg, others, windows.len(), broll_window() * bar_s / 16.0);
    let cuts = picture::picture_cuts(runs, windows, shots, bar_s, broll_window(), duration_s);
    // The seams' strategies and the shots are part of the stem: a different mix or picture is a different render.
    for s in arranged.seams { ids += s.strategy + "|"; }
    for s in shots { ids += s.entry_id + "@" + s.in_s + "|"; }
    let stem = "ch" + ctx.channel.number + "-" + hex(fnv(ids + chapter_bpm));
    let plan = plan_for(ctx, cfg, drawn, songs, arranged.seams, layers, cuts, shots, chapter_bpm, seed, fitted, stem);
    let plan_path = cfg.dir + "/" + stem + ".plan.json";
    write_file(ctx, plan_path, to_json(plan));
    let notes = seam_notes(plan, drawn, fitted.bars);
    write_file(ctx, cfg.dir + "/" + stem + ".audit.txt", notes);

    let chapter_ms = (fitted.hooks * fitted.bars * bar_s * 1000.0).round().to_int();

    let names = "chapter:" + stem;
    for d in drawn { names += "|" + d.entry_id; }
    log("megamix: chapter=" + names + " bpm=" + chapter_bpm + " key=" + plan.notes.key + " bars=" + (fitted.hooks * fitted.bars));
    for i in 0..plan.notes.hooks.len() {
        let h = plan.notes.hooks[i];
        log("megamix: seam=" + i + " bar=" + (i * fitted.bars) + " entry=" + h.entry_id + " landing_s=" + h.landing_s + " semitones=" + h.semitones
            + " strategy=" + (if i == 0 { "opens" } else { plan.notes.seams[i - 1].strategy }));
    }

    let t = timeline();
    let standing = stand_in(ctx, cfg);
    let seg = segment();
    if standing == () {
        seg = seg.black().looped();
    } else {
        seg = seg.pending(plan.output, standing, plan_path).looped().own_audio();
    }
    seg = seg.fixed_ms(chapter_ms).name(names);
    if cfg.card_script != () {
        seg = seg.overlay(cfg.card_script, #{ cards: plan.cards, bar_s: bar_s });
    }
    t.push(seg);
    t
}

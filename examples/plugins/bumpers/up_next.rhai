// An up-next promo: a clip of the film that follows the gap, with its title,
// "Tonight" or "Tomorrow" and start time, and the channel drawn over it.
//
// The clip comes from the first source that yields one:
//   1. picks        a hand-picked window of this film
//   2. search       the best Shot plex-db-ex's explorer finds for `phrase`,
//                   played from the film's own file (needs the `http` grant)
//   3. broll_generic  a generic clip, when neither of the above has one
//
// config (the block's `fill:` entry):
//   picks:          #{ "<entry_id>": #{ path: "/media/x.mkv", in: 120, out: 135 } }
//                   `in` and `out` are seconds into the file
//   plexdb_url:     base URL of the plex-db-ex explorer, e.g. "http://host:8765"
//   phrase:         text the Shot search looks for
//   slot_rating:    the highest rating this slot may air: "G", "PG", "PG-13",
//                   "R" or "NC-17" (TV-Y, TV-G, TV-PG, TV-14, TV-MA rank beside
//                   them). A film above it, or an unrated one, gets neither a pick
//                   nor a search. Unset allows any rating.
//   broll_generic:  [ #{ path, in, out }, ... ] drawn with ctx.rng
//
// The overlay path is relative to the channel config's directory, so this
// script is written for a channel under examples/channels/.
fn hooks() { ["bumper"] }
fn capabilities() { ["http"] }

fn min_clip_ms() { 5000 }

fn rating_rank(rating) {
    if rating == () { return (); }
    switch rating.to_upper() {
        "G" | "TV-Y" | "TV-G" => 0,
        "PG" | "TV-PG" => 1,
        "PG-13" | "TV-14" => 2,
        "R" | "TV-MA" => 3,
        "NC-17" => 4,
        _ => (),
    }
}

// True when `rating` may not air in a slot capped at `slot_rating`. An unrated
// or unknown film exceeds any cap.
fn rating_exceeds(rating, slot_rating) {
    if slot_rating == () { return false; }
    let cap = rating_rank(slot_rating);
    if cap == () { throw "up_next: slot_rating " + slot_rating + " is not a rating this script knows"; }
    let rank = rating_rank(rating);
    rank == () || rank > cap
}

fn json_str(text) {
    // replace() edits the string in place and returns nothing.
    let escaped = text;
    escaped.replace("\\", "\\\\");
    escaped.replace("\"", "\\\"");
    escaped.replace("\n", "\\n");
    escaped.replace("\r", "\\r");
    escaped.replace("\t", "\\t");
    "\"" + escaped + "\""
}

// The best Shot of the film for the phrase, as #{ path, in_ms, out_ms }, or ().
fn search_clip(ctx, film) {
    let cfg = ctx.config;
    let plex_id = if film.source_ids == () { () } else { film.source_ids.plex };
    if cfg.plexdb_url == () || cfg.phrase == () || plex_id == () || film.path == () { return (); }
    let body = "{\"phrase\":" + json_str(cfg.phrase)
        + ",\"pool\":[" + json_str("" + plex_id) + "]"
        + ",\"draw\":{\"n\":1,\"seed\":" + ctx.rng.int(0, 1000000000) + "}}";
    let reply = ();
    try {
        reply = ctx.http(cfg.plexdb_url + "/api/search/run", "POST", body,
                         #{ "Content-Type": "application/json" });
    } catch (e) {
        log("up_next: item=" + film.entry_id + " search=skipped reason=" + e);
        return ();
    }
    let shots = parse_json(reply).shots;
    if shots == () || shots.len() == 0 {
        log("up_next: item=" + film.entry_id + " search=empty");
        return ();
    }
    let shot = shots[0];
    for s in shots {
        if s.drawn == 1 { shot = s; break; }
    }
    log("up_next: item=" + film.entry_id + " search=hit in_ms=" + shot.in_ms + " out_ms=" + shot.out_ms);
    #{ path: film.path, in_ms: shot.in_ms, out_ms: shot.out_ms }
}

fn pick_clip(ctx, film) {
    let picks = ctx.config.picks;
    let pick = if picks == () { () } else { picks[film.entry_id] };
    if pick == () { return (); }
    #{ path: pick.path, in_ms: pick["in"] * 1000, out_ms: pick.out * 1000 }
}

fn broll_clip(ctx) {
    let pick = (ctx.config.broll_generic ?? []).random(ctx.rng);
    if pick == () { return (); }
    #{ path: pick.path, in_ms: pick["in"] * 1000, out_ms: pick.out * 1000 }
}

fn clock(hour, minute) {
    let h = hour % 12;
    if h == 0 { h = 12; }
    h + ":" + (if minute < 10 { "0" } else { "" }) + minute + (if hour < 12 { " AM" } else { " PM" })
}

// "Tonight 8:00 PM" for an evening start, "Today 9:00 AM" for an earlier one on
// the gap's own weekday, and "Tomorrow 8:00 PM" on a later weekday.
fn when_line(ctx, film) {
    if film.start == () { return ""; }
    let at = local_time(film.start);
    let now = local_time(film.start - ctx.gap_ms / 1000);
    let day = if at.weekday != now.weekday { "Tomorrow" } else if at.hour >= 17 { "Tonight" } else { "Today" };
    day + " " + clock(at.hour, at.minute)
}

fn card(seg, film, ctx) {
    seg.overlay("../../crates/etv-overlay/fixtures/scripts/text_card.rhai",
                #{ lines: ["Up next", film.title ?? "", when_line(ctx, film), ctx.channel.name],
                   font_size: 48.0 })
}

fn build(ctx) {
    let t = timeline();
    let film = ctx.next;
    if film == () { return t; }

    let slot_rating = ctx.config.slot_rating;
    // A film above the slot's rating gets neither a pick nor a search.
    let win = ();
    if rating_exceeds(film.content_rating, slot_rating) {
        log("up_next: item=" + film.entry_id + " rating=" + film.content_rating + " exceeds slot " + slot_rating + "; no pick or search");
    } else {
        win = pick_clip(ctx, film);
        if win == () && ctx.config.plexdb_url != () { win = search_clip(ctx, film); }
    }
    let source = "film";
    if win == () {
        win = broll_clip(ctx);
        source = "broll";
    }
    if win == () { return t; }

    let len = win.out_ms - win.in_ms;
    if len > ctx.gap_ms { len = ctx.gap_ms; }
    log("up_next: item=" + film.entry_id + " source=" + source + " path=" + win.path + " in_ms=" + win.in_ms + " len_ms=" + len);
    t.push(card(segment().clip(clip(win.path, win.in_ms, win.in_ms + len))
        .name("up-next-" + source)
        .flex_ms(if len < min_clip_ms() { len } else { min_clip_ms() }, len, 1), film, ctx));
    // What the clip leaves of the gap is held on the same card.
    t.push(card(segment().black().name("up-next-hold").flex(0.0, 3600.0, 0).looped(), film, ctx));
    t
}

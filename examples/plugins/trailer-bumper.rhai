// A trailer bumper: airs the Plex trailer of the film that follows the gap, or
// trailers for films later in the schedule.
//
// config (the block's `fill:` entry):
//   mode: next        the next film's own trailer, with an "up next" card over it
//   mode: upcoming    trailers for the next film and the films after it, nearest
//                     first, each film at most once in the gap (max_films,
//                     default 4)
//
// It returns an empty timeline when no film has a trailer or Plex does not
// answer, which makes the fill pass draw its next entry. Plex is reached at
// {{PLEX_URL}} with {{PLEX_TOKEN}}: the station fills both in from its own
// environment, and ETV-next fills them in again when it plays the trailer, so
// the token is never in this script or in a playout file. The station sends
// {{PLEX_TOKEN}} only to the PLEX_URL host.
fn hooks() { ["bumper"] }
fn capabilities() { ["http"] }

fn min_trailer_ms() { 5000 }


// The first trailer among a film's Plex extras: #{ url, len_ms }, or ().
fn trailer_for(ctx, item) {
    let ids = item.source_ids;
    let rating_key = if ids == () { () } else { ids.plex };
    if rating_key == () {
        log("trailer: item=" + item.entry_id + " found=false reason=no plex id");
        return ();
    }
    let body = ();
    try {
        body = ctx.http(
            "{{PLEX_URL}}" + "/library/metadata/" + rating_key
                + "/extras?X-Plex-Token=" + "{{PLEX_TOKEN}}",
            "GET", "", #{ Accept: "application/json" });
    } catch (e) {
        log("trailer: item=" + item.entry_id + " found=false reason=plex call failed: " + e);
        return ();
    }
    let extras = parse_json(body).MediaContainer.Metadata;
    if extras == () {
        log("trailer: item=" + item.entry_id + " found=false reason=no extras");
        return ();
    }
    for extra in extras {
        if extra.subtype != "trailer" || extra.Media == () { continue; }
        let media = extra.Media[0];
        if media.Part == () { continue; }
        let key = media.Part[0].key;
        let len_ms = media.duration ?? extra.duration;
        if key == () || len_ms == () || len_ms < min_trailer_ms() { continue; }
        let url = if key.starts_with("http") {
            key
        } else {
            // Plex's own part keys can already carry a query (`?fmt=4&bitrate=750`).
            "{{PLEX_URL}}" + key + (if key.contains("?") { "&" } else { "?" })
                + "X-Plex-Token=" + "{{PLEX_TOKEN}}"
        };
        log("trailer: item=" + item.entry_id + " found=true len_ms=" + len_ms);
        return #{ url: url, len_ms: len_ms };
    }
    log("trailer: item=" + item.entry_id + " found=false reason=no trailer extra");
    ()
}

fn card(seg, title, word) {
    seg.overlay("shared/bumpers/card.rhai", #{ title: title, word: word })
}

fn build(ctx) {
    let mode = ctx.config.mode ?? "next";
    let t = timeline();

    if mode == "next" {
        if ctx.next == () || ctx.next.type != "movie" { return t; }
        let trailer = trailer_for(ctx, ctx.next);
        if trailer == () { return t; }
        let len = if trailer.len_ms < ctx.gap_ms { trailer.len_ms } else { ctx.gap_ms };
        t.push(card(segment().http(trailer.url, 0, trailer.len_ms)
            .flex_ms(if len < min_trailer_ms() { len } else { min_trailer_ms() }, len, 1),
            ctx.next.title, "Up next"));
        // What the trailer leaves of the gap is held on the same card.
        t.push(card(segment().black().flex(0.0, 3600.0, 0).looped(), ctx.next.title, "Up next"));
        return t;
    }

    if mode != "upcoming" { throw "trailer-bumper: mode must be next or upcoming, got " + mode; }
    let films = [];
    let seen = [];
    let candidates = if ctx.next == () { ctx.upcoming } else { [ctx.next] + ctx.upcoming };
    for item in candidates {
        if item.type != "movie" || seen.contains(item.entry_id) { continue; }
        seen.push(item.entry_id);
        films.push(item);
    }
    let max_films = ctx.config.max_films ?? 4;
    let remaining = ctx.gap_ms;
    let used = 0;
    for item in films {
        if used >= max_films || remaining < min_trailer_ms() { break; }
        let trailer = trailer_for(ctx, item);
        if trailer == () { continue; }
        let len = if trailer.len_ms < remaining { trailer.len_ms } else { remaining };
        t.push(card(segment().http(trailer.url, 0, trailer.len_ms)
            .flex_ms(if len < min_trailer_ms() { len } else { min_trailer_ms() }, len, 1),
            item.title, "Coming soon"));
        remaining -= len;
        used += 1;
    }
    if used == 0 { return t; }
    // What the trailers leave of the gap is black, so the draw can reach it.
    t.push(segment().black().flex(0.0, 3600.0, 0).looped());
    t
}

// An archival bumper: airs a reel a person has named in a JSON manifest beside
// this script. No rule decides which footage may air; a clip is in the manifest
// because somebody put it there.
//
// manifest (`config.manifest`, default "archival.json", read from this folder):
//   { "clips": [ { "path": "...", "tags": ["ident"], "burned_in_text": true,
//                  "minutes": null } ] }
//   tags: ident, bumper, promo, countdown, intermission, snipe
//   minutes: set on countdown clips only; the clip airs only in a gap of about
//            that many minutes
//
// config (the block's `fill:` entry):
//   manifest:  manifest file name in this script's folder
//   tags:      [ "ident", ... ] only clips carrying one of these tags are drawn;
//              unset draws any clip
//   fps:       frame rate of the reels, default 30. Two frames come off both
//              ends of every clip, so a reel's first and last frames never air.
//
// The clip that aired least recently on this channel is drawn, a clip that
// never aired first. burned_in_text: true airs the reel bare; false draws the
// channel's name over it.
//
// The overlay path is relative to the channel config's directory, so this
// script is written for a channel under examples/channels/.
fn hooks() { ["bumper"] }
fn capabilities() { ["probe", "read"] }

fn trim_frames() { 2 }

fn load_clips(ctx) {
    let name = ctx.config.manifest ?? "archival.json";
    let text = ctx.read(name);
    if text == () { throw "archival: manifest " + name + " not found beside the script"; }
    let doc = ();
    try {
        doc = parse_json(text);
    } catch (e) {
        throw "archival: manifest " + name + " is not valid JSON: " + e;
    }
    if type_of(doc) != "map" || type_of(doc.clips) != "array" {
        throw "archival: manifest " + name + " must be { \"clips\": [ ... ] }";
    }
    let n = 0;
    for c in doc.clips {
        if type_of(c) != "map" || type_of(c.path) != "string" || type_of(c.tags) != "array"
            || type_of(c.burned_in_text) != "bool" {
            throw "archival: manifest " + name + " clip " + n + " needs a string path, a tags array and a boolean burned_in_text";
        }
        n += 1;
    }
    doc.clips
}

fn wanted(c, tags) {
    if tags == () { return true; }
    for t in tags {
        if c.tags.contains(t) { return true; }
    }
    false
}

// A countdown clip airs only in a gap of about its own length in minutes.
fn fits_gap(c, gap_ms) {
    if !c.tags.contains("countdown") { return true; }
    if c.minutes == () { return false; }
    ((gap_ms * 1.0) / 60000.0).round().to_int() == c.minutes.to_int()
}

fn last_aired_ms(ctx, path) {
    let rows = ctx.airings(#{ kind: "bumper", source: path, order: "desc", limit: 1 });
    if rows.len() == 0 { -1 } else { rows[0].start_ms }
}

fn build(ctx) {
    let t = timeline();
    let candidates = [];
    for c in load_clips(ctx) {
        if wanted(c, ctx.config.tags) && fits_gap(c, ctx.gap_ms) { candidates.push(c); }
    }

    let oldest = ();
    let ties = [];
    for c in candidates {
        let at = last_aired_ms(ctx, c.path);
        if oldest == () || at < oldest {
            oldest = at;
            ties = [c];
        } else if at == oldest {
            ties.push(c);
        }
    }
    let c = ties.random(ctx.rng);
    if c == () { return t; }

    let probed = ctx.probe(c.path);
    if probed == () || probed.duration_ms == () {
        log("archival: path=" + c.path + " skipped=unprobeable");
        return t;
    }
    let trim = ((trim_frames() * 1000.0) / ((ctx.config.fps ?? 30) * 1.0)).round().to_int();
    let in_ms = trim;
    let out_ms = probed.duration_ms - trim;
    if out_ms - in_ms > ctx.gap_ms { out_ms = in_ms + ctx.gap_ms; }
    if out_ms <= in_ms {
        log("archival: path=" + c.path + " skipped=shorter than its trims");
        return t;
    }
    log("archival: path=" + c.path + " in_ms=" + in_ms + " out_ms=" + out_ms + " last_aired_ms=" + oldest);

    let seg = segment().clip(clip(c.path, in_ms, out_ms)).name("archival").fixed_ms(out_ms - in_ms);
    if !c.burned_in_text {
        seg = seg.overlay("../../crates/etv-overlay/fixtures/scripts/text_card.rhai",
                          #{ lines: [ctx.channel.name], font_size: 48.0 });
    }
    t.push(seg);
    t
}

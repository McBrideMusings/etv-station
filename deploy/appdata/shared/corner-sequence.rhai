// Corner sequence: short beats that slide out from behind a bottom-right logo,
// one at a time, and tuck back in.
//
//   1  WHAT     the block, then what is on — a film's title and year, or a
//               show (year), season/episode and episode title
//   2  WHY      why this title is on, when the item carries a reason
//   3  UP NEXT  what follows, near the end of the item
//
// Beats 1 and 2 run as a pair: when an item starts, and again every
// `interval_secs`. Beat 3 runs once, `up_next_lead_secs` before the item ends,
// and wins over the pair if the two would overlap.
//
// WHERE THE TEXT COMES FROM. Nothing here is computed; every string is a key
// someone else wrote:
//
//   title, sub_title, season, episode, year      the item's program fields
//   next_title, next_sub_title, next_season, …   the next item's
//   metadata.pool_label                           the pool's `label:`
//   metadata.why, metadata.why_reasons            a pool plugin's reason: the
//                                                 lead phrase, and the bare
//                                                 labels it follows with
//                                                 (taste-cosine.rhai writes
//                                                 them from the pool's
//                                                 `config: { why: … }`)
//
// The station carries `metadata` to this script untouched and never reads
// it, so a plugin and this overlay agree on `why`/`why_reasons` between
// themselves. An item without a `why` skips beat 2; one without a label shows
// `fallback_eyebrow` over its title.
//
// LAYERS. This graphic drives exactly four layers of the overlay spec, by
// index, and must be the first graphic in it so the `below:` indices line up:
//
//   0 scrim (edge: bottom)
//   1 eyebrow   text, corner: bottom_right, align: right
//   2 line      text, below: 1, align: right
//   3 detail    text, below: 2, align: right
//
// The logo is a later graphic, so it draws on top and the text tucks in
// behind it. Layer 1's `margin` sets the block's right edge (clear of the
// logo); `drop_px` moves the block down so its last line sits on the logo's
// baseline — the engine anchors a corner block by its own box, and a margin is
// one number for both axes.
//
// MOTION. Each beat slides left from under the logo with ease-out, fading in
// once it has cleared most of the logo (see `fade`), holds, then slides back
// with ease-in and fades out. Only the eyebrow moves; lines 2 and 3 hang from
// it with `below:`, so they travel with it.
//
// Config (all optional, via the graphic's `config:`):
//   interval_secs       — seconds between beat pairs (default 300)
//   hold_secs           — seconds a beat sits fully out (default 8.0)
//   gap_secs            — seconds between beat 1 and beat 2 (default 1.5)
//   slide_secs          — seconds a slide in or out takes (default 0.5)
//   slide_distance_px   — how far a beat travels (default 90)
//   drop_px             — layer 1's downward shift (default 0)
//   row_gap_px          — extra space above lines 2 and 3 (default 6); `below:`
//                          alone sets each line flush on the box above it
//   up_next_lead_secs   — beat 3 starts this long before the end (default 180)
//   why_joiner          — between the labels in beat 2's detail (default " · ")
//   why_eyebrow         — beat 2's heading (default "WHY IT'S ON")
//   next_eyebrow        — beat 3's heading (default "UP NEXT")
//   fallback_eyebrow    — beat 1's heading when the item has no label
//                          (default "NOW")
//   max_chars           — hard cut per line (default 48), ellipsis appended

fn ease_out_cubic(p) {
    let q = 1.0 - p;
    1.0 - q * q * q
}

fn ease_in_cubic(p) {
    p * p * p
}

// A numeric config value as a float, whether the YAML wrote `8` or `8.0` —
// the engine ignores an integer `offset_x`/`offset_y`, and mixing an integer
// into float arithmetic throws.
fn num(v, fallback) {
    if v == () { fallback } else if type_of(v) == "i64" { v.to_float() } else { v }
}

fn clip(s, max_chars) {
    if s.len > max_chars { s.sub_string(0, max_chars) + "…" } else { s }
}

// A string field of a metadata blob, or "" when the blob is absent, is not a
// map, or the key is missing or not text. Index access on `()` would throw.
fn text_of(blob, key) {
    if type_of(blob) == "map" && blob.contains(key) && type_of(blob[key]) == "string" {
        blob[key]
    } else {
        ""
    }
}

// The labels of a string-array field of a metadata blob joined with `sep`, or
// "" when the blob or field is absent or holds anything else. The labels
// are never inspected: a title and a keyword read the same here.
fn joined_list(blob, key, sep) {
    let out = "";
    if type_of(blob) == "map" && blob.contains(key) && type_of(blob[key]) == "array" {
        for v in blob[key] {
            if type_of(v) != "string" { continue; }
            if out != "" { out += sep; }
            out += v;
        }
    }
    out
}

// "S3 E12", or "" when either number is unknown.
fn season_episode(s, e) {
    if s >= 0 && e >= 0 { "S" + s + " E" + e } else { "" }
}

fn joined(a, b) {
    if a == "" { b } else if b == "" { a } else { a + " · " + b }
}

// Opacity for a beat `p` of the way out (0 = under the logo, 1 = in place).
// Held at zero for the first 30% of the travel: the logo is a wordmark, not a
// solid plate, so text passing under it shows through between the letters
// unless it is still invisible there.
fn fade(p) {
    if p <= 0.3 { 0.0 } else { (p - 0.3) / 0.7 }
}

// Where a beat of length `hold` is at local time `lt`: alpha and how far it
// still sits toward (under) the logo.
fn motion(lt, slide, hold, distance) {
    if lt < 0.0 { return #{ alpha: 0.0, dx: distance }; }
    if lt < slide {
        let p = ease_out_cubic(lt / slide);
        return #{ alpha: fade(p), dx: distance * (1.0 - p) };
    }
    if lt < slide + hold { return #{ alpha: 1.0, dx: 0.0 }; }
    let out = lt - slide - hold;
    if out < slide {
        let p = ease_in_cubic(out / slide);
        return #{ alpha: fade(1.0 - p), dx: distance * p };
    }
    #{ alpha: 0.0, dx: distance }
}

let interval = num(config.interval_secs, 300.0);
let hold = num(config.hold_secs, 8.0);
let gap = num(config.gap_secs, 1.5);
let slide = num(config.slide_secs, 0.5);
let distance = num(config.slide_distance_px, 90.0);
let drop = num(config.drop_px, 0.0);
let row_gap = num(config.row_gap_px, 6.0);
let lead = num(config.up_next_lead_secs, 180.0);
let why_eyebrow = if config.why_eyebrow == () { "WHY IT'S ON" } else { config.why_eyebrow };
let next_eyebrow = if config.next_eyebrow == () { "UP NEXT" } else { config.next_eyebrow };
let fallback_eyebrow = if config.fallback_eyebrow == () { "NOW" } else { config.fallback_eyebrow };
let why_joiner = if config.why_joiner == () { " · " } else { config.why_joiner };
let max_chars = if config.max_chars == () { 48 } else { config.max_chars };

let beat_len = slide + hold + slide;
let why = text_of(metadata, "why");
let pair_len = if why == "" { beat_len } else { beat_len + gap + beat_len };

// Beat 1: the block, then what is on.
let label = text_of(metadata, "pool_label");
let is_episode = season >= 0 && episode >= 0;
let what = #{
    eyebrow: if label == "" { fallback_eyebrow } else { label.to_upper() },
    line: if is_episode && year > 0 { title + " (" + year + ")" } else { title },
    detail: if is_episode {
        joined(season_episode(season, episode), sub_title)
    } else if year > 0 {
        "" + year
    } else {
        ""
    },
};

// Beat 3: what follows.
let next_is_episode = next_season >= 0 && next_episode >= 0;
let next = #{
    eyebrow: next_eyebrow,
    line: if !next_is_episode && next_year > 0 { next_title + " (" + next_year + ")" } else { next_title },
    detail: if next_is_episode { joined(season_episode(next_season, next_episode), next_sub_title) } else { "" },
};

// Which beat is up, and where in it we are. -1.0 is "nothing showing".
let beat = ();
let lt = -1.0;

if next_title != "" && item_remaining >= 0.0 && item_remaining <= lead && item_remaining > lead - beat_len {
    beat = next;
    lt = lead - item_remaining;
} else if title != "" {
    // The pair, keyed to the item's own clock when it has just started,
    // otherwise to the process clock's interval. An interval pair that would
    // overlap beat 3 is skipped whole: cut short by beat 3, it would come back
    // mid-hold, with no slide in, once beat 3 ended.
    let near_next = next_title != "" && item_remaining >= 0.0
        && item_remaining <= lead + pair_len && item_remaining > lead - beat_len - pair_len;
    let pt = -1.0;
    if item_elapsed >= 0.0 && item_elapsed < pair_len {
        pt = item_elapsed;
    } else if !near_next {
        let t = time % interval;
        if t < pair_len { pt = t; }
    }
    if pt >= 0.0 {
        if pt < beat_len {
            beat = what;
            lt = pt;
        } else if why != "" && pt >= beat_len + gap {
            beat = #{ eyebrow: why_eyebrow, line: why, detail: joined_list(metadata, "why_reasons", why_joiner) };
            lt = pt - beat_len - gap;
        }
    }
}

let m = motion(lt, slide, hold, distance);
let showing = beat != () && m.alpha > 0.004;

let layers = [
    #{ visible: showing, opacity: m.alpha },
    #{
        visible: showing,
        opacity: m.alpha,
        content: if showing { clip(beat.eyebrow, max_chars) } else { "" },
        offset_x: m.dx,
        offset_y: drop,
    },
    #{
        visible: showing,
        opacity: m.alpha,
        content: if showing { clip(beat.line, max_chars) } else { "" },
        offset_y: row_gap,
    },
    #{
        visible: showing && beat.detail != "",
        opacity: m.alpha,
        content: if showing { clip(beat.detail, max_chars) } else { "" },
        offset_y: row_gap,
    },
];

#{ layers: layers }

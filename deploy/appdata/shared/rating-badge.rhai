// Content-rating badge: the item's own rating ("TV-14", "R") as the small boxed
// icon a broadcaster puts in a corner when a programme starts.
//
// It is a graphic by itself, with no knowledge of any other. Declare it in the
// station overlay beside the Now/Next snipe and the block card (see
// docs/schema.md "Rating badge") and it layers over or under them; nothing here
// reads, hides or depends on another graphic.
//
// WHERE THE RATING COMES FROM
//
// `content_rating` is the catalog's rating for the item currently airing — an
// MPAA code on a film, a TV Parental Guideline on an episode — carried in the
// playout file's `program.content_rating`. The script draws a badge only for the
// eleven it has art for and nothing for anything else ("Not Rated", "Approved",
// "16+", or no rating at all): a badge reading something we invented would be
// worse than none.
//
//   TV-Y  TV-Y7  TV-G  TV-PG  TV-14  TV-MA      G  PG  PG-13  R  NC-17
//
// Plex sometimes qualifies a rating with a country ("gb/15"); the part after the
// slash is what is compared, so "us/TV-MA" draws TV-MA and "gb/15" draws nothing.
//
// THE ART
//
// One image layer per rating, all declared in the graphic's spec at the same
// corner and margin, in exactly this order — the script addresses them by index:
//
//   0 TV-Y  1 TV-Y7  2 TV-G  3 TV-PG  4 TV-14  5 TV-MA
//   6 G     7 PG     8 PG-13 9 R       10 NC-17
//
// `tools/make-rating-icons.sh` draws the eleven PNGs into
// deploy/appdata/shared/ratings/. To restyle the badge, edit that script and
// run it, or replace the PNGs: any 8-bit RGBA image of the same name works, at
// any size, because the layer's `height` sets what is drawn.
//
// WHEN IT SHOWS
//
// At the start of every item that has a rating: it fades in, holds, fades out,
// timed off `item_elapsed`. `repeat_secs` brings it back part-way through a long
// item (a film) on that interval. A viewer who tunes in after the first seconds
// sees no badge until the next repeat; that is also how a broadcaster's does it.
//
// Reads from per-frame Rhai scope:
//   content_rating  — the current item's catalog rating, "" when none
//   item_elapsed    — seconds since the current item started (-1 unknown)
//
// Config (all optional, via the graphic's `config:`):
//   hold_secs     — seconds fully visible (default 15.0)
//   fade_in_secs  — default 0.5
//   fade_out_secs — default 0.8
//   repeat_secs   — if above 0, show again every this many seconds of the item
//                    (default 0: start of item only)

// The rating with any "country/" prefix dropped, trimmed and upper-cased.
fn normalize(raw) {
    // trim() edits in place and returns unit, so it cannot be chained.
    let r = raw;
    r.trim();
    let slash = r.index_of('/');
    if slash >= 0 {
        r = r.sub_string(slash + 1);
    }
    r.to_upper()
}

// Position of `code` in `ratings`, or -1.
fn position_of(ratings, code) {
    for i in 0..ratings.len() {
        if ratings[i] == code {
            return i;
        }
    }
    -1
}

// Opacity at `t` seconds into one showing: ramp up, hold, ramp down, then 0.
fn opacity_at(t, fade_in, hold, fade_out) {
    if t < 0.0 {
        0.0
    } else if t < fade_in {
        t / fade_in
    } else if t < fade_in + hold {
        1.0
    } else if t < fade_in + hold + fade_out {
        1.0 - (t - fade_in - hold) / fade_out
    } else {
        0.0
    }
}

let hold = if config.hold_secs == () { 15.0 } else { config.hold_secs };
let fade_in = if config.fade_in_secs == () { 0.5 } else { config.fade_in_secs };
let fade_out = if config.fade_out_secs == () { 0.8 } else { config.fade_out_secs };
let repeat = if config.repeat_secs == () { 0.0 } else { config.repeat_secs };

// The art's order. Keep in step with the layer list and make-rating-icons.sh.
let ratings = ["TV-Y", "TV-Y7", "TV-G", "TV-PG", "TV-14", "TV-MA", "G", "PG", "PG-13", "R", "NC-17"];

let idx = position_of(ratings, normalize(content_rating));

// Seconds into the current showing: the item's own clock, wrapped by `repeat`.
let t = -1.0;
if idx >= 0 && item_elapsed >= 0.0 {
    t = item_elapsed;
    if repeat > 0.0 {
        t = item_elapsed % repeat;
    }
}
let alpha = opacity_at(t, fade_in, hold, fade_out);

let layers = [];
for i in 0..ratings.len() {
    layers.push(#{ visible: i == idx && alpha > 0.004, opacity: alpha });
}

#{ layers: layers }

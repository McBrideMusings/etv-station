// Block card: names the block that is on air ("THIS IS / The Zombies Block")
// and, when the pool carries one, a content advisory beneath it.
//
// Where the text comes from. The station writes two keys into the metadata of
// every item a labelled pool draws (`label:` / `advisory:` on the pool in
// channel.yaml; docs/schema.md "Pool label and advisory"):
//
//   metadata.pool_label       "The Zombies Block"
//   metadata.pool_advisory    "Contains graphic violence"     (optional)
//
// Nothing here parses the audit trail — that is for `admin audit`, not for
// display. An item from a pool with no label carries neither key and this
// graphic draws nothing for it.
//
// WHEN IT SHOWS
//
//   On a change. The first seconds of any item whose label differs from the
//   previous item's (`prev_metadata`), so the card lands when a block starts —
//   Kaiju ends, Zombies begins — and not in the middle of one. The first item
//   after a restart has no previous item, so it counts as a change.
//
//   An item from an unlabelled pool has an empty label, so by default it counts
//   as different from a labelled neighbour: in a pattern of `films x1, bumpers
//   x1` where only `films` is labelled, every film follows a bumper and every
//   film draws the card. Set `announce_after_unlabelled: false` to compare only
//   against a labelled previous item; the card then no longer lands on a block
//   that follows unlabelled programming, and the interval card is the only one.
//
//   On an interval. `time % interval_secs`, shifted by `offset_secs`, so a
//   viewer who tuned in halfway through a block still learns which one it is.
//
// The change card wins when both would fire at once. A card is keyed to the
// item's own clock (`item_elapsed`) on a change, and to the process clock
// (`time`) on the interval, so the two never run together.
//
// HOW IT SITS WITH THE NOW/NEXT SNIPE
//
// The snipe (now-next-snipe.rhai) draws bottom-left. This card draws top-left,
// so the two can be on screen together without drawing over each other, and
// neither script needs to know the other exists. Its default `offset_secs` of
// 150 against the snipe's cycle start at 0 keeps them apart on the interval
// anyway. Each is its own graphic in the overlay spec: this script drives
// exactly its own four layers, by index —
//
//   0 scrim   1 eyebrow   2 label   3 advisory
//
// MOTION is the snipe's: slide in from the left with ease-out and a small
// overshoot, hold, slide out with ease-in, each row trailing the one above it by
// `stagger_secs`. See now-next-snipe.rhai for why each of the three choices.
// The helpers are copied rather than shared because a Rhai script cannot import
// another file here.
//
// Reads from per-frame Rhai scope:
//   time                       — process-elapsed seconds since overlay start
//   item_elapsed               — seconds since the current item started (-1 unknown)
//   metadata / prev_metadata   — the current and the previous item's blob
//
// Config (all optional, via the graphic's `config:`):
//   interval_secs       — seconds between interval cards (default 300)
//   offset_secs         — where in that interval the card starts (default 150)
//   hold_secs           — seconds the card sits fully visible with no advisory
//                          (default 5.0)
//   advisory_hold_secs  — the same when there is an advisory to read (default 8.0)
//   rise_secs           — how long a row takes to arrive (default 0.45)
//   fall_secs           — how long it takes to leave (default 0.35)
//   stagger_secs        — how far each row trails the one above (default 0.08)
//   slide_distance_px   — how far a row travels (default 140)
//   overshoot           — drift past the resting place before settling (default 0.9)
//   row_px              — vertical step between rows (default 26)
//   announce_after_unlabelled — whether an item whose previous item has no
//                          label counts as a block change (default true)
//   eyebrow             — the small line above the label (default "THIS IS")
//   max_chars           — hard cut for the label (default 40); the advisory
//                          allows 1.6x that, ellipsis appended

fn ease_out_cubic(p) {
    let q = 1.0 - p;
    1.0 - q * q * q
}

fn ease_out_back(p, s) {
    let q = p - 1.0;
    1.0 + (s + 1.0) * q * q * q + s * q * q
}

fn ease_in_cubic(p) {
    p * p * p
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

fn element_at(lt, delay, rise, hold, fall, distance, overshoot) {
    let t = lt - delay;
    if t < 0.0 {
        return #{ alpha: 0.0, dx: -distance };
    }
    if t < rise {
        let p = t / rise;
        return #{
            alpha: ease_out_cubic(p),
            dx: -distance * (1.0 - ease_out_back(p, overshoot)),
        };
    }
    let held = t - rise;
    if held < hold {
        return #{ alpha: 1.0, dx: 0.0 };
    }
    let leaving = held - hold;
    if leaving < fall {
        let p = leaving / fall;
        return #{
            alpha: 1.0 - ease_in_cubic(p),
            dx: -distance * ease_in_cubic(p),
        };
    }
    #{ alpha: 0.0, dx: -distance }
}

let interval = if config.interval_secs == () { 300.0 } else { config.interval_secs };
let offset = if config.offset_secs == () { 150.0 } else { config.offset_secs };
let hold_plain = if config.hold_secs == () { 5.0 } else { config.hold_secs };
let hold_advisory = if config.advisory_hold_secs == () { 8.0 } else { config.advisory_hold_secs };
let rise = if config.rise_secs == () { 0.45 } else { config.rise_secs };
let fall = if config.fall_secs == () { 0.35 } else { config.fall_secs };
let stagger = if config.stagger_secs == () { 0.08 } else { config.stagger_secs };
let distance = if config.slide_distance_px == () { 140.0 } else { config.slide_distance_px };
let overshoot = if config.overshoot == () { 0.9 } else { config.overshoot };
let row = if config.row_px == () { 26.0 } else { config.row_px };
let eyebrow = if config.eyebrow == () { "THIS IS" } else { config.eyebrow };
let max_chars = if config.max_chars == () { 40 } else { config.max_chars };
let announce_after_unlabelled =
    if config.announce_after_unlabelled == () { true } else { config.announce_after_unlabelled };

let label = clip(text_of(metadata, "pool_label"), max_chars);
let advisory = clip(text_of(metadata, "pool_advisory"), max_chars * 8 / 5);
let prev_label = clip(text_of(prev_metadata, "pool_label"), max_chars);

let hold = if advisory != "" { hold_advisory } else { hold_plain };
// A card is finished when its last row has left: three rows trail the first.
let card_len = rise + hold + fall + stagger * 3.0;

// Where in the card's own timeline we are; -1.0 is "not showing".
let lt = -1.0;
if label != "" {
    let changed = label != prev_label && (prev_label != "" || announce_after_unlabelled);
    if item_elapsed >= 0.0 && item_elapsed < card_len && changed {
        lt = item_elapsed;
    } else {
        let t = (time - offset) % interval;
        let t = if t < 0.0 { t + interval } else { t };
        if t < card_len {
            lt = t;
        }
    }
}

let showing = lt >= 0.0;
let m_eyebrow = #{ alpha: 0.0, dx: -distance };
let m_label = #{ alpha: 0.0, dx: -distance };
let m_advisory = #{ alpha: 0.0, dx: -distance };
if showing {
    m_eyebrow = element_at(lt, 0.0, rise, hold, fall, distance, overshoot);
    m_label = element_at(lt, stagger, rise, hold, fall, distance, overshoot);
    m_advisory = element_at(lt, stagger * 2.0, rise, hold, fall, distance, overshoot);
}

// The scrim leads the cascade so the plate is there when the first glyphs land.
let layers = [
    #{ visible: showing && m_eyebrow.alpha > 0.004, opacity: m_eyebrow.alpha },
];

layers.push(#{
    visible: showing && m_eyebrow.alpha > 0.004,
    opacity: m_eyebrow.alpha,
    content: eyebrow,
    offset_x: m_eyebrow.dx,
    offset_y: 0.0,
});
layers.push(#{
    visible: showing && m_label.alpha > 0.004,
    opacity: m_label.alpha,
    content: label,
    offset_x: m_label.dx,
    offset_y: row * 0.8,
});
layers.push(#{
    visible: showing && advisory != "" && m_advisory.alpha > 0.004,
    opacity: m_advisory.alpha,
    content: advisory,
    offset_x: m_advisory.dx,
    offset_y: row * 2.2,
});

#{ layers: layers }

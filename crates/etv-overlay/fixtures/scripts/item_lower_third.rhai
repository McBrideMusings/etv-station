// Animated lower-third bound to the playing item's own data: "First category —
// Year — Title". Appears `first_secs` into the item, slides in, holds, slides out,
// and repeats every `interval_secs` of the item's own clock, so a long film
// gets it several times and an episode once or twice.
//
// Reads from per-frame Rhai scope:
//   item_elapsed, item_duration — seconds into / length of the current item
//   title, year, categories     — the item's program fields; any may be empty
//   metadata                    — the pool plugin's blob; `metadata.lower_third`
//                                 (a string) replaces the label when set
//
// Config (all optional, via the layer's spec `config:`):
//   first_secs (300), interval_secs (900), hold_secs (10), slide_secs (0.6),
//   slide_distance_px (360)
//
// A field the item lacks is left out of the label; an item with nothing to
// say draws nothing.
fn ease(p) {
    p * p * (3.0 - 2.0 * p)
}

let first = if config.first_secs == () { 300.0 } else { config.first_secs };
let interval = if config.interval_secs == () { 900.0 } else { config.interval_secs };
let hold = if config.hold_secs == () { 10.0 } else { config.hold_secs };
let slide = if config.slide_secs == () { 0.6 } else { config.slide_secs };
let distance = if config.slide_distance_px == () { 360.0 } else { config.slide_distance_px };

let parts = [];
if metadata.lower_third != () && metadata.lower_third != "" {
    parts.push(metadata.lower_third);
} else {
    if categories.len() > 0 { parts.push(categories[0]); }
    if year >= 0 { parts.push(year.to_string()); }
    if title != "" { parts.push(title); }
}
let label = "";
for (part, i) in parts {
    label += if i == 0 { part } else { " — " + part };
}

let since = item_elapsed - first;
let t = if since >= 0.0 { since % interval } else { -1.0 };
let hold_end = slide + hold;
let end = hold_end + slide;

let visible = label != "" && t >= 0.0 && t < end;
let offset_x = distance;
if visible {
    if t < slide {
        offset_x = distance * (1.0 - ease(t / slide));
    } else if t < hold_end {
        offset_x = 0.0;
    } else {
        offset_x = distance * ease((t - hold_end) / slide);
    }
}

#{
    layers: [
        #{
            visible: visible,
            content: label,
            offset_x: offset_x,
        },
    ],
}

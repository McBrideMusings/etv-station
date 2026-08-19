// Animated title chyron: slides the current item's title out from behind a
// corner logo, holds it, then slides it back. Repeats on a fixed interval
// regardless of item boundaries (a 100-minute film gets the same number of
// reminders as a 20-minute episode).
//
// Designed to live on layer 1 of a two-layer overlay (layer 0 the logo, at
// the same corner). Layer 1's own `margin` in the spec should already sit it
// just clear of the logo at rest (offset_x: 0) — `slide_distance_px` is how
// far past that, toward the logo, it starts and ends each cycle, so at the
// extremes it visually tucks in behind the logo's footprint.
//
// Reads from per-frame Rhai scope:
//   time  — process-elapsed seconds since the overlay started
//   title — current item's program.title (empty if unknown)
//
// Config (all optional, via the layer's spec `config:`):
//   interval_secs     — seconds between the start of one slide-in and the
//                        next (default 300 = 5 minutes)
//   hold_secs         — seconds the title stays fully visible (default 10)
//   slide_secs        — seconds each slide (in or out) takes (default 0.6)
//   slide_distance_px — how far offset_x travels (default 360)
// Smoothstep — eases in and out of the slide rather than moving linearly.
fn ease(p) {
    p * p * (3.0 - 2.0 * p)
}

// Dot access on an absent key reads as unit rather than throwing (unlike
// index access on a missing map key) — same fallback idiom every other
// config-reading script in this project uses.
let interval = if config.interval_secs == () { 300.0 } else { config.interval_secs };
let hold = if config.hold_secs == () { 10.0 } else { config.hold_secs };
let slide = if config.slide_secs == () { 0.6 } else { config.slide_secs };
let distance = if config.slide_distance_px == () { 360.0 } else { config.slide_distance_px };

let t = time % interval;
let slide_in_end = slide;
let hold_end = slide_in_end + hold;
let slide_out_end = hold_end + slide;

let visible = title != "" && t < slide_out_end;
let offset_x = distance;

if visible {
    if t < slide_in_end {
        offset_x = distance * (1.0 - ease(t / slide_in_end));
    } else if t < hold_end {
        offset_x = 0.0;
    } else {
        offset_x = distance * ease((t - hold_end) / slide);
    }
}

#{
    layers: [
        #{},
        #{
            visible: visible,
            content: title,
            offset_x: offset_x,
        },
    ],
}

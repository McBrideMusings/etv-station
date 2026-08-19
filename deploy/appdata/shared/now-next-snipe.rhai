// Now/Next snipe: two cards shown one after the other in the same spot —
// NOW arrives, holds, leaves; NEXT arrives, holds, leaves; then the frame is
// clean until the next cycle. See docs/CONTEXT.md "Broadcast graphics
// vocabulary" — a snipe advertises what is coming, as opposed to a lower
// third, which labels what is on now.
//
// Sequential rather than a stacked pair on purpose: one line of text at a
// time is one thing to read, and reusing the same screen position means the
// eye does not have to re-find where the information is.
//
// It formats each card from whichever metadata the item actually carries, so
// one script serves a film channel and a series channel with no per-channel
// variant:
//
//   film     Highest 2 Lowest (2025)
//   episode  Bob's Burgers  S12E01
//
// The discriminator is `season >= 0`: the station writes season and episode
// only for `kind == "episode"` (crates/etv-station/src/resolve.rs:1456), and
// an absent number arrives here as -1.
//
// MOTION
//
// Both cards travel in from the left and leave back to the left, so a card's
// origin and its exit are the same edge — the position it occupies is
// predictable rather than a thing that flies through. Three deliberate
// choices, each costing a few lines:
//
//   Asymmetric easing. Entry uses ease-out (fast first, settling last),
//   because entry is the moment the eye is drawn and an ease-in-out entrance
//   spends its first third barely moving, which reads as sluggish. Exit uses
//   ease-in and is shorter — nobody watches a thing leave.
//
//   Stagger. The value lags its label by `stagger_secs`, so the card
//   assembles rather than sliding in as one rigid block.
//
//   Opacity alongside translate. Fading as it travels is what makes the card
//   read as arriving instead of scrolling past, and it lets the travel
//   distance stay short — long travel with no fade is what makes a graphic
//   look cheap.
//
// The two cards carry different amounts of detail. NOW names the episode
// fully — "Bob's Burgers  S12E01" with the episode title on a second row.
// NEXT is just the show name. What is coming up only has to be identifiable
// enough to decide whether to stay, and neither a number nor an episode title
// helps with that. The main row sits at the same height on both cards, so the
// asymmetry is a row appearing beneath it rather than the card moving.
//
// Drives exactly four layers, by index, declared at the STATION level (see
// station.yaml and ADR 0008):
//
//   0 scrim   1 label   2 value   3 sub
//
// A channel appends its own bug with `overlay: {extend: …}`, which lands at
// index 4 or beyond and is never touched here — an empty map would overwrite
// nothing, but the array simply ends, and layers past its end keep whatever
// the spec declared. Reordering the station's four silently re-targets this
// script, so add new shared layers at the end.
//
// The three text layers are authored at `corner: bottom_left` with the same
// `margin`; this script does the gutter and row placement itself, so they
// differ only in type size and colour.
//
// Reads from per-frame Rhai scope:
//   time                       — process-elapsed seconds since overlay start
//   title / season / episode / year            — the current item
//   next_title / next_season / next_episode / next_year — the lookahead
//
// Config (all optional, via the spec's `config:`):
//   interval_secs     — seconds from one cycle's start to the next
//                        (default 300; `admin overlay-watch` overrides this
//                        to a few seconds so the cycle is watchable)
//   hold_secs         — seconds each card sits fully visible (default 4.5)
//   rise_secs         — how long a card takes to arrive (default 0.45)
//   fall_secs         — how long it takes to leave (default 0.35)
//   gap_secs          — beat between the NOW card leaving and NEXT arriving
//                        (default 0.25)
//   stagger_secs      — how far the value trails its label (default 0.08)
//   slide_distance_px — how far a card travels (default 140)
//   overshoot         — how far past its resting place a card drifts before
//                        settling; 0.0 disables (default 0.9)
//   value_indent_px   — gutter width; where the value sits relative to the
//                        label (default 76)
//   sub_row_px        — how far above the spec's margin the main row sits, so
//                        the episode title can occupy the margin row beneath
//                        it (default 26)
//   max_chars         — hard cut for a title, ellipsis appended (default 40)

// Fast first, settling last. The curve for anything arriving.
fn ease_out_cubic(p) {
    let q = 1.0 - p;
    1.0 - q * q * q
}

// Ease-out with a small overshoot past the target before settling back —
// follow-through, so the card lands with some weight instead of stopping dead.
fn ease_out_back(p, s) {
    let q = p - 1.0;
    1.0 + (s + 1.0) * q * q * q + s * q * q
}

// Slow first, fast last. Only ever used on the way out.
fn ease_in_cubic(p) {
    p * p * p
}

// Rhai has no format spec, so a two-digit season/episode is hand-padded.
fn pad2(n) {
    if n < 10 { "0" + n } else { "" + n }
}

// Hard cut, per the decision to not measure text: a text layer is single-line
// and does not wrap, so a long title would otherwise run off the frame edge.
fn clip(s, max_chars) {
    if s.len > max_chars { s.sub_string(0, max_chars) + "…" } else { s }
}

// One card's text, from whatever that item carries. An episode reads as
// "Show  S02E05"; anything else is a film and reads as "Title (2025)". A year
// of -1 (absent) just drops the parenthetical rather than printing "(-1)".
//
// The clip lands on the title alone and the suffix is appended after, so a
// long name loses its own tail rather than the season/episode or the year —
// "It's Always Sunny in Philadelphia Fore…  S09E12", not a row that trails off
// before ever saying which episode.
// `with_episode` is false on the NEXT card: SxxExx only means something to
// someone already following the show, and what is coming up only has to be
// identifiable enough to decide whether to stay. A film's year survives on
// both cards — it disambiguates a remake, which is the same job for a title
// nobody is mid-way through.
fn format_card(t, s, e, y, max_chars, with_episode) {
    if t == "" {
        return "";
    }
    if s >= 0 && e >= 0 {
        if with_episode {
            clip(t, max_chars) + "  S" + pad2(s) + "E" + pad2(e)
        } else {
            clip(t, max_chars)
        }
    } else if y >= 0 {
        clip(t, max_chars) + " (" + y + ")"
    } else {
        clip(t, max_chars)
    }
}

// Where one element of a card sits at `lt` seconds into that card's own
// timeline, having been delayed by `delay`. Returns its opacity and its
// horizontal offset from the resting position.
//
// Negative dx is left of rest, which is off-frame for a bottom_left layer:
// a card is parked off-frame both before it arrives and after it leaves.
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

// Dot access on an absent key reads as unit rather than throwing (unlike
// index access on a missing map key) — same fallback idiom every other
// config-reading script in this project uses.
let interval = if config.interval_secs == () { 300.0 } else { config.interval_secs };
let hold = if config.hold_secs == () { 4.5 } else { config.hold_secs };
let rise = if config.rise_secs == () { 0.45 } else { config.rise_secs };
let fall = if config.fall_secs == () { 0.35 } else { config.fall_secs };
let gap = if config.gap_secs == () { 0.25 } else { config.gap_secs };
let stagger = if config.stagger_secs == () { 0.08 } else { config.stagger_secs };
let distance = if config.slide_distance_px == () { 140.0 } else { config.slide_distance_px };
let overshoot = if config.overshoot == () { 0.9 } else { config.overshoot };
let indent = if config.value_indent_px == () { 76.0 } else { config.value_indent_px };
let sub_row = if config.sub_row_px == () { 26.0 } else { config.sub_row_px };
let max_chars = if config.max_chars == () { 40 } else { config.max_chars };

let now_text = format_card(title, season, episode, year, max_chars, true);
let next_text = format_card(next_title, next_season, next_episode, next_year, max_chars, false);

// One card occupies rise + hold + fall; the value's stagger extends that,
// since the card is not finished until its last element has left.
let card_len = rise + hold + fall + stagger;
let next_start = card_len + gap;

let t = time % interval;

// Which card is on screen, and how far into its own timeline we are. -1.0 is
// "neither" — the quiet stretch that makes up most of an on-air cycle.
let label = "";
let value = "";
let sub = "";
let lt = -1.0;
if t < card_len {
    label = "NOW";
    value = now_text;
    // The episode title, on the NOW card only. A film leaves `sub_title`
    // empty, so this costs nothing on a film channel — no kind check needed.
    sub = clip(sub_title, max_chars);
    lt = t;
} else if next_text != "" && t >= next_start && t < next_start + card_len {
    label = "NEXT";
    value = next_text;
    lt = t - next_start;
}

let showing = lt >= 0.0 && value != "";

let m_label = #{ alpha: 0.0, dx: -distance };
let m_value = #{ alpha: 0.0, dx: -distance };
let m_sub = #{ alpha: 0.0, dx: -distance };
if showing {
    m_label = element_at(lt, 0.0, rise, hold, fall, distance, overshoot);
    m_value = element_at(lt, stagger, rise, hold, fall, distance, overshoot);
    // One more beat down the cascade, so the card assembles top row first.
    m_sub = element_at(lt, stagger * 2.0, rise, hold, fall, distance, overshoot);
}

// The scrim fades with the cards rather than sitting on the frame for good: a
// permanent band across the bottom third is a cost paid every second of every
// programme to make a graphic readable for nine seconds every five minutes.
// It leads the cascade (no stagger delay) so the plate is already there when
// the first glyphs land on it.
let layers = [
    #{ visible: showing && m_label.alpha > 0.004, opacity: m_label.alpha },
];

// The main row sits `sub_row` above the spec's margin on BOTH cards, whether
// or not a second row follows it, so switching cards never moves the line the
// eye is already resting on.
layers.push(#{
    visible: showing && m_label.alpha > 0.004,
    opacity: m_label.alpha,
    content: label,
    offset_x: m_label.dx,
    offset_y: -sub_row,
});
layers.push(#{
    visible: showing && m_value.alpha > 0.004,
    opacity: m_value.alpha,
    content: value,
    offset_x: indent + m_value.dx,
    offset_y: -sub_row,
});
layers.push(#{
    visible: showing && sub != "" && m_sub.alpha > 0.004,
    opacity: m_sub.alpha,
    content: sub,
    offset_x: indent + m_sub.dx,
    offset_y: 0.0,
});

#{ layers: layers }

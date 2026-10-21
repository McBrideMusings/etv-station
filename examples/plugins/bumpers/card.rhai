// The drawing half of text_card.rhai: one screen of a text card, white on the
// black segment under it. Run as a bumper segment's item overlay, every input
// arrives in `vars`; run as a spec's graphic (a preview with
// `etv-overlay render-still`), the same keys are read from `config`:
//   lines        the screen's lines, top to bottom; `*italic*` spans are read
//   reveal       seconds into the screen at which each line appears
//   credit       optional string, set under the widest line at `credit_at`
//   credit_at    seconds into the screen at which the credit appears
//   show_until   seconds into the screen at which it cuts to black
//   longest      characters in the card's longest line, so every screen of a
//                card is the same size and none runs off the frame
//   size         text size multiplier (default 1)
//   font_family  the one family every line and the credit use (default
//                sans-serif, the OS sans-serif)
//
// Every measure scales with the frame (`frame_width`, `frame_height`): text is
// 5% of the frame height, and the credit hangs 30px at 640px wide past the
// widest line's right edge. The credit's cap top sits 1.5x the gap between one
// line's baseline and the next line's cap top below the last line's baseline.
let v = if vars.lines != () { vars } else { config };
let lines = v.lines ?? [];
let reveal = v.reveal ?? [];
let credit = v.credit ?? "";
let family = v.font_family ?? "sans-serif";

let fw = if frame_width > 0.0 { frame_width } else { 1920.0 };
let fh = if frame_height > 0.0 { frame_height } else { 1080.0 };

// Condensed bold runs about half a text-height per character; shrink the card
// when its longest line would pass 88% of the frame width.
let base = fh * 0.05 * (v.size ?? 1.0);
let longest = v.longest ?? 0;
let fit = 1.0;
if longest > 0 && longest * 0.5 * base > fw * 0.88 { fit = fw * 0.88 / (longest * 0.5 * base); }
let px = base * fit;

// A still preview has no clock: show the whole screen.
let known = item_elapsed >= 0.0;
let shown = lines.len();
if known {
    shown = 0;
    for at in reveal { if at <= item_elapsed { shown += 1; } }
}
let on = !known || item_elapsed < (v.show_until ?? 1.0e9);
let credit_up = credit != "" && (!known || item_elapsed >= (v.credit_at ?? 0.0));

let text = "";
for (line, i) in lines {
    if i > 0 { text += "\n"; }
    text += line;
}

#{ visible: on, layers: [
    #{ type: "text", content: text, corner: "center", align: "center", markup: true, bold: true,
       font_family: family, font_size: px, visible_lines: shown },
    #{ type: "text", content: credit, below: 0, align: "right", bold: true, italic: true,
       font_family: family, font_size: px * 0.6,
       offset_x: 30.0 * fw / 640.0, offset_y: px * 0.225, visible: credit_up },
] }

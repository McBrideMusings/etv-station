// A text card: centered lines on the frame, revealed one after another, with a
// right-aligned credit under the widest line. Run as an item overlay, every
// input arrives in `vars`; run as a spec's graphic (a preview with
// `etv-overlay render-still`), the same keys are read from `config`:
//   lines      array of strings; `*italic*` and `**bold**` spans are read
//   credit     optional string, set under the block once every line is up
//   line_gap   seconds between lines (default: the first 60% of the item,
//              split evenly)
//   font_size  pixels (default 40)
//   font_family  default sans-serif, the OS sans-serif; a family from the station's `fonts:` list may be named instead
let v = if vars.lines != () { vars } else { config };
let lines = v.lines ?? [];
let n = lines.len();
let size = v.font_size ?? 40.0;
let family = v.font_family ?? "sans-serif";
let gap = v.line_gap ?? (if item_duration > 0.0 && n > 0 { item_duration * 0.6 / n } else { 1.5 });

let text = "";
for (line, i) in lines {
    if i > 0 { text += "\n"; }
    text += line;
}

let shown = if item_elapsed < 0.0 { n } else { (item_elapsed / gap).to_int() + 1 };
if shown > n { shown = n; }

// Hold the last lines on screen, then drop out over the final half second.
let fade = if item_remaining >= 0.0 && item_remaining < 0.5 { item_remaining / 0.5 } else { 1.0 };

#{ opacity: fade, layers: [
    #{ type: "text", content: text, corner: "center", align: "center", markup: true,
       font_family: family, font_size: size, visible_lines: shown },
    #{ type: "text", content: v.credit ?? "", below: 0, align: "right",
       font_family: family, font_size: size, visible: shown >= n },
] }

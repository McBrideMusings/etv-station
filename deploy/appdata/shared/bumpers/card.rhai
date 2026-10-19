// Bumper card: an eyebrow word over a programme title, centred on the frame
// ("Next" / "Rush Hour"). Run as a bumper segment's item overlay, so the
// station passes `.overlay(...)`'s vars; run as a spec graphic (a preview with
// `etv-overlay render-still`), the same keys are read from `config`:
//   title  the programme named (default: nothing is drawn)
//   word   the eyebrow above it, drawn upper-case ("Next", "Up next")
//
// Used by examples/plugins/trailer-bumper.rhai and text-card-bumper.rhai.
// Every key may be unset (docs/schema.md "An item's own overlay"): no title
// draws nothing, no word draws the title alone.
let v = if vars.title != () || vars.word != () { vars } else { config };
let title = v.title ?? "";
let word = (v.word ?? "").to_upper();

// Fade in over the first half second, out over the last. An unknown clock
// (a still preview) shows the card fully.
let fade_in = if item_elapsed >= 0.0 && item_elapsed < 0.5 { item_elapsed / 0.5 } else { 1.0 };
let fade_out = if item_remaining >= 0.0 && item_remaining < 0.5 { item_remaining / 0.5 } else { 1.0 };
let opacity = if fade_in < fade_out { fade_in } else { fade_out };

let shown = title != "";

// A `below` layer is not drawn when the layer it hangs from is not drawn, so
// with no word the title takes layer 0 and the second layer stays hidden.
let bold = "**" + title + "**";
#{ opacity: opacity, layers: [
    if word == "" {
        #{ type: "text", content: bold, corner: "center", align: "center", markup: true,
           font_family: "Inter", font_size: 56.0, visible: shown }
    } else {
        #{ type: "text", content: word, corner: "center", align: "center",
           font_family: "Inter", font_size: 28.0, visible: shown }
    },
    #{ type: "text", content: bold, below: 0, align: "center", markup: true,
       font_family: "Inter", font_size: 56.0, visible: shown && word != "" },
] }

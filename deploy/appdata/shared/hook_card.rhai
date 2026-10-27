// Hook card: the artist and song of the hook that has just started, bottom-left
// for one bar, then hidden. The year is not drawn.
//
// The megamix plugin hands it the copy as `vars.cards` (an array of
// `#{ at_s, artist, title }`, `at_s` seconds from the start of the chapter) and
// the length of one bar as `vars.bar_s`, so it runs as a bumper segment's item
// overlay. Run in a channel's overlay cascade it reads the same two keys from
// `metadata.vars` instead, which is where the station files them on the item.
// Every key may be unset: no cards draws nothing.
//
// A title past 28 characters breaks at its last space inside that width; the
// rest goes to a second line, cut with an ellipsis past the same width. The
// artist line is anchored from the bottom with room for two title lines, so it
// does not move when the title wraps.

// Type and spacing for the lower third.
let margin = 64;
let artist_size = 34.0;
let title_size = 30.0;
let title_lines = 2;
let line_height = 1.25;
let wrap_at = 28;
let face = "Inter";
let artist_color = [255, 255, 255, 230];  // white at 0.9
let title_color = [255, 255, 255, 217];   // white at 0.85

let copy = if type_of(vars.cards) == "array" {
    vars
} else if type_of(metadata) == "map" && type_of(metadata.vars) == "map" {
    metadata.vars
} else {
    #{}
};
let cards = if type_of(copy.cards) == "array" { copy.cards } else { [] };
let bar_s = if copy.bar_s == () { 2.0 } else { copy.bar_s };

let shown = ();
if item_elapsed >= 0.0 {
    for card in cards {
        if item_elapsed >= card.at_s && item_elapsed < card.at_s + bar_s { shown = card; }
    }
}

// The title on one or two lines.
fn wrapped(title, width) {
    if title.len() <= width { return title; }
    let cut = width;
    while cut > 0 && title.sub_string(cut, 1) != " " { cut -= 1; }
    if cut == 0 { cut = width; }
    // trim() works in place, so each half is trimmed as a variable.
    let first = title.sub_string(0, cut);
    first.trim();
    let rest = title.sub_string(cut);
    rest.trim();
    // A second line past the width is cut, so the block never grows a third line.
    if rest.len() > width { rest = rest.sub_string(0, width - 1) + "…"; }
    first + "\n" + rest
}

let visible = shown != ();
let artist = if visible { (shown.artist ?? "").to_string().to_upper() } else { "" };
let title = if visible { wrapped((shown.title ?? "").to_string(), wrap_at) } else { "" };

#{ layers: [
    #{ type: "text", content: artist, font_family: face, font_size: artist_size, bold: true,
       color: artist_color, corner: "bottom_left", margin: margin,
       offset_y: -(title_size * line_height * title_lines), visible: visible },
    #{ type: "text", content: title, font_family: face, font_size: title_size,
       color: title_color, below: 0, visible: visible && title != "" },
] }

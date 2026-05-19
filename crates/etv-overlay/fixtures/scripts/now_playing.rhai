// Lower-third "Now playing: {title}" shown for the first 10 seconds of
// each playout item. Designed to live on layer 1 of a layered overlay
// (layer 0 typically being a watermark or logo).
//
// Reads from per-frame Rhai scope:
//   title          — current item's program.title (empty if unknown)
//   item_elapsed   — seconds since current item.start (-1.0 if unknown)
#{
    layers: [
        #{},  // leave layer 0 (watermark/logo) at its TOML defaults
        #{
            visible: item_elapsed >= 0.0 && item_elapsed < 10.0,
            content: "Now playing: " + title,
        },
    ],
}

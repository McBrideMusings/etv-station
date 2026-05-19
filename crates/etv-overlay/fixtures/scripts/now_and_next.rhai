// Combined script for a three-layer demo channel:
//   layer 0 — watermark (e.g. channel logo), always on
//   layer 1 — "Now playing: {title}", first 10s of each item
//   layer 2 — "Up next: {next_title}", last 10s of each item
//
// When playout metadata isn't yet wired (item_elapsed < 0), both text
// layers stay hidden so the overlay degrades to just the watermark.
#{
    layers: [
        #{},
        #{
            visible: item_elapsed >= 0.0 && item_elapsed < 10.0,
            content: "Now playing: " + title,
        },
        #{
            visible: item_remaining >= 0.0 && item_remaining < 10.0,
            content: "Up next: " + next_title,
        },
    ],
}

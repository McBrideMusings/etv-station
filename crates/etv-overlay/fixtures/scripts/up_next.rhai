// "Up next: {next_title}" shown during the last 10 seconds of each item.
// Pairs with now_playing.rhai but on a separate text layer.
//
// Reads:
//   next_title     — next item's program.title (empty if at schedule end)
//   item_remaining — seconds until current item.finish (-1.0 if unknown)
#{
    layers: [
        #{},
        #{
            visible: item_remaining >= 0.0 && item_remaining < 10.0,
            content: "Up next: " + next_title,
        },
    ],
}

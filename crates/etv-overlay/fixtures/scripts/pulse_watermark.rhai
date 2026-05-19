// Blink the watermark on for 5 seconds out of every 60. The global
// `visible` flag short-circuits all layers, so this works regardless of
// how many layers the channel TOML declares.
//
// Reads:
//   time — process-elapsed seconds since the overlay started
#{
    visible: (time % 60.0) < 5.0,
}

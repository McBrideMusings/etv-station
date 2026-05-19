// Rotate the watermark through the four corners once per minute.
//
// Reads:
//   time — process-elapsed seconds
let minute = (time / 60.0).to_int();
let corners = ["top_left", "top_right", "bottom_right", "bottom_left"];
let idx = (minute % 4).abs();
#{
    layers: [
        #{ corner: corners[idx] },
    ],
}

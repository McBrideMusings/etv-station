// Test fixture: draws `title` for `window_secs` seconds out of every
// `period_secs`, offset by `phase_secs` — modeling a short on-screen "snipe"
// (a now/next bug) that recurs throughout a long item rather than sitting on
// screen continuously. Exists to prove dump-text's interval-based sampling
// catches text a fixed low sample count can land on zero times.
//
// Config (all required, via the layer's spec `config:`):
//   period_secs — seconds between one appearance and the next
//   window_secs — seconds the text stays visible each cycle
//   phase_secs  — offset from `time == 0` to the first appearance
let period = config.period_secs;
let window = config.window_secs;
let phase = config.phase_secs;

// `+ period` keeps the operand non-negative before `%` — `time` starts at
// 0.0 and `phase` can be larger than that.
let t = (time - phase + period) % period;
let visible = title != "" && t < window;

#{
    layers: [
        #{
            visible: visible,
            content: title,
        },
    ],
}

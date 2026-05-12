// Linear fade-in over 10 seconds, hold at full opacity afterward.
let duration = 10.0;
let opacity = if time >= duration {
    1.0
} else if time <= 0.0 {
    0.0
} else {
    time / duration
};
#{ "visible": true, "opacity": opacity }

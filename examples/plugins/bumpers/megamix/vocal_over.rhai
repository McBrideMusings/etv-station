// The vocal-over arrangement (`blend: vocal_over`): the first song plays its
// vocal and instrumental together; every later song opens with
// `vocal_over_bars` bars of its vocal over the previous song's instrumental,
// which carries on from where its own bars ended and fades out over one bar,
// then plays its own vocal and instrumental together.
//
// plan(songs, cfg) returns the chapter's audio layers (docs/schema.md § Render
// plan). `songs` are laid end to end in order, each #{ entry_id, landing_s,
// native_bpm, ratio, pitch, bars }; `cfg` is the fill entry's config plus
// `bar_s` (seconds per chapter bar) and `region_bars` (bars past its landing a
// song may be read).
//
// config:
//   vocal_over_bars: bars a song's vocal sings over the previous song, default 2

// A fade in at the start and a fade out at the end of a `len_s`-second layer,
// each at most half the layer.
fn edge_fades(len_s, fade_in, fade_out) {
    let fi = if fade_in < len_s / 2.0 { fade_in } else { len_s / 2.0 };
    let fo = if fade_out < len_s / 2.0 { fade_out } else { len_s / 2.0 };
    [
        #{ kind: "in", start_s: 0.0, len_s: fi },
        #{ kind: "out", start_s: len_s - fo, len_s: fo },
    ]
}

// `bars` bars of song `h`'s `stem`, read from `from_bar` bars past its landing
// and placed at chapter bar `at_bar`, stretched to the chapter tempo and
// shifted to its key.
fn song_layer(name, h, stem, from_bar, bars, at_bar, bar_s, level_db, fade_in, fade_out) {
    let native_bar = 240.0 / h.native_bpm;
    let len_s = bars * bar_s;
    #{
        name: name,
        source: h.entry_id,
        stem: stem,
        src_start_s: h.landing_s + from_bar * native_bar,
        out_start_s: at_bar * bar_s,
        out_len_s: len_s,
        ratio: h.ratio,
        pitch: h.pitch,
        level_db: level_db,
        fades: edge_fades(len_s, fade_in, fade_out),
    }
}

fn plan(songs, cfg) {
    let over_cfg = cfg.vocal_over_bars ?? 2;
    let bar_s = cfg.bar_s;
    for i in 0..songs.len() - 1 {
        let bars = songs[i].bars;
        if over_cfg < 0 || bars + over_cfg > cfg.region_bars {
            throw "megamix/vocal_over: config.vocal_over_bars must be between 0 and " + (cfg.region_bars - bars) + " for " + bars + "-bar hooks";
        }
    }
    let vocal_db = -20.0;
    let seam_vocal_db = -19.0;
    let instrumental_db = -17.0;
    let layers = [];
    let at = 0;
    for i in 0..songs.len() {
        let h = songs[i];
        let label = "hook " + (i + 1) + " (" + h.entry_id + ") ";
        let over = if i == 0 { 0 } else { over_cfg };
        let full = h.bars - over;
        if over > 0 {
            let prev = songs[i - 1];
            layers.push(song_layer("hook " + i + " (" + prev.entry_id + ") instrumental under the next vocal",
                prev, "instrumental", prev.bars, over, at, bar_s,
                instrumental_db, 0.02, bar_s));
            layers.push(song_layer(label + "vocals over the previous song",
                h, "vocals", 0, over, at, bar_s, seam_vocal_db, 0.5, 0.02));
        }
        let full_at = at + over;
        let instrumental_in = if i == 0 { 1.0 } else { 0.3 };
        layers.push(song_layer(label + "vocals",
            h, "vocals", over, full, full_at, bar_s, vocal_db, 0.02, 0.02));
        layers.push(song_layer(label + "instrumental",
            h, "instrumental", over, full, full_at, bar_s, instrumental_db, instrumental_in, 0.02));
        at += h.bars;
    }
    layers
}

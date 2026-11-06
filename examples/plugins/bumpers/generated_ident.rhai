// A generated station ident: a clip a render broker (ComfyUI behind local-genai)
// makes from parameters this script derives, aired once it exists. The channel's
// logo and title are drawn over it by an overlay script, never by the model.
//
// config (the block's `fill:` entry):
//   dir:         folder the broker delivers into, as the station sees it (required)
//   subdir:      folder inside dir for this script's renders, default "idents"
//   broker:      program that submits a job (required to render at all)
//   broker_args: arguments placed before the job, default []
//   profile:     broker profile, default "etv-ident"
//   prompts:     one positive prompt per variant, cycled (required)
//   negative:    negative prompt, default ""
//   models:      one model per variant, cycled; unset leaves the profile's model
//   cfg:         guidance, default 1.0. At 1.0 the negative prompt has no effect,
//                so keep text-inviting words out of `prompts`.
//   variants:    how many distinct idents a channel has per epoch, default 4
//   epoch:       the channel's seed epoch; raise it to re-roll every ident
//   stand_in:    clip that airs until a render lands; unset airs black
//   abandon_min: minutes before a .pending marker is given up on, default 60
//   seconds:     length of the ident slot, default 8
//   audio:       "mute" (default), "bed", or "own"; "own" airs the render's audio
//                only when the file has an audio stream
//   overlay:     overlay script drawn over the ident; it gets vars { lines }
//
// The broker is run as `broker broker_args... <render path> <job json>` with
// job = #{ profile, params: #{ prompt, negative, cfg, seed, model? }, deliver }.
// It returns at once; the render lands at <render path> later. A render is
// recognised by the `lg_params` tag in the file, which must equal the params this
// script derived, so a file from a different prompt or seed is never reused.
fn hooks() { ["bumper"] }
fn capabilities() { ["probe", "exec"] }

fn conf(cfg, key, fallback) {
    let v = cfg[key];
    if v == () { fallback } else { v }
}

fn fnv(text) {
    let h = 2166136261;
    for ch in text.chars() {
        h = ((h ^ ch.to_int()) * 16777619) & 0xFFFFFFFF;
    }
    h
}

fn derive(ctx, cfg) {
    let prompts = conf(cfg, "prompts", []);
    if prompts.len() == 0 { throw "generated_ident: config.prompts needs at least one prompt"; }
    let variants = conf(cfg, "variants", 4);
    if type_of(variants) != "i64" || variants < 1 { throw "generated_ident: config.variants must be a whole number above 0"; }
    let variant = ctx.rng.int(0, variants - 1);
    let epoch = conf(cfg, "epoch", 0);
    let params = #{
        prompt: prompts[variant % prompts.len()],
        negative: conf(cfg, "negative", ""),
        cfg: conf(cfg, "cfg", 1.0) * 1.0,
        seed: fnv(ctx.channel.name + "|" + epoch + "|" + variant),
    };
    let models = conf(cfg, "models", []);
    if models.len() > 0 { params.model = models[variant % models.len()]; }
    params
}

// True when the file's `lg_params` tag holds exactly the params we derived.
fn matches(probed, want) {
    let raw = probed.tags.lg_params;
    if raw == () { return false; }
    let got = ();
    try { got = parse_json(raw); } catch (e) { return false; }
    if type_of(got) != "map" { return false; }
    if got.prompt != want.prompt || got.negative != want.negative || got.seed != want.seed { return false; }
    if (got.cfg ?? 0.0) * 1.0 != want.cfg { return false; }
    got.model == want.model
}

// "none", "fresh" or "stale": whether a .pending marker exists and how old it is.
fn marker_state(ctx, marker, abandon_min) {
    let r = ctx.exec("sh", ["-c",
        "if [ ! -e \"$1\" ]; then echo none; elif [ -n \"$(find \"$1\" -mmin +\"$2\")\" ]; then echo stale; else echo fresh; fi",
        "sh", marker, abandon_min.to_string()]);
    let state = r.stdout;
    state.trim();
    if r.code != 0 { throw "marker check failed: " + r.stderr; }
    state
}

fn submit(ctx, cfg, params, path, rel, marker) {
    let job = #{ profile: conf(cfg, "profile", "etv-ident"), params: params, deliver: rel };
    let made = ctx.exec("mkdir", ["-p", cfg.dir + "/" + conf(cfg, "subdir", "idents")]);
    if made.code != 0 { throw "mkdir failed: " + made.stderr; }
    let w = ctx.exec("sh", ["-c", "printf submitted > \"$1\"", "sh", marker]);
    if w.code != 0 { throw "could not write " + marker + ": " + w.stderr; }
    let args = [];
    for a in conf(cfg, "broker_args", []) { args.push(a); }
    args.push(path);
    args.push(to_json(job));
    let r = ctx.exec(cfg.broker, args);
    if r.code != 0 {
        ctx.exec("rm", ["-f", marker]);
        throw "broker exited " + r.code + ": " + r.stderr;
    }
}

fn shape(cfg, seg, has_audio) {
    let mode = conf(cfg, "audio", "mute");
    if mode == "bed" { seg = seg.bed_audio(); }
    else if mode == "own" && has_audio { seg = seg.own_audio(); }
    else { seg = seg.mute(); }
    seg
}

fn finish(ctx, cfg, seg) {
    if cfg.overlay != () { seg = seg.overlay(cfg.overlay, #{ lines: [ctx.channel.name] }); }
    let t = timeline();
    t.push(seg);
    t
}

fn has_audio_stream(probed) {
    for s in probed.streams { if s.kind == "audio" { return true; } }
    false
}

fn stand_in_segment(cfg, slot_ms) {
    let seg = segment();
    seg = if cfg.stand_in == () { seg.black() } else { seg.render(cfg.stand_in) };
    seg.looped().fixed_ms(slot_ms)
}

fn build(ctx) {
    let cfg = ctx.config;
    if cfg.dir == () { throw "generated_ident: config.dir (the broker's delivery folder) is required"; }
    let slot_ms = ((conf(cfg, "seconds", 8) * 1000.0).round()).to_int();
    if slot_ms > ctx.gap_ms { slot_ms = ctx.gap_ms; }

    let params = derive(ctx, cfg);
    let stem = "ch" + ctx.channel.number + "-" + fnv(to_json(params)).to_hex();
    let rel = conf(cfg, "subdir", "idents") + "/" + stem + ".mp4";
    let path = cfg.dir + "/" + rel;
    let marker = path + ".pending";

    let probed = ctx.probe(path);
    if probed != () && matches(probed, params) {
        let ms = if probed.duration_ms == () || probed.duration_ms > slot_ms { slot_ms } else { probed.duration_ms };
        log("generated_ident: path=" + path + " found=true");
        let seg = segment().render(path).fixed_ms(ms).name("generated-ident");
        return finish(ctx, cfg, shape(cfg, seg, has_audio_stream(probed)));
    }

    let state = "none";
    let can_exec = cfg.broker != ();
    if can_exec {
        try {
            state = marker_state(ctx, marker, conf(cfg, "abandon_min", 60));
            if state == "none" || state == "stale" {
                submit(ctx, cfg, params, path, rel, marker);
                log("generated_ident: path=" + path + " submitted=true abandoned=" + (state == "stale"));
            } else {
                log("generated_ident: path=" + path + " pending=fresh");
            }
        } catch (e) {
            log("generated_ident: path=" + path + " stand_in=" + e);
            can_exec = false;
        }
    }

    let seg = stand_in_segment(cfg, slot_ms);
    if can_exec && cfg.stand_in != () {
        seg = segment().pending(path, cfg.stand_in).looped().fixed_ms(slot_ms);
    }
    finish(ctx, cfg, shape(cfg, seg.name("generated-ident"), false))
}

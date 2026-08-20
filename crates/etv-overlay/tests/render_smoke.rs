use std::io::Write;

use etv_overlay::overlay_spec::{Corner, OverlayKind, OverlaySpec, PixelFormat};
use etv_overlay::program_context::{ProgramContext, ProgramContextSource};
use etv_overlay::rhai_engine::{OverlayState, RhaiEngine};
use etv_overlay::vello_renderer::VelloRenderer;
use time::macros::datetime;

#[test]
fn renders_empty_frame_all_transparent() {
    let mut renderer = match VelloRenderer::new(64, 64, PixelFormat::Rgba8) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping: no GPU available ({e})");
            return;
        }
    };
    let state = OverlayState::from_layers(vec![OverlayKind::Empty]);
    let frame = renderer.render_frame(&state).expect("render");
    assert_eq!(frame.len(), 64 * 64 * 4);
    // All pixels should be transparent (alpha=0) for an empty scene
    let nonzero_alpha = frame.chunks(4).filter(|px| px[3] != 0).count();
    assert_eq!(nonzero_alpha, 0, "empty scene should have no opaque pixels");
}

#[test]
fn renders_watermark_with_visible_box() {
    let mut renderer = match VelloRenderer::new(320, 240, PixelFormat::Rgba8) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping: no GPU available ({e})");
            return;
        }
    };
    let state = OverlayState::from_layers(vec![OverlayKind::Watermark {
        corner: Corner::TopRight,
        margin: 20,
        box_size: 80,
        color: [200, 30, 30, 255],
    }]);
    let frame = renderer.render_frame(&state).expect("render");
    assert_eq!(frame.len(), 320 * 240 * 4);

    // Probe a pixel that should lie inside the watermark box.
    // Top-right corner, margin=20, box_size=80 → box spans x in [220, 300], y in [20, 100].
    let x = 260usize;
    let y = 60usize;
    let idx = (y * 320 + x) * 4;
    let alpha = frame[idx + 3];
    assert!(
        alpha > 100,
        "expected opaque watermark pixel, got alpha={alpha}"
    );

    // Probe a pixel far from the watermark — should be transparent.
    let bg_idx = (200 * 320 + 50) * 4;
    assert_eq!(
        frame[bg_idx + 3],
        0,
        "expected transparent background pixel"
    );
}

/// A logo pointing at a file that cannot be decoded as a PNG must not fail
/// `render_frame` — the frame still comes out, and every other layer
/// (a sibling watermark here) still draws. Losing the logo is cosmetic;
/// losing the whole frame would starve the channel's HLS fifo of a writer
/// and black out the channel (#302).
#[test]
fn an_undecodable_logo_drops_only_its_layer() {
    let mut renderer = match VelloRenderer::new(320, 240, PixelFormat::Rgba8) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping: no GPU available ({e})");
            return;
        }
    };

    let dir = tempfile::tempdir().unwrap();
    let bad_logo = dir.path().join("not_a_png.png");
    std::fs::write(&bad_logo, b"this is not a png file").unwrap();

    let state = OverlayState::from_layers(vec![
        OverlayKind::Logo {
            path: bad_logo,
            corner: Corner::TopLeft,
            margin: 10,
            height: 40,
        },
        OverlayKind::Watermark {
            corner: Corner::TopRight,
            margin: 20,
            box_size: 80,
            color: [200, 30, 30, 255],
        },
    ]);

    let frame = renderer
        .render_frame(&state)
        .expect("an undecodable logo must not fail the whole render");
    assert_eq!(frame.len(), 320 * 240 * 4);

    // The sibling watermark still drew: top-right box spans x in [220, 300],
    // y in [20, 100] — the bad logo cost only its own layer.
    let x = 260usize;
    let y = 60usize;
    let idx = (y * 320 + x) * 4;
    assert!(
        frame[idx + 3] > 100,
        "watermark should still render even though the logo failed to decode, got alpha={}",
        frame[idx + 3]
    );
}

/// A config value reaches the rendered pixels, not merely the script's scope
/// (#125). One script, one layer set, two configs naming different corners —
/// the watermark lands in a different place on the frame each time.
///
/// Probing the frame rather than asserting on `OverlayState` is the point: a
/// value that arrived in scope but never made it through the renderer would
/// satisfy a state assertion and still ship a broken overlay.
#[test]
fn a_config_value_moves_the_rendered_watermark() {
    let mut renderer = match VelloRenderer::new(320, 240, PixelFormat::Rgba8) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping: no GPU available ({e})");
            return;
        }
    };

    let mut script = tempfile::NamedTempFile::new().unwrap();
    write!(script, r#"#{{ layers: [ #{{ corner: config.corner }} ] }}"#).unwrap();

    let base = vec![OverlayKind::Watermark {
        corner: Corner::TopRight,
        margin: 20,
        box_size: 80,
        color: [200, 30, 30, 255],
    }];

    let mut render_with = |corner: &str| {
        // Through the real load path, so what reaches the script is what an
        // authored overlay spec would put there.
        let spec = OverlaySpec::from_yaml_str(&format!(
            "width: 320\nheight: 240\nframerate: 30\nconfig:\n  corner: {corner}\n"
        ))
        .unwrap();
        let mut engine = RhaiEngine::with_config(base.clone(), spec.config.as_ref());
        engine.load_script(script.path()).unwrap();
        let state = engine.evaluate(0.0, 0, &ProgramContext::unknown());
        renderer.render_frame(&state).expect("render")
    };

    let top_right = render_with("top_right");
    let bottom_left = render_with("bottom_left");

    // Top-right box spans x in [220, 300], y in [20, 100]; bottom-left spans
    // x in [20, 100], y in [140, 220]. Each probe is opaque in one frame and
    // transparent in the other, so neither could pass by accident.
    let at = |frame: &[u8], x: usize, y: usize| frame[(y * 320 + x) * 4 + 3];

    assert!(
        at(&top_right, 260, 60) > 100,
        "top_right config should paint the top-right box"
    );
    assert_eq!(
        at(&top_right, 60, 180),
        0,
        "top_right config should leave the bottom-left empty"
    );
    assert!(
        at(&bottom_left, 60, 180) > 100,
        "bottom_left config should paint the bottom-left box"
    );
    assert_eq!(
        at(&bottom_left, 260, 60),
        0,
        "bottom_left config should leave the top-right empty"
    );
    assert_ne!(
        top_right, bottom_left,
        "the two configs must not produce identical frames"
    );
}

/// End-to-end check that program-context metadata reaches the renderer: a
/// Rhai script reads `title` from scope and overrides a text layer's
/// content, and the rendered frame actually contains glyphs.
#[test]
fn renders_metadata_driven_text() {
    let mut renderer = match VelloRenderer::new(640, 360, PixelFormat::Rgba8) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping: no GPU available ({e})");
            return;
        }
    };

    // Stand up a tiny chunk-JSON folder so ProgramContextSource has something
    // to load. Item window covers our probe time at 2026-04-13T00:05Z.
    let dir = tempfile::tempdir().unwrap();
    let mut f = std::fs::File::create(dir.path().join("0_1.json")).unwrap();
    f.write_all(
        r#"{
            "version": "test",
            "items": [{
                "id": "a",
                "start": "2026-04-13T00:00:00Z",
                "finish": "2026-04-13T00:10:00Z",
                "program": { "title": "TestProgram" }
            }]
        }"#
        .as_bytes(),
    )
    .unwrap();

    let mut source = ProgramContextSource::new(dir.path().to_path_buf());
    source.refresh().unwrap();
    let ctx = source.current_at(datetime!(2026-04-13 00:05 UTC));
    assert_eq!(ctx.title, "TestProgram");
    assert!(ctx.item_elapsed > 0.0);

    let script_path = dir.path().join("script.rhai");
    std::fs::write(
        &script_path,
        r#"#{ layers: [ #{ visible: true, content: "Now playing: " + title } ] }"#,
    )
    .unwrap();

    let placeholder = OverlayKind::Text {
        content: "placeholder".to_string(),
        font_family: "system-ui".to_string(),
        font_size: 48.0,
        letter_spacing: 0.0,
        color: [255, 255, 255, 255],
        corner: Corner::TopLeft,
        margin: 16,
    };
    let mut engine = RhaiEngine::new(vec![placeholder]);
    engine.load_script(&script_path).unwrap();

    let state = engine.evaluate(0.0, 0, &ctx);
    match &state.layers[0].kind {
        OverlayKind::Text { content, .. } => {
            assert_eq!(content, "Now playing: TestProgram");
        }
        _ => panic!("expected text layer"),
    }

    let frame = renderer.render_frame(&state).expect("render");
    assert_eq!(frame.len(), 640 * 360 * 4);
    let nonzero_alpha = frame.chunks(4).filter(|px| px[3] > 0).count();
    assert!(
        nonzero_alpha > 200,
        "expected glyphs to produce visible pixels, got {nonzero_alpha} non-transparent",
    );
}

/// Spot-check the showcase demo script at four points in its 60s cycle:
/// mid-typewriter, mid-hold, mid-blank, mid-up-next. Verifies the script
/// shipped with the showcase channel does what the docs claim.
#[test]
fn showcase_script_phases() {
    let script_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples/overlays/scripts/now_next_cycle.rhai");
    if !script_path.exists() {
        eprintln!("skipping: showcase script not at {}", script_path.display());
        return;
    }

    let now = OverlayKind::Text {
        content: "placeholder".to_string(),
        font_family: "system-ui".to_string(),
        font_size: 40.0,
        letter_spacing: 0.0,
        color: [255, 255, 255, 255],
        corner: Corner::BottomLeft,
        margin: 48,
    };
    let next = OverlayKind::Text {
        content: "placeholder".to_string(),
        font_family: "system-ui".to_string(),
        font_size: 36.0,
        letter_spacing: 0.0,
        color: [220, 220, 220, 220],
        corner: Corner::BottomLeft,
        margin: 48,
    };
    let mut engine = RhaiEngine::new(vec![now, next]);
    engine.load_script(&script_path).unwrap();

    let mut ctx = ProgramContext::unknown();
    ctx.title = "Color Bars".into();
    ctx.next_title = "SMPTE Bars".into();
    ctx.item_elapsed = 5.0;
    ctx.item_remaining = 85.0;

    // Mid-typewriter (t=1.5s into 3s window): partial title visible.
    let s = engine.evaluate(1.5, 45, &ctx);
    assert!(s.layers[0].visible);
    assert!(!s.layers[1].visible);
    match &s.layers[0].kind {
        OverlayKind::Text { content, .. } => {
            assert!(content.starts_with("Now playing: "));
            assert!(content.len() < "Now playing: Color Bars".len());
        }
        _ => panic!("layer 0 should be text"),
    }

    // Mid-hold (t=6s): full title, full opacity.
    let s = engine.evaluate(6.0, 180, &ctx);
    match &s.layers[0].kind {
        OverlayKind::Text { content, .. } => {
            assert_eq!(content, "Now playing: Color Bars");
        }
        _ => panic!(),
    }
    assert!((s.layers[0].opacity - 1.0).abs() < 1e-3);

    // Mid-blank (t=20s): both layers off.
    let s = engine.evaluate(20.0, 600, &ctx);
    assert!(!s.layers[0].visible);
    assert!(!s.layers[1].visible);

    // Mid up-next hold (t=36s = 6s into B phase): full next_title.
    let s = engine.evaluate(36.0, 1080, &ctx);
    assert!(!s.layers[0].visible);
    assert!(s.layers[1].visible);
    match &s.layers[1].kind {
        OverlayKind::Text { content, .. } => {
            assert_eq!(content, "Up next: SMPTE Bars");
        }
        _ => panic!(),
    }

    // Fade-out tail (t=43.5s = 13.5s into B): opacity should be ~0.5.
    let s = engine.evaluate(43.5, 1305, &ctx);
    assert!(s.layers[1].visible);
    assert!(
        (s.layers[1].opacity - 0.5).abs() < 0.05,
        "expected opacity ~0.5, got {}",
        s.layers[1].opacity,
    );
}

/// Verify that a script setting `visible: false` on a layer suppresses it
/// in the rendered frame. Pairs with renders_watermark_with_visible_box,
/// which checks the opposite (a visible watermark produces opaque pixels).
#[test]
fn per_layer_invisible_layer_does_not_render() {
    let mut renderer = match VelloRenderer::new(320, 240, PixelFormat::Rgba8) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping: no GPU available ({e})");
            return;
        }
    };

    let dir = tempfile::tempdir().unwrap();
    let script_path = dir.path().join("hide.rhai");
    std::fs::write(&script_path, r#"#{ layers: [ #{ visible: false } ] }"#).unwrap();

    let watermark = OverlayKind::Watermark {
        corner: Corner::TopRight,
        margin: 20,
        box_size: 80,
        color: [200, 30, 30, 255],
    };
    let mut engine = RhaiEngine::new(vec![watermark]);
    engine.load_script(&script_path).unwrap();

    let state = engine.evaluate(0.0, 0, &ProgramContext::unknown());
    let frame = renderer.render_frame(&state).expect("render");
    let nonzero_alpha = frame.chunks(4).filter(|px| px[3] != 0).count();
    assert_eq!(
        nonzero_alpha, 0,
        "layer hidden by script should produce no opaque pixels",
    );
}
/// `title_chyron.rhai` walks through slide-in, hold, slide-out and idle on a
/// repeating interval, animating `offset_x` rather than toggling visibility.
/// Exercised against the real fixture file (not a copy) so a script edit that
/// breaks the cycle math fails here rather than only being caught by eye in
/// `admin overlay-watch`.
#[test]
fn title_chyron_slides_holds_then_slides_back_on_a_repeating_interval() {
    let script_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/scripts/title_chyron.rhai");
    let text = OverlayKind::Text {
        content: "placeholder".to_string(),
        font_family: "system-ui".to_string(),
        font_size: 34.0,
        letter_spacing: 0.0,
        color: [255, 255, 255, 235],
        corner: Corner::BottomRight,
        margin: 160,
    };
    let mut engine = RhaiEngine::new(vec![OverlayKind::Empty, text]);
    engine.load_script(&script_path).unwrap();

    let mut ctx = ProgramContext::unknown();
    ctx.title = "Die Hard".into();

    // Defaults: interval 300s, slide 0.6s, hold 10s -> slide-out ends at 11.2s.
    let at = |t: f64| engine.evaluate(t, 0, &ctx).layers[1].clone();

    let no_title = engine.evaluate(0.0, 0, &ProgramContext::unknown()).layers[1].clone();
    assert!(
        !no_title.visible,
        "an unknown title must not draw a chyron for nothing",
    );

    let start = at(0.0);
    assert!(start.visible, "slide-in should already be visible at t=0");
    assert!(
        (start.offset_x - 360.0).abs() < 1e-4,
        "at t=0 the chyron should sit fully at slide_distance_px, got {}",
        start.offset_x,
    );

    let mid_slide_in = at(0.3);
    assert!(
        mid_slide_in.offset_x > 0.0 && mid_slide_in.offset_x < 360.0,
        "mid slide-in should be partway between distance and 0, got {}",
        mid_slide_in.offset_x,
    );

    let held = at(5.0);
    assert!(held.visible);
    assert_eq!(
        held.offset_x, 0.0,
        "during the hold window the chyron should sit at rest",
    );
    match &held.kind {
        OverlayKind::Text { content, .. } => assert_eq!(content, "Die Hard"),
        _ => panic!("expected text layer"),
    }

    let mid_slide_out = at(10.9);
    assert!(
        mid_slide_out.offset_x > 0.0 && mid_slide_out.offset_x < 360.0,
        "mid slide-out should be partway back toward distance, got {}",
        mid_slide_out.offset_x,
    );

    let idle = at(60.0);
    assert!(
        !idle.visible,
        "outside the slide/hold window the chyron should be hidden until the next interval",
    );

    // The interval repeats: t = 300.0 (one full interval later) is the same
    // phase as t = 0.0.
    let next_cycle_start = at(300.0);
    assert!(next_cycle_start.visible);
    assert!((next_cycle_start.offset_x - 360.0).abs() < 1e-4);
}

/// The demo fixture (`fixtures/title_chyron.yaml`) sets its cycle timing via
/// `config:` as real YAML integers, not the float literals the script's own
/// fallback defaults use — this is what actually exercises the int/float
/// mixing the bare-default test above doesn't touch.
#[test]
fn title_chyron_fixture_config_overrides_the_script_defaults() {
    let path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/title_chyron.yaml");
    let spec = OverlaySpec::from_path(&path).unwrap();
    let mut engine = RhaiEngine::with_config(spec.layers.clone(), spec.config.as_ref());
    engine.load_script(spec.script.as_ref().unwrap()).unwrap();

    let mut ctx = ProgramContext::unknown();
    ctx.title = "Die Hard".into();

    // Fixture config: interval 12s, hold 4s, slide 0.5s, distance 300px.
    let held = engine.evaluate(2.0, 0, &ctx).layers[1].clone();
    assert!(held.visible);
    assert_eq!(held.offset_x, 0.0);

    let idle = engine.evaluate(8.0, 0, &ctx).layers[1].clone();
    assert!(
        !idle.visible,
        "8s is well past this fixture's slide_out_end (5s), so it must be idle",
    );
}

/// The Now/Next snipe as it actually ships (`deploy/appdata/shared/`), driving
/// the four station-level layers ADR 0008 put there.
///
/// Exercises the deployed file rather than a copy: this script is the graphic
/// on all 64 channels, and a copy under `fixtures/` would be free to drift from
/// what is on air without anything noticing.
fn snipe_engine() -> (RhaiEngine, std::path::PathBuf) {
    let script_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../deploy/appdata/shared/now-next-snipe.rhai");
    let text = |size: f32| OverlayKind::Text {
        content: String::new(),
        font_family: "Helvetica".to_string(),
        font_size: size,
        letter_spacing: 0.0,
        color: [255, 255, 255, 240],
        corner: Corner::BottomLeft,
        margin: 40,
    };
    let engine = RhaiEngine::new(vec![
        OverlayKind::Scrim {
            edge: etv_overlay::overlay_spec::Edge::Bottom,
            size: 150,
            color: [0, 0, 0, 175],
        },
        text(14.0),
        text(23.0),
        text(16.0),
    ]);
    (engine, script_path)
}

fn episode_context() -> ProgramContext {
    let mut ctx = ProgramContext::unknown();
    ctx.title = "Bob's Burgers".into();
    ctx.sub_title = "Manic Pixie Crap Show".into();
    ctx.season = 12;
    ctx.episode = 1;
    ctx.year = 2021;
    ctx.next_title = "Brooklyn Nine-Nine".into();
    ctx.next_season = 4;
    ctx.next_episode = 22;
    ctx
}

fn content_of(layer: &etv_overlay::rhai_engine::LayerState) -> String {
    match &layer.kind {
        OverlayKind::Text { content, .. } => content.clone(),
        other => panic!("expected a text layer, got {other:?}"),
    }
}

/// The two cards must never share the screen — that was the whole point of
/// making them sequential rather than a stacked pair.
#[test]
fn the_snipe_shows_now_and_next_one_at_a_time() {
    let (mut engine, path) = snipe_engine();
    engine.load_script(&path).unwrap();
    let ctx = episode_context();

    // Card geometry: rise .45 + hold 4.5 + fall .35 + stagger .08 = 5.38,
    // then a .25 gap, so NEXT starts at 5.63 and ends at 11.01.
    let now = engine.evaluate(2.5, 0, &ctx);
    assert_eq!(content_of(&now.layers[1]), "NOW");
    assert_eq!(content_of(&now.layers[2]), "Bob's Burgers  S12E01");

    let next = engine.evaluate(8.0, 0, &ctx);
    assert_eq!(content_of(&next.layers[1]), "NEXT");
    // No SxxExx on the NEXT card: what is coming only has to be identifiable.
    assert_eq!(content_of(&next.layers[2]), "Brooklyn Nine-Nine");

    // And the frame is genuinely clear between cycles.
    let quiet = engine.evaluate(11.5, 0, &ctx);
    assert!(
        quiet.layers.iter().all(|l| !l.visible),
        "every layer, scrim included, should be gone in the gap",
    );
}

/// The episode title is a NOW-card row only. It is also what proves the sub row
/// is driven independently rather than mirroring the value row.
#[test]
fn only_the_now_card_carries_the_episode_title() {
    let (mut engine, path) = snipe_engine();
    engine.load_script(&path).unwrap();
    let ctx = episode_context();

    let now = engine.evaluate(2.5, 0, &ctx);
    assert!(now.layers[3].visible);
    assert_eq!(content_of(&now.layers[3]), "Manic Pixie Crap Show");

    let next = engine.evaluate(8.0, 0, &ctx);
    assert!(
        !next.layers[3].visible,
        "the NEXT card has no second row to draw",
    );
}

/// One script, both item kinds — the discriminator being `season >= 0`, which
/// is the only thing distinguishing a film from an episode in the playout JSON.
#[test]
fn a_film_formats_with_its_year_instead_of_a_season_and_episode() {
    let (mut engine, path) = snipe_engine();
    engine.load_script(&path).unwrap();

    let mut ctx = ProgramContext::unknown();
    ctx.title = "Everything Everywhere All at Once".into();
    ctx.year = 2022;

    let now = engine.evaluate(2.5, 0, &ctx);
    assert_eq!(
        content_of(&now.layers[2]),
        "Everything Everywhere All at Once (2022)",
    );
    assert!(
        !now.layers[3].visible,
        "a film has no episode title, so the second row stays down",
    );
}

/// The scrim exists to make the text readable, so it has to be on screen
/// whenever the text is — and gone when the text is, rather than sitting on
/// the frame permanently.
#[test]
fn the_scrim_fades_with_the_card_it_protects() {
    let (mut engine, path) = snipe_engine();
    engine.load_script(&path).unwrap();
    let ctx = episode_context();

    let held = engine.evaluate(2.5, 0, &ctx);
    assert!(held.layers[0].visible);
    assert!(
        (held.layers[0].opacity - 1.0).abs() < 1e-3,
        "fully up while the card is held, got {}",
        held.layers[0].opacity,
    );

    let arriving = engine.evaluate(0.2, 0, &ctx);
    assert!(
        arriving.layers[0].opacity > 0.0 && arriving.layers[0].opacity < 1.0,
        "mid-entry the scrim should be part-way in, got {}",
        arriving.layers[0].opacity,
    );

    assert!(!engine.evaluate(11.5, 0, &ctx).layers[0].visible);
}

/// With no schedule loaded there is nothing to advertise, and a channel must
/// degrade to just its bug rather than an empty plate with empty labels.
#[test]
fn an_unknown_program_draws_no_snipe_at_all() {
    let (mut engine, path) = snipe_engine();
    engine.load_script(&path).unwrap();

    let state = engine.evaluate(2.5, 0, &ProgramContext::unknown());
    assert!(state.layers.iter().all(|l| !l.visible));
}

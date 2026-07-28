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
        let spec = OverlaySpec::from_toml_str(&format!(
            "width = 320\nheight = 240\nframerate = 30\n\n[config]\ncorner = \"{corner}\"\n"
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
        color: [255, 255, 255, 255],
        corner: Corner::BottomLeft,
        margin: 48,
    };
    let next = OverlayKind::Text {
        content: "placeholder".to_string(),
        font_family: "system-ui".to_string(),
        font_size: 36.0,
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

use etv_overlay::overlay_spec::{Corner, OverlayKind, PixelFormat};
use etv_overlay::rhai_engine::OverlayState;
use etv_overlay::vello_renderer::VelloRenderer;

#[test]
fn renders_empty_frame_all_transparent() {
    let mut renderer = match VelloRenderer::new(64, 64, PixelFormat::Rgba8) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping: no GPU available ({e})");
            return;
        }
    };
    let state = OverlayState::from_kind(OverlayKind::Empty);
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
    let state = OverlayState::from_kind(OverlayKind::Watermark {
        corner: Corner::TopRight,
        margin: 20,
        box_size: 80,
        color: [200, 30, 30, 255],
    });
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

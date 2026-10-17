//! The text-card overlay script (`fixtures/scripts/text_card.rhai`) drawn by the
//! real renderer: centered multi-line text, an italic line, a per-line reveal
//! timed from `item_elapsed`, and a right-aligned credit hung under the widest
//! line.

use std::path::PathBuf;

use etv_overlay::overlay_spec::{OverlayKind, PixelFormat};
use etv_overlay::program_context::{ItemOverlay, ProgramContext};
use etv_overlay::rhai_engine::ItemOverlayEngine;
use etv_overlay::vello_renderer::VelloRenderer;

const W: usize = 640;
const H: usize = 360;

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/scripts/text_card.rhai")
}

fn overlay(with_credit: bool) -> ItemOverlay {
    let mut vars = serde_json::Map::new();
    vars.insert(
        "lines".into(),
        serde_json::json!([
            "**Tweets**",
            "when the dog sees you",
            "*\"I stood in the rain so you would not have to.\"*"
        ]),
    );
    vars.insert("font_size".into(), 20.into());
    if with_credit {
        vars.insert("credit".into(), "- @someone, July 20".into());
    }
    ItemOverlay::Script {
        script: script(),
        vars,
    }
}

/// An item `duration` seconds long, `elapsed` seconds in.
fn at(elapsed: f64, duration: f64) -> ProgramContext {
    let mut ctx = ProgramContext::unknown();
    ctx.item_elapsed = elapsed;
    ctx.item_remaining = duration - elapsed;
    ctx
}

fn frame(with_credit: bool, ctx: &ProgramContext) -> Option<Vec<u8>> {
    let mut renderer = match VelloRenderer::new(W as u32, H as u32, PixelFormat::Rgba8) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping: no GPU available ({e})");
            return None;
        }
    };
    let mut items = ItemOverlayEngine::default();
    let state = items
        .engine_for(&overlay(with_credit))
        .try_evaluate(0.0, 0, ctx)
        .expect("the card script evaluates");
    Some(renderer.render_frame(&state).expect("render"))
}

/// `(left, top, right, bottom)` of the inked pixels, bottom and right exclusive.
fn ink(frame: &[u8]) -> Option<(usize, usize, usize, usize)> {
    let mut b: Option<(usize, usize, usize, usize)> = None;
    for y in 0..H {
        for x in 0..W {
            if frame[(y * W + x) * 4 + 3] > 40 {
                b = Some(match b {
                    None => (x, y, x + 1, y + 1),
                    Some((l, t, r, bt)) => (l.min(x), t.min(y), r.max(x + 1), bt.max(y + 1)),
                });
            }
        }
    }
    b
}

/// The pixels `frame` has ink on that `without` does not.
fn added(frame: &[u8], without: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; frame.len()];
    for i in (0..frame.len()).step_by(4) {
        if frame[i + 3] > 40 && without[i + 3] <= 40 {
            out[i + 3] = 255;
        }
    }
    out
}

#[test]
fn the_script_reveals_one_line_per_gap_and_the_credit_after_the_last() {
    let mut items = ItemOverlayEngine::default();
    let engine = items.engine_for(&overlay(true));
    // 30s item, three lines: the default gap is 30 * 0.6 / 3 = 6s.
    let shown = |elapsed: f64| -> (Option<u32>, bool) {
        let state = engine.try_evaluate(0.0, 0, &at(elapsed, 30.0)).unwrap();
        let OverlayKind::Text { visible_lines, .. } = &state.layers[0].kind else {
            panic!("layer 0 is text");
        };
        (*visible_lines, state.layers[1].visible)
    };
    assert_eq!(shown(0.0), (Some(1), false));
    assert_eq!(shown(5.9), (Some(1), false));
    assert_eq!(shown(6.0), (Some(2), false));
    assert_eq!(shown(12.0), (Some(3), true));
    assert_eq!(shown(29.0), (Some(3), true));
}

#[test]
fn an_item_overlay_reads_the_items_total_duration() {
    let dir = tempfile::tempdir().unwrap();
    let script_path = dir.path().join("duration.rhai");
    std::fs::write(
        &script_path,
        r#"#{ layers: [ #{ type: "text", content: item_duration.to_string() } ] }"#,
    )
    .unwrap();
    let mut items = ItemOverlayEngine::default();
    let engine = items.engine_for(&ItemOverlay::Script {
        script: script_path.clone(),
        vars: serde_json::Map::new(),
    });
    let content =
        |ctx: &ProgramContext| match &engine.try_evaluate(0.0, 0, ctx).unwrap().layers[0].kind {
            OverlayKind::Text { content, .. } => content.clone(),
            other => panic!("{other:?}"),
        };
    assert_eq!(content(&at(10.0, 45.0)), "45.0");
    assert_eq!(content(&ProgramContext::unknown()), "-1.0");
}

#[test]
fn the_block_is_centered_and_revealing_a_line_does_not_move_the_others() {
    let Some(first) = frame(false, &at(0.0, 30.0)) else {
        return;
    };
    let all = frame(false, &at(20.0, 30.0)).unwrap();

    let (l, t, r, b) = ink(&all).expect("the full card inks");
    // The block is centered as a whole on the frame (ink sits a few pixels
    // inside the line box, hence the tolerance).
    let (cx, cy) = ((l + r) as f64 / 2.0, (t + b) as f64 / 2.0);
    assert!(
        (cx - W as f64 / 2.0).abs() < 8.0,
        "center x {cx} in {l}..{r}"
    );
    assert!(
        (cy - H as f64 / 2.0).abs() < 12.0,
        "center y {cy} in {t}..{b}"
    );

    // Revealed first line: the same top edge as the full block, far less ink
    // below it.
    let (_, t1, _, b1) = ink(&first).expect("the first line inks");
    assert_eq!(t1, t, "the first line stays where the whole block put it");
    assert!(b1 + 15 < b, "only one of three lines is up: {b1} vs {b}");
    // Pixels of the first frame are all still on in the full one.
    assert_eq!(
        added(&first, &all).iter().filter(|a| **a != 0).count(),
        0,
        "no pixel of line one moved when lines two and three appeared"
    );
}

#[test]
fn the_credit_hangs_under_the_widest_line_flush_right() {
    let Some(without) = frame(false, &at(20.0, 30.0)) else {
        return;
    };
    let with = frame(true, &at(20.0, 30.0)).unwrap();

    let (_, _, block_right, block_bottom) = ink(&without).unwrap();
    let credit = ink(&added(&with, &without)).expect("the credit inks");
    assert!(
        credit.1 >= block_bottom - 4,
        "credit top {} is under the block bottom {block_bottom}",
        credit.1
    );
    // The widest line is the quote; the credit's right edge lines up with it.
    assert!(
        (credit.2 as i64 - block_right as i64).abs() <= 8,
        "credit right {} vs block right {block_right}",
        credit.2
    );
    // And the credit is narrower than the block, so flush right is visible.
    assert!(credit.0 > block_right / 2, "credit left {}", credit.0);
}

#[test]
fn the_credit_waits_for_the_last_line() {
    let Some(early) = frame(true, &at(6.5, 30.0)) else {
        return;
    };
    let without = frame(false, &at(6.5, 30.0)).unwrap();
    // Two renders of one scene differ in faint antialiasing noise, so compare
    // inked pixels, not bytes.
    assert_eq!(
        ink(&added(&early, &without)),
        None,
        "two lines up, no credit yet"
    );
    assert_eq!(ink(&added(&without, &early)), None);
}

//! `examples/overlays/hook_card.rhai`: the artist and title of a megamix hook
//! show for one bar from the hook's start, verbatim, and a long title wraps
//! without moving the artist line.

use etv_overlay::overlay_spec::OverlayKind;
use etv_overlay::program_context::{ItemOverlay, ProgramContext};
use etv_overlay::rhai_engine::ItemOverlayEngine;
use serde_json::json;

/// What layers 0 and 1 draw at `item_elapsed` seconds, as (visible, text, offset_y).
fn drawn(title: &str, item_elapsed: f64) -> Vec<(bool, String, f32)> {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/overlays/hook_card.rhai");
    let vars = json!({
        "bar_s": 2.0,
        "cards": [
            { "at_s": 0.0, "artist": "Ariana Grande", "title": "Into You" },
            { "at_s": 10.0, "artist": "Ariana Grande", "title": title },
        ],
    });
    let overlay = ItemOverlay::Script {
        script,
        vars: vars.as_object().unwrap().clone(),
    };
    let mut ctx = ProgramContext::unknown();
    ctx.item_elapsed = item_elapsed;
    let mut items = ItemOverlayEngine::default();
    let state = items
        .engine_for(&overlay)
        .try_evaluate(0.0, 0, &ctx)
        .expect("hook_card.rhai evaluates");
    state
        .layers
        .iter()
        .map(|l| match &l.kind {
            OverlayKind::Text { content, .. } => (l.visible, content.clone(), l.offset_y),
            other => panic!("expected text, got {other:?}"),
        })
        .collect()
}

#[test]
fn the_card_shows_for_one_bar_from_each_hook_start() {
    let text = |t| drawn("Thank U, Next", t);
    assert!(!text(-1.0)[0].0, "unknown clock draws nothing");
    assert_eq!(text(0.0)[0], (true, "ARIANA GRANDE".into(), -75.0));
    assert_eq!(text(0.0)[1].1, "Into You");
    assert!(text(1.99)[0].0, "still up in the last frame of the bar");
    assert!(!text(2.0)[0].0, "hidden at the bar's end");
    assert!(!text(9.9)[0].0);
    assert_eq!(text(10.0)[1].1, "Thank U, Next", "the next hook's own copy");
}

#[test]
fn a_long_title_wraps_at_28_and_leaves_the_artist_where_it_was() {
    let long = "Thank U, Next (Live from the Sweetener Sessions)";
    let short = drawn("Into You", 10.5);
    let wrapped = drawn(long, 10.5);
    assert_eq!(
        wrapped[1].1,
        "Thank U, Next (Live from the\nSweetener Sessions)"
    );
    assert_eq!(wrapped[0], short[0], "artist text and offset are unchanged");
}

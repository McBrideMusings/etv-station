//! A playout entry's `overlay` field, read from chunk files on disk the way the
//! overlay process reads them, selects what draws over that item in place of the
//! channel's overlay.

use std::path::Path;

use etv_overlay::overlay_spec::{Corner, OverlayKind};
use etv_overlay::program_context::{ItemOverlay, ProgramContext, ProgramContextSource};
use etv_overlay::rhai_engine::ItemOverlayEngine;
use time::macros::datetime;

/// Four back-to-back minutes: a bumper naming a script and vars, an archival
/// clip with `"none"`, a bumper whose script names no vars, and an ordinary
/// item with no `overlay` at all.
fn write_fixture(folder: &Path, script: &Path, no_vars_script: &Path) {
    let chunk = format!(
        r#"{{
        "version": "test",
        "items": [
            {{ "id": "card", "start": "2026-10-17T00:00:00Z", "finish": "2026-10-17T00:01:00Z",
               "program": {{ "title": "Up Next" }},
               "overlay": {{ "script": {script:?}, "vars": {{ "title": "Up Next", "count": 3 }} }} }},
            {{ "id": "clip", "start": "2026-10-17T00:01:00Z", "finish": "2026-10-17T00:02:00Z",
               "overlay": "none" }},
            {{ "id": "bare", "start": "2026-10-17T00:02:00Z", "finish": "2026-10-17T00:03:00Z",
               "overlay": {{ "script": {no_vars_script:?} }} }},
            {{ "id": "movie", "start": "2026-10-17T00:03:00Z", "finish": "2026-10-17T00:04:00Z",
               "program": {{ "title": "Movie" }} }}
        ]
    }}"#
    );
    std::fs::write(folder.join("1_2.json"), chunk).unwrap();
}

struct Fixture {
    _dir: tempfile::TempDir,
    source: ProgramContextSource,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("card.rhai");
    std::fs::write(
        &script,
        r#"
        if vars.title != "Up Next" || vars.count != 3 {
            throw "vars were " + vars.to_json();
        }
        #{ layers: [
            #{ type: "text", content: vars.title, corner: "bottom_left", font_size: 40.0, opacity: 0.5 },
            #{ type: "image", path: "logo.png", corner: "top_left" },
        ] }
        "#,
    )
    .unwrap();
    let no_vars_script = dir.path().join("bare.rhai");
    std::fs::write(
        &no_vars_script,
        r#"
        if type_of(vars) != "map" || vars.title != () {
            throw "vars should be an empty map, got " + vars.to_json();
        }
        #{}
        "#,
    )
    .unwrap();
    write_fixture(dir.path(), &script, &no_vars_script);
    let mut source = ProgramContextSource::new(dir.path().to_path_buf());
    source.refresh().unwrap();
    Fixture { _dir: dir, source }
}

#[test]
fn an_item_with_a_script_runs_it_with_its_vars_bound_as_a_map() {
    let f = fixture();
    let ctx = f.source.current_at(datetime!(2026-10-17 00:00:30 UTC));
    let overlay = ctx
        .overlay
        .clone()
        .expect("the card item carries an overlay");
    assert!(matches!(overlay, ItemOverlay::Script { .. }));

    let mut items = ItemOverlayEngine::default();
    let engine = items.engine_for(&overlay);
    let state = engine
        .try_evaluate(0.0, 0, &ctx)
        .expect("the script saw title and count bound in vars");
    assert!(state.visible, "declaring layers makes the overlay draw");
    assert_eq!(
        state.layers.len(),
        2,
        "only the script's own layers draw, none of the channel's"
    );

    let text = &state.layers[0];
    match &text.kind {
        OverlayKind::Text {
            content,
            corner,
            font_size,
            ..
        } => {
            assert_eq!(content, "Up Next", "content comes from vars");
            assert_eq!(*corner, Corner::BottomLeft);
            assert_eq!(*font_size, 40.0);
        }
        other => panic!("layer 0 should be text, got {other:?}"),
    }
    assert!(text.visible);
    assert_eq!(text.opacity, 0.5, "a per-frame key beside `type` applies");

    match &state.layers[1].kind {
        OverlayKind::Image { path, .. } => assert_eq!(
            path,
            &f._dir.path().join("logo.png"),
            "a relative image path resolves against the script's directory"
        ),
        other => panic!("layer 1 should be an image, got {other:?}"),
    }
}

#[test]
fn declared_text_from_vars_is_capped_on_a_character_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("long.rhai");
    std::fs::write(
        &script,
        r#"#{ layers: [ #{ type: "text", content: vars.title } ] }"#,
    )
    .unwrap();
    let mut vars = serde_json::Map::new();
    // 513 bytes, so the 512-byte cap lands inside the trailing two-byte 'é'.
    vars.insert("title".into(), format!("{}é", "a".repeat(511)).into());
    let overlay = ItemOverlay::Script { script, vars };

    let mut items = ItemOverlayEngine::default();
    let state = items
        .engine_for(&overlay)
        .try_evaluate(0.0, 0, &ProgramContext::unknown())
        .expect("a long title evaluates");
    match &state.layers[0].kind {
        OverlayKind::Text { content, .. } => assert_eq!(content, &"a".repeat(511)),
        other => panic!("layer 0 should be text, got {other:?}"),
    }
}

#[test]
fn a_layer_declaration_that_does_not_parse_is_a_script_error() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("bad.rhai");
    std::fs::write(&script, r#"#{ layers: [ #{ type: "sparkle" } ] }"#).unwrap();
    let overlay = ItemOverlay::Script {
        script,
        vars: serde_json::Map::new(),
    };
    let ctx = ProgramContext::unknown();

    let mut items = ItemOverlayEngine::default();
    let engine = items.engine_for(&overlay);
    let err = engine
        .try_evaluate(0.0, 0, &ctx)
        .expect_err("an unknown layer type is refused");
    assert!(err.to_string().contains("sparkle"), "error names it: {err}");
    let state = engine.evaluate(0.0, 0, &ctx);
    assert!(
        state.layers.is_empty() && !state.visible,
        "draws nothing instead"
    );
}

#[test]
fn none_draws_nothing() {
    let f = fixture();
    let ctx = f.source.current_at(datetime!(2026-10-17 00:01:30 UTC));
    assert_eq!(ctx.overlay, Some(ItemOverlay::None));

    let mut items = ItemOverlayEngine::default();
    let state = items
        .engine_for(ctx.overlay.as_ref().unwrap())
        .evaluate(0.0, 0, &ctx);
    assert!(state.layers.is_empty());
    assert!(!state.visible);
}

#[test]
fn a_script_with_no_vars_reads_an_empty_map() {
    let f = fixture();
    let ctx = f.source.current_at(datetime!(2026-10-17 00:02:30 UTC));
    let overlay = ctx
        .overlay
        .clone()
        .expect("the bare item carries an overlay");

    let mut items = ItemOverlayEngine::default();
    items
        .engine_for(&overlay)
        .try_evaluate(0.0, 0, &ctx)
        .expect("vars is an empty map when the entry names none");
}

#[test]
fn an_item_without_the_field_keeps_the_channel_overlay() {
    let f = fixture();
    let ctx = f.source.current_at(datetime!(2026-10-17 00:03:30 UTC));
    assert_eq!(ctx.overlay, None);
}

#[test]
fn a_malformed_overlay_reads_as_absent_and_leaves_its_neighbours() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("1_2.json"),
        r#"{ "items": [
            { "id": "a", "start": "2026-10-17T00:00:00Z", "finish": "2026-10-17T00:01:00Z",
              "overlay": "off" },
            { "id": "b", "start": "2026-10-17T00:01:00Z", "finish": "2026-10-17T00:02:00Z",
              "overlay": "none" }
        ] }"#,
    )
    .unwrap();
    let mut src = ProgramContextSource::new(dir.path().to_path_buf());
    src.refresh().unwrap();
    assert_eq!(
        src.current_at(datetime!(2026-10-17 00:00:30 UTC)).overlay,
        None
    );
    assert_eq!(
        src.current_at(datetime!(2026-10-17 00:01:30 UTC)).overlay,
        Some(ItemOverlay::None)
    );
}

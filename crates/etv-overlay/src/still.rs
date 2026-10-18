//! One overlay frame, rendered on demand rather than streamed: what
//! `etv-overlay render-still` writes to a PNG, and what the web UI's Review view
//! lays over a frame of the airing a tune-in sample lands on. Both go through
//! [`render_rgba`], so the still a reviewer looks at and the frame the running
//! overlay process draws come from the same engine and the same renderer.
use crate::overlay_spec::{OverlayKind, OverlaySpec};
use crate::program_context::ProgramContext;
use crate::rhai_engine::RhaiEngine;
use crate::vello_renderer::VelloRenderer;

/// How far into a script's timeline [`lookahead_moment`] looks, and how finely.
/// Ten minutes covers the snipe's default five-minute cycle twice.
const LOOKAHEAD_HORIZON_SECS: f64 = 600.0;
const LOOKAHEAD_STEP_SECS: f64 = 0.25;

/// Stands in for `next_title` while [`lookahead_moment`] scans, so a text
/// drawn from the next title is told apart from one drawn from the current
/// title even when both name the same show.
const NEXT_TITLE_PROBE: &str = "\u{E000}next";

/// Render `spec` at script time `time` against `program`, as the raw frame
/// bytes the overlay process would write to its fifo: `spec.width *
/// spec.height` pixels in `spec.pixel_format`.
pub fn render_rgba(
    spec: &OverlaySpec,
    time: f64,
    program: &ProgramContext,
) -> anyhow::Result<Vec<u8>> {
    let engine = RhaiEngine::for_spec(spec)?;
    let frame_index = (time * spec.framerate.max(1) as f64) as u64;
    let state = engine.evaluate(time, frame_index, program);
    let mut renderer = VelloRenderer::new(spec.width, spec.height, spec.pixel_format)?;
    renderer.render_frame(&state)
}

/// The first script time at which `spec` draws a fully opaque text layer
/// made from `program.next_title` — the moment a Now/Next snipe's NEXT card
/// is on screen. `None` when the program has no next title or the script
/// never draws it within ten minutes; an error when the script fails to
/// evaluate, so a broken script is not mistaken for one with no NEXT card.
///
/// The scan runs against a copy of `program` whose next title is a probe
/// string, so the card that matches is the one drawn from the lookahead and
/// not a NOW card that happens to carry the same show name.
pub fn lookahead_moment(
    spec: &OverlaySpec,
    program: &ProgramContext,
) -> anyhow::Result<Option<f64>> {
    if program.next_title.is_empty() {
        return Ok(None);
    }
    let probe = ProgramContext {
        next_title: NEXT_TITLE_PROBE.to_string(),
        ..program.clone()
    };
    let engine = RhaiEngine::for_spec(spec)?;
    let steps = (LOOKAHEAD_HORIZON_SECS / LOOKAHEAD_STEP_SECS) as u64;
    for i in 0..steps {
        let t = i as f64 * LOOKAHEAD_STEP_SECS;
        let frame_index = (t * spec.framerate.max(1) as f64) as u64;
        let state = engine
            .try_evaluate(t, frame_index, &probe)
            .map_err(|e| anyhow::anyhow!("rhai script eval failed at t={t}: {e}"))?;
        if !state.visible || state.opacity < 0.999 {
            continue;
        }
        let names_next = state.layers.iter().any(|l| {
            l.visible
                && l.opacity >= 0.999
                && matches!(&l.kind, OverlayKind::Text { content, .. } if content.contains(NEXT_TITLE_PROBE))
        });
        if names_next {
            return Ok(Some(t));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(next_title: &str) -> ProgramContext {
        ProgramContext {
            title: "Alpha".into(),
            next_title: next_title.into(),
            ..ProgramContext::unknown()
        }
    }

    fn spec_with_script(dir: &std::path::Path, script: &str) -> OverlaySpec {
        let path = dir.join("s.rhai");
        std::fs::write(&path, script).unwrap();
        let mut spec = OverlaySpec::from_yaml_str(
            "width: 64\nheight: 36\nframerate: 30\ngraphics:\n  - layers:\n      - type: text\n        content: \"\"\n",
        )
        .unwrap();
        spec.graphics[0].script = Some(path);
        spec
    }

    #[test]
    fn finds_the_first_instant_the_next_title_is_drawn() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec_with_script(
            dir.path(),
            r#"if time >= 6.0 { #{ layers: [#{ content: "NEXT " + next_title }] } } else { #{ layers: [#{ content: "NOW " + title }] } }"#,
        );
        let t = lookahead_moment(&spec, &program("Beta")).unwrap();
        assert_eq!(t, Some(6.0));
    }

    #[test]
    fn a_now_card_naming_the_same_show_is_not_the_next_card() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec_with_script(
            dir.path(),
            r#"if time >= 6.0 { #{ layers: [#{ content: "NEXT " + next_title }] } } else { #{ layers: [#{ content: "NOW " + title }] } }"#,
        );
        let ctx = ProgramContext {
            title: "Heroes".into(),
            next_title: "Heroes".into(),
            ..ProgramContext::unknown()
        };
        assert_eq!(lookahead_moment(&spec, &ctx).unwrap(), Some(6.0));
    }

    #[test]
    fn no_next_title_means_no_moment() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec_with_script(
            dir.path(),
            r#"#{ layers: [#{ content: "NEXT " + next_title }] }"#,
        );
        assert_eq!(lookahead_moment(&spec, &program("")).unwrap(), None);
    }
}

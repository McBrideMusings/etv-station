//! What an overlay's script says at one instant, as text — what
//! `etv-overlay explain` prints. It reads the [`OverlayState`] the renderer
//! would draw from, so the wording it reports is the wording on screen.
use std::fmt::Write as _;
use std::path::Path;

use crate::overlay_spec::OverlayKind;
use crate::rhai_engine::{LayerState, OverlayState};

/// The sentence a missing spec under `deploy/appdata/` fails with. That
/// directory is gitignored, so a worktree holds only its `README.md` and
/// `shared/`; a bare file-not-found would not say why.
pub const DEPLOY_APPDATA_ABSENT: &str = "this spec lives under deploy/appdata/, which is gitignored: \
     it exists in the main checkout and nowhere else, so run explain there (a worktree has only \
     README.md and shared/ under deploy/appdata/)";

/// Error for a spec path that does not exist, naming the main-checkout-only
/// constraint when the path is under `deploy/appdata/`.
pub fn missing_spec_error(path: &Path) -> anyhow::Error {
    let under_appdata = path
        .to_string_lossy()
        .replace('\\', "/")
        .contains("deploy/appdata/");
    if under_appdata {
        anyhow::anyhow!("{}: {DEPLOY_APPDATA_ABSENT}", path.display())
    } else {
        anyhow::anyhow!("overlay spec {} does not exist", path.display())
    }
}

/// Every layer of `state`, visible or not, one block per layer.
pub fn describe(state: &OverlayState) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "visible: {}", state.visible);
    let _ = writeln!(out, "opacity: {}", state.opacity);
    for (i, layer) in state.layers.iter().enumerate() {
        describe_layer(&mut out, i, layer);
    }
    out
}

fn describe_layer(out: &mut String, index: usize, layer: &LayerState) {
    let _ = writeln!(out, "layer {index}: {}", kind_name(&layer.kind));
    match &layer.kind {
        OverlayKind::Text { content, .. } => {
            let _ = writeln!(out, "  text: {content:?}");
        }
        OverlayKind::Image { path, .. } => {
            let _ = writeln!(out, "  path: {}", path.display());
        }
        _ => {}
    }
    let _ = writeln!(out, "  visible: {}", layer.visible);
    let _ = writeln!(out, "  opacity: {}", layer.opacity);
    let _ = writeln!(out, "  offset: {}, {}", layer.offset_x, layer.offset_y);
}

fn kind_name(kind: &OverlayKind) -> &'static str {
    match kind {
        OverlayKind::Empty => "empty",
        OverlayKind::Watermark { .. } => "watermark",
        OverlayKind::Image { .. } => "image",
        OverlayKind::Scrim { .. } => "scrim",
        OverlayKind::Text { .. } => "text",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay_spec::OverlaySpec;
    use crate::program_context::ProgramContext;
    use crate::rhai_engine::RhaiEngine;

    fn state_at(t: f64) -> OverlayState {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/title_chyron.yaml");
        let spec = OverlaySpec::from_path(&path).unwrap();
        let program = ProgramContext {
            title: "MOVIE TITLE".to_string(),
            ..ProgramContext::unknown()
        };
        RhaiEngine::for_spec(&spec)
            .unwrap()
            .try_evaluate(t, 0, &program)
            .unwrap()
    }

    #[test]
    fn a_visible_text_layer_reports_its_resolved_string() {
        let text = describe(&state_at(1.0));
        assert!(text.contains("text: \"MOVIE TITLE\""), "{text}");
        assert!(text.contains("  visible: true"), "{text}");
    }

    #[test]
    fn a_hidden_layer_is_still_listed() {
        let text = describe(&state_at(8.0));
        assert!(
            text.contains("layer 1: text\n  text: \"MOVIE TITLE\"\n  visible: false"),
            "{text}"
        );
    }

    #[test]
    fn a_missing_deploy_appdata_spec_names_the_main_checkout_constraint() {
        let err = missing_spec_error(Path::new("deploy/appdata/channels/001-x/overlay.toml"));
        assert!(err.to_string().contains("main checkout"), "{err}");
        let other = missing_spec_error(Path::new("nope.yaml"));
        assert!(!other.to_string().contains("main checkout"), "{other}");
    }
}

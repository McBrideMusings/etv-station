use std::path::Path;

use rhai::{AST, Dynamic, Engine, Scope};

use crate::overlay_spec::{Corner, OverlayKind};
use crate::program_context::ProgramContext;

/// Cap for per-frame text content overrides set by Rhai scripts. A runaway
/// script that builds a huge string should not flood the renderer; if a
/// script returns a longer string we truncate and log once per evaluation.
const MAX_SCRIPT_TEXT_LEN: usize = 512;

/// One layer's resolved state for the current frame. Produced by
/// [`RhaiEngine::evaluate`] by starting from the spec's base layers and
/// applying any per-layer overrides the script returned.
#[derive(Debug, Clone)]
pub struct LayerState {
    pub kind: OverlayKind,
    pub visible: bool,
    pub opacity: f32,
}

impl LayerState {
    pub fn from_kind(kind: OverlayKind) -> Self {
        let visible = !matches!(kind, OverlayKind::Empty);
        Self {
            kind,
            visible,
            opacity: 1.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OverlayState {
    /// Global visibility — when false, no layers are drawn regardless of
    /// per-layer settings. Lets a script blank the whole overlay cheaply.
    pub visible: bool,
    /// Global opacity, multiplied with each layer's per-layer opacity.
    pub opacity: f32,
    pub layers: Vec<LayerState>,
}

impl OverlayState {
    pub fn from_layers(layers: Vec<OverlayKind>) -> Self {
        let states: Vec<LayerState> = layers.into_iter().map(LayerState::from_kind).collect();
        let any_drawable = states.iter().any(|l| l.visible);
        Self {
            visible: any_drawable,
            opacity: 1.0,
            layers: states,
        }
    }
}

pub struct RhaiEngine {
    engine: Engine,
    ast: Option<AST>,
    base_layers: Vec<OverlayKind>,
}

impl RhaiEngine {
    pub fn new(base_layers: Vec<OverlayKind>) -> Self {
        let mut engine = Engine::new();
        // Bound script complexity so a runaway script can't stall the per-frame
        // render loop. 64 nesting / 50k ops is plenty for fade and blink curves
        // but stops infinite loops at evaluation time.
        engine.set_max_expr_depths(64, 64);
        engine.set_max_operations(50_000);
        Self {
            engine,
            ast: None,
            base_layers,
        }
    }

    pub fn load_script(&mut self, path: &Path) -> anyhow::Result<()> {
        let source = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read rhai script {}: {e}", path.display()))?;
        let ast = self
            .engine
            .compile(&source)
            .map_err(|e| anyhow::anyhow!("compile rhai script: {e}"))?;
        self.ast = Some(ast);
        Ok(())
    }

    /// Evaluate the script (if any) against the given frame time and program
    /// context. Returns the resolved per-frame state ready for the renderer.
    pub fn evaluate(
        &self,
        time_seconds: f64,
        frame_index: u64,
        program: &ProgramContext,
    ) -> OverlayState {
        let mut state = OverlayState::from_layers(self.base_layers.clone());
        let Some(ast) = self.ast.as_ref() else {
            return state;
        };

        let mut scope = Scope::new();
        scope.push_constant("time", time_seconds);
        scope.push_constant("frame", frame_index as i64);
        scope.push_constant("title", program.title.clone());
        scope.push_constant("sub_title", program.sub_title.clone());
        scope.push_constant("next_title", program.next_title.clone());
        scope.push_constant("next_sub_title", program.next_sub_title.clone());
        scope.push_constant("item_elapsed", program.item_elapsed);
        scope.push_constant("item_remaining", program.item_remaining);

        match self.engine.eval_ast_with_scope::<Dynamic>(&mut scope, ast) {
            Ok(value) => {
                if let Some(map) = value.try_cast::<rhai::Map>() {
                    apply_script_result(&mut state, &map);
                }
            }
            Err(e) => {
                tracing::warn!("rhai script eval failed: {e}");
            }
        }
        state
    }
}

fn apply_script_result(state: &mut OverlayState, map: &rhai::Map) {
    if let Some(visible) = map.get("visible").and_then(|v| v.as_bool().ok()) {
        state.visible = visible;
    }
    if let Some(opacity) = map.get("opacity").and_then(|v| v.as_float().ok()) {
        state.opacity = (opacity as f32).clamp(0.0, 1.0);
    }
    if let Some(layers) = map
        .get("layers")
        .and_then(|v| v.clone().try_cast::<rhai::Array>())
    {
        for (i, entry) in layers.into_iter().enumerate().take(state.layers.len()) {
            if let Some(layer_map) = entry.try_cast::<rhai::Map>() {
                apply_layer_override(&mut state.layers[i], &layer_map);
            }
        }
    }
}

fn apply_layer_override(layer: &mut LayerState, map: &rhai::Map) {
    if let Some(visible) = map.get("visible").and_then(|v| v.as_bool().ok()) {
        layer.visible = visible;
    }
    if let Some(opacity) = map.get("opacity").and_then(|v| v.as_float().ok()) {
        layer.opacity = (opacity as f32).clamp(0.0, 1.0);
    }
    if let Some(content) = map
        .get("content")
        .and_then(|v| v.clone().into_string().ok())
    {
        apply_content_override(&mut layer.kind, content);
    }
    if let Some(corner) = map
        .get("corner")
        .and_then(|v| v.clone().into_string().ok())
        .and_then(|s| parse_corner(&s))
    {
        apply_corner_override(&mut layer.kind, corner);
    }
}

fn apply_content_override(kind: &mut OverlayKind, mut content: String) {
    // content override on a non-Text layer is a script bug; ignore silently
    // per-frame so it doesn't spam logs.
    let OverlayKind::Text { content: c, .. } = kind else {
        return;
    };
    if content.len() > MAX_SCRIPT_TEXT_LEN {
        tracing::warn!(
            len = content.len(),
            cap = MAX_SCRIPT_TEXT_LEN,
            "rhai script returned oversize text content; truncating",
        );
        content.truncate(MAX_SCRIPT_TEXT_LEN);
    }
    *c = content;
}

fn apply_corner_override(kind: &mut OverlayKind, new_corner: Corner) {
    match kind {
        OverlayKind::Watermark { corner, .. }
        | OverlayKind::Logo { corner, .. }
        | OverlayKind::Text { corner, .. } => {
            *corner = new_corner;
        }
        OverlayKind::Empty => {}
    }
}

fn parse_corner(s: &str) -> Option<Corner> {
    match s {
        "top_left" => Some(Corner::TopLeft),
        "top_right" => Some(Corner::TopRight),
        "bottom_left" => Some(Corner::BottomLeft),
        "bottom_right" => Some(Corner::BottomRight),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_script(body: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(body.as_bytes()).unwrap();
        file
    }

    fn watermark() -> OverlayKind {
        OverlayKind::Watermark {
            corner: Corner::TopRight,
            margin: 32,
            box_size: 160,
            color: [220, 50, 50, 220],
        }
    }

    fn text(content: &str) -> OverlayKind {
        OverlayKind::Text {
            content: content.into(),
            font_family: "system-ui".into(),
            font_size: 32.0,
            color: [255, 255, 255, 255],
            corner: Corner::BottomLeft,
            margin: 32,
        }
    }

    #[test]
    fn empty_engine_returns_base_state() {
        let engine = RhaiEngine::new(vec![]);
        let state = engine.evaluate(0.0, 0, &ProgramContext::unknown());
        assert!(!state.visible);
        assert_eq!(state.opacity, 1.0);
    }

    #[test]
    fn fade_in_script() {
        let script = write_script(
            r#"
            let duration = 10.0;
            let opacity = if time >= duration { 1.0 } else { time / duration };
            #{ "visible": true, "opacity": opacity }
            "#,
        );
        let mut engine = RhaiEngine::new(vec![watermark()]);
        engine.load_script(script.path()).unwrap();

        let s0 = engine.evaluate(0.0, 0, &ProgramContext::unknown());
        assert!(s0.visible);
        assert!(s0.opacity.abs() < 1e-4);

        let s5 = engine.evaluate(5.0, 150, &ProgramContext::unknown());
        assert!((s5.opacity - 0.5).abs() < 1e-4);

        let s10 = engine.evaluate(10.0, 300, &ProgramContext::unknown());
        assert!((s10.opacity - 1.0).abs() < 1e-4);
    }

    #[test]
    fn per_layer_content_override_templates_title() {
        let script = write_script(
            r#"
            #{
                layers: [
                    #{ visible: item_elapsed >= 0.0 && item_elapsed < 10.0,
                       content: "Now playing: " + title },
                ],
            }
            "#,
        );
        let mut engine = RhaiEngine::new(vec![text("placeholder")]);
        engine.load_script(script.path()).unwrap();

        let mut ctx = ProgramContext::unknown();
        ctx.title = "The Office".into();
        ctx.item_elapsed = 3.0;

        let state = engine.evaluate(0.0, 0, &ctx);
        assert_eq!(state.layers.len(), 1);
        assert!(state.layers[0].visible);
        match &state.layers[0].kind {
            OverlayKind::Text { content, .. } => {
                assert_eq!(content, "Now playing: The Office");
            }
            _ => panic!("expected text layer"),
        }
    }

    #[test]
    fn per_layer_visibility_hides_layer() {
        let script = write_script(
            r#"
            #{ layers: [ #{ visible: false } ] }
            "#,
        );
        let mut engine = RhaiEngine::new(vec![watermark()]);
        engine.load_script(script.path()).unwrap();
        let state = engine.evaluate(0.0, 0, &ProgramContext::unknown());
        assert!(!state.layers[0].visible);
    }

    #[test]
    fn per_layer_corner_override() {
        let script = write_script(
            r#"
            #{ layers: [ #{ corner: "bottom_right" } ] }
            "#,
        );
        let mut engine = RhaiEngine::new(vec![watermark()]);
        engine.load_script(script.path()).unwrap();
        let state = engine.evaluate(0.0, 0, &ProgramContext::unknown());
        match &state.layers[0].kind {
            OverlayKind::Watermark { corner, .. } => {
                assert_eq!(*corner, Corner::BottomRight);
            }
            _ => panic!("expected watermark"),
        }
    }

    #[test]
    fn oversize_content_is_truncated() {
        let mut layer = LayerState::from_kind(text("orig"));
        apply_content_override(&mut layer.kind, "x".repeat(MAX_SCRIPT_TEXT_LEN + 100));
        if let OverlayKind::Text { content, .. } = &layer.kind {
            assert_eq!(content.len(), MAX_SCRIPT_TEXT_LEN);
        } else {
            panic!("expected text");
        }
    }

    #[test]
    fn extra_layer_entries_are_ignored() {
        // Script returns 3 layer entries, base has 1.
        let script = write_script(
            r#"
            #{ layers: [ #{ visible: false }, #{ visible: false }, #{ visible: false } ] }
            "#,
        );
        let mut engine = RhaiEngine::new(vec![watermark()]);
        engine.load_script(script.path()).unwrap();
        let state = engine.evaluate(0.0, 0, &ProgramContext::unknown());
        assert_eq!(state.layers.len(), 1);
        assert!(!state.layers[0].visible);
    }
}

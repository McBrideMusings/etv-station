use std::path::Path;

use rhai::{AST, Dynamic, Engine, Scope};

use crate::overlay_spec::OverlayKind;

#[derive(Debug, Clone)]
pub struct OverlayState {
    pub visible: bool,
    pub opacity: f32,
    pub kind: OverlayKind,
}

impl OverlayState {
    pub fn from_kind(kind: OverlayKind) -> Self {
        Self {
            visible: !matches!(kind, OverlayKind::Empty),
            opacity: 1.0,
            kind,
        }
    }
}

pub struct RhaiEngine {
    engine: Engine,
    ast: Option<AST>,
    base_kind: OverlayKind,
}

impl RhaiEngine {
    pub fn new(base_kind: OverlayKind) -> Self {
        let mut engine = Engine::new();
        // Bound script complexity so a runaway script can't stall the per-frame
        // render loop. 64 nesting / 50k ops is plenty for fade and blink curves
        // but stops infinite loops at evaluation time.
        engine.set_max_expr_depths(64, 64);
        engine.set_max_operations(50_000);
        Self {
            engine,
            ast: None,
            base_kind,
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

    pub fn evaluate(&self, time_seconds: f64, frame_index: u64) -> OverlayState {
        let mut state = OverlayState::from_kind(self.base_kind.clone());
        let Some(ast) = self.ast.as_ref() else {
            return state;
        };

        let mut scope = Scope::new();
        scope.push_constant("time", time_seconds);
        scope.push_constant("frame", frame_index as i64);

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

    #[test]
    fn empty_engine_returns_base_state() {
        let engine = RhaiEngine::new(OverlayKind::Empty);
        let state = engine.evaluate(0.0, 0);
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
        let mut engine = RhaiEngine::new(OverlayKind::Watermark {
            corner: crate::overlay_spec::Corner::TopRight,
            margin: 32,
            box_size: 160,
            color: [220, 50, 50, 220],
        });
        engine.load_script(script.path()).unwrap();

        let s0 = engine.evaluate(0.0, 0);
        assert!(s0.visible);
        assert!(s0.opacity.abs() < 1e-4);

        let s5 = engine.evaluate(5.0, 150);
        assert!((s5.opacity - 0.5).abs() < 1e-4);

        let s10 = engine.evaluate(10.0, 300);
        assert!((s10.opacity - 1.0).abs() < 1e-4);

        let s20 = engine.evaluate(20.0, 600);
        assert!((s20.opacity - 1.0).abs() < 1e-4);
    }
}

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
    /// Pixel offset added on top of `corner`/`margin`'s resolved position, for
    /// a script animating a layer sliding across the frame (a chyron, a pan).
    /// Zero reproduces the spec's static position exactly.
    pub offset_x: f32,
    pub offset_y: f32,
}

impl LayerState {
    pub fn from_kind(kind: OverlayKind) -> Self {
        let visible = !matches!(kind, OverlayKind::Empty);
        Self {
            kind,
            visible,
            opacity: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
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
    /// The spec's `config`, already converted. Held as a `Dynamic` rather than
    /// as the carrier value because [`RhaiEngine::evaluate`] runs once per frame:
    /// converting there would re-walk the whole table 30 times a second to
    /// produce the same result. Cloning a `Dynamic` map is a refcount.
    config: Dynamic,
}

impl RhaiEngine {
    pub fn new(base_layers: Vec<OverlayKind>) -> Self {
        Self::with_config(base_layers, None)
    }

    /// As [`RhaiEngine::new`], with the spec's opaque `config` table. Anything
    /// that fails to convert is dropped to an empty map and warned about rather
    /// than failing the render — an overlay is decoration over a channel that is
    /// already on the air, so a bad config costs the decoration, not the stream.
    pub fn with_config(base_layers: Vec<OverlayKind>, config: Option<&serde_json::Value>) -> Self {
        let mut engine = Engine::new();
        // Bound script complexity so a runaway script can't stall the per-frame
        // render loop. 64 nesting / 50k ops is plenty for fade and blink curves
        // but stops infinite loops at evaluation time.
        engine.set_max_expr_depths(64, 64);
        engine.set_max_operations(50_000);
        let config = match config {
            Some(value) => rhai::serde::to_dynamic(value).unwrap_or_else(|e| {
                tracing::warn!("overlay config is not representable in Rhai: {e}");
                Dynamic::from_map(rhai::Map::new())
            }),
            None => Dynamic::from_map(rhai::Map::new()),
        };
        Self {
            engine,
            ast: None,
            base_layers,
            config,
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
    ///
    /// A script error is warned and swallowed, falling back to the base
    /// state — an overlay is decoration over a channel already on the air, so
    /// a bad script must cost the decoration, not the stream. Use
    /// [`Self::try_evaluate`] where a caller needs to see the error instead.
    pub fn evaluate(
        &self,
        time_seconds: f64,
        frame_index: u64,
        program: &ProgramContext,
    ) -> OverlayState {
        match self.try_evaluate(time_seconds, frame_index, program) {
            Ok(state) => state,
            Err(e) => {
                tracing::warn!("rhai script eval failed: {e}");
                OverlayState::from_layers(self.base_layers.clone())
            }
        }
    }

    /// As [`Self::evaluate`], but surfaces a script evaluation error instead
    /// of warning and falling back — same engine, same scope construction, so
    /// a caller inspecting the script's output (`etv-overlay dump-text`)
    /// cannot see a result the renderer wouldn't also produce.
    pub fn try_evaluate(
        &self,
        time_seconds: f64,
        frame_index: u64,
        program: &ProgramContext,
    ) -> Result<OverlayState, Box<rhai::EvalAltResult>> {
        let mut state = OverlayState::from_layers(self.base_layers.clone());
        let Some(ast) = self.ast.as_ref() else {
            return Ok(state);
        };

        let mut scope = Scope::new();
        // The spec's own `config`, handed over unread. An absent one is an empty
        // map rather than a missing constant, so a script can read
        // `config.whatever` unconditionally and get unit for anything unset —
        // which is also why a mistyped key is silent.
        scope.push_constant("config", self.config.clone());
        scope.push_constant("time", time_seconds);
        scope.push_constant("frame", frame_index as i64);
        scope.push_constant("title", program.title.clone());
        scope.push_constant("sub_title", program.sub_title.clone());
        // The current item's guide description. Its last line names who has
        // been watching when the channel sets `scoring.attribution` (#113);
        // `watched_by` is that line on its own, for a script that wants the
        // credit without the synopsis above it.
        scope.push_constant("description", program.description.clone());
        scope.push_constant("watched_by", watched_by_line(&program.description));
        // `-1` for absent, per `ProgramContext`. `season >= 0` is how a script
        // tells an episode from a film, since the station writes season and
        // episode only for episodes.
        scope.push_constant("season", program.season);
        scope.push_constant("episode", program.episode);
        scope.push_constant("year", program.year);
        scope.push_constant("next_title", program.next_title.clone());
        scope.push_constant("next_sub_title", program.next_sub_title.clone());
        scope.push_constant("next_season", program.next_season);
        scope.push_constant("next_episode", program.next_episode);
        scope.push_constant("next_year", program.next_year);
        scope.push_constant("item_elapsed", program.item_elapsed);
        scope.push_constant("item_remaining", program.item_remaining);

        let value = self
            .engine
            .eval_ast_with_scope::<Dynamic>(&mut scope, ast)?;
        if let Some(map) = value.try_cast::<rhai::Map>() {
            apply_script_result(&mut state, &map);
        }
        Ok(state)
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

/// The attribution credit on its own, pulled back out of the description the
/// station appended it to (#113).
///
/// The station writes the synopsis, a blank line, then the credit, so the
/// credit is the last paragraph and starts with a known prefix. Matching on the
/// prefix rather than "take the last paragraph" is what keeps a film whose
/// synopsis happens to end in a short paragraph from being drawn on screen as
/// if it were a credit.
///
/// Empty when the channel does not attribute, which is the common case — a
/// script gates on `watched_by != ""`.
fn watched_by_line(description: &str) -> String {
    const PREFIX: &str = "Watched recently by ";
    description
        .lines()
        .rev()
        .find(|l| l.starts_with(PREFIX))
        .unwrap_or_default()
        .to_string()
}

fn apply_layer_override(layer: &mut LayerState, map: &rhai::Map) {
    if let Some(visible) = map.get("visible").and_then(|v| v.as_bool().ok()) {
        layer.visible = visible;
    }
    if let Some(opacity) = map.get("opacity").and_then(|v| v.as_float().ok()) {
        layer.opacity = (opacity as f32).clamp(0.0, 1.0);
    }
    if let Some(offset_x) = map.get("offset_x").and_then(|v| v.as_float().ok()) {
        layer.offset_x = offset_x as f32;
    }
    if let Some(offset_y) = map.get("offset_y").and_then(|v| v.as_float().ok()) {
        layer.offset_y = offset_y as f32;
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
        // A scrim is anchored to an edge, not a corner, and `Empty` has no
        // geometry at all — a `corner` override on either is a script bug, and
        // ignoring it silently is what the content override already does.
        OverlayKind::Scrim { .. } | OverlayKind::Empty => {}
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
            letter_spacing: 0.0,
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

    /// A config bag built the way loading a spec builds one — YAML text in,
    /// carrier value out, through the same deserializer
    /// [`OverlaySpec::config`](crate::overlay_spec::OverlaySpec::config) is read
    /// with, so these tests assert on exactly what an authored spec produces.
    fn yaml_config(src: &str) -> serde_json::Value {
        #[derive(serde::Deserialize)]
        struct Holder {
            #[serde(deserialize_with = "crate::config_carrier::deserialize_config")]
            config: Option<serde_json::Value>,
        }
        let indented: String = src.lines().map(|l| format!("  {l}\n")).collect();
        serde_norway::from_str::<Holder>(&format!("config:\n{indented}"))
            .unwrap()
            .config
            .unwrap()
    }

    /// The spec's `config` reaches the script with its nesting and scalar types
    /// intact (#125). Nothing here knows what `fade_seconds` means — the script
    /// does, and the station only carries the value.
    #[test]
    fn config_arrives_nested_with_types_intact() {
        let script = write_script(
            r#"
            if config.fade.seconds != 0.4 { throw "float did not survive"; }
            if config.fade.steps != 3 { throw "int did not survive"; }
            if config.fade.enabled != true { throw "bool did not survive"; }
            if config.fade.labels[1] != "b" { throw "nested array did not survive"; }
            if config.name != "lower-third" { throw "top-level string did not survive"; }
            #{ "visible": true, "opacity": config.fade.seconds }
            "#,
        );
        let cfg = yaml_config(
            r#"name: lower-third
fade:
  seconds: 0.4
  steps: 3
  enabled: true
  labels: [a, b]
"#,
        );
        let mut engine = RhaiEngine::with_config(vec![watermark()], Some(&cfg));
        engine.load_script(script.path()).unwrap();

        let state = engine.evaluate(0.0, 0, &ProgramContext::unknown());
        assert!(state.visible);
        assert!(
            (state.opacity - 0.4).abs() < 1e-6,
            "the config value should reach the resolved state, got {}",
            state.opacity
        );
    }

    /// Two specs, one script, different numbers — the point of the field.
    #[test]
    fn the_same_script_renders_differently_per_config() {
        let script = write_script(r#"#{ "visible": true, "opacity": config.opacity }"#);
        let render = |opacity: &str| {
            let cfg = yaml_config(&format!("opacity: {opacity}\n"));
            let mut engine = RhaiEngine::with_config(vec![watermark()], Some(&cfg));
            engine.load_script(script.path()).unwrap();
            engine.evaluate(0.0, 0, &ProgramContext::unknown()).opacity
        };
        assert!((render("0.25") - 0.25).abs() < 1e-6);
        assert!((render("0.75") - 0.75).abs() < 1e-6);
    }

    /// An absent config is an empty map, not a missing constant: a script may
    /// read straight through it without guarding, and an unrecognised key is
    /// unit rather than an evaluation failure.
    #[test]
    fn an_absent_config_is_an_empty_map_and_unknown_keys_are_unit() {
        let script = write_script(
            r#"
            if config.len() != 0 { throw "expected an empty map"; }
            if config.anything != () { throw "expected unit"; }
            #{ "visible": true, "opacity": 1.0 }
            "#,
        );
        let mut engine = RhaiEngine::new(vec![watermark()]);
        engine.load_script(script.path()).unwrap();

        let state = engine.evaluate(0.0, 0, &ProgramContext::unknown());
        assert!(
            state.visible,
            "reading an unset config must not fail evaluation"
        );
    }

    /// A mistyped key is silent by construction — it reads as unset and the
    /// script takes its own fallback, because nothing here knows the spelling.
    #[test]
    fn a_mistyped_key_falls_back_without_warning() {
        let script = write_script(
            r#"
            let fade = if config.fade_seconds == () { 1.0 } else { config.fade_seconds };
            #{ "visible": true, "opacity": fade }
            "#,
        );
        let cfg = yaml_config("fade_secconds: 0.2\n");
        let mut engine = RhaiEngine::with_config(vec![watermark()], Some(&cfg));
        engine.load_script(script.path()).unwrap();

        let state = engine.evaluate(0.0, 0, &ProgramContext::unknown());
        assert!(
            (state.opacity - 1.0).abs() < 1e-6,
            "the typo should read as unset and take the script's fallback"
        );
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

    /// The credit is pulled back out of the description by its prefix, not by
    /// "take the last paragraph" — a synopsis ending in a short paragraph must
    /// not be drawn on screen as if it named viewers.
    #[test]
    fn watched_by_finds_the_credit_and_ignores_a_plain_synopsis() {
        assert_eq!(
            watched_by_line("A hobbit sets out.\n\nWatched recently by bob and carol"),
            "Watched recently by bob and carol",
        );
        assert_eq!(
            watched_by_line("Watched recently by bob"),
            "Watched recently by bob",
        );
        assert_eq!(
            watched_by_line("A hobbit sets out.\n\nHe comes back."),
            "",
            "a description with no credit must produce no credit",
        );
        assert_eq!(watched_by_line(""), "");
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

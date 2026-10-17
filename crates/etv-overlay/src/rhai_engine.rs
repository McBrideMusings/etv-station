use std::path::{Path, PathBuf};

use rhai::{AST, Dynamic, Engine, Scope};

use crate::overlay_spec::{Corner, OverlayKind, OverlaySpec};
use crate::program_context::{ItemOverlay, ProgramContext};

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

    /// One frame from every graphic's resolved state, in declaration order.
    /// Each graphic's own global `visible`/`opacity` (what a script returns
    /// outside its `layers`) is folded into its layers first, so one graphic
    /// blanking itself or fading cannot touch another's. Nothing graphics-wide
    /// remains, so the combined `opacity` is 1.0.
    pub fn combine(states: impl IntoIterator<Item = OverlayState>) -> Self {
        let mut layers = Vec::new();
        for state in states {
            for mut layer in state.layers {
                layer.visible &= state.visible;
                layer.opacity *= state.opacity;
                layers.push(layer);
            }
        }
        Self {
            visible: layers.iter().any(|l| l.visible),
            opacity: 1.0,
            layers,
        }
    }
}

/// One graphic at run time: its compiled script, the layers it drives, and the
/// `config` its script reads.
struct Graphic {
    /// `None` for a static graphic, whose layers draw as declared.
    ast: Option<AST>,
    base_layers: Vec<OverlayKind>,
    /// The graphic's `config`, already converted. Held as a `Dynamic` rather
    /// than as the carrier value because [`RhaiEngine::evaluate`] runs once per
    /// frame: converting there would re-walk the whole table 30 times a second
    /// to produce the same result. Cloning a `Dynamic` map is a refcount.
    config: Dynamic,
    /// The `vars` map a playout item's overlay selection binds. An empty map
    /// for every graphic that is not an item's override, so a script reads
    /// `vars.whatever` unconditionally and gets unit for anything unset.
    vars: Dynamic,
    /// The directory the script was read from, which a relative `path` in a
    /// layer the script declares resolves against.
    script_dir: Option<PathBuf>,
}

impl Graphic {
    fn new(base_layers: Vec<OverlayKind>, config: Option<&serde_json::Value>) -> Self {
        let config = match config {
            Some(value) => rhai::serde::to_dynamic(value).unwrap_or_else(|e| {
                tracing::warn!("overlay config is not representable in Rhai: {e}");
                Dynamic::from_map(rhai::Map::new())
            }),
            None => Dynamic::from_map(rhai::Map::new()),
        };
        Self {
            ast: None,
            base_layers,
            config,
            vars: Dynamic::from_map(rhai::Map::new()),
            script_dir: None,
        }
    }
}

/// Every graphic of one overlay spec, evaluated against one shared Rhai engine.
/// A graphic's script sees its own layers by index and its own `config`; the
/// frame is the graphics' layers concatenated in declaration order.
pub struct RhaiEngine {
    engine: Engine,
    graphics: Vec<Graphic>,
}

impl RhaiEngine {
    /// An engine with one static graphic of `base_layers` — the shape a test or
    /// a `clear`ed block wants. Spec-driven callers use [`Self::for_spec`].
    pub fn new(base_layers: Vec<OverlayKind>) -> Self {
        Self::with_config(base_layers, None)
    }

    /// As [`RhaiEngine::new`], with the graphic's opaque `config` table.
    /// Anything that fails to convert is dropped to an empty map and warned
    /// about rather than failing the render — an overlay is decoration over a
    /// channel that is already on the air, so a bad config costs the
    /// decoration, not the stream.
    pub fn with_config(base_layers: Vec<OverlayKind>, config: Option<&serde_json::Value>) -> Self {
        let mut engine = Self::bare();
        engine.graphics.push(Graphic::new(base_layers, config));
        engine
    }

    fn bare() -> Self {
        let mut engine = Engine::new();
        // Bound script complexity so a runaway script can't stall the per-frame
        // render loop. 64 nesting / 50k ops is plenty for fade and blink curves
        // but stops infinite loops at evaluation time. The budget is per
        // script evaluation, so each graphic gets the whole of it.
        engine.set_max_expr_depths(64, 64);
        engine.set_max_operations(50_000);
        Self {
            engine,
            graphics: Vec::new(),
        }
    }

    /// The engine for one loaded spec: each graphic's layers, `config` and
    /// script compiled.
    pub fn for_spec(spec: &OverlaySpec) -> anyhow::Result<Self> {
        let mut engine = Self::bare();
        for (i, graphic) in spec.graphics.iter().enumerate() {
            engine.graphics.push(Graphic::new(
                graphic.layers.clone(),
                graphic.config.as_ref(),
            ));
            if let Some(script) = &graphic.script {
                engine
                    .load_script(script)
                    .map_err(|e| match &graphic.name {
                        Some(name) => anyhow::anyhow!("graphic {name:?}: {e}"),
                        None => anyhow::anyhow!("graphic {i}: {e}"),
                    })?;
            }
        }
        Ok(engine)
    }

    /// The engine for one playout item's own overlay selection, in place of the
    /// channel's overlay: `None` draws nothing, a script runs with its `vars`
    /// bound and no declared layers, so it draws only the layers it declares in
    /// its result (see [`apply_script_result`]). A script that fails to load
    /// returns the error; the caller decides what the item shows instead.
    pub fn for_item_overlay(overlay: &ItemOverlay) -> anyhow::Result<Self> {
        let mut engine = Self::bare();
        engine.graphics.push(Graphic::new(Vec::new(), None));
        if let ItemOverlay::Script { script, vars } = overlay {
            let vars = rhai::serde::to_dynamic(vars).unwrap_or_else(|e| {
                tracing::warn!("item overlay vars are not representable in Rhai: {e}");
                Dynamic::from_map(rhai::Map::new())
            });
            if let Some(graphic) = engine.graphics.last_mut() {
                graphic.vars = vars;
            }
            engine.load_script(script)?;
        }
        Ok(engine)
    }

    /// Compile `path` as the script of the most recently added graphic.
    pub fn load_script(&mut self, path: &Path) -> anyhow::Result<()> {
        let source = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read rhai script {}: {e}", path.display()))?;
        let ast = self
            .engine
            .compile(&source)
            .map_err(|e| anyhow::anyhow!("compile rhai script {}: {e}", path.display()))?;
        let graphic = self
            .graphics
            .last_mut()
            .ok_or_else(|| anyhow::anyhow!("no graphic to attach a script to"))?;
        graphic.ast = Some(ast);
        graphic.script_dir = path.parent().map(Path::to_path_buf);
        Ok(())
    }

    /// Evaluate every graphic against the given frame time and program
    /// context. Returns the resolved per-frame state ready for the renderer.
    ///
    /// A script error is warned and swallowed, and that graphic falls back to
    /// its base layers while the others still draw — an overlay is decoration
    /// over a channel already on the air, so a bad script must cost the
    /// decoration, not the stream. Use [`Self::try_evaluate`] where a caller
    /// needs to see the error instead.
    pub fn evaluate(
        &self,
        time_seconds: f64,
        frame_index: u64,
        program: &ProgramContext,
    ) -> OverlayState {
        let states = self.graphics.iter().map(|graphic| {
            self.evaluate_graphic(graphic, time_seconds, frame_index, program)
                .unwrap_or_else(|e| {
                    tracing::warn!("rhai script eval failed: {e}");
                    OverlayState::from_layers(graphic.base_layers.clone())
                })
        });
        OverlayState::combine(states)
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
        let states = self
            .graphics
            .iter()
            .map(|graphic| self.evaluate_graphic(graphic, time_seconds, frame_index, program))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(OverlayState::combine(states))
    }

    fn evaluate_graphic(
        &self,
        graphic: &Graphic,
        time_seconds: f64,
        frame_index: u64,
        program: &ProgramContext,
    ) -> Result<OverlayState, Box<rhai::EvalAltResult>> {
        let mut state = OverlayState::from_layers(graphic.base_layers.clone());
        let Some(ast) = graphic.ast.as_ref() else {
            return Ok(state);
        };

        let mut scope = Scope::new();
        // The spec's own `config`, handed over unread. An absent one is an empty
        // map rather than a missing constant, so a script can read
        // `config.whatever` unconditionally and get unit for anything unset —
        // which is also why a mistyped key is silent.
        scope.push_constant("config", graphic.config.clone());
        scope.push_constant("vars", graphic.vars.clone());
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
        // The item's catalog rating, verbatim ("TV-MA", "PG-13"), "" when none.
        scope.push_constant("content_rating", program.content_rating.clone());
        scope.push_constant("next_title", program.next_title.clone());
        scope.push_constant("next_sub_title", program.next_sub_title.clone());
        scope.push_constant("next_season", program.next_season);
        scope.push_constant("next_episode", program.next_episode);
        scope.push_constant("next_year", program.next_year);
        scope.push_constant("item_elapsed", program.item_elapsed);
        scope.push_constant("item_remaining", program.item_remaining);
        // The item's whole length, `item_elapsed + item_remaining`, so a script
        // can pace a reveal against the item it sits on (a card whose lines
        // land at fixed fractions of its length). `-1.0` when either half is
        // unknown, the same sentinel as the two it is made from.
        scope.push_constant("item_duration", item_duration(program));
        // The current item's opaque metadata blob (ADR 0016) — the one
        // channel through which anything plugin-specific reaches a script.
        // No Rust code here reads a key inside it. Same conversion as
        // `config` above; anything unrepresentable in Rhai drops to unit
        // rather than failing the whole evaluation.
        scope.push_constant(
            "metadata",
            rhai::serde::to_dynamic(&program.metadata).unwrap_or_else(|e| {
                tracing::warn!("item metadata is not representable in Rhai: {e}");
                Dynamic::UNIT
            }),
        );
        // The item before the current one, for a script comparing two
        // consecutive items. Same conversion and same opacity as `metadata`.
        scope.push_constant(
            "prev_metadata",
            rhai::serde::to_dynamic(&program.prev_metadata).unwrap_or_else(|e| {
                tracing::warn!("previous item metadata is not representable in Rhai: {e}");
                Dynamic::UNIT
            }),
        );

        let value = self
            .engine
            .eval_ast_with_scope::<Dynamic>(&mut scope, ast)?;
        if let Some(map) = value.try_cast::<rhai::Map>() {
            apply_script_result(&mut state, &map, graphic.script_dir.as_deref())?;
        }
        Ok(state)
    }
}

/// The engine for whichever playout item overlay is airing, rebuilt only when
/// the selection changes. An item whose script cannot load gets an engine that
/// draws nothing, kept under that selection so the failure is warned once rather
/// than once per frame.
#[derive(Default)]
pub struct ItemOverlayEngine {
    current: Option<(ItemOverlay, RhaiEngine)>,
}

impl ItemOverlayEngine {
    pub fn engine_for(&mut self, overlay: &ItemOverlay) -> &RhaiEngine {
        let stale = self
            .current
            .as_ref()
            .is_none_or(|(loaded, _)| loaded != overlay);
        if stale {
            let engine = RhaiEngine::for_item_overlay(overlay).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "item overlay failed to load; drawing nothing for it");
                RhaiEngine::for_item_overlay(&ItemOverlay::None)
                    .expect("the none overlay has no script to fail")
            });
            tracing::info!(
                ?overlay,
                "item overlay selected; channel overlay suppressed"
            );
            self.current = Some((overlay.clone(), engine));
        }
        &self.current.as_ref().expect("just set").1
    }
}

/// Folds a script's returned map into `state`. Each `layers` entry addresses
/// the layer at its index. An entry carrying `type` declares that layer outright,
/// in the same shape as a spec's `layers:` entry, replacing a declared layer or
/// appending past the last one — how a script with no declared layers (an
/// item's own overlay) draws anything. An entry without `type` overrides the
/// declared layer at its index, and one past the end has nothing to override.
///
/// A declaration that does not parse is a script error: the graphic falls back
/// to its declared layers, as for any other.
fn apply_script_result(
    state: &mut OverlayState,
    map: &rhai::Map,
    script_dir: Option<&Path>,
) -> Result<(), Box<rhai::EvalAltResult>> {
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
        let mut declared_any = false;
        for (i, entry) in layers.into_iter().enumerate() {
            let Some(layer_map) = entry.try_cast::<rhai::Map>() else {
                continue;
            };
            if layer_map.contains_key("type") {
                declared_any = true;
                let layer = LayerState::from_kind(declared_layer(&layer_map, script_dir)?);
                if i < state.layers.len() {
                    state.layers[i] = layer;
                } else {
                    // A declaration past a gap leaves the skipped indices empty,
                    // so every entry still lands at the index it was written at.
                    state
                        .layers
                        .resize_with(i, || LayerState::from_kind(OverlayKind::Empty));
                    state.layers.push(layer);
                }
            }
            // The per-frame keys beside `type` (`visible`, `opacity`,
            // `offset_x`/`offset_y`) are not part of a layer's kind, so a
            // declared layer takes them here, as overrides, like any other —
            // and its `content` passes the same length cap.
            if let Some(layer) = state.layers.get_mut(i) {
                apply_layer_override(layer, &layer_map);
            }
        }
        // A graphic with nothing drawable declared starts invisible; a layer
        // its script declared this frame makes it visible, unless the script
        // set `visible` itself.
        let set_visible = map.get("visible").is_some_and(|v| v.as_bool().is_ok());
        if declared_any && !set_visible {
            state.visible = state.layers.iter().any(|l| l.visible);
        }
    }
    Ok(())
}

/// A layer a script declared, parsed as a spec's `layers:` entry would be. A
/// relative image path resolves against the script's own directory, as a spec's
/// resolves against the spec's. The error names the `type` and the parse
/// failure, never the whole map, whose text may be a long `vars` value.
fn declared_layer(
    map: &rhai::Map,
    script_dir: Option<&Path>,
) -> Result<OverlayKind, Box<rhai::EvalAltResult>> {
    let mut kind: OverlayKind = rhai::serde::from_dynamic(&Dynamic::from_map(map.clone()))
        .map_err(|e| {
            let ty = map.get("type").map(ToString::to_string).unwrap_or_default();
            format!("layer declaration of type {ty:?}: {e}")
        })?;
    if let (OverlayKind::Image { path, .. }, Some(dir)) = (&mut kind, script_dir)
        && path.is_relative()
    {
        *path = dir.join(&*path);
    }
    Ok(kind)
}

fn item_duration(program: &ProgramContext) -> f64 {
    if program.item_elapsed < 0.0 || program.item_remaining < 0.0 {
        -1.0
    } else {
        program.item_elapsed + program.item_remaining
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
    if let Some(lines) = map.get("visible_lines").and_then(|v| v.as_int().ok())
        && let OverlayKind::Text { visible_lines, .. } = &mut layer.kind
    {
        *visible_lines = Some(lines.clamp(0, u32::MAX as i64) as u32);
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
        // Back off to a char boundary: `truncate` panics inside a multibyte
        // character, and script text can carry any `vars` value.
        let mut cut = MAX_SCRIPT_TEXT_LEN;
        while !content.is_char_boundary(cut) {
            cut -= 1;
        }
        content.truncate(cut);
    }
    *c = content;
}

fn apply_corner_override(kind: &mut OverlayKind, new_corner: Corner) {
    match kind {
        OverlayKind::Watermark { corner, .. }
        | OverlayKind::Image { corner, .. }
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
        "center" => Some(Corner::Center),
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
            align: Default::default(),
            bold: false,
            italic: false,
            markup: false,
            visible_lines: None,
            below: None,
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
            (state.layers[0].opacity - 0.4).abs() < 1e-6,
            "the config value should reach the resolved state, got {}",
            state.layers[0].opacity
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
            engine.evaluate(0.0, 0, &ProgramContext::unknown()).layers[0].opacity
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
            (state.layers[0].opacity - 1.0).abs() < 1e-6,
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
        assert!(s0.layers[0].opacity.abs() < 1e-4);

        let s5 = engine.evaluate(5.0, 150, &ProgramContext::unknown());
        assert!((s5.layers[0].opacity - 0.5).abs() < 1e-4);

        let s10 = engine.evaluate(10.0, 300, &ProgramContext::unknown());
        assert!((s10.layers[0].opacity - 1.0).abs() < 1e-4);
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

    /// A script reads a key out of the opaque `metadata` blob directly from
    /// scope (ADR 0016) — no Rust code here interprets `attribution`.
    #[test]
    fn script_reads_a_key_out_of_metadata() {
        let script = write_script(
            r#"
            #{ layers: [ #{ content: "By " + metadata.attribution } ] }
            "#,
        );
        let mut engine = RhaiEngine::new(vec![text("placeholder")]);
        engine.load_script(script.path()).unwrap();

        let mut ctx = ProgramContext::unknown();
        ctx.metadata = serde_json::json!({ "attribution": "Bob Example" });

        let state = engine.evaluate(0.0, 0, &ctx);
        match &state.layers[0].kind {
            OverlayKind::Text { content, .. } => assert_eq!(content, "By Bob Example"),
            _ => panic!("expected text layer"),
        }
    }

    /// An item with no metadata must evaluate without panicking or erroring —
    /// the script sees unit, not a missing-field error.
    #[test]
    fn missing_metadata_evaluates_without_panicking() {
        let script = write_script(
            r#"
            #{ visible: metadata == () }
            "#,
        );
        let mut engine = RhaiEngine::new(vec![watermark()]);
        engine.load_script(script.path()).unwrap();

        let state = engine
            .try_evaluate(0.0, 0, &ProgramContext::unknown())
            .unwrap();
        assert!(state.visible);
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

    /// A `scoring.explain` line sits between the synopsis and the
    /// attribution credit — the reversed line search still finds the credit
    /// as the last matching line, unbothered by the extra paragraph ahead of
    /// it.
    #[test]
    fn watched_by_finds_the_credit_past_an_explain_line() {
        assert_eq!(
            watched_by_line("A hobbit sets out.\n\nWhy: Ranked highest\n\nWatched recently by bob"),
            "Watched recently by bob",
        );
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

    /// Two graphics in one spec: each script sees its own `config`, drives its
    /// own layers by index from zero, and draws in declaration order, so adding
    /// the second cannot re-target the first.
    #[test]
    fn graphics_are_independent_and_draw_in_declaration_order() {
        let first = write_script(r#"#{ layers: [ #{ content: "first " + config.tag } ] }"#);
        let second = write_script(r#"#{ layers: [ #{ content: "second " + config.tag } ] }"#);
        let spec = |script: &tempfile::NamedTempFile, tag: &str| crate::overlay_spec::GraphicSpec {
            script: Some(script.path().to_path_buf()),
            config: Some(serde_json::json!({ "tag": tag })),
            layers: vec![text("placeholder")],
            ..Default::default()
        };
        let overlay = OverlaySpec {
            width: 64,
            height: 36,
            framerate: 30,
            pixel_format: Default::default(),
            graphics: vec![spec(&first, "a"), spec(&second, "b")],
        };
        let engine = RhaiEngine::for_spec(&overlay).unwrap();
        let state = engine.evaluate(0.0, 0, &ProgramContext::unknown());
        let texts: Vec<_> = state
            .layers
            .iter()
            .map(|l| match &l.kind {
                OverlayKind::Text { content, .. } => content.clone(),
                _ => panic!("expected text"),
            })
            .collect();
        assert_eq!(texts, ["first a", "second b"]);
    }

    /// One graphic hiding itself, or failing, leaves the others drawing: a
    /// script's global `visible: false` is folded into its own layers only.
    #[test]
    fn one_graphic_blanking_or_failing_leaves_the_other_drawing() {
        let hide = write_script(r#"#{ visible: false }"#);
        let boom = write_script(r#"throw "boom";"#);
        let graphic = |script: Option<&tempfile::NamedTempFile>| crate::overlay_spec::GraphicSpec {
            script: script.map(|s| s.path().to_path_buf()),
            layers: vec![watermark()],
            ..Default::default()
        };
        let overlay = OverlaySpec {
            width: 64,
            height: 36,
            framerate: 30,
            pixel_format: Default::default(),
            graphics: vec![graphic(Some(&hide)), graphic(Some(&boom)), graphic(None)],
        };
        let engine = RhaiEngine::for_spec(&overlay).unwrap();
        let state = engine.evaluate(0.0, 0, &ProgramContext::unknown());
        let visible: Vec<bool> = state.layers.iter().map(|l| l.visible).collect();
        // Blanked; fell back to its base layers after the error; static.
        assert_eq!(visible, [false, true, true]);
        // `try_evaluate` still reports the failing script.
        assert!(
            engine
                .try_evaluate(0.0, 0, &ProgramContext::unknown())
                .is_err()
        );
    }

    /// `prev_metadata` reaches the script as the previous item's blob, and as
    /// unit when there is none — which `type_of` can tell from a map.
    #[test]
    fn script_reads_the_previous_items_metadata() {
        let script = write_script(
            r#"
            let prev = if type_of(prev_metadata) == "map" { prev_metadata.pool_label } else { "none" };
            #{ layers: [ #{ content: metadata.pool_label + " after " + prev } ] }
            "#,
        );
        let mut engine = RhaiEngine::new(vec![text("placeholder")]);
        engine.load_script(script.path()).unwrap();
        let content = |ctx: &ProgramContext| match &engine.evaluate(0.0, 0, ctx).layers[0].kind {
            OverlayKind::Text { content, .. } => content.clone(),
            _ => panic!("expected text layer"),
        };

        let mut ctx = ProgramContext::unknown();
        ctx.metadata = serde_json::json!({ "pool_label": "Zombies" });
        assert_eq!(content(&ctx), "Zombies after none");
        ctx.prev_metadata = serde_json::json!({ "pool_label": "Kaiju" });
        assert_eq!(content(&ctx), "Zombies after Kaiju");
    }
}

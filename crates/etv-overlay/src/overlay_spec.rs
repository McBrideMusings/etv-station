use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OverlaySpec {
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    #[serde(default)]
    pub pixel_format: PixelFormat,
    pub script: Option<PathBuf>,
    /// Arbitrary configuration handed to `script` verbatim, reaching it as the
    /// `config` constant. Any YAML shape is accepted — mappings, sequences,
    /// strings, numbers, booleans, nested to any depth — and no key is
    /// reserved.
    ///
    /// **Nothing here reads it.** It is converted and passed through, and that
    /// is the whole contract: no validation, no known keys, no injected
    /// defaults, and an unrecognised key is not an error. A key means whatever
    /// the script decides it means, which is what lets two channels share one
    /// overlay script with different type sizes or corners.
    ///
    /// Unset yields an empty map rather than a missing constant, so a script may
    /// read `config.anything` unconditionally and get unit back.
    ///
    /// **A typo is silent, by construction** — a mistyped key reads as unset and
    /// the script takes its own fallback, because nothing here knows the correct
    /// spelling. A script wanting strictness declares and checks its own keys.
    ///
    /// **A non-finite float is refused at load, naming the key.** `weight = inf`
    /// (or `-inf`, or `nan`) has no carrier representation, so rather than let
    /// it reach the script as unit the spec fails to load and says which key is
    /// at fault. An author wanting "never decays" writes a large finite number.
    /// The channel-config side refuses the same value in the same words (#130).
    ///
    /// **A date arrives as the text the author wrote.** `date: 2026-07-28`
    /// reaches the script as the string `"2026-07-28"`, which is what a channel
    /// YAML's `released: 2026-07-28` already hands a scorer. A script wanting a
    /// moment rather than a label parses it, on the same terms as every other
    /// key here — the meaning is the script's.
    ///
    /// The same contract the scorer-plugin side carries on
    /// `etv_station::config::Pool::config`, down to the carrier type and the
    /// deserializer: one opaque-config value type for the whole project, read
    /// through one [`crate::config_carrier::deserialize_config`], so the two
    /// surfaces cannot drift. Only the plumbing differs, because a scorer
    /// receives one `ctx` map and an overlay receives flat scope constants.
    #[serde(
        default,
        deserialize_with = "crate::config_carrier::deserialize_config",
        skip_serializing_if = "Option::is_none"
    )]
    pub config: Option<serde_json::Value>,
    /// Layers are rendered bottom-up in declaration order. A single Rhai script
    /// (if `script` is set) controls visibility/opacity uniformly across all
    /// layers — per-layer scripts are a future extension.
    #[serde(default, alias = "kind", deserialize_with = "deserialize_layers")]
    pub layers: Vec<OverlayKind>,
}

fn deserialize_layers<'de, D>(deserializer: D) -> Result<Vec<OverlayKind>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(OverlayKind),
        Many(Vec<OverlayKind>),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(k) => vec![k],
        OneOrMany::Many(v) => v,
    })
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum PixelFormat {
    #[default]
    Rgba8,
}

impl PixelFormat {
    pub fn ffmpeg_arg(self) -> &'static str {
        match self {
            PixelFormat::Rgba8 => "rgba",
        }
    }

    pub fn bytes_per_pixel(self) -> u32 {
        match self {
            PixelFormat::Rgba8 => 4,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OverlayKind {
    #[default]
    Empty,
    Watermark {
        corner: Corner,
        #[serde(default = "default_margin")]
        margin: u32,
        #[serde(default = "default_box_size")]
        box_size: u32,
        #[serde(default = "default_color")]
        color: [u8; 4],
    },
    /// Drop a PNG into one of the four corners (e.g. a TV channel logo).
    /// Aspect ratio is preserved; `height` controls the rendered height in
    /// pixels and width is derived from the image's aspect. Grayscale,
    /// grayscale+alpha, palette, and 16-bit PNGs are normalized to 8-bit
    /// RGB/RGBA at decode time; a source that still cannot be decoded (a
    /// genuinely corrupt file, or a missing path) drops just this layer —
    /// logged once — rather than failing the whole render (#302).
    Logo {
        path: PathBuf,
        corner: Corner,
        #[serde(default = "default_margin")]
        margin: u32,
        #[serde(default = "default_logo_height")]
        height: u32,
    },
    /// A gradient band along one edge of the frame, opaque at the edge and
    /// fading to nothing `size` pixels in — the standard way broadcast
    /// graphics stay readable over content they do not control.
    ///
    /// Sized off the frame, never off the text it protects: a plate fitted to
    /// a string would have to measure shaped text, which nothing outside the
    /// renderer can do, and would resize on every title. A band that always
    /// covers the bottom 140px protects whatever is drawn there.
    ///
    /// A script animating this should drive `opacity`, not `offset_y` — the
    /// band is anchored to the edge, and sliding it reveals a hard line where
    /// the gradient stops.
    Scrim {
        #[serde(default = "default_scrim_edge")]
        edge: Edge,
        /// How far the band reaches in from its edge, in pixels.
        #[serde(default = "default_scrim_size")]
        size: u32,
        /// The colour at the edge. Its alpha is the band's strongest point;
        /// the far end is always fully transparent.
        #[serde(default = "default_scrim_color")]
        color: [u8; 4],
    },
    /// Static single-line text overlay (channel banner, "TEST PATTERN", etc).
    /// Dynamic content templating (e.g. `{title}` from program metadata) is
    /// not yet wired — `content` is taken verbatim. See follow-up issue for
    /// the station→overlay metadata bridge.
    Text {
        content: String,
        #[serde(default = "default_font_family")]
        font_family: String,
        #[serde(default = "default_font_size")]
        font_size: f32,
        /// Extra space between glyphs, in pixels, added on top of the font's
        /// own metrics. Negative tightens.
        ///
        /// Tracking is size-specific, which is why this is per-layer and not a
        /// spec-wide setting: an all-caps eyebrow label ("NOW", "NEXT") wants
        /// roughly +0.05–0.12em to stop reading as cramped, while large
        /// display text wants slightly negative. Zero — the default — is the
        /// font's own spacing and reproduces every spec written before this
        /// field existed.
        #[serde(default)]
        letter_spacing: f32,
        #[serde(default = "default_text_color")]
        color: [u8; 4],
        #[serde(default)]
        corner: Corner,
        #[serde(default = "default_margin")]
        margin: u32,
    },
}

fn default_margin() -> u32 {
    32
}

fn default_box_size() -> u32 {
    160
}

fn default_color() -> [u8; 4] {
    [220, 50, 50, 220]
}

fn default_logo_height() -> u32 {
    96
}

fn default_font_family() -> String {
    "system-ui".to_string()
}

fn default_font_size() -> f32 {
    48.0
}

fn default_text_color() -> [u8; 4] {
    [255, 255, 255, 255]
}

fn default_scrim_edge() -> Edge {
    Edge::Bottom
}

fn default_scrim_size() -> u32 {
    140
}

/// Black at 70% is the working default: dark enough to hold white text over a
/// bright scene, light enough that it does not read as a bar across the frame.
fn default_scrim_color() -> [u8; 4] {
    [0, 0, 0, 180]
}

/// Which side of the frame a [`OverlayKind::Scrim`] is anchored to. Distinct
/// from [`Corner`] — a band has one edge, not two.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Edge {
    #[default]
    Bottom,
    Top,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Corner {
    #[default]
    TopRight,
    TopLeft,
    BottomRight,
    BottomLeft,
}

/// The four fields baked into the fifo's byte layout at spawn: they size the
/// canvas, the frame buffer, and the `-video_size`/`-framerate`/`-pixel_format`
/// arguments ETV-next's ffmpeg was started with. A resolved-config swap that
/// changes any of them cannot be applied to a running process (#48), so they
/// are compared as a unit rather than field by field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub pixel_format: PixelFormat,
}

impl std::fmt::Display for Geometry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}x{}@{} {}",
            self.width,
            self.height,
            self.framerate,
            self.pixel_format.ffmpeg_arg()
        )
    }
}

impl OverlaySpec {
    pub fn from_yaml_str(s: &str) -> Result<Self, serde_norway::Error> {
        serde_norway::from_str(s)
    }

    pub fn from_path(path: &std::path::Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read overlay spec {}: {e}", path.display()))?;
        let spec =
            Self::from_yaml_str(&raw).map_err(|e| anyhow::anyhow!("parse overlay spec: {e}"))?;
        Ok(spec.with_paths_relative_to(path.parent()))
    }

    /// Re-root this spec's `script` and every `logo` path against `base`, the
    /// directory the spec was authored in. Split out from [`Self::from_path`]
    /// because a spec can also arrive inline in a channel YAML, where the same
    /// base-relative rule has to hold against a different file's directory.
    pub fn with_paths_relative_to(mut self, base: Option<&std::path::Path>) -> Self {
        if let Some(script) = self.script.take() {
            self.script = Some(resolve_relative(&script, base));
        }
        for layer in &mut self.layers {
            if let OverlayKind::Logo { path: logo, .. } = layer {
                *logo = resolve_relative(logo, base);
            }
        }
        self
    }

    pub fn geometry(&self) -> Geometry {
        Geometry {
            width: self.width,
            height: self.height,
            framerate: self.framerate,
            pixel_format: self.pixel_format,
        }
    }

    pub fn frame_byte_len(&self) -> usize {
        (self.width * self.height * self.pixel_format.bytes_per_pixel()) as usize
    }
}

fn resolve_relative(p: &std::path::Path, base: Option<&std::path::Path>) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        match base {
            Some(b) => b.join(p),
            None => p.to_path_buf(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_watermark_config() {
        let yaml = r#"
width: 1920
height: 1080
framerate: 30
pixel_format: rgba8
kind:
  type: watermark
  corner: top_right
  margin: 48
  box_size: 200
  color: [255, 100, 100, 200]
"#;
        let spec = OverlaySpec::from_yaml_str(yaml).unwrap();
        assert_eq!(spec.width, 1920);
        assert_eq!(spec.height, 1080);
        assert_eq!(spec.framerate, 30);
        assert_eq!(spec.pixel_format, PixelFormat::Rgba8);
        assert_eq!(spec.layers.len(), 1);
        match &spec.layers[0] {
            OverlayKind::Watermark {
                corner,
                margin,
                box_size,
                color,
            } => {
                assert_eq!(*corner, Corner::TopRight);
                assert_eq!(*margin, 48);
                assert_eq!(*box_size, 200);
                assert_eq!(*color, [255, 100, 100, 200]);
            }
            _ => panic!("expected watermark kind"),
        }
    }

    #[test]
    fn parses_layers_array() {
        let yaml = r#"
width: 1280
height: 720
framerate: 30
layers:
  - type: logo
    path: logo.png
    corner: bottom_right
    margin: 24
    height: 96
  - type: text
    content: PIERCE
    font_family: Helvetica
    font_size: 36.0
    color: [255, 255, 255, 230]
    corner: bottom_right
    margin: 132
"#;
        let spec = OverlaySpec::from_yaml_str(yaml).unwrap();
        assert_eq!(spec.layers.len(), 2);
        assert!(matches!(spec.layers[0], OverlayKind::Logo { .. }));
        assert!(matches!(spec.layers[1], OverlayKind::Text { .. }));
    }

    #[test]
    fn parses_text_overlay() {
        let yaml = r#"
width: 1280
height: 720
framerate: 30
kind:
  type: text
  content: ETV STATION
  font_family: Helvetica
  font_size: 64.0
  letter_spacing: 1.5
  color: [255, 255, 255, 230]
  corner: bottom_left
  margin: 40
"#;
        let spec = OverlaySpec::from_yaml_str(yaml).unwrap();
        assert_eq!(spec.layers.len(), 1);
        match &spec.layers[0] {
            OverlayKind::Text {
                content,
                font_family,
                font_size,
                letter_spacing,
                color,
                corner,
                margin,
            } => {
                assert_eq!(content, "ETV STATION");
                assert_eq!(font_family, "Helvetica");
                assert!((*font_size - 64.0).abs() < 1e-4);
                assert!((*letter_spacing - 1.5).abs() < 1e-4);
                assert_eq!(*color, [255, 255, 255, 230]);
                assert_eq!(*corner, Corner::BottomLeft);
                assert_eq!(*margin, 40);
            }
            _ => panic!("expected text kind"),
        }
    }

    #[test]
    fn text_uses_defaults_when_minimal() {
        let yaml = r#"
width: 640
height: 360
framerate: 25
kind:
  type: text
  content: hi
"#;
        let spec = OverlaySpec::from_yaml_str(yaml).unwrap();
        assert_eq!(spec.layers.len(), 1);
        match &spec.layers[0] {
            OverlayKind::Text {
                content,
                font_family,
                font_size,
                letter_spacing,
                color,
                corner,
                margin,
            } => {
                assert_eq!(content, "hi");
                assert_eq!(font_family, "system-ui");
                assert!((*font_size - 48.0).abs() < 1e-4);
                // Absent `letter_spacing` is the font's own spacing, so every
                // spec written before the field existed renders unchanged.
                assert_eq!(*letter_spacing, 0.0);
                assert_eq!(*color, [255, 255, 255, 255]);
                assert_eq!(*corner, Corner::TopRight);
                assert_eq!(*margin, 32);
            }
            _ => panic!("expected text kind"),
        }
    }

    #[test]
    fn parses_empty_default() {
        let yaml = r#"
width: 320
height: 240
framerate: 24
"#;
        let spec = OverlaySpec::from_yaml_str(yaml).unwrap();
        assert_eq!(spec.pixel_format, PixelFormat::Rgba8);
        assert!(spec.layers.is_empty());
    }

    /// A `config:` mapping parses into the carrier type with its nesting and
    /// scalar types intact — the shape a script reads (#125, #129).
    #[test]
    fn config_parses_into_the_shared_carrier_type() {
        let yaml = r#"
width: 320
height: 240
framerate: 30
config:
  name: lower-third
  fade:
    seconds: 0.4
    steps: 3
    enabled: true
    labels: [a, b]
"#;
        let spec = OverlaySpec::from_yaml_str(yaml).unwrap();
        let config = spec.config.as_ref().unwrap();
        assert_eq!(config["name"], serde_json::json!("lower-third"));
        assert_eq!(config["fade"]["seconds"], serde_json::json!(0.4));
        assert_eq!(config["fade"]["steps"], serde_json::json!(3));
        assert_eq!(config["fade"]["enabled"], serde_json::json!(true));
        assert_eq!(config["fade"]["labels"][1], serde_json::json!("b"));
    }

    /// A float the carrier cannot hold fails the whole spec load and names the
    /// key that holds it, wherever in the bag it sits — rather than reaching the
    /// script as unit with nothing said (#130).
    #[test]
    fn a_non_finite_float_in_config_fails_the_load_and_names_the_key() {
        let spec = |bag: &str| format!("width: 320\nheight: 240\nframerate: 30\nconfig:\n{bag}");
        for (bag, key) in [
            ("  weight: .inf\n", "config.weight"),
            ("  weight: .nan\n", "config.weight"),
            ("  weight: -.inf\n", "config.weight"),
            ("  steps: [1.0, .inf]\n", "config.steps[1]"),
            ("  fade:\n    weight: .nan\n", "config.fade.weight"),
        ] {
            let err = OverlaySpec::from_yaml_str(&spec(bag))
                .expect_err("a non-finite float must fail the load")
                .to_string();
            assert!(err.contains(key), "error did not name `{key}`: {err}");
            assert!(
                err.contains("large finite number"),
                "error did not tell the author what to write instead: {err}"
            );
        }
    }

    /// A date arrives as the text the author wrote, wherever it sits and in
    /// whichever form it is spelled — the same string a channel YAML's
    /// `released: 2026-07-28` already hands a scorer plugin (#129). Both
    /// surfaces now read one format through one deserializer, which is what
    /// makes that agreement structural rather than a coincidence of two
    /// conversions.
    #[test]
    fn a_date_arrives_as_its_authored_text() {
        let yaml = r#"
width: 320
height: 240
framerate: 30
config:
  date: 2026-07-28
  offset: 2026-07-28T10:32:00Z
  local: 2026-07-28T10:32:00
  clock: "10:32:00"
  window: [2026-07-28, 2026-07-29]
  nested:
    since: 2026-07-28
"#;
        let spec = OverlaySpec::from_yaml_str(yaml).unwrap();
        let config = spec.config.as_ref().unwrap();
        assert_eq!(config["date"], serde_json::json!("2026-07-28"));
        assert_eq!(config["offset"], serde_json::json!("2026-07-28T10:32:00Z"));
        assert_eq!(config["local"], serde_json::json!("2026-07-28T10:32:00"));
        assert_eq!(config["clock"], serde_json::json!("10:32:00"));
        assert_eq!(config["window"][1], serde_json::json!("2026-07-29"));
        assert_eq!(config["nested"]["since"], serde_json::json!("2026-07-28"));
    }

    /// An unset `config` stays unset — the empty map a script reads is made at
    /// the engine, not injected into the spec.
    #[test]
    fn an_absent_config_stays_absent() {
        let spec = OverlaySpec::from_yaml_str("width: 1\nheight: 1\nframerate: 1\n").unwrap();
        assert!(spec.config.is_none());
    }

    #[test]
    fn frame_byte_len_matches() {
        let spec = OverlaySpec {
            width: 100,
            height: 100,
            framerate: 30,
            pixel_format: PixelFormat::Rgba8,
            script: None,
            config: None,
            layers: vec![],
        };
        assert_eq!(spec.frame_byte_len(), 40_000);
    }
}

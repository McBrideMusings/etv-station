use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config_carrier::non_finite_message;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OverlaySpec {
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    #[serde(default)]
    pub pixel_format: PixelFormat,
    pub script: Option<PathBuf>,
    /// Arbitrary configuration handed to `script` verbatim, reaching it as the
    /// `config` constant. Any TOML shape is accepted — tables, arrays, strings,
    /// numbers, booleans, nested to any depth — and no key is reserved.
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
    /// **A TOML datetime arrives as the text the author wrote.** TOML has a
    /// first-class datetime and the carrier type does not, so `date =
    /// 2026-07-28` reaches the script as the string `"2026-07-28"` — offset,
    /// local, date-only and time-only forms alike, each rendered in TOML's own
    /// spelling. Nothing is dropped and nothing needs a reserved key, and it
    /// makes the two surfaces agree on identical authored text: `date:
    /// 2026-07-28` in a channel YAML already reaches a scorer as that same
    /// string. A script wanting a moment rather than a label parses it, on the
    /// same terms as every other key here — the meaning is the script's.
    ///
    /// The same contract the scorer-plugin side carries on
    /// `etv_station::config::Pool::config`, down to the carrier type: one
    /// opaque-config value type for the whole project, so a third scripting
    /// surface inherits the decision instead of picking one by looking at which
    /// file format it happens to parse. Only the plumbing differs, because a
    /// scorer receives one `ctx` map and an overlay receives flat scope
    /// constants.
    #[serde(
        default,
        deserialize_with = "deserialize_config",
        skip_serializing_if = "Option::is_none"
    )]
    pub config: Option<serde_json::Value>,
    /// Layers are rendered bottom-up in declaration order. A single Rhai script
    /// (if `script` is set) controls visibility/opacity uniformly across all
    /// layers — per-layer scripts are a future extension.
    #[serde(default, alias = "kind", deserialize_with = "deserialize_layers")]
    pub layers: Vec<OverlayKind>,
}

/// Read [`OverlaySpec::config`] through TOML's own value type before handing it
/// to the shared carrier, because TOML has one type the carrier does not.
///
/// Deserializing the carrier straight from the TOML parser *works*, but a
/// datetime arrives wrapped in a reserved key the toml crate uses to smuggle it
/// through serde — a private spelling that would then be what an overlay script
/// sees. Walking the TOML value instead is the only step where this pair of
/// formats needs one, and it is what lets the datetime land as plain text.
fn deserialize_config<'de, D>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<toml::Value>::deserialize(deserializer)?
        .map(|value| toml_to_carrier(value, "config"))
        .transpose()
        .map_err(serde::de::Error::custom)
}

/// TOML value → the shared carrier type, mapping the one type TOML has and the
/// carrier does not: a datetime becomes its TOML text.
///
/// Fallible for the one type TOML has that the carrier cannot hold at all — a
/// non-finite float. `weight = inf` is refused here rather than flattened to
/// null, and `path` is what lets the error name `config.weight` instead of just
/// the file. The channel-config side refuses the same value through the same
/// [`non_finite_message`] (#130).
pub(crate) fn toml_to_carrier(value: toml::Value, path: &str) -> Result<serde_json::Value, String> {
    use serde_json::Value as Carrier;
    Ok(match value {
        toml::Value::String(s) => Carrier::String(s),
        toml::Value::Integer(i) => Carrier::Number(i.into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(f)
            .map(Carrier::Number)
            .ok_or_else(|| non_finite_message(path, f))?,
        toml::Value::Boolean(b) => Carrier::Bool(b),
        toml::Value::Datetime(d) => Carrier::String(d.to_string()),
        toml::Value::Array(a) => Carrier::Array(
            a.into_iter()
                .enumerate()
                .map(|(i, v)| toml_to_carrier(v, &format!("{path}[{i}]")))
                .collect::<Result<_, _>>()?,
        ),
        toml::Value::Table(t) => Carrier::Object(
            t.into_iter()
                .map(|(k, v)| {
                    let value = toml_to_carrier(v, &format!("{path}.{k}"))?;
                    Ok((k, value))
                })
                .collect::<Result<_, String>>()?,
        ),
    })
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
    /// pixels and width is derived from the image's aspect. Source must be an
    /// 8-bit RGB or RGBA PNG; other formats (16-bit, palette) are rejected at
    /// decode time.
    Logo {
        path: PathBuf,
        corner: Corner,
        #[serde(default = "default_margin")]
        margin: u32,
        #[serde(default = "default_logo_height")]
        height: u32,
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Corner {
    #[default]
    TopRight,
    TopLeft,
    BottomRight,
    BottomLeft,
}

impl OverlaySpec {
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    pub fn from_path(path: &std::path::Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read overlay spec {}: {e}", path.display()))?;
        let mut spec: Self =
            Self::from_toml_str(&raw).map_err(|e| anyhow::anyhow!("parse overlay spec: {e}"))?;
        let base = path.parent();
        if let Some(script) = spec.script.take() {
            spec.script = Some(resolve_relative(&script, base));
        }
        for layer in &mut spec.layers {
            if let OverlayKind::Logo { path: logo, .. } = layer {
                *logo = resolve_relative(logo, base);
            }
        }
        Ok(spec)
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
        let toml = r#"
width = 1920
height = 1080
framerate = 30
pixel_format = "rgba8"

[kind]
type = "watermark"
corner = "top_right"
margin = 48
box_size = 200
color = [255, 100, 100, 200]
"#;
        let spec = OverlaySpec::from_toml_str(toml).unwrap();
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
        let toml = r#"
width = 1280
height = 720
framerate = 30

[[layers]]
type = "logo"
path = "logo.png"
corner = "bottom_right"
margin = 24
height = 96

[[layers]]
type = "text"
content = "PIERCE"
font_family = "Helvetica"
font_size = 36.0
color = [255, 255, 255, 230]
corner = "bottom_right"
margin = 132
"#;
        let spec = OverlaySpec::from_toml_str(toml).unwrap();
        assert_eq!(spec.layers.len(), 2);
        assert!(matches!(spec.layers[0], OverlayKind::Logo { .. }));
        assert!(matches!(spec.layers[1], OverlayKind::Text { .. }));
    }

    #[test]
    fn parses_text_overlay() {
        let toml = r#"
width = 1280
height = 720
framerate = 30

[kind]
type = "text"
content = "ETV STATION"
font_family = "Helvetica"
font_size = 64.0
color = [255, 255, 255, 230]
corner = "bottom_left"
margin = 40
"#;
        let spec = OverlaySpec::from_toml_str(toml).unwrap();
        assert_eq!(spec.layers.len(), 1);
        match &spec.layers[0] {
            OverlayKind::Text {
                content,
                font_family,
                font_size,
                color,
                corner,
                margin,
            } => {
                assert_eq!(content, "ETV STATION");
                assert_eq!(font_family, "Helvetica");
                assert!((*font_size - 64.0).abs() < 1e-4);
                assert_eq!(*color, [255, 255, 255, 230]);
                assert_eq!(*corner, Corner::BottomLeft);
                assert_eq!(*margin, 40);
            }
            _ => panic!("expected text kind"),
        }
    }

    #[test]
    fn text_uses_defaults_when_minimal() {
        let toml = r#"
width = 640
height = 360
framerate = 25

[kind]
type = "text"
content = "hi"
"#;
        let spec = OverlaySpec::from_toml_str(toml).unwrap();
        assert_eq!(spec.layers.len(), 1);
        match &spec.layers[0] {
            OverlayKind::Text {
                content,
                font_family,
                font_size,
                color,
                corner,
                margin,
            } => {
                assert_eq!(content, "hi");
                assert_eq!(font_family, "system-ui");
                assert!((*font_size - 48.0).abs() < 1e-4);
                assert_eq!(*color, [255, 255, 255, 255]);
                assert_eq!(*corner, Corner::TopRight);
                assert_eq!(*margin, 32);
            }
            _ => panic!("expected text kind"),
        }
    }

    #[test]
    fn parses_empty_default() {
        let toml = r#"
width = 320
height = 240
framerate = 24
"#;
        let spec = OverlaySpec::from_toml_str(toml).unwrap();
        assert_eq!(spec.pixel_format, PixelFormat::Rgba8);
        assert!(spec.layers.is_empty());
    }

    /// A `[config]` table parses into the carrier type with its nesting and
    /// scalar types intact — the shape a script reads (#125, #129).
    #[test]
    fn config_parses_into_the_shared_carrier_type() {
        let toml = r#"
width = 320
height = 240
framerate = 30

[config]
name = "lower-third"

[config.fade]
seconds = 0.4
steps = 3
enabled = true
labels = ["a", "b"]
"#;
        let spec = OverlaySpec::from_toml_str(toml).unwrap();
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
        let spec =
            |bag: &str| format!("width = 320\nheight = 240\nframerate = 30\n\n[config]\n{bag}");
        for (bag, key) in [
            ("weight = inf\n", "config.weight"),
            ("weight = nan\n", "config.weight"),
            ("weight = -inf\n", "config.weight"),
            ("steps = [1.0, inf]\n", "config.steps[1]"),
            ("[config.fade]\nweight = nan\n", "config.fade.weight"),
        ] {
            let err = OverlaySpec::from_toml_str(&spec(bag))
                .expect_err("a non-finite float must fail the load")
                .to_string();
            assert!(err.contains(key), "error did not name `{key}`: {err}");
            assert!(
                err.contains("large finite number"),
                "error did not tell the author what to write instead: {err}"
            );
        }
    }

    /// TOML's native datetime — the one type it has that the carrier does not —
    /// arrives as the text the author wrote, wherever it sits and in every form
    /// TOML spells it. Nothing is dropped and no reserved key leaks through
    /// (#129), and this is the same string a channel YAML's `released:
    /// 2026-07-28` already hands a scorer plugin.
    #[test]
    fn a_toml_datetime_arrives_as_its_authored_text() {
        let toml = r#"
width = 320
height = 240
framerate = 30

[config]
date = 2026-07-28
offset = 2026-07-28T10:32:00Z
local = 2026-07-28T10:32:00
clock = 10:32:00
window = [2026-07-28, 2026-07-29]

[config.nested]
since = 2026-07-28
"#;
        let spec = OverlaySpec::from_toml_str(toml).unwrap();
        let config = spec.config.as_ref().unwrap();
        assert_eq!(config["date"], serde_json::json!("2026-07-28"));
        assert_eq!(config["offset"], serde_json::json!("2026-07-28T10:32:00Z"));
        assert_eq!(config["local"], serde_json::json!("2026-07-28T10:32:00"));
        assert_eq!(config["clock"], serde_json::json!("10:32:00"));
        assert_eq!(config["window"][1], serde_json::json!("2026-07-29"));
        assert_eq!(config["nested"]["since"], serde_json::json!("2026-07-28"));

        // The toml crate smuggles a datetime through serde under a reserved
        // key; deserializing the carrier straight from the parser would make
        // that private spelling the thing a script sees. Nothing here may
        // contain it at any depth.
        let rendered = config.to_string();
        assert!(
            !rendered.contains("$__toml"),
            "a toml-private key leaked into the config a script reads: {rendered}"
        );
    }

    /// An unset `config` stays unset — the empty map a script reads is made at
    /// the engine, not injected into the spec.
    #[test]
    fn an_absent_config_stays_absent() {
        let spec = OverlaySpec::from_toml_str("width = 1\nheight = 1\nframerate = 1\n").unwrap();
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

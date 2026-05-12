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

    #[test]
    fn frame_byte_len_matches() {
        let spec = OverlaySpec {
            width: 100,
            height: 100,
            framerate: 30,
            pixel_format: PixelFormat::Rgba8,
            script: None,
            layers: vec![],
        };
        assert_eq!(spec.frame_byte_len(), 40_000);
    }
}

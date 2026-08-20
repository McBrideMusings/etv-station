use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use parley::{
    Alignment, AlignmentOptions, FontContext, FontFamily, Layout, LayoutContext,
    PositionedLayoutItem, StyleProperty,
};
use vello::kurbo::{Affine, Point, Rect, RoundedRect};
use vello::peniko::{
    Blob, Color, Fill, Gradient, ImageAlphaType, ImageBrush as PenikoImage, ImageData, ImageFormat,
};
use vello::wgpu;
use vello::{AaConfig, AaSupport, Glyph, RenderParams, Renderer, RendererOptions, Scene};

use crate::overlay_spec::{Corner, Edge, OverlayKind, PixelFormat};
use crate::rhai_engine::{LayerState, OverlayState};

const COPY_BYTES_PER_ROW_ALIGNMENT: u32 = 256;

/// Vendored Latin-subset Inter Regular (~68KB, SIL OFL). Registered into the
/// renderer's FontContext so OverlayKind::Text renders inside slim Linux
/// deploy containers that ship without a system font stack. See
/// `assets/fonts/README.md` for provenance.
const FALLBACK_FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/Inter-Regular.ttf");
const FALLBACK_FONT_FAMILY: &str = "Inter";

/// The Parley font stack `draw_text` shapes with: the configured family first,
/// then the vendored Inter fallback so anything the family doesn't cover (or
/// any host where the family is missing, e.g. a slim deploy container) still
/// produces glyphs. The unit tests assert this still appends the fallback, so
/// a refactor that silently drops it fails the build.
fn font_stack(font_family: &str) -> String {
    format!("{font_family}, {FALLBACK_FONT_FAMILY}")
}

/// Build and line-break a Parley layout for `content`, shaping it with
/// [`font_stack`]. Shared by [`VelloRenderer::draw_text`] and the unit tests so
/// both exercise the same `font_stack` + shaping path.
fn build_text_layout(
    font_context: &mut FontContext,
    layout_context: &mut LayoutContext<()>,
    content: &str,
    font_family: &str,
    font_size: f32,
    letter_spacing: f32,
) -> Layout<()> {
    let stack = font_stack(font_family);
    let mut builder = layout_context.ranged_builder(font_context, content, 1.0, true);
    builder.push_default(StyleProperty::FontFamily(FontFamily::Source(stack.into())));
    builder.push_default(StyleProperty::FontSize(font_size));
    builder.push_default(StyleProperty::LetterSpacing(letter_spacing));
    let mut layout = builder.build(content);
    layout.break_all_lines(None);
    layout.align(Alignment::Start, AlignmentOptions::default());
    layout
}

pub struct VelloRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    renderer: Renderer,
    target: wgpu::Texture,
    target_view: wgpu::TextureView,
    readback: wgpu::Buffer,
    width: u32,
    height: u32,
    padded_bytes_per_row: u32,
    unpadded_bytes_per_row: u32,
    // `None` caches a decode failure so a logo that keeps failing every frame
    // (missing file, unsupported format) is only ever opened and decoded once
    // per process, not 30x/sec. Fixing the file in place doesn't clear this
    // cache — the overlay process needs a restart to pick it up, which the
    // supervisor already does on config reload.
    image_cache: HashMap<PathBuf, Option<PenikoImage>>,
    font_context: FontContext,
    layout_context: LayoutContext<()>,
    warned_missing_glyphs: HashSet<String>,
    warned_bad_images: HashSet<PathBuf>,
}

impl VelloRenderer {
    pub fn new(width: u32, height: u32, pixel_format: PixelFormat) -> anyhow::Result<Self> {
        if !matches!(pixel_format, PixelFormat::Rgba8) {
            anyhow::bail!("only rgba8 is supported in the spike");
        }

        // Offscreen render target only — no surface, so no display handle.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .map_err(|e| anyhow::anyhow!("no wgpu adapter available: {e}"))?;

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("etv-overlay-device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            trace: wgpu::Trace::Off,
        }))
        .map_err(|e| anyhow::anyhow!("wgpu device request failed: {e}"))?;

        let renderer = Renderer::new(
            &device,
            RendererOptions {
                use_cpu: false,
                antialiasing_support: AaSupport::area_only(),
                num_init_threads: NonZeroUsize::new(1),
                pipeline_cache: None,
            },
        )
        .map_err(|e| anyhow::anyhow!("vello renderer init failed: {e}"))?;

        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("etv-overlay-target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

        let unpadded_bytes_per_row = width * 4;
        let padded_bytes_per_row = align_up(unpadded_bytes_per_row, COPY_BYTES_PER_ROW_ALIGNMENT);
        let buffer_size = (padded_bytes_per_row as u64) * (height as u64);

        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("etv-overlay-readback"),
            size: buffer_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut font_context = FontContext::new();
        font_context
            .collection
            .register_fonts(Blob::from(FALLBACK_FONT_BYTES.to_vec()), None);

        Ok(Self {
            device,
            queue,
            renderer,
            target,
            target_view,
            readback,
            width,
            height,
            padded_bytes_per_row,
            unpadded_bytes_per_row,
            image_cache: HashMap::new(),
            font_context,
            layout_context: LayoutContext::new(),
            warned_missing_glyphs: HashSet::new(),
            warned_bad_images: HashSet::new(),
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn render_frame(&mut self, state: &OverlayState) -> anyhow::Result<Vec<u8>> {
        let mut scene = Scene::new();
        if state.visible {
            self.build_scene(&mut scene, state)?;
        }

        self.renderer
            .render_to_texture(
                &self.device,
                &self.queue,
                &scene,
                &self.target_view,
                &RenderParams {
                    base_color: Color::TRANSPARENT,
                    width: self.width,
                    height: self.height,
                    antialiasing_method: AaConfig::Area,
                },
            )
            .map_err(|e| anyhow::anyhow!("vello render: {e}"))?;

        self.copy_target_to_buffer();
        self.poll_until_mapped()?;
        let frame = self.read_padded_buffer();
        self.readback.unmap();
        Ok(frame)
    }

    fn build_scene(&mut self, scene: &mut Scene, state: &OverlayState) -> anyhow::Result<()> {
        for layer in &state.layers {
            if !layer.visible {
                continue;
            }
            let effective_opacity = state.opacity * layer.opacity;
            self.build_layer(scene, layer, effective_opacity)?;
        }
        Ok(())
    }

    fn build_layer(
        &mut self,
        scene: &mut Scene,
        layer: &LayerState,
        opacity: f32,
    ) -> anyhow::Result<()> {
        match &layer.kind {
            OverlayKind::Empty => {}
            OverlayKind::Watermark {
                corner,
                margin,
                box_size,
                color,
            } => {
                let (x0, y0) = corner_origin(*corner, *margin, *box_size, self.width, self.height);
                let x0 = x0 + layer.offset_x as i64;
                let y0 = y0 + layer.offset_y as i64;
                let rect = RoundedRect::new(
                    x0 as f64,
                    y0 as f64,
                    (x0 + *box_size as i64) as f64,
                    (y0 + *box_size as i64) as f64,
                    18.0,
                );
                let alpha = (color[3] as f32 / 255.0) * opacity;
                let fill = Color::from_rgba8(color[0], color[1], color[2], (alpha * 255.0) as u8);
                scene.fill(Fill::NonZero, Affine::IDENTITY, fill, None, &rect);
            }
            OverlayKind::Scrim { edge, size, color } => {
                // Anchored to the frame edge, so `offset_x`/`offset_y` are
                // deliberately ignored: translating the band would drag the
                // transparent end into view and expose a hard cut where the
                // gradient stops. A script fades a scrim with `opacity`.
                let (w, h) = (self.width as f64, self.height as f64);
                let reach = *size as f64;
                // `rect` is the band; `(from, to)` runs across it, opaque end
                // first, so one gradient definition serves all four edges.
                let (rect, from, to) = match edge {
                    Edge::Bottom => (
                        Rect::new(0.0, h - reach, w, h),
                        Point::new(0.0, h),
                        Point::new(0.0, h - reach),
                    ),
                    Edge::Top => (
                        Rect::new(0.0, 0.0, w, reach),
                        Point::new(0.0, 0.0),
                        Point::new(0.0, reach),
                    ),
                    Edge::Left => (
                        Rect::new(0.0, 0.0, reach, h),
                        Point::new(0.0, 0.0),
                        Point::new(reach, 0.0),
                    ),
                    Edge::Right => (
                        Rect::new(w - reach, 0.0, w, h),
                        Point::new(w, 0.0),
                        Point::new(w - reach, 0.0),
                    ),
                };
                let alpha = (color[3] as f32 / 255.0) * opacity;
                let near = Color::from_rgba8(color[0], color[1], color[2], (alpha * 255.0) as u8);
                // The far end keeps the same RGB at zero alpha rather than
                // going transparent-black: interpolating toward a different
                // colour is what produces the grey haze a naive scrim shows
                // over a bright scene.
                let far = Color::from_rgba8(color[0], color[1], color[2], 0);
                let brush = Gradient::new_linear(from, to).with_stops([near, far]);
                scene.fill(Fill::NonZero, Affine::IDENTITY, &brush, None, &rect);
            }
            OverlayKind::Logo {
                path,
                corner,
                margin,
                height: logo_height,
            } => {
                // A logo that cannot be decoded drops just this layer instead
                // of taking the whole render down: every other layer still
                // draws, render_frame still returns a full frame, and the
                // fifo keeps a writer (#302).
                let Some(image) = self.load_or_get_image(path) else {
                    return Ok(());
                };
                let image = image.clone();
                let aspect = image.image.width as f64 / image.image.height as f64;
                let h = *logo_height as f64;
                let w = h * aspect;
                let (x0, y0) =
                    corner_origin_f64(*corner, *margin as f64, w, h, self.width, self.height);
                let x0 = x0 + layer.offset_x as f64;
                let y0 = y0 + layer.offset_y as f64;
                let scale_x = w / image.image.width as f64;
                let scale_y = h / image.image.height as f64;
                let transform =
                    Affine::translate((x0, y0)) * Affine::scale_non_uniform(scale_x, scale_y);
                let image_with_alpha = image.with_alpha(opacity);
                scene.draw_image(&image_with_alpha, transform);
            }
            OverlayKind::Text {
                content,
                font_family,
                font_size,
                letter_spacing,
                color,
                corner,
                margin,
            } => {
                self.draw_text(
                    scene,
                    content,
                    font_family,
                    *font_size,
                    *letter_spacing,
                    *color,
                    *corner,
                    *margin,
                    layer.offset_x,
                    layer.offset_y,
                    opacity,
                );
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_text(
        &mut self,
        scene: &mut Scene,
        content: &str,
        font_family: &str,
        font_size: f32,
        letter_spacing: f32,
        color: [u8; 4],
        corner: Corner,
        margin: u32,
        offset_x: f32,
        offset_y: f32,
        opacity: f32,
    ) {
        if content.is_empty() {
            return;
        }
        // `build_text_layout` appends the vendored fallback to the stack so
        // anything the configured family doesn't cover (or any host where the
        // family is missing, e.g. a slim deploy container) still produces
        // glyphs.
        let layout = build_text_layout(
            &mut self.font_context,
            &mut self.layout_context,
            content,
            font_family,
            font_size,
            letter_spacing,
        );

        let text_w = layout.width() as f64;
        let text_h = layout.height() as f64;
        let (x0, y0) = corner_origin_f64(
            corner,
            margin as f64,
            text_w,
            text_h,
            self.width,
            self.height,
        );
        let x0 = x0 + offset_x as f64;
        let y0 = y0 + offset_y as f64;

        let alpha = (color[3] as f32 / 255.0) * opacity;
        let brush = Color::from_rgba8(color[0], color[1], color[2], (alpha * 255.0) as u8);

        let mut total_glyphs = 0usize;
        for line in layout.lines() {
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                    continue;
                };
                let run = glyph_run.run();
                let run_font_size = run.font_size();
                let glyphs: Vec<Glyph> = glyph_run
                    .positioned_glyphs()
                    .map(|g| Glyph {
                        id: g.id,
                        x: g.x,
                        y: g.y,
                    })
                    .collect();
                if glyphs.is_empty() {
                    continue;
                }
                total_glyphs += glyphs.len();
                scene
                    .draw_glyphs(run.font())
                    .font_size(run_font_size)
                    .brush(brush)
                    .transform(Affine::translate((x0, y0)))
                    .draw(Fill::NonZero, glyphs.into_iter());
            }
        }

        // Non-empty content that produced no glyphs means even the bundled
        // Inter fallback couldn't shape the string — likely a non-Latin
        // codepoint, since the vendored font is a Latin subset. Log once per
        // font_family so the operator sees the symptom instead of an
        // empty-looking overlay.
        if total_glyphs == 0 && self.warned_missing_glyphs.insert(font_family.to_string()) {
            tracing::error!(
                font_family = font_family,
                content = content,
                "text overlay produced no glyphs; configured font family is missing on host and vendored fallback could not shape the content (non-Latin?)",
            );
        }
    }

    /// Infallible: a PNG that cannot be decoded is not a render failure, only
    /// a missing layer. Logs once per path (not once per frame) and caches
    /// the negative result so a persistently bad logo is opened and decoded
    /// exactly once, not on every frame.
    fn load_or_get_image(&mut self, path: &Path) -> Option<&PenikoImage> {
        use std::collections::hash_map::Entry;
        let warned = &mut self.warned_bad_images;
        let slot = match self.image_cache.entry(path.to_path_buf()) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                let decoded = match decode_png(path) {
                    Ok(image) => Some(image),
                    Err(err) => {
                        if warned.insert(path.to_path_buf()) {
                            tracing::error!(
                                path = %path.display(),
                                error = %err,
                                "logo image could not be decoded; dropping this layer and continuing to render",
                            );
                        }
                        None
                    }
                };
                e.insert(decoded)
            }
        };
        slot.as_ref()
    }

    fn copy_target_to_buffer(&self) {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("etv-overlay-copy"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.padded_bytes_per_row),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(std::iter::once(encoder.finish()));
    }

    fn poll_until_mapped(&self) -> anyhow::Result<()> {
        let slice = self.readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let _ = self.device.poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        });
        rx.recv()
            .map_err(|e| anyhow::anyhow!("readback channel closed: {e}"))?
            .map_err(|e| anyhow::anyhow!("readback map: {e}"))?;
        Ok(())
    }

    fn read_padded_buffer(&self) -> Vec<u8> {
        let slice = self.readback.slice(..);
        let view = slice.get_mapped_range();
        let mut out = Vec::with_capacity((self.unpadded_bytes_per_row * self.height) as usize);
        for row in 0..self.height as usize {
            let start = row * self.padded_bytes_per_row as usize;
            let end = start + self.unpadded_bytes_per_row as usize;
            out.extend_from_slice(&view[start..end]);
        }
        drop(view);
        out
    }
}

fn corner_origin(
    corner: Corner,
    margin: u32,
    box_size: u32,
    width: u32,
    height: u32,
) -> (i64, i64) {
    let m = margin as i64;
    let s = box_size as i64;
    let w = width as i64;
    let h = height as i64;
    match corner {
        Corner::TopLeft => (m, m),
        Corner::TopRight => (w - m - s, m),
        Corner::BottomLeft => (m, h - m - s),
        Corner::BottomRight => (w - m - s, h - m - s),
    }
}

fn corner_origin_f64(
    corner: Corner,
    margin: f64,
    width: f64,
    height: f64,
    canvas_width: u32,
    canvas_height: u32,
) -> (f64, f64) {
    let cw = canvas_width as f64;
    let ch = canvas_height as f64;
    match corner {
        Corner::TopLeft => (margin, margin),
        Corner::TopRight => (cw - margin - width, margin),
        Corner::BottomLeft => (margin, ch - margin - height),
        Corner::BottomRight => (cw - margin - width, ch - margin - height),
    }
}

fn decode_png(path: &Path) -> anyhow::Result<PenikoImage> {
    let file = std::fs::File::open(path)
        .map_err(|e| anyhow::anyhow!("open logo {}: {e}", path.display()))?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    // Folds 16-bit -> 8-bit, sub-8-bit -> 8-bit, Indexed -> Rgb/Rgba, and
    // Grayscale+tRNS -> GrayscaleAlpha at read time. It does NOT fold
    // Grayscale/8 or GrayscaleAlpha/8 into RGB — those are handled below by
    // matching on the decoded OutputInfo, not the on-disk color type. Must be
    // set before read_info(); setting it on the Reader afterwards has no
    // effect on the already-computed output buffer size.
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .map_err(|e| anyhow::anyhow!("read png info {}: {e}", path.display()))?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame_info = reader
        .next_frame(&mut buf)
        .map_err(|e| anyhow::anyhow!("decode png {}: {e}", path.display()))?;
    buf.truncate(frame_info.buffer_size());

    // Match on `frame_info` (the OutputInfo from `next_frame`), not
    // `reader.info()` — the latter still reports the file's on-disk format
    // after a transformation, which would silently mis-expand a normalized
    // buffer.
    let rgba = match (frame_info.color_type, frame_info.bit_depth) {
        (png::ColorType::Rgba, png::BitDepth::Eight) => buf,
        (png::ColorType::Rgb, png::BitDepth::Eight) => expand_rgb_to_rgba(&buf),
        (png::ColorType::Grayscale, png::BitDepth::Eight) => expand_gray_to_rgba(&buf),
        (png::ColorType::GrayscaleAlpha, png::BitDepth::Eight) => expand_gray_alpha_to_rgba(&buf),
        (ct, bd) => {
            anyhow::bail!(
                "unsupported PNG format ({ct:?}/{bd:?}) in {} after normalization",
                path.display()
            );
        }
    };

    Ok(PenikoImage::new(ImageData {
        data: Blob::from(rgba),
        format: ImageFormat::Rgba8,
        alpha_type: ImageAlphaType::Alpha,
        width: frame_info.width,
        height: frame_info.height,
    }))
}

fn expand_rgb_to_rgba(rgb: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgb.len() / 3 * 4);
    for chunk in rgb.chunks_exact(3) {
        out.extend_from_slice(chunk);
        out.push(255);
    }
    out
}

fn expand_gray_to_rgba(gray: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(gray.len() * 4);
    for &sample in gray {
        out.extend_from_slice(&[sample, sample, sample, 255]);
    }
    out
}

fn expand_gray_alpha_to_rgba(gray_alpha: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(gray_alpha.len() * 2);
    for chunk in gray_alpha.chunks_exact(2) {
        let (sample, alpha) = (chunk[0], chunk[1]);
        out.extend_from_slice(&[sample, sample, sample, alpha]);
    }
    out
}

fn align_up(value: u32, alignment: u32) -> u32 {
    value.div_ceil(alignment) * alignment
}

#[cfg(test)]
mod tests {
    use super::*;
    use parley::fontique::{Collection, CollectionOptions};

    /// A `FontContext` with NO system fonts, holding only the vendored Inter —
    /// mirrors a slim deploy container with no system font stack. Lets the
    /// fallback tests run host-independently (no system font can satisfy a
    /// query) and without a GPU.
    fn fonts_free_context() -> FontContext {
        let mut fcx = FontContext {
            collection: Collection::new(CollectionOptions {
                shared: false,
                system_fonts: false,
            }),
            source_cache: Default::default(),
        };
        fcx.collection
            .register_fonts(Blob::from(FALLBACK_FONT_BYTES.to_vec()), None);
        fcx
    }

    /// Directly guards the regression #60 names: a refactor of the text font
    /// stack that silently drops the vendored Inter fallback fails here.
    #[test]
    fn font_stack_appends_vendored_fallback() {
        let stack = font_stack("Helvetica");
        assert_eq!(stack, "Helvetica, Inter");
        assert!(stack.ends_with(FALLBACK_FONT_FAMILY));
    }

    /// With system fonts disabled (the slim-container case), a text layer whose
    /// configured `font_family` is absent must still shape glyphs — and they
    /// must come from the vendored Inter blob, since it is the only font
    /// present. Proves the bundled font registers and renders Latin text where
    /// no system fallback exists. Host-independent and GPU-free.
    #[test]
    fn missing_family_renders_via_vendored_inter() {
        let mut fcx = fonts_free_context();
        let mut lcx: LayoutContext<()> = LayoutContext::new();
        let layout = build_text_layout(
            &mut fcx,
            &mut lcx,
            "Fallback Glyphs 123",
            "NoSuchFontFamilyExists12345",
            32.0,
            0.0,
        );

        let mut total_glyphs = 0usize;
        let mut any_run = false;
        let mut all_vendored = true;
        for line in layout.lines() {
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                    continue;
                };
                let n = glyph_run.positioned_glyphs().count();
                if n == 0 {
                    continue;
                }
                any_run = true;
                total_glyphs += n;
                let font_bytes: &[u8] = glyph_run.run().font().data.as_ref();
                if font_bytes != FALLBACK_FONT_BYTES {
                    all_vendored = false;
                }
            }
        }

        assert!(
            total_glyphs > 0,
            "a missing font_family should still shape glyphs via the vendored fallback",
        );
        assert!(
            any_run && all_vendored,
            "glyphs must be shaped by the vendored Inter blob, not another font",
        );
    }

    #[test]
    fn align_up_rounds_to_multiple() {
        assert_eq!(align_up(0, 256), 0);
        assert_eq!(align_up(1, 256), 256);
        assert_eq!(align_up(256, 256), 256);
        assert_eq!(align_up(257, 256), 512);
        assert_eq!(align_up(1920 * 4, 256), 7680);
    }

    #[test]
    fn corner_origin_top_right() {
        let (x, y) = corner_origin(Corner::TopRight, 32, 160, 1920, 1080);
        assert_eq!(x, 1920 - 32 - 160);
        assert_eq!(y, 32);
    }

    #[test]
    fn corner_origin_bottom_left() {
        let (x, y) = corner_origin(Corner::BottomLeft, 24, 100, 1280, 720);
        assert_eq!(x, 24);
        assert_eq!(y, 720 - 24 - 100);
    }

    /// Writes a small PNG with the given color type/depth using the `png`
    /// crate's own encoder, so these tests exercise `decode_png` against a
    /// real file rather than a hand-built byte string.
    fn write_png(
        dir: &std::path::Path,
        name: &str,
        color_type: png::ColorType,
        bit_depth: png::BitDepth,
        width: u32,
        height: u32,
        data: &[u8],
    ) -> PathBuf {
        let path = dir.join(name);
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
        encoder.set_color(color_type);
        encoder.set_depth(bit_depth);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(data).unwrap();
        path
    }

    #[test]
    fn decode_png_expands_grayscale_eight() {
        let dir = tempfile::tempdir().unwrap();
        // 2x1, mid-grey (128) then white (255).
        let path = write_png(
            dir.path(),
            "gray.png",
            png::ColorType::Grayscale,
            png::BitDepth::Eight,
            2,
            1,
            &[128, 255],
        );
        let image = decode_png(&path).expect("grayscale/8 should decode");
        let data = image.image.data.data();
        assert_eq!(data.len(), 8);
        // First pixel: mid-grey replicated into r/g/b, alpha opaque.
        assert_eq!(&data[0..4], &[128, 128, 128, 255]);
        assert_eq!(&data[4..8], &[255, 255, 255, 255]);
    }

    #[test]
    fn decode_png_expands_grayscale_alpha_eight() {
        let dir = tempfile::tempdir().unwrap();
        // 1x1, grey=100 at half alpha=128.
        let path = write_png(
            dir.path(),
            "gray_alpha.png",
            png::ColorType::GrayscaleAlpha,
            png::BitDepth::Eight,
            1,
            1,
            &[100, 128],
        );
        let image = decode_png(&path).expect("grayscale-alpha/8 should decode");
        let data = image.image.data.data();
        assert_eq!(data.len(), 4);
        assert_eq!(&data[0..4], &[100, 100, 100, 128]);
    }

    #[test]
    fn decode_png_expands_indexed_eight() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("indexed.png");
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), 1, 1);
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Eight);
        // Palette entry 0 = pure red.
        encoder.set_palette(vec![255u8, 0, 0]);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[0u8]).unwrap();
        drop(writer);

        let image = decode_png(&path).expect("indexed/8 should decode");
        let data = image.image.data.data();
        assert_eq!(data.len(), 4);
        assert_eq!(&data[0..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn decode_png_normalizes_sixteen_bit_rgba() {
        let dir = tempfile::tempdir().unwrap();
        // 1x1 RGBA/16: each sample is 2 bytes big-endian. 0xFFFF -> 255,
        // 0x0000 -> 0 after STRIP_16.
        let path = write_png(
            dir.path(),
            "rgba16.png",
            png::ColorType::Rgba,
            png::BitDepth::Sixteen,
            1,
            1,
            &[0xFF, 0xFF, 0x00, 0x00, 0x80, 0x00, 0xFF, 0xFF],
        );
        let image = decode_png(&path).expect("rgba/16 should decode");
        let data = image.image.data.data();
        assert_eq!(data.len(), 4);
        assert_eq!(data[0], 255);
        assert_eq!(data[1], 0);
        assert_eq!(data[3], 255);
    }

    #[test]
    fn decode_png_still_errors_on_garbage_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not_a_png.png");
        std::fs::write(&path, b"this is not a png file").unwrap();
        assert!(
            decode_png(&path).is_err(),
            "garbage bytes must still fail to decode, exercising the drop-the-layer path",
        );
    }
}

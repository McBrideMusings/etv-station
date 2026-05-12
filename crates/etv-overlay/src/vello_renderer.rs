use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use parley::{
    Alignment, AlignmentOptions, FontContext, FontStack, LayoutContext, PositionedLayoutItem,
    StyleProperty,
};
use vello::kurbo::{Affine, RoundedRect};
use vello::peniko::{Blob, Color, Fill, Image as PenikoImage, ImageFormat};
use vello::wgpu;
use vello::{AaConfig, AaSupport, Glyph, RenderParams, Renderer, RendererOptions, Scene};

use crate::overlay_spec::{Corner, OverlayKind, PixelFormat};
use crate::rhai_engine::OverlayState;

const COPY_BYTES_PER_ROW_ALIGNMENT: u32 = 256;

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
    image_cache: HashMap<PathBuf, PenikoImage>,
    font_context: FontContext,
    layout_context: LayoutContext<()>,
}

impl VelloRenderer {
    pub fn new(width: u32, height: u32, pixel_format: PixelFormat) -> anyhow::Result<Self> {
        if !matches!(pixel_format, PixelFormat::Rgba8) {
            anyhow::bail!("only rgba8 is supported in the spike");
        }

        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .ok_or_else(|| anyhow::anyhow!("no wgpu adapter available"))?;

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("etv-overlay-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))
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
            font_context: FontContext::new(),
            layout_context: LayoutContext::new(),
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
            self.build_layer(scene, layer, state.opacity)?;
        }
        Ok(())
    }

    fn build_layer(
        &mut self,
        scene: &mut Scene,
        layer: &OverlayKind,
        opacity: f32,
    ) -> anyhow::Result<()> {
        match layer {
            OverlayKind::Empty => {}
            OverlayKind::Watermark {
                corner,
                margin,
                box_size,
                color,
            } => {
                let (x0, y0) = corner_origin(*corner, *margin, *box_size, self.width, self.height);
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
            OverlayKind::Logo {
                path,
                corner,
                margin,
                height: logo_height,
            } => {
                let image = self.load_or_get_image(path)?.clone();
                let aspect = image.width as f64 / image.height as f64;
                let h = *logo_height as f64;
                let w = h * aspect;
                let (x0, y0) =
                    corner_origin_f64(*corner, *margin as f64, w, h, self.width, self.height);
                let scale_x = w / image.width as f64;
                let scale_y = h / image.height as f64;
                let transform =
                    Affine::translate((x0, y0)) * Affine::scale_non_uniform(scale_x, scale_y);
                let image_with_alpha = image.with_alpha(opacity);
                scene.draw_image(&image_with_alpha, transform);
            }
            OverlayKind::Text {
                content,
                font_family,
                font_size,
                color,
                corner,
                margin,
            } => {
                self.draw_text(
                    scene,
                    content,
                    font_family,
                    *font_size,
                    *color,
                    *corner,
                    *margin,
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
        color: [u8; 4],
        corner: Corner,
        margin: u32,
        opacity: f32,
    ) {
        if content.is_empty() {
            return;
        }
        let mut builder =
            self.layout_context
                .ranged_builder(&mut self.font_context, content, 1.0, true);
        builder.push_default(StyleProperty::FontStack(FontStack::Source(
            font_family.into(),
        )));
        builder.push_default(StyleProperty::FontSize(font_size));
        let mut layout = builder.build(content);
        layout.break_all_lines(None);
        layout.align(None, Alignment::Start, AlignmentOptions::default());

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

        let alpha = (color[3] as f32 / 255.0) * opacity;
        let brush = Color::from_rgba8(color[0], color[1], color[2], (alpha * 255.0) as u8);

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
                        id: g.id as u32,
                        x: g.x,
                        y: g.y,
                    })
                    .collect();
                if glyphs.is_empty() {
                    continue;
                }
                scene
                    .draw_glyphs(run.font())
                    .font_size(run_font_size)
                    .brush(brush)
                    .transform(Affine::translate((x0, y0)))
                    .draw(Fill::NonZero, glyphs.into_iter());
            }
        }
    }

    fn load_or_get_image(&mut self, path: &Path) -> anyhow::Result<&PenikoImage> {
        use std::collections::hash_map::Entry;
        match self.image_cache.entry(path.to_path_buf()) {
            Entry::Occupied(e) => Ok(e.into_mut()),
            Entry::Vacant(e) => Ok(e.insert(decode_png(path)?)),
        }
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
        let _ = self.device.poll(wgpu::Maintain::Wait);
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
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder
        .read_info()
        .map_err(|e| anyhow::anyhow!("read png info {}: {e}", path.display()))?;
    let info = reader.info().clone();
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame_info = reader
        .next_frame(&mut buf)
        .map_err(|e| anyhow::anyhow!("decode png {}: {e}", path.display()))?;
    buf.truncate(frame_info.buffer_size());

    let rgba = match (info.color_type, info.bit_depth) {
        (png::ColorType::Rgba, png::BitDepth::Eight) => buf,
        (png::ColorType::Rgb, png::BitDepth::Eight) => expand_rgb_to_rgba(&buf),
        (ct, bd) => {
            anyhow::bail!(
                "unsupported PNG format ({ct:?}/{bd:?}) in {}; convert to 8-bit RGBA first",
                path.display()
            );
        }
    };

    Ok(PenikoImage::new(
        Blob::from(rgba),
        ImageFormat::Rgba8,
        info.width,
        info.height,
    ))
}

fn expand_rgb_to_rgba(rgb: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgb.len() / 3 * 4);
    for chunk in rgb.chunks_exact(3) {
        out.extend_from_slice(chunk);
        out.push(255);
    }
    out
}

fn align_up(value: u32, alignment: u32) -> u32 {
    value.div_ceil(alignment) * alignment
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

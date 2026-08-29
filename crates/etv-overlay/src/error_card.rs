use crate::overlay_spec::PixelFormat;
use crate::vello_renderer::VelloRenderer;

/// Render a full-frame diagnostic card naming a playout item that failed to
/// transcode, PNG-encoded and ready to write to disk. `ersatztv-channel`
/// swaps this in for the black/silence fallback when an item's ffmpeg
/// pipeline fails (etv-station-next#386), so the failure is visible on the
/// stream instead of only in the container log.
pub fn render_error_card_png(
    width: u32,
    height: u32,
    channel_label: &str,
    item_title: &str,
    error_text: &str,
) -> anyhow::Result<Vec<u8>> {
    let mut renderer = VelloRenderer::new(width, height, PixelFormat::Rgba8)?;
    let rgba = renderer.render_error_card(channel_label, item_title, error_text)?;
    encode_png(width, height, &rgba)
}

/// The card is fully opaque by construction (a background rect covers the
/// whole canvas before anything else is drawn), so the alpha channel is
/// dropped rather than carried into the still-image file this feeds ffmpeg.
fn encode_png(width: u32, height: u32, rgba: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut rgb = Vec::with_capacity(rgba.len() / 4 * 3);
    for pixel in rgba.chunks_exact(4) {
        rgb.extend_from_slice(&pixel[..3]);
    }

    let mut buf = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut buf, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| anyhow::anyhow!("png header: {e}"))?;
        writer
            .write_image_data(&rgb)
            .map_err(|e| anyhow::anyhow!("png encode: {e}"))?;
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_png_drops_alpha_and_decodes_back_opaque() {
        // 2x1: opaque red, then opaque blue, alpha discarded either way.
        let rgba = [255u8, 0, 0, 255, 0, 0, 255, 128];
        let png_bytes = encode_png(2, 1, &rgba).expect("encode should succeed");

        let mut decoder = png::Decoder::new(png_bytes.as_slice());
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder.read_info().expect("valid png");
        let mut out = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut out).expect("decode frame");
        out.truncate(info.buffer_size());

        assert_eq!(info.color_type, png::ColorType::Rgb);
        assert_eq!(&out[0..3], &[255, 0, 0]);
        assert_eq!(&out[3..6], &[0, 0, 255]);
    }
}

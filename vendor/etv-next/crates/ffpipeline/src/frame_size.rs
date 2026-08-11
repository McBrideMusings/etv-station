use std::str::FromStr;

use derive_more::Display;

use crate::pipeline::FrameState;

#[derive(Debug, Clone, Copy, PartialEq, Display)]
#[display("FrameSize(w={},h={})", width, height)]
pub struct FrameSize {
    pub width: u32,
    pub height: u32,
}

impl FromStr for FrameSize {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split('x');
        let (w, h) = match (parts.next(), parts.next(), parts.next()) {
            (Some(w), Some(h), None) => (w, h),
            _ => {
                return Err(format!(
                    "invalid frame size format: '{s}', expected 'WIDTHxHEIGHT'"
                ));
            }
        };
        let width = w
            .trim()
            .parse::<u32>()
            .map_err(|e| format!("invalid width '{w}': {e}"))?;
        let height = h
            .trim()
            .parse::<u32>()
            .map_err(|e| format!("invalid height '{h}': {e}"))?;
        Ok(FrameSize { width, height })
    }
}

impl FrameSize {
    pub(crate) fn square_pixel_size_contain(&self, frame_state: &FrameState) -> FrameSize {
        let mut source_width = frame_state.size.width as f64;
        let source_height = frame_state.size.height as f64;

        if frame_state.is_anamorphic
            && let Some(sar) = Self::sar_as_float(frame_state)
        {
            source_width *= sar;
        }

        let min_percent = f64::min(
            self.width as f64 / source_width,
            self.height as f64 / source_height,
        );

        FrameSize {
            width: Self::round_to_even(source_width * min_percent).min(self.width),
            height: Self::round_to_even(source_height * min_percent).min(self.height),
        }
    }

    pub(crate) fn square_pixel_size_cover(&self, frame_state: &FrameState) -> FrameSize {
        let mut source_width = frame_state.size.width as f64;
        let source_height = frame_state.size.height as f64;

        if frame_state.is_anamorphic
            && let Some(sar) = Self::sar_as_float(frame_state)
        {
            source_width *= sar;
        }

        let max_percent = f64::max(
            self.width as f64 / source_width,
            self.height as f64 / source_height,
        );

        FrameSize {
            width: Self::round_to_even(source_width * max_percent),
            height: Self::round_to_even(source_height * max_percent),
        }
    }

    /// Rounds to the nearest integer, then up to the next even number if that
    /// is odd. Every downstream filter here (scale, zscale, pad, the encoder)
    /// works in yuv420p, which subsamples chroma by 2 in both directions and
    /// rejects an odd frame dimension outright.
    fn round_to_even(value: f64) -> u32 {
        (value.round_ties_even() as u32).next_multiple_of(2)
    }

    fn sar_as_float(frame_state: &FrameState) -> Option<f64> {
        let sample_aspect_ratio: Option<&str> = frame_state
            .sample_aspect_ratio
            .as_ref()
            .map(|sar| sar.as_ref());

        // some media servers don't provide sample aspect ratio so we have to calculate it
        if sample_aspect_ratio.is_none() || sample_aspect_ratio == Some("0:0") {
            match &frame_state.display_aspect_ratio {
                Some(display_aspect_ratio) => {
                    // check for decimal DAR
                    match display_aspect_ratio.parse::<f64>() {
                        Ok(dar) => {
                            let res =
                                frame_state.size.width as f64 / frame_state.size.height as f64;
                            Some(dar / res)
                        }
                        Err(_) => {
                            // assume it's a ratio
                            Self::parse_ratio(display_aspect_ratio)
                        }
                    }
                }
                None => None,
            }
        } else if let Some(sample_aspect_ratio) = sample_aspect_ratio {
            Self::parse_ratio(sample_aspect_ratio)
        } else {
            None
        }
    }

    fn parse_ratio(ratio: &str) -> Option<f64> {
        let split: Vec<&str> = ratio.split(':').collect();
        if let Ok(num) = split[0].parse::<f64>()
            && let Ok(den) = split[1].parse::<f64>()
        {
            Some(num / den)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::{FrameSurface, PixelFormat};

    #[test]
    fn anamorphic_square_pixels_1280x720() {
        let state = FrameState {
            size: FrameSize {
                width: 720,
                height: 480,
            },
            is_anamorphic: true,
            is_interlaced: false,
            sample_aspect_ratio: Some(String::from("32:27")),
            display_aspect_ratio: Some(String::from("16:9")),
            surface: FrameSurface::System,
            pixel_format: PixelFormat::Yuv420p,
            is_hdr: false,
        };

        let target = FrameSize {
            width: 1280,
            height: 720,
        };

        assert_eq!(target.square_pixel_size_contain(&state), target);
    }

    #[test]
    fn anamorphic_square_pixels_1920x1080() {
        let state = FrameState {
            size: FrameSize {
                width: 720,
                height: 480,
            },
            is_anamorphic: true,
            is_interlaced: false,
            sample_aspect_ratio: Some(String::from("32:27")),
            display_aspect_ratio: Some(String::from("16:9")),
            surface: FrameSurface::System,
            pixel_format: PixelFormat::Yuv420p,
            is_hdr: false,
        };

        let target = FrameSize {
            width: 1920,
            height: 1080,
        };

        assert_eq!(target.square_pixel_size_contain(&state), target);
    }

    #[test]
    fn contain_rounds_an_odd_height_up_to_even() {
        // 3840x1604 HDR source scaled to fit inside a 1280x720 channel target
        // computes a raw height of 1604 * (1280/3840) = 534.667, which rounds
        // to the odd 535. zscale (used by the HDR tonemap filter) rejects any
        // frame dimension not divisible by yuv420p's 2x chroma subsampling,
        // so an odd height here plays as black. See issue #232.
        let state = FrameState {
            size: FrameSize {
                width: 3840,
                height: 1604,
            },
            is_anamorphic: false,
            is_interlaced: false,
            sample_aspect_ratio: None,
            display_aspect_ratio: None,
            surface: FrameSurface::System,
            pixel_format: PixelFormat::Yuv420p,
            is_hdr: true,
        };

        let target = FrameSize {
            width: 1280,
            height: 720,
        };

        assert_eq!(
            target.square_pixel_size_contain(&state),
            FrameSize {
                width: 1280,
                height: 536,
            }
        );
    }

    #[test]
    fn cover_rounds_an_odd_width_up_to_even() {
        // 500x333 source covering a 640x480 target: the limiting ratio is
        // 480/333 (height), so width = 500 * 480/333 = 720.72..., which rounds
        // to the odd 721. square_pixel_size_cover has no target clamp (cover
        // intentionally overflows the box), so this exercises the same
        // round_to_even helper as `contain` but through its unclamped path.
        let state = FrameState {
            size: FrameSize {
                width: 500,
                height: 333,
            },
            is_anamorphic: false,
            is_interlaced: false,
            sample_aspect_ratio: None,
            display_aspect_ratio: None,
            surface: FrameSurface::System,
            pixel_format: PixelFormat::Yuv420p,
            is_hdr: false,
        };

        let target = FrameSize {
            width: 640,
            height: 480,
        };

        assert_eq!(
            target.square_pixel_size_cover(&state),
            FrameSize {
                width: 722,
                height: 480,
            }
        );
    }
}

use std::fmt::Formatter;
use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;
use strum::{Display, EnumString};

use crate::ArgVec;
use crate::audio_codec::AudioCodec;
use crate::audio_decoder::AudioDecoder;
use crate::audio_filter::AudioFilter;
use crate::error::FFPipelineError;
use crate::ffmpeg_info::FfmpegInfo;
use crate::filter_chain::{FilterChain, PipelineFilter};
use crate::frame_rate::FrameRate;
use crate::frame_size::FrameSize;
use crate::global_option::{GlobalOption, LogLevel};
use crate::hw_accel::{HardwareAccel, HwAccel};
use crate::input::{
    FfmpegInputArgs, InputSettings, InputSource, RawVideoInputSource, WatermarkInput,
};
use crate::output_option::OutputOption;
use crate::output_settings::{
    OutputSettings, ScalingMode, SubtitleMode, VideoFilterOptions, YadifOptions,
};
use crate::overlay_filter::{FramePoint, OverlayFilter, OverlaySource, SoftwareOverlay};
use crate::video_codec::VideoCodec;
use crate::video_decoder::VideoDecoder;
use crate::video_filter::{
    ColorChannelMixerFilter, CropFilter, DeinterlaceFilter, FadeFilter, FormatFilter, LoopFilter,
    PadFilter, ScaleFilter, SoftwareDeinterlaceFilter, SoftwareDeinterlaceOptions,
    SubtitleImageScaleFilter, SubtitlesFilter, ToneMapFilter, VideoFilter,
};

pub const KEYFRAME_INTERVAL_SECONDS: u32 = 2;
pub const SEGMENT_SECONDS: u32 = 4;

#[derive(Debug, Clone, Copy, Display, EnumString)]
#[strum(serialize_all = "lowercase")]
pub enum AudioFormat {
    Aac,
    Ac3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kbps(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hz(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display, EnumString, Serialize)]
#[strum(serialize_all = "lowercase")]
pub enum VideoFormat {
    Av1,
    H264,
    Hevc,
    Mpeg2Video,
    Vc1,
    Vp8,
    Vp9,
}

/// How fast ffmpeg is allowed to read an input.
///
/// A channel needs both speeds out of one process. It has to get segments onto
/// disk fast enough that a player joining now has something to play, and it then
/// has to slow to wall-clock speed or it would race to the end of the file. Two
/// processes cannot do this: handing over mid-item breaks the timestamp run and
/// forces an `EXT-X-DISCONTINUITY` into the playlist, which some players treat
/// as the end of the stream.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ReadRate {
    /// No limit. Live inputs arrive at their own speed and must not be throttled.
    Unthrottled,
    /// Wall-clock speed, after reading `initial_burst` at full speed to fill the
    /// segment buffer. A zero burst throttles from the first frame.
    Realtime { initial_burst: Duration },
}

impl ReadRate {
    /// The ffmpeg input arguments for this rate, in order.
    fn as_args(&self) -> ArgVec {
        match self {
            ReadRate::Unthrottled => ArgVec::new(),
            ReadRate::Realtime { initial_burst } => {
                let mut result = args!["-readrate", "1.0"];
                if !initial_burst.is_zero() {
                    result.extend(args![
                        "-readrate_initial_burst",
                        format!("{}", initial_burst.as_secs())
                    ]);
                }
                result
            }
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub struct PtsOffset {
    pub duration: Duration,
}

impl Default for PtsOffset {
    fn default() -> Self {
        PtsOffset {
            duration: Duration::ZERO,
        }
    }
}

pub(crate) struct OutputContext {
    pub(crate) media_frame_rate: FrameRate,
    pub(crate) audio_codec: AudioCodec,
    pub(crate) audio_channels: Option<u32>,
    pub(crate) video_codec: VideoCodec,
    pub(crate) pts_offset: Option<PtsOffset>,
    pub(crate) preferred_surface: FrameSurface,
    pub(crate) preferred_pixel_format: Option<PixelFormat>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
pub enum FrameSurface {
    System,
    Amf,
    Cuda,
    Qsv,
    Rkmpp,
    Vaapi,
    VideoToolbox,
    Vulkan,
    OpenCL,
}

impl FrameSurface {
    pub(crate) fn device_name(&self) -> Option<&'static str> {
        match self {
            FrameSurface::Amf => Some("amf"),
            FrameSurface::Cuda => Some("cuda"),
            FrameSurface::OpenCL => Some("opencl"),
            FrameSurface::Qsv => Some("qsv"),
            FrameSurface::Rkmpp => Some("rkmpp"),
            FrameSurface::Vaapi => Some("vaapi"),
            FrameSurface::Vulkan => Some("vulkan"),
            FrameSurface::VideoToolbox => Some("videotoolbox"),
            FrameSurface::System => None,
        }
    }
}

pub type SurfaceSet = std::collections::HashSet<FrameSurface>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra,
    Yuv420p,
    Yuv420p10le,
    Yuva420p,
    Yuva420p10le,
    Nv12,
    Nv15,
    P010le,
    P016,
}

gen_subset!(HwPixelFormat, PixelFormat, Nv12, Nv15, P010le);

impl PixelFormat {
    pub(crate) fn parse(pix_fmt: &str) -> PixelFormat {
        match pix_fmt.to_lowercase().as_str() {
            "bgra" => PixelFormat::Bgra,
            "yuv420p" => PixelFormat::Yuv420p,
            "yuv420p10le" => PixelFormat::Yuv420p10le,
            "yuva420p" => PixelFormat::Yuva420p,
            "yuva420p10le" => PixelFormat::Yuva420p10le,
            "nv12" => PixelFormat::Nv12,
            "nv15" => PixelFormat::Nv15,
            "p010le" => PixelFormat::P010le,
            _ => {
                log::warn!("assuming unknown pixel format {} is yuv420p", pix_fmt);
                PixelFormat::Yuv420p
            }
        }
    }

    pub(crate) fn bit_depth(&self) -> u8 {
        match self {
            PixelFormat::Bgra
            | PixelFormat::Yuv420p
            | PixelFormat::Yuva420p
            | PixelFormat::Nv12 => 8,
            PixelFormat::Yuv420p10le
            | PixelFormat::Yuva420p10le
            | PixelFormat::P010le
            | PixelFormat::Nv15 => 10,
            PixelFormat::P016 => 16,
        }
    }

    pub(crate) fn has_alpha(&self) -> bool {
        matches!(
            self,
            PixelFormat::Bgra | PixelFormat::Yuva420p | PixelFormat::Yuva420p10le
        )
    }

    pub(crate) fn as_arg(&self) -> &str {
        match self {
            PixelFormat::Bgra => "bgra",
            PixelFormat::Yuv420p => "yuv420p",
            PixelFormat::Yuv420p10le => "yuv420p10le",
            PixelFormat::Yuva420p => "yuva420p",
            PixelFormat::Yuva420p10le => "yuva420p10le",
            PixelFormat::Nv12 => "nv12",
            PixelFormat::Nv15 => "nv15",
            PixelFormat::P010le => "p010le",
            PixelFormat::P016 => "p016",
        }
    }
}

#[derive(Clone, Debug, derive_more::Display)]
#[display(
    "FrameState(size={},is_anamorphic={},surface={})",
    size,
    is_anamorphic,
    surface
)]
pub struct FrameState {
    pub(crate) size: FrameSize,
    pub(crate) is_anamorphic: bool,
    pub(crate) is_interlaced: bool,
    pub(crate) sample_aspect_ratio: Option<String>,
    pub(crate) display_aspect_ratio: Option<String>,
    pub(crate) surface: FrameSurface,
    pub(crate) pixel_format: PixelFormat,
    pub(crate) is_hdr: bool,
}

pub enum PipelineInput {
    Audio {
        input_source: InputSource,
        index: u32,
        path: String,
        seek: Duration,
        channels: u32,
        decoder: AudioDecoder,
    },
    Video {
        input_source: InputSource,
        index: u32,
        path: String,
        seek: Duration,
        read_rate: ReadRate,
        decoder: VideoDecoder,
    },
    Subtitle {
        input_source: InputSource,
        index: u32,
        path: String,
        seek: Duration,
    },
    Watermark {
        input: WatermarkInput,
        index: u32,
        path: String,
        extra_input_args: ArgVec,
    },
    Overlay {
        input_source: InputSource,
        path: String,
        read_rate: ReadRate,
    },
}

impl PipelineInput {
    fn sort_order(&self) -> u8 {
        match self {
            PipelineInput::Video { .. } => 0,
            PipelineInput::Audio { .. } => 1,
            PipelineInput::Subtitle { .. } => 2,
            PipelineInput::Watermark { .. } => 3,
            PipelineInput::Overlay { .. } => 4,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EnvironmentVariable {
    pub key: String,
    pub value: String,
}

pub struct Pipeline {
    ffmpeg_info: FfmpegInfo,
    accel: Option<HardwareAccel>,
    filter_options: VideoFilterOptions,
    initial_state: FrameState,

    global_options: Vec<GlobalOption>,
    inputs: Vec<PipelineInput>,
    filter_chain: FilterChain,
    output_options: Vec<OutputOption>,
    env_vars: Vec<EnvironmentVariable>,

    output_context: OutputContext,
}

impl Pipeline {
    fn full(
        ffmpeg_info: &FfmpegInfo,
        input_settings: InputSettings,
        output_settings: OutputSettings,
    ) -> Result<Pipeline, FFPipelineError> {
        let mut final_output_settings = output_settings;

        if let Some(accel) = &final_output_settings.accel
            && accel
                .known_accel()
                .map(|a| !ffmpeg_info.has_hw_accel(a))
                .unwrap_or(false)
        {
            log::warn!("ffmpeg does not support requested accel {:?}", accel);
            final_output_settings.accel = None;
        }

        let duration = std::cmp::min(
            input_settings.audio_input.out_point - input_settings.audio_input.in_point,
            input_settings.video_input.out_point - input_settings.video_input.in_point,
        );

        let audio_codec = match final_output_settings.audio.format {
            Some(AudioFormat::Aac) => AudioCodec::Aac,
            Some(AudioFormat::Ac3) => AudioCodec::Ac3,
            _ => AudioCodec::Copy,
        };

        let video_stream = input_settings.select_video_stream()?;
        let audio_stream = input_settings.select_audio_stream()?;
        let subtitle_stream = input_settings.select_subtitle_stream();
        let watermark_stream = input_settings.select_watermark_stream();

        // TODO: add target profile to config
        let video_codec = match (
            final_output_settings.accel.as_ref(),
            final_output_settings.video_format,
        ) {
            (Some(a), Some(format)) => a
                .codec_for_format(
                    &format,
                    final_output_settings.bit_depth.unwrap_or(8),
                    final_output_settings.video_size,
                )
                .filter(|_| a.can_encode(&format, final_output_settings.bit_depth.unwrap_or(8)))
                .unwrap_or(match format {
                    VideoFormat::Hevc => VideoCodec::libx265(),
                    VideoFormat::H264 => VideoCodec::libx264(),
                    _ => VideoCodec::copy(),
                }),
            (_, Some(VideoFormat::H264)) => VideoCodec::libx264(),
            (_, Some(VideoFormat::Hevc)) => VideoCodec::libx265(),
            _ => VideoCodec::copy(),
        };

        let is_still_image = input_settings.video_input.probe_result.is_still_image();
        let video_decoder = VideoDecoder::new(
            ffmpeg_info,
            video_stream,
            is_still_image,
            &final_output_settings,
        );

        let initial_state = FrameState {
            size: FrameSize {
                width: video_stream
                    .width
                    .ok_or(FFPipelineError::VideoInputIsRequired)?,
                height: video_stream
                    .height
                    .ok_or(FFPipelineError::VideoInputIsRequired)?,
            },
            is_anamorphic: video_stream.is_anamorphic(),
            // if user does not want to deinterlace, pretend content is not interlaced
            is_interlaced: final_output_settings.deinterlace && video_stream.is_interlaced(),
            sample_aspect_ratio: video_stream.sample_aspect_ratio.to_owned(),
            display_aspect_ratio: video_stream.display_aspect_ratio.to_owned(),
            surface: video_decoder.output_surface(),
            pixel_format: video_decoder
                .output_format(&PixelFormat::parse(video_stream.pix_fmt.as_str())),
            is_hdr: video_stream.color_params.is_hdr(),
        };

        let preferred_pixel_format = match final_output_settings.bit_depth {
            Some(10) => video_codec.preferred_pixel_format_10bit,
            Some(8) => video_codec.preferred_pixel_format_8bit,
            _ => None,
        };

        let output_context = OutputContext {
            audio_codec,
            audio_channels: final_output_settings.audio.channels,
            video_codec: video_codec.clone(),
            pts_offset: final_output_settings.pts_offset,
            media_frame_rate: video_stream.frame_rate.to_owned(),
            preferred_surface: video_codec.preferred_surface,
            preferred_pixel_format,
        };

        let mut filters = vec![
            PipelineFilter::Audio(AudioFilter::LoudNorm {
                settings: final_output_settings.audio.loudness.clone(),
                sample_rate: final_output_settings.audio.sample_rate,
            }),
            PipelineFilter::Audio(AudioFilter::Resample),
            PipelineFilter::Audio(AudioFilter::Pad),
        ];

        filters.extend([
            PipelineFilter::Video(LoopFilter { is_still_image }.into()),
            PipelineFilter::Video(
                DeinterlaceFilter {
                    filter: SoftwareDeinterlaceFilter::Yadif(YadifOptions::default()),
                    options: SoftwareDeinterlaceOptions {
                        bwdif: final_output_settings.filter_options.bwdif.clone(),
                        w3fdif: final_output_settings.filter_options.w3fdif.clone(),
                        yadif: final_output_settings.filter_options.yadif.clone(),
                    },
                    input_is_interlaced: initial_state.is_interlaced,
                }
                .into(),
            ),
            PipelineFilter::Video(
                ScaleFilter {
                    size: final_output_settings.video_size,
                    scaling_mode: final_output_settings.scaling_mode,
                    input_is_anamorphic: initial_state.is_anamorphic,
                    force_original_aspect_ratio: None,
                }
                .into(),
            ),
            // Tone map after the scale, never before it. Every tone mapper on
            // the list is priced per pixel, and the one the VAAPI path now
            // takes — libplacebo, on system frames, see `TonemapLibplacebo` —
            // additionally pays a download at whatever size it runs at. On a
            // 3840x1608 HDR source that is 0.461 CPU-seconds per output second
            // ahead of the scale against 0.133 behind it.
            PipelineFilter::Video(
                ToneMapFilter {
                    algorithm: final_output_settings.filter_options.tonemap.tonemap.clone(),
                    output_format: match final_output_settings.bit_depth {
                        Some(10) => PixelFormat::Yuv420p10le,
                        _ => PixelFormat::Yuv420p,
                    },
                }
                .into(),
            ),
            PipelineFilter::Video(
                PadFilter {
                    size: final_output_settings.video_size.to_owned(),
                    scaling_mode: final_output_settings.scaling_mode,
                }
                .into(),
            ),
            PipelineFilter::Video(
                CropFilter {
                    size: final_output_settings.video_size.to_owned(),
                    scaling_mode: final_output_settings.scaling_mode,
                }
                .into(),
            ),
        ]);

        let mut inputs = vec![
            PipelineInput::Audio {
                input_source: input_settings.audio_input.input_source.to_owned(),
                index: audio_stream.stream_index,
                path: input_settings.audio_input.probe_result.path.to_owned(),
                seek: input_settings.audio_input.in_point,
                channels: audio_stream.channels,
                decoder: AudioDecoder::new(audio_stream, &final_output_settings),
            },
            PipelineInput::Video {
                input_source: input_settings.video_input.input_source.to_owned(),
                index: video_stream.stream_index,
                path: input_settings.video_input.probe_result.path.to_owned(),
                seek: if is_still_image {
                    Duration::ZERO
                } else {
                    input_settings.video_input.in_point
                },
                read_rate: final_output_settings.read_rate,
                decoder: video_decoder,
            },
        ];

        if let Some(subtitle_stream) = subtitle_stream
            && let Some(subtitle_input) = input_settings.subtitle_input.as_ref()
        {
            if subtitle_stream.is_subtitle_image()
                && let Some(size) = final_output_settings.video_size
            {
                inputs.push(PipelineInput::Subtitle {
                    input_source: subtitle_input.input_source.to_owned(),
                    index: subtitle_stream.stream_index,
                    path: subtitle_input.probe_result.path.to_owned(),
                    seek: subtitle_input.in_point,
                });

                let secondary_initial_state = FrameState {
                    size,
                    is_anamorphic: subtitle_stream.is_anamorphic(),
                    is_interlaced: false,
                    sample_aspect_ratio: subtitle_stream.sample_aspect_ratio.to_owned(),
                    display_aspect_ratio: subtitle_stream.display_aspect_ratio.to_owned(),
                    surface: FrameSurface::System,
                    pixel_format: if subtitle_stream.pix_fmt.is_empty() {
                        PixelFormat::Bgra
                    } else {
                        PixelFormat::parse(&subtitle_stream.pix_fmt)
                    },
                    is_hdr: false,
                };

                filters.push(PipelineFilter::Overlay(OverlayFilter {
                    kind: SoftwareOverlay::default().into(),
                    secondary: vec![SubtitleImageScaleFilter { size }.into()],
                    secondary_initial_state,
                    secondary_source: OverlaySource::Subtitle,
                    location: None,
                }));
            } else if !subtitle_stream.is_subtitle_image()
                && final_output_settings.subtitle_mode == SubtitleMode::Burn
            {
                // only use force_style with SRT, which doesn't have any styling of its own
                let mut final_force_style = None;
                if subtitle_stream.codec == "srt" || subtitle_stream.codec == "subrip" {
                    final_force_style = final_output_settings.subtitle_force_style;
                }

                filters.push(PipelineFilter::Video(
                    SubtitlesFilter {
                        path: subtitle_input.probe_result.path.to_owned(),
                        seek: subtitle_input.in_point,
                        fonts_folder: final_output_settings.fonts_folder.to_owned(),
                        force_style: final_force_style,
                    }
                    .into(),
                ))
            }
        } else if let Some(overlay) = input_settings.overlay_input.as_ref() {
            // Live overlay (Vello fifo etc). Gated on no image-subtitle being
            // present so the two overlay paths don't collide. Reuses the same
            // OverlayFilter machinery image subtitles use.
            inputs.push(PipelineInput::Overlay {
                input_source: InputSource::RawVideo(RawVideoInputSource {
                    pixel_format: overlay.pixel_format.clone(),
                    width: overlay.width,
                    height: overlay.height,
                    framerate: overlay.framerate,
                }),
                path: overlay.fifo_path.clone(),
                // Paced independent of the main video's own read_rate (which is
                // Unthrottled for live/lavfi sources): the overlay writer only
                // ever produces frames at wall-clock speed, and pacing ffmpeg's
                // reads of this pipe to match is what keeps a whole frame sitting
                // in the pipe by the time ffmpeg reads it. Without this, ffmpeg
                // drains the pipe as fast as it can loop, outrunning the writer
                // and handing the rawvideo demuxer a partial frame — which it
                // treats as fatal rather than retrying.
                read_rate: ReadRate::Realtime {
                    initial_burst: Duration::ZERO,
                },
            });

            let secondary_initial_state = FrameState {
                size: FrameSize {
                    width: overlay.width,
                    height: overlay.height,
                },
                is_anamorphic: false,
                is_interlaced: false,
                sample_aspect_ratio: None,
                display_aspect_ratio: None,
                surface: FrameSurface::System,
                pixel_format: PixelFormat::Bgra,
                is_hdr: false,
            };

            filters.push(PipelineFilter::Overlay(OverlayFilter {
                kind: SoftwareOverlay::default().into(),
                secondary: vec![
                    ScaleFilter {
                        size: final_output_settings.video_size,
                        scaling_mode: ScalingMode::ScaleAndPad,
                        input_is_anamorphic: false,
                        force_original_aspect_ratio: None,
                    }
                    .into(),
                ],
                secondary_initial_state,
                // The live overlay reuses the subtitle label/secondary input slot,
                // so the filter draws from `subtitle_label` (set in the Overlay
                // pipeline-input arm). Honor the OverlaySpec's top-left x/y; the
                // FramePoint is u32 so negative offsets clamp to 0 (on-screen).
                secondary_source: OverlaySource::Subtitle,
                location: Some(FramePoint {
                    x: overlay.x.max(0) as u32,
                    y: overlay.y.max(0) as u32,
                }),
            }));
        }

        if let Some(watermark_stream) = watermark_stream
            && let Some(watermark_input) = input_settings.watermark_input.as_ref()
            && let Some(height) = watermark_stream.height
            && let Some(width) = watermark_stream.width
        {
            let extra_input_args = if watermark_stream.is_still_image() {
                args![
                    "-loop",
                    "1",
                    "-framerate",
                    output_context.media_frame_rate.r_frame_rate.clone(),
                    "-t",
                    format!("{}ms", duration.as_millis())
                ]
            } else if watermark_stream.codec == "gif" || watermark_stream.codec == "apng" {
                args![
                    "-ignore_loop",
                    "0",
                    "-t",
                    format!("{}ms", duration.as_millis())
                ]
            } else {
                args![
                    "-stream_loop",
                    "-1",
                    "-t",
                    format!("{}ms", duration.as_millis())
                ]
            };

            inputs.push(PipelineInput::Watermark {
                input: watermark_input.clone(),
                index: watermark_stream.stream_index,
                path: watermark_input.probe_result.path.to_owned(),
                extra_input_args,
            });

            let secondary_initial_state = FrameState {
                size: FrameSize { width, height },
                is_anamorphic: false,
                is_interlaced: false,
                sample_aspect_ratio: Some(String::from("1:1")),
                display_aspect_ratio: None,
                surface: FrameSurface::System,
                pixel_format: if watermark_stream.pix_fmt.is_empty() {
                    PixelFormat::Bgra
                } else {
                    PixelFormat::parse(&watermark_stream.pix_fmt)
                },
                is_hdr: false,
            };

            let video_size = final_output_settings
                .video_size
                .as_ref()
                .unwrap_or(&initial_state.size);

            let source_content_size = match final_output_settings.scaling_mode {
                ScalingMode::ScaleAndPad => video_size.square_pixel_size_contain(&initial_state),
                ScalingMode::Crop | ScalingMode::Stretch => *video_size,
            };

            let scaled_size = watermark_input.scaled_size(
                FrameSize { width, height },
                final_output_settings.video_size,
            );

            let location = Some(watermark_input.frame_location(
                &source_content_size,
                &scaled_size,
                video_size,
            ));

            let mut secondary_filters: Vec<VideoFilter> = vec![
                ColorChannelMixerFilter {
                    alpha: watermark_input.opacity_percent.unwrap_or(100f32) / 100.0f32,
                }
                .into(),
                FormatFilter {
                    format: match secondary_initial_state.pixel_format.bit_depth() {
                        10 => PixelFormat::Yuva420p10le,
                        _ => PixelFormat::Yuva420p,
                    },
                }
                .into(),
                ScaleFilter {
                    size: Some(scaled_size),
                    scaling_mode: ScalingMode::ScaleAndPad,
                    input_is_anamorphic: false,
                    force_original_aspect_ratio: None,
                }
                .into(),
            ];

            let fade_filters = FadeFilter::for_watermark(
                watermark_input.timing.as_ref(),
                input_settings.start,
                input_settings.video_input.in_point,
                input_settings.video_input.out_point,
            );

            secondary_filters.extend(fade_filters.iter().map(|f| f.clone().into()));

            filters.push(PipelineFilter::Overlay(OverlayFilter {
                kind: SoftwareOverlay::default().into(),
                secondary: secondary_filters,
                secondary_initial_state,
                secondary_source: OverlaySource::Watermark,
                location,
            }));
        }

        let mut env_vars = Vec::new();

        if let Some(reports_folder) = final_output_settings
            .reports_folder
            .as_deref()
            .filter(|s| !s.is_empty())
            && let Some(report_id) = final_output_settings
                .report_id
                .as_deref()
                .filter(|s| !s.is_empty())
        {
            let folder = PathBuf::from(reports_folder);
            if let Err(err) = std::fs::create_dir_all(&folder) {
                log::warn!("failed to create ffmpeg reports folder: {err}; will not save report");
            } else {
                let file = folder
                    .join(format!(".in-flight-{}.log", report_id))
                    .to_string_lossy()
                    .to_string()
                    .replace(r"%", r"%%");

                #[cfg(target_os = "windows")]
                let mut file = file;

                #[cfg(target_os = "windows")]
                {
                    file = file.replace(r"\", r"/").replace(r":/", r"\:/");
                }

                env_vars = vec![EnvironmentVariable {
                    key: String::from("FFREPORT"),
                    value: format!("file={file}:level=32"),
                }]
            }
        }

        Ok(Pipeline {
            ffmpeg_info: ffmpeg_info.clone(),
            accel: final_output_settings.accel.clone(),
            filter_options: final_output_settings.filter_options,
            initial_state: initial_state.clone(),
            global_options: vec![
                // hardware accel should use a single thread
                GlobalOption::Threads(match &final_output_settings.accel {
                    Some(_) => 1,
                    _ => 0,
                }),
                GlobalOption::NoStdIn,
                GlobalOption::HideBanner,
                GlobalOption::LogLevel(LogLevel::Error),
                GlobalOption::StandardFormatFlags,
            ],
            inputs,
            filter_chain: FilterChain::new(filters),
            output_options: vec![
                OutputOption::NoDemuxDecodeDelay,
                OutputOption::MovFlagsFastStart,
                OutputOption::CudaNoAutoScale,
                OutputOption::AudioCodec(audio_codec),
                OutputOption::AudioBitrate(final_output_settings.audio.bitrate),
                OutputOption::AudioBuffer(final_output_settings.audio.buffer),
                OutputOption::AudioChannels(final_output_settings.audio.channels),
                OutputOption::AudioSampleRate(final_output_settings.audio.sample_rate),
                OutputOption::VideoCodec(video_codec),
                OutputOption::VideoBitrate(final_output_settings.video_bitrate),
                OutputOption::VideoBuffer(final_output_settings.video_buffer),
                OutputOption::DoNotMapMetadata,
                OutputOption::Duration(duration),
                OutputOption::TsOffset(final_output_settings.pts_offset),
                OutputOption::VideoTrackTimeScale(90_000),
                OutputOption::FrameRate(final_output_settings.frame_rate.clone()),
                OutputOption::Format(final_output_settings.format),
            ],
            output_context,
            env_vars,
        })
    }

    pub fn optimize(&mut self) {
        // audio copy shouldn't have bitrate etc
        if self.output_context.audio_codec == AudioCodec::Copy {
            self.output_options.retain(|o| {
                !matches!(
                    o,
                    OutputOption::AudioBitrate(_)
                        | OutputOption::AudioBuffer(_)
                        | OutputOption::AudioChannels(_)
                        | OutputOption::AudioSampleRate(_)
                )
            });

            self.filter_chain.disable_audio();
        };

        // remove audio channels output option if input channel count matches
        if let Some(audio_channels) = self.inputs.iter().find_map(|s| match s {
            PipelineInput::Audio { channels, .. } => Some(channels),
            _ => None,
        }) && Some(audio_channels) == self.output_context.audio_channels.as_ref()
        {
            self.output_options
                .retain(|o| !matches!(o, OutputOption::AudioChannels(_)));
        }

        // video copy shouldn't have bitrate, etc
        if self.output_context.video_codec.codec_name == VideoCodec::COPY {
            self.output_options.retain(|o| {
                !matches!(
                    o,
                    OutputOption::VideoBitrate(_) | OutputOption::VideoBuffer(_)
                )
            });

            self.filter_chain.disable_video();
        }

        self.filter_chain
            .evaluate(&self.initial_state, &self.ffmpeg_info);
        self.filter_chain.resolve(
            &self.ffmpeg_info,
            &self.accel,
            &self.filter_options,
            &self.initial_state,
            &self.output_context.preferred_surface,
            &self.output_context.preferred_pixel_format,
        );

        // prepend decoder filters;
        // this is a special case that's only really needed for CUDA's hwupload workaround
        if let Some(video_decoder) = self.inputs.iter().find_map(|s| match s {
            PipelineInput::Video { decoder, .. } => Some(decoder),
            _ => None,
        }) {
            self.filter_chain.prepend(video_decoder.filters());
        }

        self.filter_chain.optimize();

        if let Some(accel) = &self.accel {
            let mut surfaces = self.filter_chain.surfaces().clone();
            surfaces.insert(self.initial_state.surface);
            surfaces.insert(self.output_context.preferred_surface);
            if surfaces.iter().any(|s| *s != FrameSurface::System) {
                let args = accel.init_hw_device(&surfaces);
                self.global_options.push(GlobalOption::InitHwDevice(args));
            }
        }
    }

    pub fn args(&self) -> ArgVec {
        let mut result: ArgVec = Vec::new();

        let mut audio_label = String::from("0:a");
        let mut video_label = String::from("0:v");
        let mut subtitle_label = None;
        let mut watermark_label = None;

        let mut distinct_paths: Vec<&str> = Vec::new();

        let mut sorted_inputs: Vec<&PipelineInput> = self.inputs.iter().collect();
        sorted_inputs.sort_by_key(|i| i.sort_order());

        result.extend(self.global_options.iter().flat_map(|o| o.as_arg()));

        for input in sorted_inputs.iter() {
            match input {
                PipelineInput::Video {
                    input_source,
                    index,
                    path,
                    seek,
                    read_rate,
                    decoder,
                    ..
                } => {
                    distinct_paths.push(path.as_str());

                    result.extend(decoder.as_arg());

                    let video_input_index =
                        distinct_paths.iter().position(|p| p == path).unwrap_or(0);
                    video_label = format!("{}:{}", video_input_index, index);

                    if !seek.is_zero() {
                        result.extend(args!["-ss", format!("{}ms", seek.as_millis())]);
                    }

                    result.extend(read_rate.as_args());

                    result.extend(input_source.args_for_input());
                    // TODO: if audio has same input and args, should use here

                    result.extend(args!["-i", path.to_owned()]);
                }
                PipelineInput::Audio {
                    input_source,
                    index,
                    path,
                    decoder,
                    ..
                } => {
                    // if we haven't yet used this input, add it
                    if !distinct_paths.contains(&path.as_str()) {
                        distinct_paths.push(path.as_str());

                        result.extend(decoder.as_arg());

                        // TODO: seek?

                        result.extend(input_source.args_for_input());
                        result.extend(args!["-i", path.to_owned()]);
                    }

                    let audio_input_index =
                        distinct_paths.iter().position(|p| p == path).unwrap_or(0);
                    audio_label = format!("{}:{}", audio_input_index, index);
                }
                PipelineInput::Subtitle {
                    input_source,
                    index,
                    path,
                    seek,
                    ..
                } => {
                    if !distinct_paths.contains(&path.as_str()) {
                        distinct_paths.push(path.as_str());

                        if !seek.is_zero() {
                            result.extend(args!["-ss", format!("{}ms", seek.as_millis())]);
                        }

                        result.extend(input_source.args_for_input());
                        result.extend(args!["-i", path.to_owned()]);
                    }

                    let subtitle_input_index =
                        distinct_paths.iter().position(|p| p == path).unwrap_or(0);
                    subtitle_label = Some(format!("{}:{}", subtitle_input_index, index));
                }
                PipelineInput::Watermark {
                    input,
                    index,
                    path,
                    extra_input_args,
                } => {
                    if !distinct_paths.contains(&path.as_str()) {
                        distinct_paths.push(path.as_str());

                        result.extend(input.input_source.args_for_input());
                        result.extend(extra_input_args.clone());
                        result.extend(args!["-i", path.to_owned()]);
                    }

                    let watermark_input_index =
                        distinct_paths.iter().position(|p| p == path).unwrap_or(0);
                    watermark_label = Some(format!("{}:{}", watermark_input_index, index))
                }
                PipelineInput::Overlay {
                    input_source,
                    path,
                    read_rate,
                } => {
                    if !distinct_paths.contains(&path.as_str()) {
                        distinct_paths.push(path.as_str());
                        result.extend(read_rate.as_args());
                        result.extend(input_source.args_for_input());
                        result.extend(args!["-i", path.to_owned()]);
                    }

                    let overlay_input_index =
                        distinct_paths.iter().position(|p| p == path).unwrap_or(0);
                    // The overlay filter machinery uses `subtitle_label` for
                    // whichever secondary input the OverlayFilter draws from.
                    // For now overlay and image-subtitle are mutually exclusive
                    // (gated at the channel session layer); when overlay is in
                    // use we hand its label here.
                    if subtitle_label.is_none() {
                        subtitle_label = Some(format!("{}:0", overlay_input_index));
                    }
                }
            }
        }

        let mut filter_chain = self.filter_chain.to_owned();
        filter_chain.build(
            &audio_label,
            &video_label,
            subtitle_label.as_ref(),
            watermark_label.as_ref(),
        );

        result.extend(filter_chain.as_arg());

        result.extend(args!["-map", filter_chain.video_label().to_owned()]);
        result.extend(args!["-map", filter_chain.audio_label().to_owned()]);

        result.extend(
            self.output_options
                .iter()
                .flat_map(|o| o.as_arg(&self.output_context)),
        );

        result
    }

    pub fn envs(&self) -> Vec<EnvironmentVariable> {
        let mut result = self.env_vars.clone();

        if let Some(a) = &self.accel {
            result.extend(a.envs())
        }

        result
    }
}

impl std::fmt::Display for Pipeline {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "args: {}", self.args().join(" "))
    }
}

pub fn generate_pipeline(
    ffmpeg_info: &FfmpegInfo,
    input_settings: InputSettings,
    output_settings: OutputSettings,
) -> Result<Pipeline, FFPipelineError> {
    Pipeline::full(ffmpeg_info, input_settings, output_settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_name_returns_correct_ffmpeg_device_strings() {
        assert_eq!(FrameSurface::Cuda.device_name(), Some("cuda"));
        assert_eq!(FrameSurface::OpenCL.device_name(), Some("opencl"));
        assert_eq!(FrameSurface::Qsv.device_name(), Some("qsv"));
        assert_eq!(FrameSurface::Vaapi.device_name(), Some("vaapi"));
        assert_eq!(FrameSurface::Vulkan.device_name(), Some("vulkan"));
        assert_eq!(
            FrameSurface::VideoToolbox.device_name(),
            Some("videotoolbox")
        );
        assert_eq!(FrameSurface::System.device_name(), None);
    }

    /// A channel has to fill its segment buffer fast and then hold wall-clock
    /// speed, and it has to do both in one process. Splitting it across two used
    /// to put an `EXT-X-DISCONTINUITY` in the middle of an episode, which stopped
    /// the picture dead on players that treat the tag as the end of the stream.
    #[test]
    fn realtime_read_rate_bursts_then_throttles_in_one_process() {
        let args = ReadRate::Realtime {
            initial_burst: Duration::from_secs(44),
        }
        .as_args();

        assert_eq!(
            args,
            vec!["-readrate", "1.0", "-readrate_initial_burst", "44"]
        );
    }

    #[test]
    fn a_full_buffer_needs_no_burst() {
        let args = ReadRate::Realtime {
            initial_burst: Duration::ZERO,
        }
        .as_args();

        assert_eq!(args, vec!["-readrate", "1.0"]);
    }

    #[test]
    fn live_input_is_never_throttled() {
        assert!(ReadRate::Unthrottled.as_args().is_empty());
    }

    /// Convert mode turns text subtitles into WebVTT, which cannot hold a
    /// picture. A Blu-ray/DVD rip whose only subtitle track is PGS or VobSub
    /// must still show up on screen, the same way Burn mode already paints
    /// picture subtitles onto the video. Regression test for the bug where
    /// `subtitle_source` stayed `None` and the item played with no subtitles
    /// at all — this proves the pipeline still builds the overlay filter (and
    /// still feeds ffmpeg the subtitle stream as an input) for a picture
    /// subtitle even when `subtitle_mode` is `Convert`, not just `Burn`.
    #[test]
    fn convert_mode_paints_picture_subtitles_onto_the_video() {
        use time::OffsetDateTime;

        use crate::frame_rate::FrameRate;
        use crate::frame_size::FrameSize;
        use crate::input::{InputSource, LocalInputSource, ProbedInput};
        use crate::output_format::OutputFormat;
        use crate::output_settings::{AudioOutputSettings, ScalingMode};
        use crate::probe::{
            CodecType, ProbeResult, ProbeResultAudioStream, ProbeResultColorParams,
            ProbeResultStream, ProbeResultVideoStream,
        };

        let path = String::from("/media/bluray-rip.mkv");
        let source = InputSource::Local(LocalInputSource { path: path.clone() });

        let video_stream = ProbeResultStream::Video(Box::new(ProbeResultVideoStream {
            stream_index: 0,
            codec: String::from("h264"),
            codec_type: CodecType::Video,
            profile: String::new(),
            height: Some(1080),
            width: Some(1920),
            frame_rate: FrameRate::default(),
            sample_aspect_ratio: Some(String::from("1:1")),
            display_aspect_ratio: None,
            pix_fmt: String::from("yuv420p"),
            color_params: ProbeResultColorParams {
                color_range: None,
                color_space: None,
                color_transfer: None,
                color_primaries: None,
            },
            field_order: None,
            language: None,
        }));
        let audio_stream = ProbeResultStream::Audio(ProbeResultAudioStream {
            stream_index: 1,
            codec: String::from("aac"),
            channels: 2,
        });
        // A PGS track: the picture-based subtitle format Blu-ray rips carry.
        let subtitle_stream = ProbeResultStream::Video(Box::new(ProbeResultVideoStream {
            stream_index: 2,
            codec: String::from("hdmv_pgs_subtitle"),
            codec_type: CodecType::Subtitle,
            profile: String::new(),
            height: None,
            width: None,
            frame_rate: FrameRate::default(),
            sample_aspect_ratio: None,
            display_aspect_ratio: None,
            pix_fmt: String::new(),
            color_params: ProbeResultColorParams {
                color_range: None,
                color_space: None,
                color_transfer: None,
                color_primaries: None,
            },
            field_order: None,
            language: None,
        }));

        let probe_result = ProbeResult {
            path: path.clone(),
            streams: vec![video_stream, audio_stream, subtitle_stream],
            duration: None,
            format_name: None,
        };

        let probed = |stream_index: Option<u32>| ProbedInput {
            input_source: source.clone(),
            probe_result: probe_result.clone(),
            in_point: Duration::ZERO,
            out_point: Duration::from_secs(3600),
            stream_index,
        };

        let input_settings = InputSettings {
            start: OffsetDateTime::UNIX_EPOCH,
            audio_input: probed(None),
            video_input: probed(None),
            subtitle_input: Some(probed(None)),
            watermark_input: None,
            overlay_input: None,
            subtitle_language_tag: String::from("en"),
        };

        let output_settings = OutputSettings {
            audio: AudioOutputSettings {
                format: Some(AudioFormat::Aac),
                bitrate: Some(Kbps(192)),
                buffer: Some(Kbps(384)),
                channels: Some(2),
                sample_rate: Some(Hz(48000)),
                loudness: None,
            },
            video_format: Some(VideoFormat::H264),
            bit_depth: Some(8),
            video_bitrate: Some(Kbps(5000)),
            video_buffer: Some(Kbps(10000)),
            video_size: Some(FrameSize {
                width: 1920,
                height: 1080,
            }),
            scaling_mode: ScalingMode::ScaleAndPad,
            filter_options: VideoFilterOptions::default(),
            deinterlace: false,
            accel: None,
            format: OutputFormat::Hls {
                playlist: String::from("/tmp/live.m3u8"),
                segment_template: String::from("/tmp/segment_%03d.ts"),
                troubleshoot: false,
            },
            pts_offset: None,
            read_rate: ReadRate::Unthrottled,
            frame_rate: None,
            // The channel is set to Convert — not Burn — and what this test
            // pins is that a picture subtitle still gets drawn onto the
            // video anyway, because WebVTT (Convert's normal output) cannot
            // represent a picture.
            subtitle_mode: SubtitleMode::Convert,
            fonts_folder: None,
            subtitle_force_style: None,
            reports_folder: None,
            report_id: None,
        };

        let ffmpeg_info = FfmpegInfo::default();
        let mut pipeline = Pipeline::full(&ffmpeg_info, input_settings, output_settings)
            .expect("pipeline should build for a picture subtitle in convert mode");
        pipeline.optimize();
        let args = pipeline.args().join(" ");

        // The subtitle stream must be fed to ffmpeg as an input...
        assert!(
            args.contains(&path),
            "expected the subtitle-bearing file to be an ffmpeg input; args: {args}"
        );
        // ...and painted onto the video via the overlay filter, exactly like
        // Burn mode does — proving Convert mode no longer drops it silently.
        assert!(
            args.contains("overlay"),
            "expected an overlay filter drawing the picture subtitle onto the video; args: {args}"
        );
    }

    /// Regression test for the black/silence fallback dying with
    /// `[rawvideo] Invalid buffer size, packet size N < expected frame_size M`.
    ///
    /// The fallback's main video track is `-f lavfi color=black`, which has no
    /// file behind it for `-readrate` to pace — see [`ReadRate`]'s docs. That
    /// left ffmpeg free to drain the live overlay rawvideo pipe as fast as it
    /// could loop, handing the demuxer short reads whenever it outran the
    /// overlay writer. Pins that the overlay input gets its own `-readrate`,
    /// independent of the main video's, so it stays paced even when nothing
    /// else in the graph is.
    #[test]
    fn overlay_pipe_is_read_rate_limited_even_when_main_video_is_unthrottled() {
        use time::OffsetDateTime;

        use crate::frame_rate::FrameRate;
        use crate::frame_size::FrameSize;
        use crate::input::{InputSource, LavfiInputSource, OverlayInput, ProbedInput};
        use crate::output_format::OutputFormat;
        use crate::output_settings::{AudioOutputSettings, ScalingMode};
        use crate::probe::{
            CodecType, ProbeResult, ProbeResultAudioStream, ProbeResultColorParams,
            ProbeResultStream, ProbeResultVideoStream,
        };

        let path = String::from("color=c=black:s=1280x720");
        let source = InputSource::Lavfi(LavfiInputSource {
            params: path.clone(),
        });

        let video_stream = ProbeResultStream::Video(Box::new(ProbeResultVideoStream {
            stream_index: 0,
            codec: String::from("rawvideo"),
            codec_type: CodecType::Video,
            profile: String::new(),
            height: Some(720),
            width: Some(1280),
            frame_rate: FrameRate::default(),
            sample_aspect_ratio: Some(String::from("1:1")),
            display_aspect_ratio: None,
            pix_fmt: String::from("yuv420p"),
            color_params: ProbeResultColorParams {
                color_range: None,
                color_space: None,
                color_transfer: None,
                color_primaries: None,
            },
            field_order: None,
            language: None,
        }));
        let audio_stream = ProbeResultStream::Audio(ProbeResultAudioStream {
            stream_index: 0,
            codec: String::from("pcm_s16le"),
            channels: 2,
        });

        let probe_result = ProbeResult {
            path: path.clone(),
            streams: vec![video_stream, audio_stream],
            duration: None,
            format_name: None,
        };

        let probed = |stream_index: Option<u32>| ProbedInput {
            input_source: source.clone(),
            probe_result: probe_result.clone(),
            in_point: Duration::ZERO,
            out_point: Duration::from_secs(60),
            stream_index,
        };

        let input_settings = InputSettings {
            start: OffsetDateTime::UNIX_EPOCH,
            audio_input: probed(None),
            video_input: probed(None),
            subtitle_input: None,
            watermark_input: None,
            overlay_input: Some(OverlayInput {
                fifo_path: String::from("pipe:10"),
                pixel_format: String::from("rgba"),
                width: 1280,
                height: 720,
                framerate: 30,
                x: 0,
                y: 0,
            }),
            subtitle_language_tag: String::from("en"),
        };

        let output_settings = OutputSettings {
            audio: AudioOutputSettings {
                format: Some(AudioFormat::Aac),
                bitrate: Some(Kbps(192)),
                buffer: Some(Kbps(384)),
                channels: Some(2),
                sample_rate: Some(Hz(48000)),
                loudness: None,
            },
            video_format: Some(VideoFormat::H264),
            bit_depth: Some(8),
            video_bitrate: Some(Kbps(5000)),
            video_buffer: Some(Kbps(10000)),
            video_size: Some(FrameSize {
                width: 1280,
                height: 720,
            }),
            scaling_mode: ScalingMode::ScaleAndPad,
            filter_options: VideoFilterOptions::default(),
            deinterlace: false,
            accel: None,
            format: OutputFormat::Hls {
                playlist: String::from("/tmp/live.m3u8"),
                segment_template: String::from("/tmp/segment_%03d.ts"),
                troubleshoot: false,
            },
            pts_offset: None,
            // Mirrors the black/silence fallback: a lavfi source has no file
            // for `-readrate` to pace, so ffmpeg reads it unthrottled.
            read_rate: ReadRate::Unthrottled,
            frame_rate: None,
            subtitle_mode: SubtitleMode::Burn,
            fonts_folder: None,
            subtitle_force_style: None,
            reports_folder: None,
            report_id: None,
        };

        let ffmpeg_info = FfmpegInfo::default();
        let mut pipeline = Pipeline::full(&ffmpeg_info, input_settings, output_settings)
            .expect("pipeline should build for a lavfi source with a live overlay");
        pipeline.optimize();
        let args = pipeline.args();

        let pipe_index = args
            .iter()
            .position(|a| a == "pipe:10")
            .expect("overlay input path pipe:10 should appear in the ffmpeg args");
        let readrate_immediately_precedes_pipe = args.get(..pipe_index).is_some_and(|before| {
            before
                .windows(2)
                .rev()
                .take(10)
                .any(|pair| pair == ["-readrate", "1.0"])
        });
        assert!(
            readrate_immediately_precedes_pipe,
            "expected -readrate 1.0 in the overlay input's own arg block, right before pipe:10; args: {args:?}"
        );
    }
}

//! Regression test for the live-overlay fifo input.
//!
//! When the channel session passes an `OverlayInput` to `InputSettings`, the
//! generated ffmpeg arg list should contain a rawvideo input pointing at the
//! fifo path with the requested pixel format / video_size / framerate, plus
//! the existing overlay filter (with `eof_action=pass` so a dying overlay
//! process doesn't take the channel down).
//!
//! This test does not actually invoke ffmpeg — it just inspects the args.

use std::time::Duration;

use ffpipeline::ffmpeg_info::FfmpegInfo;
use ffpipeline::frame_size::FrameSize;
use ffpipeline::input::OverlayInput;
use ffpipeline::pipeline::generate_pipeline;

mod common;

#[tokio::test]
async fn overlay_spec_adds_rawvideo_input_and_filter() {
    let Some(env) = common::test_env().await else {
        eprintln!("skipping: ffmpeg/ffprobe not on PATH");
        return;
    };

    let source = common::fixture_path("480p_h264.ts");
    let probe = common::probe_file(&env.ffmpeg, &env.ffprobe, &source).await;

    let mut input = common::build_input(&source, probe, Duration::from_secs(2), None);
    input.overlay_input = Some(OverlayInput {
        fifo_path: String::from("/tmp/etv-overlay-test.fifo"),
        pixel_format: String::from("rgba"),
        width: 320,
        height: 240,
        framerate: 25,
        x: 0,
        y: 0,
    });

    let dir = tempfile::tempdir().unwrap();
    let output = common::build_output(
        dir.path(),
        common::TestOutputParams {
            video_size: Some(FrameSize {
                width: 320,
                height: 240,
            }),
            ..Default::default()
        },
    );

    // Skip the FfmpegInfo from disk and use a defaulted one — we only care
    // about the arg list, not the encoder selection.
    let ffmpeg_info = FfmpegInfo::default();
    let mut pipeline = generate_pipeline(&ffmpeg_info, input, output).unwrap();
    pipeline.optimize();
    let args: Vec<String> = pipeline
        .args()
        .into_iter()
        .map(|c| c.into_owned())
        .collect();
    let joined = args.join(" ");

    assert!(
        joined.contains("-f rawvideo"),
        "expected rawvideo input flag, got: {joined}"
    );
    assert!(
        joined.contains("-pixel_format rgba"),
        "expected rgba pixel format, got: {joined}"
    );
    assert!(
        joined.contains("-video_size 320x240"),
        "expected video_size 320x240, got: {joined}"
    );
    assert!(
        joined.contains("-framerate 25"),
        "expected framerate 25, got: {joined}"
    );
    assert!(
        joined.contains("/tmp/etv-overlay-test.fifo"),
        "expected fifo path in -i arg, got: {joined}"
    );
    assert!(
        joined.contains("eof_action=pass"),
        "expected eof_action=pass on overlay filter, got: {joined}"
    );
}

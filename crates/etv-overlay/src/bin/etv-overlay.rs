use std::io::BufWriter;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use clap::{Parser, Subcommand};
use etv_overlay::fifo_writer::{FifoWriter, default_fifo_path};
use etv_overlay::overlay_spec::OverlaySpec;
use etv_overlay::rhai_engine::{OverlayState, RhaiEngine};
use etv_overlay::vello_renderer::VelloRenderer;

#[derive(Parser)]
#[command(name = "etv-overlay")]
#[command(about = "Vello+Rhai overlay renderer for Velo phase B spike")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Render a single overlay frame to a PNG file (no ffmpeg)
    RenderStill {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 0.0)]
        time: f64,
    },
    /// Pipe overlay frames through ffmpeg and produce a muxed mp4
    Run {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        fifo: Option<PathBuf>,
        #[arg(long)]
        ffmpeg: Option<PathBuf>,
        #[arg(long, default_value_t = false)]
        keep_fifo: bool,
    },
    /// Render frames directly to a fifo, blocking until the reader disconnects
    Pipe {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        fifo: PathBuf,
        #[arg(long, default_value_t = false)]
        create_fifo: bool,
        /// Optional path to touch after the first frame has been rendered and
        /// written to the fifo. The supervisor uses this to detect that the
        /// renderer is past cold-start (wgpu init, vello pipeline build,
        /// image cache miss) so callers can avoid sampling torn frames.
        #[arg(long)]
        ready_file: Option<PathBuf>,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("etv_overlay=info,warn")),
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::RenderStill {
            config,
            output,
            time,
        } => render_still(config, output, time),
        Cmd::Run {
            input,
            config,
            output,
            fifo,
            ffmpeg,
            keep_fifo,
        } => run_with_ffmpeg(input, config, output, fifo, ffmpeg, keep_fifo),
        Cmd::Pipe {
            config,
            fifo,
            create_fifo,
            ready_file,
        } => pipe_to_fifo(config, fifo, create_fifo, ready_file),
    }
}

fn render_still(config: PathBuf, output: PathBuf, time: f64) -> anyhow::Result<()> {
    let spec = OverlaySpec::from_path(&config)?;
    let mut renderer = VelloRenderer::new(spec.width, spec.height, spec.pixel_format)?;
    let state = evaluate_state(&spec, time, 0)?;
    let frame = renderer.render_frame(&state)?;
    write_png(&output, spec.width, spec.height, &frame)?;
    tracing::info!(path = %output.display(), "wrote still frame");
    Ok(())
}

fn run_with_ffmpeg(
    input: PathBuf,
    config: PathBuf,
    output: PathBuf,
    fifo: Option<PathBuf>,
    ffmpeg_bin: Option<PathBuf>,
    keep_fifo: bool,
) -> anyhow::Result<()> {
    let spec = OverlaySpec::from_path(&config)?;
    let fifo_path = fifo.unwrap_or_else(|| default_fifo_path("run"));
    let mut fifo = FifoWriter::create(fifo_path.clone())?;

    let ffmpeg = ffmpeg_bin.unwrap_or_else(|| PathBuf::from("ffmpeg"));
    let filter = "[0:v][1:v]overlay=x=0:y=0:eof_action=pass:format=auto[v]";

    let mut child = Command::new(&ffmpeg)
        .args(["-hide_banner", "-loglevel", "warning", "-y", "-i"])
        .arg(&input)
        .args([
            "-f",
            "rawvideo",
            "-pixel_format",
            spec.pixel_format.ffmpeg_arg(),
            "-video_size",
        ])
        .arg(format!("{}x{}", spec.width, spec.height))
        .args(["-framerate"])
        .arg(spec.framerate.to_string())
        .arg("-i")
        .arg(fifo.path())
        .args([
            "-filter_complex",
            filter,
            "-map",
            "[v]",
            "-map",
            "0:a?",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-preset",
            "veryfast",
            "-c:a",
            "copy",
            "-shortest",
        ])
        .arg(&output)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn ffmpeg: {e}"))?;

    fifo.open_for_writing()?;
    let mut renderer = VelloRenderer::new(spec.width, spec.height, spec.pixel_format)?;
    let engine = build_engine(&spec)?;

    let frame_period = Duration::from_secs_f64(1.0 / spec.framerate.max(1) as f64);
    let mut frame_index: u64 = 0;
    let mut next_tick = std::time::Instant::now();

    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("waitpid ffmpeg: {e}");
                break;
            }
        }

        let time_seconds = frame_index as f64 / spec.framerate.max(1) as f64;
        let state = engine.evaluate(time_seconds, frame_index);
        let frame = renderer.render_frame(&state)?;

        if let Err(e) = fifo.write_frame(&frame) {
            if matches!(e.kind(), std::io::ErrorKind::BrokenPipe) {
                tracing::info!("ffmpeg closed pipe, stopping");
                break;
            }
            tracing::warn!("write frame: {e}");
            break;
        }

        frame_index += 1;
        next_tick += frame_period;
        let now = std::time::Instant::now();
        if next_tick > now {
            thread::sleep(next_tick - now);
        }
    }

    drop(fifo);
    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!("ffmpeg exited with status {status}");
    }

    if keep_fifo {
        tracing::info!("kept fifo at {}", fifo_path.display());
    }
    tracing::info!(frames = frame_index, output = %output.display(), "run complete");
    Ok(())
}

fn pipe_to_fifo(
    config: PathBuf,
    fifo_path: PathBuf,
    create_fifo: bool,
    ready_file: Option<PathBuf>,
) -> anyhow::Result<()> {
    let spec = OverlaySpec::from_path(&config)?;
    let mut fifo = if create_fifo {
        FifoWriter::create(fifo_path.clone())?
    } else {
        FifoWriter::attach(fifo_path.clone())
    };

    // Warm the renderer fully BEFORE opening the fifo. wgpu adapter init,
    // vello shader compile, and the first image-cache decode all happen in
    // the first render_frame call. Doing that work while the fifo is closed
    // guarantees ffmpeg can't read a partial frame during cold-start: nothing
    // hits the pipe until we have a complete RGBA buffer ready to write.
    // See https://github.com/McBrideMusings/etv-station/issues/54.
    let mut renderer = VelloRenderer::new(spec.width, spec.height, spec.pixel_format)?;
    let engine = build_engine(&spec)?;
    let start = std::time::Instant::now();
    let first_frame = renderer.render_frame(&engine.evaluate(0.0, 0))?;
    tracing::info!(
        warmup_ms = start.elapsed().as_millis() as u64,
        "renderer warm; opening fifo for first write",
    );

    fifo.open_for_writing()?;
    if let Err(e) = fifo.write_frame(&first_frame) {
        if matches!(e.kind(), std::io::ErrorKind::BrokenPipe) {
            tracing::info!("reader disconnected before first frame");
            return Ok(());
        }
        return Err(e.into());
    }
    if let Some(path) = ready_file.as_deref()
        && let Err(e) = touch(path)
    {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "failed to create overlay ready file; continuing",
        );
    }

    let frame_period = Duration::from_secs_f64(1.0 / spec.framerate.max(1) as f64);
    let mut frame_index: u64 = 1;
    loop {
        let time_seconds = start.elapsed().as_secs_f64();
        let state = engine.evaluate(time_seconds, frame_index);
        let frame = renderer.render_frame(&state)?;
        if let Err(e) = fifo.write_frame(&frame) {
            if matches!(e.kind(), std::io::ErrorKind::BrokenPipe) {
                tracing::info!("reader disconnected");
                return Ok(());
            }
            return Err(e.into());
        }
        frame_index += 1;
        let target = start + frame_period * frame_index as u32;
        let now = std::time::Instant::now();
        if target > now {
            thread::sleep(target - now);
        }
    }
}

fn touch(path: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::File::create(path)?;
    Ok(())
}

fn build_engine(spec: &OverlaySpec) -> anyhow::Result<RhaiEngine> {
    let mut engine = RhaiEngine::new(spec.layers.clone());
    if let Some(script) = &spec.script {
        engine.load_script(script)?;
    }
    Ok(engine)
}

fn evaluate_state(spec: &OverlaySpec, time: f64, frame_index: u64) -> anyhow::Result<OverlayState> {
    let engine = build_engine(spec)?;
    Ok(engine.evaluate(time, frame_index))
}

fn write_png(path: &std::path::Path, width: u32, height: u32, rgba: &[u8]) -> anyhow::Result<()> {
    let file = std::fs::File::create(path)
        .map_err(|e| anyhow::anyhow!("create {}: {e}", path.display()))?;
    let writer = BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut header = encoder
        .write_header()
        .map_err(|e| anyhow::anyhow!("png header: {e}"))?;
    header
        .write_image_data(rgba)
        .map_err(|e| anyhow::anyhow!("png write: {e}"))?;
    Ok(())
}

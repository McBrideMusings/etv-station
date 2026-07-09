use std::io::BufWriter;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use clap::{Parser, Subcommand};
use etv_overlay::fifo_writer::{FifoWriter, OpenOutcome, default_fifo_path};
use etv_overlay::overlay_spec::OverlaySpec;
use etv_overlay::program_context::{ProgramContext, ProgramContextSource};
use etv_overlay::rhai_engine::{OverlayState, RhaiEngine};
use etv_overlay::vello_renderer::VelloRenderer;
use time::OffsetDateTime;

/// Set by SIGTERM/SIGINT so the pipe loop — including a wait for the next
/// reader inside `reopen()` — can exit gracefully. The station daemon sends
/// SIGTERM to overlay subprocesses on shutdown.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Backoff before reopening the fifo after a reader disconnect, so a reader
/// that opens then closes rapidly can't spin the reopen loop hot or flood logs.
const REOPEN_BACKOFF: Duration = Duration::from_millis(100);

extern "C" fn handle_shutdown_signal(_sig: i32) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Install SIGTERM/SIGINT handlers that flip the shutdown flag.
fn install_shutdown_handlers() {
    use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
    let action = SigAction::new(
        SigHandler::Handler(handle_shutdown_signal),
        SaFlags::empty(),
        SigSet::empty(),
    );
    // SAFETY: the handler only does an atomic store, which is async-signal-safe.
    unsafe {
        if let Err(e) = sigaction(Signal::SIGTERM, &action) {
            tracing::warn!(error = %e, "failed to install SIGTERM handler");
        }
        if let Err(e) = sigaction(Signal::SIGINT, &action) {
            tracing::warn!(error = %e, "failed to install SIGINT handler");
        }
    }
}

/// What happened while writing one frame to the fifo.
enum FrameWrite {
    /// Written to the current reader.
    Written,
    /// The reader had gone; we reopened for a new reader and wrote the frame as
    /// the first, frame-aligned frame of its stream. The caller should
    /// re-anchor pacing so it doesn't burst the buffered wall-clock gap.
    Reopened,
    /// Shutdown was requested while waiting for a reader; stop the loop.
    Shutdown,
    /// No reader for [`IDLE_TIMEOUT`]; the channel is no longer being watched,
    /// so the overlay process should exit and free its GPU context.
    Idle,
}

/// How long the overlay tolerates having no reader (no ffmpeg attached to the
/// fifo) before exiting. This is a grace period, NOT the channel-warm window:
/// etv-next keeps a channel (and its overlay-reading ffmpeg) alive for
/// `HEARTBEAT_FILE_TIMEOUT` (90s) after the last viewer, and respawns ffmpeg per
/// playout item — so the overlay sees brief reader gaps at item boundaries even
/// while watched. The timeout only needs to comfortably exceed the largest such
/// gap; once the channel actually goes cold (worker exits, ffmpeg gone for
/// good), the overlay exits this long after and frees its GPU context.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

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
        /// Folder containing the station-emitted chunked playout JSON for
        /// this channel. When set, the per-frame Rhai scope is populated with
        /// `title` / `next_title` / `item_elapsed` / `item_remaining` for the
        /// item airing at wallclock now. Without it the context fields are
        /// empty/-1.0 — scripts that don't reference them still work.
        #[arg(long)]
        playout_folder: Option<PathBuf>,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("etv_overlay=info,warn")),
        )
        .init();

    install_shutdown_handlers();

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
            playout_folder,
        } => pipe_to_fifo(config, fifo, create_fifo, ready_file, playout_folder),
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

    if matches!(
        fifo.open_for_writing(&SHUTDOWN, None)?,
        OpenOutcome::Shutdown
    ) {
        return Ok(());
    }
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
        let state = engine.evaluate(time_seconds, frame_index, &ProgramContext::unknown());
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
    playout_folder: Option<PathBuf>,
) -> anyhow::Result<()> {
    let spec = OverlaySpec::from_path(&config)?;
    let mut fifo = if create_fifo {
        FifoWriter::create(fifo_path.clone())?
    } else {
        FifoWriter::attach(fifo_path.clone())
    };

    let mut program_source = playout_folder.map(ProgramContextSource::new);

    // Warm the renderer fully BEFORE opening the fifo. wgpu adapter init,
    // vello shader compile, and the first image-cache decode all happen in
    // the first render_frame call. Doing that work while the fifo is closed
    // guarantees ffmpeg can't read a partial frame during cold-start: nothing
    // hits the pipe until we have a complete RGBA buffer ready to write.
    // See https://github.com/McBrideMusings/etv-station/issues/54.
    let mut renderer = VelloRenderer::new(spec.width, spec.height, spec.pixel_format)?;
    let engine = build_engine(&spec)?;
    let start = std::time::Instant::now();
    let initial_ctx = current_context(program_source.as_mut());
    let first_frame = renderer.render_frame(&engine.evaluate(0.0, 0, &initial_ctx))?;
    tracing::info!(
        warmup_ms = start.elapsed().as_millis() as u64,
        "renderer warm; opening fifo for first write",
    );

    // If no reader attaches within IDLE_TIMEOUT of spawn, the channel isn't
    // being watched — exit rather than hold an idle GPU context.
    match fifo.open_for_writing(&SHUTDOWN, Some(start + IDLE_TIMEOUT))? {
        OpenOutcome::Opened => {}
        OpenOutcome::Shutdown | OpenOutcome::Idle => return Ok(()),
    }
    let mut last_activity = std::time::Instant::now();
    match write_frame_resilient(
        &mut fifo,
        &first_frame,
        &SHUTDOWN,
        last_activity + IDLE_TIMEOUT,
    )? {
        FrameWrite::Written | FrameWrite::Reopened => last_activity = std::time::Instant::now(),
        FrameWrite::Shutdown | FrameWrite::Idle => return Ok(()),
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

    let framerate = spec.framerate.max(1) as f64;
    let mut frame_index: u64 = 1;
    // Pacing is anchored separately from `start` (which drives animation time)
    // so that a multi-second blocking reopen between playout items doesn't make
    // the loop dump a burst of unpaced frames at the freshly-attached reader. On
    // a reopen we re-anchor, resuming at the real frame rate.
    let mut pace_start = start;
    let mut paced_frames: u64 = 0;
    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            tracing::info!("shutdown requested; stopping overlay pipe");
            return Ok(());
        }
        let time_seconds = start.elapsed().as_secs_f64();
        let ctx = current_context(program_source.as_mut());
        let state = engine.evaluate(time_seconds, frame_index, &ctx);
        let frame = renderer.render_frame(&state)?;
        match write_frame_resilient(&mut fifo, &frame, &SHUTDOWN, last_activity + IDLE_TIMEOUT)? {
            FrameWrite::Written => last_activity = std::time::Instant::now(),
            FrameWrite::Reopened => {
                last_activity = std::time::Instant::now();
                pace_start = std::time::Instant::now();
                paced_frames = 0;
            }
            FrameWrite::Shutdown => {
                tracing::info!("shutdown requested; stopping overlay pipe");
                return Ok(());
            }
            FrameWrite::Idle => {
                tracing::info!(
                    "no reader for {IDLE_TIMEOUT:?}; channel no longer watched, exiting"
                );
                return Ok(());
            }
        }
        frame_index += 1;
        paced_frames += 1;
        // f64-based offset so a 24/7 daemon doesn't truncate at u32 wrap (~1657 days at 30 fps).
        let target = pace_start + Duration::from_secs_f64(paced_frames as f64 / framerate);
        let now = std::time::Instant::now();
        if target > now {
            thread::sleep(target - now);
        }
    }
}

/// Write one frame, transparently waiting for the next reader if the current
/// one has gone away. etv-next spawns a fresh ffmpeg per playout item; when an
/// item ends our write returns BrokenPipe, so we reopen the fifo (blocking
/// until the next item's ffmpeg attaches) and write the frame as the first,
/// frame-aligned frame of that reader's stream. This keeps the overlay process
/// alive across the whole channel session rather than exiting per item.
fn write_frame_resilient(
    fifo: &mut FifoWriter,
    frame: &[u8],
    shutdown: &AtomicBool,
    idle_deadline: std::time::Instant,
) -> anyhow::Result<FrameWrite> {
    let mut reopened = false;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return Ok(FrameWrite::Shutdown);
        }
        match fifo.write_frame(frame) {
            Ok(()) => {
                return Ok(if reopened {
                    FrameWrite::Reopened
                } else {
                    FrameWrite::Written
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                tracing::info!("reader disconnected; waiting for next reader to reattach");
                // Backoff so rapid reader open/close churn can't spin this hot.
                thread::sleep(REOPEN_BACKOFF);
                match fifo.reopen(shutdown, Some(idle_deadline))? {
                    OpenOutcome::Opened => reopened = true,
                    OpenOutcome::Shutdown => return Ok(FrameWrite::Shutdown),
                    OpenOutcome::Idle => return Ok(FrameWrite::Idle),
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Resolve the current program context, refreshing the schedule cache if it's
/// stale. Returns [`ProgramContext::unknown`] when no source is configured
/// or when the refresh itself errors — a transient schedule problem must not
/// kill the overlay loop.
fn current_context(source: Option<&mut ProgramContextSource>) -> ProgramContext {
    let Some(source) = source else {
        return ProgramContext::unknown();
    };
    if let Err(e) = source.refresh() {
        tracing::warn!(
            folder = %source.folder().display(),
            error = %e,
            "program_context refresh failed; using last-known schedule",
        );
    }
    source.current_at(OffsetDateTime::now_utc())
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
    Ok(engine.evaluate(time, frame_index, &ProgramContext::unknown()))
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

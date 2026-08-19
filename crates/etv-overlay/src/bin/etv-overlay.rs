use std::io::BufWriter;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use clap::{Parser, Subcommand};
use etv_overlay::fifo_writer::{FifoWriter, OpenOutcome, default_fifo_path};
use etv_overlay::overlay_spec::OverlaySpec;
use etv_overlay::overlay_timeline::OverlayTimelineSource;
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
#[derive(Debug)]
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

// The ordering above is load-bearing, so check it against the real constant
// rather than the "(90s)" written in the comment. If the overlay ever tolerated
// a reader gap for longer than etv-next keeps a channel warm, it would still be
// waiting when the worker that feeds it has already gone — and a change on
// either side of the vendor boundary could introduce that silently. Assert it here
// so the drift is a build failure instead of a channel that quietly stops
// rendering its overlay.
const _: () = assert!(
    IDLE_TIMEOUT.as_secs() < ersatztv_core::HEARTBEAT_FILE_TIMEOUT.as_secs(),
    "overlay IDLE_TIMEOUT must stay below etv-next's HEARTBEAT_FILE_TIMEOUT",
);

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
        /// Program title fed to the script's `title` scope constant, for
        /// eyeballing a script that renders differently once a title is
        /// known (e.g. `now_playing.rhai`, `title_chyron.rhai`). Empty
        /// (the default) reproduces `ProgramContext::unknown()`.
        #[arg(long, default_value = "")]
        title: String,
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
    /// Loop a background clip through ffmpeg forever, muxed with the overlay
    /// and streamed to stdout as mpegts — pipe into `vlc -` for a live
    /// preview. Polls `config` for edits and hot-reloads the script/layers
    /// without restarting ffmpeg, so a spec being iterated on updates the
    /// running stream a few hundred ms after each save.
    Watch {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        fifo: Option<PathBuf>,
        #[arg(long)]
        ffmpeg: Option<PathBuf>,
        /// Multiplies the clock the Rhai script sees (`time`), so a script
        /// keyed on a multi-minute cycle can be watched on a compressed
        /// loop without waiting it out in real time. Output still plays at
        /// the spec's real framerate; only the animation clock speeds up.
        #[arg(long, default_value_t = 1.0)]
        time_scale: f64,
        /// See `RenderStill --title`.
        #[arg(long, default_value = "")]
        title: String,
    },
    /// Render frames directly to a fifo, blocking until the reader disconnects
    Pipe {
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
        /// Folder containing the station-emitted chunked playout JSON and
        /// `overlay.json` for this channel. The per-frame Rhai scope reads
        /// `title` / `next_title` / `item_elapsed` / `item_remaining` from the
        /// former; the latter supplies the spawn config (geometry, the
        /// fallback script and layers) and the per-block overrides the
        /// station → channel → block cascade resolves (#48). Required: a
        /// process spawned with nowhere to read either from has nothing to
        /// render.
        #[arg(long)]
        playout_folder: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    // stderr, not stdout: `Cmd::Watch` muxes a live mpegts stream onto its own
    // stdout for a caller to pipe into `vlc -`, and a log line interleaved
    // into that byte stream corrupts it. No other subcommand writes anything
    // meaningful to stdout, so moving logs off it costs nothing there.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
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
            title,
        } => render_still(config, output, time, title),
        Cmd::Run {
            input,
            config,
            output,
            fifo,
            ffmpeg,
            keep_fifo,
        } => run_with_ffmpeg(input, config, output, fifo, ffmpeg, keep_fifo),
        Cmd::Watch {
            input,
            config,
            fifo,
            ffmpeg,
            time_scale,
            title,
        } => watch(input, config, fifo, ffmpeg, time_scale, title),
        Cmd::Pipe {
            fifo,
            create_fifo,
            ready_file,
            playout_folder,
        } => pipe_to_fifo(fifo, create_fifo, ready_file, playout_folder),
    }
}

fn program_context_for(title: String) -> ProgramContext {
    if title.is_empty() {
        ProgramContext::unknown()
    } else {
        ProgramContext {
            title,
            ..ProgramContext::unknown()
        }
    }
}

fn render_still(config: PathBuf, output: PathBuf, time: f64, title: String) -> anyhow::Result<()> {
    let spec = OverlaySpec::from_path(&config)?;
    let mut renderer = VelloRenderer::new(spec.width, spec.height, spec.pixel_format)?;
    let state = evaluate_state(&spec, time, 0, &program_context_for(title))?;
    let frame = renderer.render_frame(&state)?;
    write_png(&output, spec.width, spec.height, &frame)?;
    tracing::info!(path = %output.display(), "wrote still frame");
    Ok(())
}

/// Kills the wrapped ffmpeg child on drop. `run_with_ffmpeg` and `watch` both
/// spawn ffmpeg, then still have fallible setup between the spawn and their
/// own explicit `child.kill()`/`wait()` at the end (`fifo.open_for_writing`,
/// `VelloRenderer::new`, `build_engine`) — an early `?`-propagated return
/// through any of those, or a shutdown signal arriving while
/// `open_for_writing` is still waiting for a reader, would otherwise skip
/// the explicit cleanup and leak the child. Wrapping it means every exit
/// gets that for free instead of needing a matching `child.kill()` at each
/// fallible call site. Most load-bearing for `watch`, whose background
/// input loops forever (`-stream_loop -1`) and never exits on its own;
/// `run_with_ffmpeg`'s finite input (`-shortest`) would eventually self-exit
/// even unwrapped, just not as soon as a signal should have stopped it.
struct KillOnDrop(std::process::Child);

impl std::ops::Deref for KillOnDrop {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for KillOnDrop {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
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

    // KillOnDrop, not a bare Child: without it, a shutdown signal arriving
    // while `open_for_writing` below is still waiting for a reader — the
    // same early-return race `watch` had — would return before this
    // function's own `child.wait()` at the bottom ever runs, leaking ffmpeg
    // for however long it takes this finite-input run to hit `-shortest` and
    // exit on its own.
    let mut child = KillOnDrop(
        Command::new(&ffmpeg)
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
            .map_err(|e| anyhow::anyhow!("spawn ffmpeg: {e}"))?,
    );

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

/// How often (in output frames) `watch` restats `config` for a hot-reload. A
/// once-a-second poll is cheap and comfortably fast enough for eyeballing an
/// edit-save-look loop; every frame would mean 30 stat(2) calls/sec for no
/// perceptible gain.
const CONFIG_POLL_FRAMES: u64 = 30;

fn watch(
    input: PathBuf,
    config: PathBuf,
    fifo: Option<PathBuf>,
    ffmpeg_bin: Option<PathBuf>,
    time_scale: f64,
    title: String,
) -> anyhow::Result<()> {
    let mut spec = OverlaySpec::from_path(&config)?;
    let base_geometry = spec.geometry();
    let fifo_path = fifo.unwrap_or_else(|| default_fifo_path("watch"));
    let mut fifo = FifoWriter::create(fifo_path.clone())?;

    let ffmpeg = ffmpeg_bin.unwrap_or_else(|| PathBuf::from("ffmpeg"));
    let filter = "[0:v][1:v]overlay=x=0:y=0:eof_action=pass:format=auto[v]";

    let mut child = KillOnDrop(
        Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "warning",
                "-y",
                "-stream_loop",
                "-1",
                "-i",
            ])
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
                "-tune",
                "zerolatency",
                "-c:a",
                "aac",
                "-f",
                "mpegts",
                "-",
            ])
            .stdin(Stdio::null())
            // ffmpeg's muxed mpegts IS this process's whole stdout contract for
            // `watch` — inheriting lets the caller's `| vlc -` read it straight
            // from ffmpeg with no extra copy through this process.
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn ffmpeg: {e}"))?,
    );

    if matches!(
        fifo.open_for_writing(&SHUTDOWN, None)?,
        OpenOutcome::Shutdown
    ) {
        return Ok(());
    }
    let mut renderer = VelloRenderer::new(spec.width, spec.height, spec.pixel_format)?;
    let mut engine = build_engine(&spec)?;
    let program = program_context_for(title);
    let mut last_mtime = fs_mtime(&config);

    let frame_period = Duration::from_secs_f64(1.0 / spec.framerate.max(1) as f64);
    let mut frame_index: u64 = 0;
    let mut next_tick = std::time::Instant::now();

    tracing::info!(
        geometry = %base_geometry,
        time_scale,
        config = %config.display(),
        "watching; edit and save the config to hot-reload",
    );

    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("waitpid ffmpeg: {e}");
                break;
            }
        }
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }

        if frame_index.is_multiple_of(CONFIG_POLL_FRAMES) {
            let mtime = fs_mtime(&config);
            if mtime != last_mtime {
                last_mtime = mtime;
                match OverlaySpec::from_path(&config) {
                    Ok(new_spec) if new_spec.geometry() == base_geometry => {
                        match build_engine(&new_spec) {
                            Ok(new_engine) => {
                                spec = new_spec;
                                engine = new_engine;
                                tracing::info!("config changed; reloaded script and layers");
                            }
                            Err(e) => tracing::warn!(
                                error = %e,
                                "new config's script failed to load; keeping the running one",
                            ),
                        }
                    }
                    Ok(new_spec) => tracing::warn!(
                        new = %new_spec.geometry(),
                        running = %base_geometry,
                        "edited config changed geometry, which `watch` can't hot-swap; \
                         restart to pick it up",
                    ),
                    Err(e) => tracing::warn!(
                        error = %e,
                        "edited config failed to parse; keeping the running one",
                    ),
                }
            }
        }

        let time_seconds = (frame_index as f64 / spec.framerate.max(1) as f64) * time_scale;
        let state = engine.evaluate(time_seconds, frame_index, &program);
        let frame = renderer.render_frame(&state)?;

        if let Err(e) = fifo.write_frame(&frame) {
            if matches!(e.kind(), std::io::ErrorKind::BrokenPipe) {
                tracing::info!("ffmpeg closed pipe (reader gone), stopping");
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
    // Unlike `run`'s ffmpeg (finite input, `-shortest`, exits on its own),
    // `watch`'s background loops forever (`-stream_loop -1`) and never exits
    // on its own — dropping the fifo only EOFs the *other* input. `KillOnDrop`
    // already guarantees the kill on every path out of this function
    // (including the early returns above, before this point); the explicit
    // kill+wait here just reaps it deterministically on the common path
    // instead of leaving that to whenever `child` drops.
    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

fn fs_mtime(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn pipe_to_fifo(
    fifo_path: PathBuf,
    create_fifo: bool,
    ready_file: Option<PathBuf>,
    playout_folder: PathBuf,
) -> anyhow::Result<()> {
    // The station's `prepare_generation` writes `overlay.json` for every
    // channel before it spawns any overlay process, so this loop only ever
    // turns on the spawn racing that write on the very first tick after it —
    // bounded rather than open-ended so a station that never wrote one (a
    // config bug this process can't see) fails loudly instead of hanging.
    let mut timeline_source = OverlayTimelineSource::new(&playout_folder);
    let wait_deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Err(e) = timeline_source.refresh() {
            tracing::warn!(error = %e, "overlay timeline refresh failed while waiting for the spawn config");
        }
        if timeline_source.is_loaded() {
            break;
        }
        if std::time::Instant::now() >= wait_deadline {
            anyhow::bail!(
                "no overlay.json appeared in {} within 10s; the station should have \
                 written one before spawning this process",
                playout_folder.display(),
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
    let base = timeline_source
        .base()
        .expect("just confirmed the timeline is loaded")
        .clone();

    let mut fifo = if create_fifo {
        FifoWriter::create(fifo_path.clone())?
    } else {
        FifoWriter::attach(fifo_path.clone())
    };

    let mut program_source = Some(ProgramContextSource::new(playout_folder));

    // Warm the renderer fully BEFORE opening the fifo. wgpu adapter init,
    // vello shader compile, and the first image-cache decode all happen in
    // the first render_frame call. Doing that work while the fifo is closed
    // guarantees ffmpeg can't read a partial frame during cold-start: nothing
    // hits the pipe until we have a complete RGBA buffer ready to write.
    // See https://github.com/McBrideMusings/etv-station/issues/54.
    let mut renderer = VelloRenderer::new(base.width, base.height, base.pixel_format)?;
    let mut active_spec = timeline_source.spec_at(OffsetDateTime::now_utc()).cloned();
    let mut engine = build_engine_for(active_spec.as_ref())?;
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
    match write_frame_resilient(&mut fifo, &first_frame, &SHUTDOWN, IDLE_TIMEOUT)? {
        FrameWrite::Written | FrameWrite::Reopened => {}
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

    let framerate = base.framerate.max(1) as f64;
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

        // A block boundary the timeline crossed swaps script + layers in
        // place — same process, same fifo, same canvas (#48, ADR 0007). The
        // geometry guard is defense in depth: `config::resolve_channel`
        // already refuses a mismatched block at load, so this should never
        // fire against a config the station actually shipped.
        if let Err(e) = timeline_source.refresh() {
            tracing::warn!(error = %e, "overlay timeline refresh failed; keeping the last good config");
        }
        let wanted = timeline_source.spec_at(OffsetDateTime::now_utc());
        if wanted != active_spec.as_ref() {
            match wanted {
                Some(spec) if spec.geometry() != base.geometry() => {
                    tracing::warn!(
                        wanted = %spec.geometry(),
                        base = %base.geometry(),
                        "block overlay geometry disagrees with the channel's spawn config; \
                         keeping the running config",
                    );
                }
                _ => match build_engine_for(wanted) {
                    Ok(new_engine) => {
                        tracing::info!("overlay timeline changed; reloaded script and layers");
                        engine = new_engine;
                        active_spec = wanted.cloned();
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to load the new overlay config; keeping the running one")
                    }
                },
            }
        }

        let time_seconds = start.elapsed().as_secs_f64();
        let ctx = current_context(program_source.as_mut());
        let state = engine.evaluate(time_seconds, frame_index, &ctx);
        let frame = renderer.render_frame(&state)?;
        match write_frame_resilient(&mut fifo, &frame, &SHUTDOWN, IDLE_TIMEOUT)? {
            FrameWrite::Written => {}
            FrameWrite::Reopened => {
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
    idle_timeout: Duration,
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
                // Anchor the idle clock at THIS disconnect, not at the last
                // completed write. Writes to the fifo block, so a reader that
                // attaches and then stalls (a wedged ffmpeg) can hold us inside
                // one write_all for longer than IDLE_TIMEOUT. A deadline
                // carried in from before that write is already expired by the
                // time we get here, which turns a routine per-item reader swap
                // into an instant "no reader" exit — and the exit kills the
                // only writer the replacement ffmpeg is waiting on, wedging the
                // channel for good. Idle means "no reader since the last one
                // left", so measure it from here.
                let deadline = std::time::Instant::now() + idle_timeout;
                match fifo.reopen(shutdown, Some(deadline))? {
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
    let mut engine = RhaiEngine::with_config(spec.layers.clone(), spec.config.as_ref());
    if let Some(script) = &spec.script {
        engine.load_script(script)?;
    }
    Ok(engine)
}

/// As [`build_engine`], for the resolved-overlay cascade's `Option<&OverlaySpec>`
/// (#48): `None` — a block that declared `overlay: clear` — is an engine with
/// no layers and no script, which renders nothing, same as an empty spec would.
fn build_engine_for(spec: Option<&OverlaySpec>) -> anyhow::Result<RhaiEngine> {
    match spec {
        Some(spec) => build_engine(spec),
        None => Ok(RhaiEngine::new(vec![])),
    }
}

fn evaluate_state(
    spec: &OverlaySpec,
    time: f64,
    frame_index: u64,
    program: &ProgramContext,
) -> anyhow::Result<OverlayState> {
    let engine = build_engine(spec)?;
    Ok(engine.evaluate(time, frame_index, program))
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

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::stat::Mode;
    use nix::unistd::mkfifo;
    use std::fs::OpenOptions;
    use std::io::Read;
    use std::time::Instant;

    /// Regression test for the channel-wedging overlay exit.
    ///
    /// A reader that attaches and then stops draining blocks us inside a single
    /// `write_frame` for as long as it likes — fifo writes block once the pipe
    /// buffer fills. The idle clock previously ran from the last *completed*
    /// write, so by the time such a reader disconnected the deadline was
    /// already in the past, and the routine wait for the next reader returned
    /// `Idle` immediately instead of waiting. That exit killed the only writer
    /// the replacement ffmpeg was about to block on, wedging the channel for
    /// good. A stall longer than the idle timeout must still leave a full
    /// timeout to pick up the next reader.
    #[test]
    fn stalled_reader_does_not_poison_the_next_reader_wait() {
        const IDLE: Duration = Duration::from_millis(500);
        // Larger than any pipe buffer, so the write is guaranteed to block
        // while the reader sits on it rather than completing outright.
        const FRAME_LEN: usize = 512 * 1024;

        let dir = tempfile::tempdir().unwrap();
        let fifo_path = dir.path().join("overlay.fifo");
        mkfifo(&fifo_path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();

        let frame = vec![0xABu8; FRAME_LEN];
        let shutdown = AtomicBool::new(false);
        let mut fifo = FifoWriter::attach(fifo_path.clone());

        // Reader 1 stalls well past IDLE with the write in flight, then leaves;
        // reader 2 attaches shortly after, as the next playout item's ffmpeg
        // would.
        let reader_path = fifo_path.clone();
        let readers = thread::spawn(move || {
            let mut f = OpenOptions::new().read(true).open(&reader_path).unwrap();
            let mut byte = [0u8; 1];
            f.read_exact(&mut byte).unwrap();
            thread::sleep(IDLE * 2);
            drop(f);
            // Comfortably clear of REOPEN_BACKOFF, comfortably inside IDLE.
            thread::sleep(REOPEN_BACKOFF * 2);
            let mut f2 = OpenOptions::new().read(true).open(&reader_path).unwrap();
            let mut buf = vec![0u8; FRAME_LEN];
            f2.read_exact(&mut buf).unwrap();
            assert!(
                buf.iter().all(|b| *b == 0xAB),
                "reader 2 must get a whole frame from a fresh boundary"
            );
        });

        assert_eq!(
            fifo.open_for_writing(&shutdown, None).unwrap(),
            OpenOutcome::Opened
        );
        let started = Instant::now();
        let outcome = write_frame_resilient(&mut fifo, &frame, &shutdown, IDLE).unwrap();

        assert!(
            matches!(outcome, FrameWrite::Reopened),
            "a stall longer than the idle timeout must not turn the next \
             reader wait into an immediate give-up; got {outcome:?}",
        );
        assert!(
            started.elapsed() > IDLE,
            "sanity: the reader really did stall past the idle timeout"
        );
        readers.join().unwrap();
    }
}

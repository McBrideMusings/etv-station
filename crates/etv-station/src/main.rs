use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use etv_station::{config, daemon, etv_next};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "etv-station",
    about = "Playout JSON generator daemon for ErsatzTV-next"
)]
struct Cli {
    /// Path to the top-level station config (TOML or YAML, by extension).
    #[arg(short, long)]
    config: PathBuf,

    /// Log output format.
    #[arg(long, value_enum, default_value_t = LogFormat::Pretty)]
    log_format: LogFormat,

    /// Print each channel's resolved output_folder (one per line) and exit,
    /// instead of running the daemon. tools/dev-run.sh uses this to discover
    /// the folders to poll through the daemon's own config loader, so it can
    /// never disagree with where the daemon actually writes.
    #[arg(long)]
    list_folders: bool,

    /// Render ETV-next's lineup.json + channelN.json into this directory from
    /// the station config, then exit. The directory must already hold
    /// `normalization.default.json`; `presentation.json` is used if present.
    /// The container entrypoint runs this before starting ETV-next, so the
    /// playout folders it reads are always the ones the daemon writes.
    #[arg(long, value_name = "DIR")]
    render_etv_next: Option<PathBuf>,

    /// Generate the named channel twice from identical inputs (same catalog
    /// snapshot, seed, and resume state) and report whether the two
    /// schedules match, then exit — a debug check for a plugin that breaks
    /// reproducible generation silently (#168). Not part of normal
    /// generation. Exit code is 0 for identical, 1 for a differing schedule
    /// or a load/resolve failure.
    #[arg(long, value_name = "CHANNEL")]
    check_determinism: Option<String>,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum LogFormat {
    Pretty,
    Json,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // Query mode: print resolved folders and exit before any tracing/runtime
    // setup, so stdout carries only the folder list for a caller to capture.
    if cli.list_folders {
        return list_folders(&cli.config);
    }

    if let Some(dir) = cli.render_etv_next.as_deref() {
        return render_etv_next(&cli.config, dir);
    }

    if let Some(channel) = cli.check_determinism.as_deref() {
        return check_determinism(&cli.config, channel);
    }

    init_tracing(cli.log_format);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            tracing::error!(event = "runtime.error", error = %err, "failed to start tokio runtime");
            return ExitCode::from(1);
        }
    };

    runtime.block_on(async move {
        let station = match config::load(&cli.config) {
            Ok(s) => s,
            Err(err) => {
                tracing::error!(event = "config.error", error = %err, "failed to load configuration");
                return ExitCode::from(1);
            }
        };

        tracing::info!(
            event = "station.load",
            station_config = %station.config_path.display(),
            tz = %station.station.tz,
            channels = station.channels.len(),
            "loaded station config",
        );
        for ch in &station.channels {
            tracing::info!(
                event = "channel.load",
                channel = %ch.name,
                config = %ch.config_path.display(),
                blocks = ch.config.rule.blocks.len(),
                output_folder = %ch.output_folder.display(),
                window_days = ch.config.window_days,
                chunk_hours = ch.config.chunk_hours,
                roll_interval_secs = ch.config.roll_interval.as_secs(),
                "loaded channel",
            );
        }

        match daemon::run(station).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                tracing::error!(event = "daemon.error", error = %err, "daemon failed");
                ExitCode::from(1)
            }
        }
    })
}

/// Load the station config and print each channel's `output_folder` verbatim,
/// one per line. Prints the value the daemon writes to (used as-is, relative to
/// the process CWD), so a caller polling these paths watches exactly the daemon's
/// output. Config-load failure goes to stderr with a non-zero exit.
fn list_folders(config_path: &Path) -> ExitCode {
    match config::load(config_path) {
        Ok(station) => {
            for ch in &station.channels {
                println!("{}", ch.output_folder.display());
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("failed to load configuration: {err}");
            ExitCode::from(1)
        }
    }
}

/// Render ETV-next's config from the station config and exit. Progress goes to
/// stdout so the container entrypoint's log shows what the lineup ended up as;
/// a failure is fatal, since ETV-next would otherwise boot on stale config.
fn render_etv_next(config_path: &Path, out_dir: &Path) -> ExitCode {
    let opts = match etv_next::RenderOptions::from_env(out_dir.to_path_buf()) {
        Ok(opts) => opts,
        Err(err) => {
            eprintln!("render-etv-next: {err}");
            return ExitCode::from(1);
        }
    };
    match etv_next::render(config_path, &opts) {
        Ok(rendered) => {
            println!(
                "render-etv-next: {} + {} channel file(s) (bind={} port={} device_id={})",
                rendered.lineup_path.display(),
                rendered.channels,
                opts.bind_address,
                opts.port,
                rendered.device_id,
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("render-etv-next: {err}");
            ExitCode::from(1)
        }
    }
}

/// Generate `channel_name` twice from identical inputs and print whether the
/// two schedules match — see `etv_station::determinism::check` (#168) for
/// what "identical inputs" means and what this can and cannot catch. Exit
/// code carries the verdict as well as failure, so a script driving this in
/// CI can rely on the exit code alone: 0 only for a proven-identical pair of
/// passes.
fn check_determinism(config_path: &Path, channel_name: &str) -> ExitCode {
    let mut station = match config::load(config_path) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("check-determinism: failed to load configuration: {err}");
            return ExitCode::from(1);
        }
    };
    match etv_station::determinism::check(&mut station, channel_name) {
        Ok(report) if report.is_identical() => {
            println!(
                "check-determinism: {} — identical ({} items)",
                report.channel, report.pass_a_len,
            );
            ExitCode::SUCCESS
        }
        Ok(report) => {
            let diff = report
                .difference
                .as_ref()
                .expect("checked above: identical branch already handled");
            println!(
                "check-determinism: {} — DIFFERS at position {}: pass A = {}, pass B = {} \
                 (lengths {} vs {})",
                report.channel,
                diff.position,
                diff.entry_a.as_deref().unwrap_or("<none — pass A ended>"),
                diff.entry_b.as_deref().unwrap_or("<none — pass B ended>"),
                report.pass_a_len,
                report.pass_b_len,
            );
            ExitCode::from(1)
        }
        Err(err) => {
            eprintln!("check-determinism: {err}");
            ExitCode::from(1)
        }
    }
}

fn init_tracing(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match format {
        LogFormat::Pretty => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
}

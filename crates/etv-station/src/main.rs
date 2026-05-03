use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use etv_station::{config, daemon};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "etv-station",
    about = "Playout JSON generator daemon for ErsatzTV-next"
)]
struct Cli {
    /// Path to the top-level station.toml.
    #[arg(short, long)]
    config: PathBuf,

    /// Log output format.
    #[arg(long, value_enum, default_value_t = LogFormat::Pretty)]
    log_format: LogFormat,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum LogFormat {
    Pretty,
    Json,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.log_format);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            tracing::error!(error = %err, "failed to start tokio runtime");
            return ExitCode::from(1);
        }
    };

    runtime.block_on(async move {
        let station = match config::load(&cli.config) {
            Ok(s) => s,
            Err(err) => {
                tracing::error!(error = %err, "failed to load configuration");
                return ExitCode::from(1);
            }
        };

        tracing::info!(
            station_config = %station.config_path.display(),
            tz = %station.station.tz,
            channels = station.channels.len(),
            "loaded station config",
        );
        for ch in &station.channels {
            tracing::info!(
                channel = %ch.name,
                config = %ch.config_path.display(),
                rule = ch.config.rule.name(),
                items = ch.config.rule.items().len(),
                output_folder = %ch.config.output_folder.display(),
                window_days = ch.config.window_days,
                chunk_hours = ch.config.chunk_hours,
                roll_interval_secs = ch.config.roll_interval.as_secs(),
                "loaded channel",
            );
        }

        match daemon::run(station).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                tracing::error!(error = %err, "daemon failed");
                ExitCode::from(1)
            }
        }
    })
}

fn init_tracing(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match format {
        LogFormat::Pretty => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
}

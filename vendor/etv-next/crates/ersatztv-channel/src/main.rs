mod channel_session;
mod dossier;
mod local_proxy;
mod playlist_manager;
mod playout_loader;
mod pts_scanner;

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use ersatztv_channel::config::ChannelConfig;
use ersatztv_channel::error::ChannelError;
use ffpipeline::ffmpeg_info::FfmpegInfo;

use crate::channel_session::ChannelSession;

#[derive(Parser, Debug)]
#[command(version = ersatztv_core::VERSION, about, long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Print debug information using the provided configuration
    Debug {
        #[arg(required = true, num_args = 1..)]
        config_paths: Vec<PathBuf>,
    },
    /// Run the channel using the provided configuration
    Run {
        #[arg(required = true, num_args = 1..)]
        config_paths: Vec<PathBuf>,
        #[arg(short, long)]
        output_folder: PathBuf,
        #[arg(short, long)]
        number: String,
        #[arg(short, long)]
        troubleshoot: bool,
    },
}

#[tokio::main]
pub async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();

    if let Err(err) = run().await {
        match err {
            ChannelError::IdleTimeout(_) => log::info!("{err}"),
            _ => log::error!("{err}"),
        };

        // Report the two "stopped reaching the viewer" verdicts with their own
        // exit code. The server counts those as failures no matter how long the
        // run lasted; anything else keeps the ordinary code and is judged on
        // uptime as before. See `ersatztv_core::STALL_EXIT_CODE`.
        let code = match err {
            ChannelError::SegmentStall(_) | ChannelError::Stalled(_) => {
                ersatztv_core::STALL_EXIT_CODE
            }
            _ => 1,
        };

        std::process::exit(code);
    }
}

async fn run() -> Result<(), ChannelError> {
    let args = Args::parse();

    match args.command {
        Commands::Run {
            config_paths,
            output_folder,
            number,
            troubleshoot,
        } => {
            let channel_config =
                ChannelConfig::from_sources(&config_paths, &output_folder, &number).await?;

            // start channel session
            let mut channel_session = ChannelSession::new(channel_config).await?;
            channel_session.run(troubleshoot).await
        }
        Commands::Debug { config_paths } => {
            let channel_config =
                ChannelConfig::from_sources(&config_paths, &std::env::temp_dir(), "debug").await?;

            log::debug!("{:?}", channel_config);

            let ffmpeg_path = channel_config
                .ffmpeg
                .ffmpeg_path
                .as_deref()
                .unwrap_or(Path::new("ffmpeg"));
            let ffmpeg_info = FfmpegInfo::load(
                ffmpeg_path,
                &channel_config.ffmpeg.disabled_filters,
                &channel_config.ffmpeg.preferred_filters,
            )
            .await?;

            log::debug!("{:?}", ffmpeg_info);

            if let Some(accel) = &channel_config.normalization.video.accel {
                let _ = accel.to_pipeline(&channel_config);
            }

            Ok(())
        }
    }
}

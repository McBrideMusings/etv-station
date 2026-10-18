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
        /// Display name shown on the error card when an item fails
        #[arg(long, default_value = "")]
        name: String,
        #[arg(short, long)]
        troubleshoot: bool,
    },
}

/// Determine the process exit code for a channel error.
///
/// IdleTimeout is a successful end of a session (the channel reached its
/// configured timeout with no viewers), so it exits 0 and is handled by the
/// supervisor's s.success() arm. SegmentStall and Stalled are specific verdicts
/// that the stream stopped reaching viewers and deserve their own exit code for
/// supervisor differentiation. Everything else is a failure.
fn exit_code(err: &ChannelError) -> i32 {
    match err {
        ChannelError::IdleTimeout(_) => 0,
        ChannelError::SegmentStall(_) | ChannelError::Stalled(_) => ersatztv_core::STALL_EXIT_CODE,
        _ => 1,
    }
}

/// Leave the error's text where the server will read it as this exit's cause.
/// Best effort: a worker that cannot write its own folder still exits with the
/// right code, and the server falls back to naming the exit status alone.
async fn write_exit_reason(output_folder: &Path, err: &ChannelError) {
    let path = output_folder.join(ersatztv_core::EXIT_REASON_FILE_NAME);
    if let Err(write_err) = tokio::fs::write(&path, err.to_string()).await {
        log::warn!(
            "failed to write exit reason to {}: {write_err}",
            path.display()
        );
    }
}

/// Write the exit reason for every error that exits non-zero; an idle timeout
/// is a clean end of session and leaves no file.
async fn write_exit_reason_if_failed(output_folder: &Path, err: &ChannelError) {
    if exit_code(err) != 0 {
        write_exit_reason(output_folder, err).await;
    }
}

#[tokio::main]
pub async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();

    let args = Args::parse();
    let output_folder = match &args.command {
        Commands::Run { output_folder, .. } => Some(output_folder.clone()),
        Commands::Debug { .. } => None,
    };

    if let Err(err) = run(args).await {
        match &err {
            ChannelError::IdleTimeout(_) => log::info!("{err}"),
            _ => log::error!("{err}"),
        };

        if let Some(output_folder) = &output_folder {
            write_exit_reason_if_failed(output_folder, &err).await;
        }
        std::process::exit(exit_code(&err));
    }
}

async fn run(args: Args) -> Result<(), ChannelError> {
    match args.command {
        Commands::Run {
            config_paths,
            output_folder,
            number,
            name,
            troubleshoot,
        } => {
            let channel_config =
                ChannelConfig::from_sources(&config_paths, &output_folder, &number, &name).await?;

            // start channel session
            let mut channel_session = ChannelSession::new(channel_config).await?;
            channel_session.run(troubleshoot).await
        }
        Commands::Debug { config_paths } => {
            let channel_config =
                ChannelConfig::from_sources(&config_paths, &std::env::temp_dir(), "debug", "debug")
                    .await?;

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

#[cfg(test)]
mod tests {
    use ersatztv_core::STALL_EXIT_CODE;

    use super::*;

    #[test]
    fn idle_timeout_exits_zero() {
        let err = ChannelError::IdleTimeout("test timeout".into());
        assert_eq!(exit_code(&err), 0);
    }

    #[test]
    fn segment_stall_exits_stall_code() {
        let err = ChannelError::SegmentStall("test stall".into());
        assert_eq!(exit_code(&err), STALL_EXIT_CODE);
    }

    #[test]
    fn stalled_exits_stall_code() {
        let err = ChannelError::Stalled("test stalled".into());
        assert_eq!(exit_code(&err), STALL_EXIT_CODE);
    }

    #[test]
    fn other_errors_exit_one() {
        let err = ChannelError::StreamFailure("test stream failure".into());
        assert_eq!(exit_code(&err), 1);
    }

    // `tools/channel-health.sh` shows this file's text as the cause of an exit.
    // An idle timeout exits 0 and must leave nothing behind.
    #[tokio::test]
    async fn exit_reason_is_written_for_a_failure_and_skipped_for_idle_timeout() {
        let failed = tempfile::tempdir().unwrap();
        let err = ChannelError::StreamFailure("ffmpeg died".into());
        write_exit_reason_if_failed(failed.path(), &err).await;
        let text =
            std::fs::read_to_string(failed.path().join(ersatztv_core::EXIT_REASON_FILE_NAME))
                .unwrap();
        assert_eq!(text, err.to_string());

        let idle = tempfile::tempdir().unwrap();
        let err = ChannelError::IdleTimeout("no viewers".into());
        write_exit_reason_if_failed(idle.path(), &err).await;
        assert!(
            !idle
                .path()
                .join(ersatztv_core::EXIT_REASON_FILE_NAME)
                .exists()
        );
    }
}

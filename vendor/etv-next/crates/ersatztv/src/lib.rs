use std::path::{Path, PathBuf};

use crate::error::LineupError;

pub mod config;
pub mod error;

pub async fn validate_config_path(
    parent: &Path,
    config_path: &str,
) -> Result<PathBuf, LineupError> {
    let mut channel_config = PathBuf::from(config_path);
    if channel_config.is_relative() {
        let joined = parent.join(&channel_config);
        let canonicalized = tokio::fs::canonicalize(&joined).await;
        channel_config = match canonicalized {
            Ok(canonical) => canonical,
            _ => joined,
        };
    }

    if !tokio::fs::try_exists(&channel_config)
        .await
        .unwrap_or(false)
    {
        return Err(LineupError::ChannelConfigDoesNotExist(format!(
            "{:?}",
            channel_config
        )));
    }

    Ok(channel_config)
}

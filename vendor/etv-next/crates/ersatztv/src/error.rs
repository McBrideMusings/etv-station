use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum LineupError {
    #[error("io error {0}")]
    Io(#[from] std::io::Error),

    #[error("unable to load lineup config: {0}")]
    LineupConfigFailure(String),

    #[error("unable to locate parent of lineup.json")]
    LineupConfigNoParent,

    #[error("channel config does not exist at path: {0}")]
    ChannelConfigDoesNotExist(String),

    #[error("unable to read channel config at {path}: {source}")]
    ChannelConfigRead {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("unable to parse channel config at {path}: {source}")]
    ChannelConfigParse {
        path: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("unable to expand tilde in playout folder {0}")]
    PlayoutFolderExpand(String),

    #[error("unable to resolve playout folder {path}: {source}")]
    PlayoutFolderResolve {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    PathResolve(#[from] ersatztv_core::PathResolveError),

    #[error("no channels have been loaded; please review your lineup config")]
    NoChannelsLoaded,

    #[error("unable to locate channel binary")]
    ChannelBinaryNotFound,

    #[error("unable to locate channel binary; tried {0}")]
    ChannelBinaryNotFoundAtPath(String),

    #[error("unable to find channel with number {0}")]
    ChannelNotFound(String),

    #[error("channel is not yet ready")]
    ChannelNotReady,

    #[error("channel {0} has failed repeatedly and is not currently serving")]
    ChannelFailed(String),

    #[error("unable to locate parent of lineup.json")]
    ScaffoldNoParent,

    #[error("the following paths already exist: {0}")]
    ScaffoldPathsExist(String),

    #[error("failed to serialize JSON")]
    ScaffoldSerializeError(#[from] serde_json::Error),

    #[error("channel already exists")]
    ScaffoldChannelExists,

    #[error("error joining task")]
    JoinError(#[from] tokio::task::JoinError),

    #[error("xmltv generation failed: {0}")]
    XmltvFailure(String),
}

impl IntoResponse for LineupError {
    fn into_response(self) -> Response {
        match self {
            LineupError::ChannelNotFound(_) => {
                (StatusCode::NOT_FOUND, self.to_string()).into_response()
            }
            LineupError::ChannelNotReady => (
                StatusCode::SERVICE_UNAVAILABLE,
                [(axum::http::header::RETRY_AFTER, "5")],
                "channel not ready",
            )
                .into_response(),
            // Same status as not-ready, but a much longer Retry-After: this
            // channel has failed every attempt, so a client polling every few
            // seconds only adds load to something already broken. Retries do
            // continue server-side, so the hint is "come back later", not "gone".
            LineupError::ChannelFailed(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                [(axum::http::header::RETRY_AFTER, "30")],
                self.to_string(),
            )
                .into_response(),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()).into_response(),
        }
    }
}

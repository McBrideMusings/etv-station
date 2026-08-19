use ersatztv_playout::error::PlayoutError;
use ffpipeline::error::FFPipelineError;
use thiserror::Error;
use time::OffsetDateTime;

#[derive(Error, Debug)]
pub enum ChannelError {
    #[error("unable to load channel config: {0}")]
    ChannelConfigFailure(String),

    #[error("unable to load channel config (io): {0}")]
    ChannelConfigIoFailure(#[from] std::io::Error),

    #[error("failed to expand playout folder")]
    ChannelConfigExpandPlayoutFolder,

    #[error("failed to expand output folder")]
    ChannelConfigExpandOutputFolder,

    #[error("channel config output folder is required")]
    ChannelConfigOutputFolderRequired,

    #[error("channel startup error: {0}")]
    ChannelStartup(String),

    #[error("date formatting error: {0}")]
    ChannelDateFormatError(#[from] time::error::Format),

    #[error("Indeterminate local time offset: {0}")]
    DateOffsetError(#[from] time::error::IndeterminateOffset),

    #[error("{0}")]
    PlayoutJsonLoadFailure(#[from] PlayoutError),

    #[error("unable to find playout JSON file for time {0}")]
    PlayoutJsonNoFileForTime(OffsetDateTime),

    #[error("unable to find current item in playout JSON")]
    PlayoutJsonNoItem { next_start: Option<OffsetDateTime> },

    // This value got pushed down into another module (pipeline)
    // See if there is a way to port this over
    // #[error("local source is invalid for playout item")]
    // PlayoutJsonInvalidLocalSource,
    #[error("audio source is required for playout item")]
    PlayoutJsonAudioSourceRequired,

    #[error("vudei source is required for playout item")]
    PlayoutJsonVideoSourceRequired,

    #[error("{0}")]
    PipelineError(#[from] FFPipelineError),

    #[error("stream failed: {0}")]
    StreamFailure(String),

    #[error("failed to scan for last pts")]
    PtsScannerFailure,

    #[error("channel {0} terminated after idle timeout")]
    IdleTimeout(String),

    #[error("channel {0} terminated after producing no segments while being watched")]
    SegmentStall(String),

    #[error("channel {0} terminated after ffmpeg stall")]
    Stalled(String),

    /// The item was still playing correctly, but the transcode had fallen far
    /// enough behind wall clock that the remaining buffer would have run out.
    ///
    /// Not a failure of the item, and deliberately never surfaced to a viewer:
    /// the session resumes the same item from where it got to, with a fresh
    /// initial burst to rebuild the lead. Treating it as a failure would replace
    /// a perfectly good film with black and silence for the rest of its slot.
    #[error("lead exhausted while the item was still playing; restarting it")]
    LeadExhausted,

    #[error("failed to capture ffmpeg stderr")]
    CaptureFFmpegStderrFailure,

    #[error("failed to capture ffmpeg stdout")]
    CaptureFFmpegStdoutFailure,

    #[error("dynamic source is required")]
    DynamicSourceRequired,

    #[error("dynamic source cannot be played directly")]
    DynamicSourceCannotBePlayedDirectly,

    #[error("dynamic source failure: {0}")]
    DynamicSourceFailure(String),

    #[error("dynamic source has no remaining time in window")]
    DynamicSourceNoRemainingTime,

    #[error("dynamic sources cannot return dynamic sources")]
    DynamicSourceCannotRecurse,

    #[error("probe hint failure")]
    ProbeHintFailure,
}

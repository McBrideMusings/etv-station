use std::time::{Duration, Instant};

use tokio::fs::{create_dir_all, read_dir, remove_dir, remove_file};

mod merge;
mod path_resolve;

pub use merge::deep_merge;
pub use path_resolve::resolve_relative_paths;

pub const READY_FILE_NAME: &str = ".ready";
pub const READY_FILE_TIMEOUT: Duration = Duration::from_secs(30);

pub const HEARTBEAT_FILE_NAME: &str = ".heartbeat";
pub const HEARTBEAT_FILE_TIMEOUT: Duration = Duration::from_secs(90);

/// Touched by the channel worker before it opens a live overlay's rawvideo fifo
/// for read. Its presence tells the overlay producer (the separate etv-station
/// daemon) that this channel is now being watched and its overlay process
/// should be spawned — closing the chicken-and-egg where ffmpeg would otherwise
/// block opening a fifo that has no writer yet.
pub const OVERLAY_WANTED_FILE_NAME: &str = ".overlay-wanted";

/// Written by the overlay producer once its renderer is warm and it has written
/// its first frame. The channel worker waits for this before opening the fifo so
/// the first read can't land mid-frame during overlay cold-start.
pub const OVERLAY_READY_FILE_NAME: &str = ".overlay-ready";

/// How long the channel worker waits for [`OVERLAY_READY_FILE_NAME`] before
/// going ahead and opening the fifo anyway. On a cold start the marker cannot
/// appear until a reader has attached (the producer writes its first frame,
/// then touches the marker), so this wait routinely expires on the first item;
/// [`OVERLAY_WRITER_TIMEOUT`] is what actually bounds the wait for a writer.
pub const OVERLAY_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the channel worker holds the overlay fifo open waiting for the
/// producer to attach and write its first frame. Must comfortably exceed the
/// producer's own spawn latency (etv-station's overlay supervisor polls for
/// [`OVERLAY_WANTED_FILE_NAME`] on a 5s interval, then has to warm a renderer).
/// When it expires the item fails and is replaced with black/silence — a
/// bounded, reported failure instead of a channel wedged forever on an open
/// that never returns.
pub const OVERLAY_WRITER_TIMEOUT: Duration = Duration::from_secs(20);

/// How long to wait between polls while waiting for an overlay fifo writer.
/// A read end with no writer reports POLLHUP immediately and forever, so the
/// poll has to be paced or it spins.
pub const OVERLAY_WRITER_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub const VERSION: &str = env!("ETV_VERSION_STRING");

pub async fn empty_folder(output_folder: &std::path::Path) -> Result<(), std::io::Error> {
    if !output_folder.exists() {
        create_dir_all(output_folder).await?;
    }

    let mut entries = read_dir(output_folder).await?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        if let Ok(file_type) = entry.file_type().await {
            if file_type.is_dir() {
                Box::pin(empty_folder(&entry.path())).await?;
                remove_dir(entry.path()).await?;
            } else {
                remove_file(entry.path()).await?;
            }
        }
    }

    Ok(())
}

pub async fn wait_for_file(path: &std::path::Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if path.exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await
    }
}

use std::time::Duration;

use tokio::fs::{create_dir_all, read_dir, remove_dir, remove_file};

mod merge;
mod path_resolve;

pub use merge::deep_merge;
pub use path_resolve::resolve_relative_paths;

pub const READY_FILE_NAME: &str = ".ready";
pub const READY_FILE_TIMEOUT: Duration = Duration::from_secs(30);

pub const HEARTBEAT_FILE_NAME: &str = ".heartbeat";
pub const HEARTBEAT_FILE_TIMEOUT: Duration = Duration::from_secs(90);

/// Carries a session's HLS sequence counters across a worker restart, so a new
/// worker numbers its first segment above the last one the previous worker
/// published.
///
/// A client polls one URL — `/session/{channel}/live.m3u8` — for the life of its
/// tune-in, and RFC 8216 §6.2.1 requires `EXT-X-MEDIA-SEQUENCE` never to
/// decrease across that URL's lifetime. But the worker is a process that exits
/// and respawns under a client that never noticed, and each respawn wipes the
/// output folder ([`empty_folder`]) and builds a fresh
/// `PlaylistManager` starting at zero. The in-memory monotonic clamp there only
/// spans one process, which is the one span where the counter was never going to
/// go backwards anyway.
///
/// So the counters live here instead, in the session folder, which outlives any
/// one worker. [`empty_folder`] preserves this file by name for that reason: the
/// wipe clears the session's *media*, not its *identity*.
pub const SEQUENCE_FILE_NAME: &str = ".sequence";

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

/// Exit code the channel worker uses when it gives up because the stream stopped
/// reaching the viewer — [`ChannelError::SegmentStall`] or
/// [`ChannelError::Stalled`]. Every other failure still exits 1.
///
/// It exists because the server cannot otherwise tell those apart from an
/// ordinary idle exit, and it was guessing: it treated any run longer than three
/// minutes as healthy. A session that sat alive for four minutes handing out
/// nothing then cleared its own failure count, so the backoff never engaged and
/// the channel respawned on a loop — each respawn wiping the HLS folder and
/// resetting segment numbering under a client that was still asking for the old
/// numbers. The worker knows which of the two happened; this carries that
/// verdict across the process boundary instead of having the server re-derive it
/// from a clock.
///
/// [`ChannelError::SegmentStall`]: ../ersatztv_channel/error/enum.ChannelError.html
/// [`ChannelError::Stalled`]: ../ersatztv_channel/error/enum.ChannelError.html
pub const STALL_EXIT_CODE: i32 = 75;

pub const VERSION: &str = env!("ETV_VERSION_STRING");

/// Empty a session's output folder, keeping the folder itself and
/// [`SEQUENCE_FILE_NAME`].
///
/// The sequence file survives because it is the one thing in here that is not
/// media: it records how far this session's segment numbering has already got,
/// and a client still polling the session URL holds the other half of that
/// agreement. Deleting it is what let a respawn renumber from zero underneath a
/// player that had already consumed segment 150 — the player then discards every
/// lower-numbered segment the new worker offers and freezes on its last decoded
/// frame. See [`SEQUENCE_FILE_NAME`].
pub async fn empty_folder(output_folder: &std::path::Path) -> Result<(), std::io::Error> {
    if !output_folder.exists() {
        create_dir_all(output_folder).await?;
    }

    let mut entries = read_dir(output_folder).await?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_name() == std::ffi::OsStr::new(SEQUENCE_FILE_NAME) {
            continue;
        }
        if let Ok(file_type) = entry.file_type().await {
            if file_type.is_dir() {
                Box::pin(empty_folder(&entry.path())).await?;
                // The recursion above deliberately preserves SEQUENCE_FILE_NAME,
                // so a child that had one is still non-empty and cannot be
                // removed. That is not a failure: it is this function's own
                // preservation rule observed one level up, and the folder is a
                // live session's identity that the next worker will reuse.
                //
                // Treating it as fatal took the whole server down at startup —
                // `main` empties the output root, which recurses into every
                // channel session folder, keeps each `.sequence`, and then tried
                // to rmdir them. Any channel whose media had already been
                // cleared left a folder containing nothing but `.sequence`, and
                // the process exited with "Directory not empty (os error 39)"
                // before serving anything. One leftover dotfile could take the
                // entire lineup offline until somebody deleted it by hand.
                match remove_dir(entry.path()).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
                    Err(e) => return Err(e),
                }
            } else {
                remove_file(entry.path()).await?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod empty_folder_tests {
    use super::{SEQUENCE_FILE_NAME, empty_folder};

    /// The sequence file is the session's identity, not its media, and a client
    /// still polling the session URL depends on it outliving the wipe. Delete it
    /// and the next worker renumbers from zero under that client, which freezes
    /// on its last decoded frame.
    #[tokio::test]
    async fn keeps_the_sequence_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        tokio::fs::write(root.join("live000000.ts"), b"segment")
            .await
            .unwrap();
        tokio::fs::write(root.join(SEQUENCE_FILE_NAME), b"{}")
            .await
            .unwrap();

        empty_folder(root).await.unwrap();

        assert!(!root.join("live000000.ts").exists(), "media should be gone");
        assert!(
            root.join(SEQUENCE_FILE_NAME).exists(),
            "the sequence file must survive the wipe"
        );
    }

    /// The case this exists for: a channel's HLS output folder — `.ts`
    /// segments, `.vtt` sidecars, and nested report directories — must come
    /// back empty, with the folder itself still present so the next writer
    /// can use it immediately.
    #[tokio::test]
    async fn removes_files_and_nested_directories_but_keeps_the_folder_itself() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        tokio::fs::write(root.join("live000000.ts"), b"segment")
            .await
            .unwrap();
        tokio::fs::write(root.join("live000000.vtt"), b"cues")
            .await
            .unwrap();
        tokio::fs::create_dir(root.join("nested")).await.unwrap();
        tokio::fs::write(root.join("nested").join("inner.txt"), b"x")
            .await
            .unwrap();

        empty_folder(root).await.unwrap();

        assert!(root.exists(), "the folder itself must survive the wipe");
        let mut entries = tokio::fs::read_dir(root).await.unwrap();
        assert!(
            entries.next_entry().await.unwrap().is_none(),
            "no files or directories should remain"
        );
    }

    /// The startup shape, and the regression this guards: `main` empties the
    /// output ROOT, which recurses into each channel's session folder. Those
    /// folders keep their `.sequence`, so they are still non-empty when the
    /// recursion returns and tries to remove them.
    ///
    /// This used to abort the whole wipe with "Directory not empty (os error
    /// 39)" and take the server down before it served anything — on 2026-08-20
    /// two channels whose media had already been cleared left folders holding
    /// nothing but `.sequence`, and the container restart-looped until the files
    /// were deleted by hand. A leftover dotfile must never cost the lineup.
    #[tokio::test]
    async fn a_child_session_folder_keeping_its_sequence_file_does_not_fail_the_wipe() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Channel 5: media already cleared, only the sequence file left. This is
        // the exact shape that took production down.
        let ch5 = root.join("5");
        tokio::fs::create_dir(&ch5).await.unwrap();
        tokio::fs::write(ch5.join(SEQUENCE_FILE_NAME), b"{}")
            .await
            .unwrap();

        // Channel 9: still has media alongside its sequence file.
        let ch9 = root.join("9");
        tokio::fs::create_dir(&ch9).await.unwrap();
        tokio::fs::write(ch9.join(SEQUENCE_FILE_NAME), b"{}")
            .await
            .unwrap();
        tokio::fs::write(ch9.join("live000042.ts"), b"segment")
            .await
            .unwrap();

        // A genuinely stale folder with no identity to preserve still goes.
        let stale = root.join("77");
        tokio::fs::create_dir(&stale).await.unwrap();
        tokio::fs::write(stale.join("live000001.ts"), b"segment")
            .await
            .unwrap();

        empty_folder(root).await.unwrap();

        assert!(
            ch5.join(SEQUENCE_FILE_NAME).exists(),
            "channel 5 keeps its identity across the wipe"
        );
        assert!(
            ch9.join(SEQUENCE_FILE_NAME).exists(),
            "channel 9 keeps its identity across the wipe"
        );
        assert!(
            !ch9.join("live000042.ts").exists(),
            "channel 9's media is still cleared"
        );
        assert!(
            !stale.exists(),
            "a folder with nothing to preserve is still removed"
        );
    }

    /// A channel that has never run yet (or whose output folder was removed
    /// out of band) has no folder to empty. This is the same case
    /// `prep_output_folder` relies on at session start, and the exit-time
    /// cleanup added in #235 hits it too whenever a worker exits before ever
    /// producing a segment.
    #[tokio::test]
    async fn creates_the_folder_when_it_does_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("does-not-exist-yet");

        empty_folder(&root).await.unwrap();

        assert!(root.is_dir());
    }
}

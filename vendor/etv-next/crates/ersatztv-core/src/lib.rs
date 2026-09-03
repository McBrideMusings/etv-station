use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::fs::{create_dir_all, read_dir, remove_dir, remove_dir_all, remove_file};

mod merge;
mod path_resolve;

pub use merge::deep_merge;
pub use path_resolve::{PathResolveError, resolve_relative_paths};

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
/// and respawns under a client that never noticed, and each respawn writes into
/// a fresh per-run segment folder ([`new_run_folder_name`]) and builds a fresh
/// `PlaylistManager` starting at zero. The in-memory monotonic clamp there only
/// spans one process, which is the one span where the counter was never going to
/// go backwards anyway.
///
/// So the counters live here instead, in the session folder, which outlives any
/// one worker run. This file sits at the channel root, a sibling of every run
/// folder, so it survives [`reap_run_folders`] regardless of which run folders
/// that reap removes: the reap clears a session's *media*, not its *identity*.
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

/// Prefix every run-folder name carries, so [`is_run_folder_name`] can tell one
/// from an ordinary file at the channel root (`live.m3u8`, `.sequence`,
/// `.heartbeat`, `.ready`, `.error-card.png`) without a registry.
pub const RUN_FOLDER_PREFIX: &str = "r";

/// Process-local counter mixed into [`new_run_folder_name`] so two calls made
/// by the *same process* within the same millisecond still sort distinctly.
/// It disambiguates nothing across processes — each worker run is its own OS
/// process, so this `AtomicU64` starts back at 0 every time and a second run
/// starting in the same millisecond as another gets counter value 0 again.
/// What actually keeps two live runs of one channel from colliding is that
/// only one worker per channel ever exists at a time — enforced by the
/// `active` map in the server — combined with the millisecond timestamp
/// itself: two runs of the *same* channel are never both live, and two
/// different channels write to different channel folders, so a same-process,
/// same-millisecond collision is only reachable within a single call site
/// making several calls in a row, which is exactly the case the tests exercise.
static RUN_FOLDER_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A new, unique name for a worker run's own segment subfolder under a
/// channel's HLS output folder.
///
/// Each worker run gets its own subfolder so a respawn never has to delete
/// segments a client's in-hand playlist still names — see the module-level
/// rationale on [`SEQUENCE_FILE_NAME`] and the per-run design in
/// `etv-station-262`. The name is a fixed-width millisecond timestamp plus a
/// zero-padded counter, in that order, so plain string comparison already
/// equals creation order (`sort()` needs no parsing). The counter only
/// disambiguates repeated calls within the same process and the same
/// millisecond — realistic under test — since a fresh process starts it back
/// at 0; two live runs of one channel are kept apart instead by there only
/// ever being one worker per channel (the `active` map) and the timestamp
/// component. See [`is_run_folder_name`] for the inverse check.
pub fn new_run_folder_name() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let counter = RUN_FOLDER_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{RUN_FOLDER_PREFIX}{millis:013}-{counter:04}")
}

/// Whether `name` is shaped like a value [`new_run_folder_name`] could have
/// produced: the prefix, then exactly 13 ASCII digits, a `-`, then exactly 4
/// ASCII digits.
///
/// This is what keeps [`list_run_folders`] and [`reap_run_folders`] from ever
/// treating `.sequence`, `.ready`, `.heartbeat`, `live.m3u8`, or
/// `.error-card.png` — every non-run-folder thing that lives at the channel
/// root — as a candidate to remove.
pub fn is_run_folder_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(RUN_FOLDER_PREFIX) else {
        return false;
    };
    let Some((millis, counter)) = rest.split_once('-') else {
        return false;
    };
    millis.len() == 13
        && millis.bytes().all(|b| b.is_ascii_digit())
        && counter.len() == 4
        && counter.bytes().all(|b| b.is_ascii_digit())
}

/// Every run folder directly under `channel_folder`, oldest first.
///
/// Ascending order is what lets [`reap_run_folders`] treat "the last one" as
/// "the live or most-recently-exited run" with no separate bookkeeping.
pub async fn list_run_folders(channel_folder: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut result = Vec::new();

    let mut entries = read_dir(channel_folder).await?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Ok(file_type) = entry.file_type().await else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if is_run_folder_name(&name) {
            result.push(entry.path());
        }
    }

    result.sort();
    Ok(result)
}

/// Remove dead run folders under `channel_folder`. When `keep_newest` is
/// `true` the single newest run folder (by [`list_run_folders`]'s ascending
/// order) is left alone; every other run folder is removed recursively.
///
/// Pure mechanics only: this function has no opinion on whether a worker is
/// live or a viewer is attached — the caller decides `keep_newest` from
/// whatever it already knows (an `active` map entry, a fresh heartbeat) and
/// this just acts on that verdict. Returns how many run folders were removed.
pub async fn reap_run_folders(
    channel_folder: &Path,
    keep_newest: bool,
) -> Result<usize, std::io::Error> {
    let mut folders = list_run_folders(channel_folder).await?;
    if keep_newest {
        folders.pop();
    }

    let mut removed = 0;
    for folder in folders {
        remove_dir_all(&folder).await?;
        removed += 1;
    }

    Ok(removed)
}

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
///
/// A worker respawn no longer calls this — see [`new_run_folder_name`] and
/// [`reap_run_folders`] for that path. What remains is the server's cold start
/// (`main` empties the whole output root once, before anything is watching) and
/// the sequence-file preservation behavior this module's tests pin.
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

#[cfg(test)]
mod run_folder_tests {
    use super::{is_run_folder_name, list_run_folders, new_run_folder_name, reap_run_folders};

    /// The direct guarantee two runs on the same channel depend on: neither can
    /// ever produce the same segment path, because their run folders never
    /// collide, and plain string order already matches creation order so a
    /// caller never has to parse the name back apart to sort it.
    #[test]
    fn two_consecutive_run_folder_names_differ_and_sort_in_creation_order() {
        let a = new_run_folder_name();
        let b = new_run_folder_name();

        assert_ne!(a, b, "two runs must never share a segment folder");

        let mut sorted = [b.clone(), a.clone()];
        sorted.sort();
        assert_eq!(
            sorted,
            [a, b],
            "plain string sort must already equal creation order"
        );
    }

    #[test]
    fn recognizes_only_run_folder_shaped_names() {
        assert!(is_run_folder_name(&new_run_folder_name()));
        assert!(is_run_folder_name("r1234567890123-0007"));

        for not_a_run_folder in [
            ".sequence",
            ".ready",
            ".heartbeat",
            "live.m3u8",
            "live_sub.m3u8",
            ".error-card.png",
            "r123-0000",           // millis too short
            "r1234567890123",      // no counter
            "x1234567890123-0000", // wrong prefix
        ] {
            assert!(
                !is_run_folder_name(not_a_run_folder),
                "{not_a_run_folder:?} must not be recognized as a run folder"
            );
        }
    }

    #[tokio::test]
    async fn lists_only_run_folders_in_creation_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        tokio::fs::write(root.join("live.m3u8"), b"").await.unwrap();
        tokio::fs::write(root.join(".sequence"), b"{}")
            .await
            .unwrap();

        let older = root.join("r0000000000001-0000");
        let newer = root.join("r0000000000002-0000");
        tokio::fs::create_dir(&newer).await.unwrap();
        tokio::fs::create_dir(&older).await.unwrap();

        let found = list_run_folders(root).await.unwrap();

        assert_eq!(found, vec![older, newer]);
    }

    #[tokio::test]
    async fn keep_newest_removes_every_run_folder_but_the_last() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let older = root.join("r0000000000001-0000");
        let newer = root.join("r0000000000002-0000");
        tokio::fs::create_dir(&older).await.unwrap();
        tokio::fs::create_dir(&newer).await.unwrap();
        tokio::fs::write(newer.join("live000000.ts"), b"segment")
            .await
            .unwrap();

        let removed = reap_run_folders(root, true).await.unwrap();

        assert_eq!(removed, 1);
        assert!(!older.exists(), "the dead run folder must be gone");
        assert!(newer.exists(), "the newest run folder must survive");
        assert!(newer.join("live000000.ts").exists());
    }

    #[tokio::test]
    async fn not_keeping_newest_removes_every_run_folder() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let older = root.join("r0000000000001-0000");
        let newer = root.join("r0000000000002-0000");
        tokio::fs::create_dir(&older).await.unwrap();
        tokio::fs::create_dir(&newer).await.unwrap();

        let removed = reap_run_folders(root, false).await.unwrap();

        assert_eq!(removed, 2);
        assert!(!older.exists());
        assert!(!newer.exists());
    }
}

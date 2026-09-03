use std::collections::HashSet;
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

/// The generated, client-facing playlist a channel worker writes at the
/// channel root — see `ersatztv-channel`'s `ChannelSession::new`. Shared as a
/// constant so [`run_folders_named_by_playlists`]'s reader and the worker's
/// writer cannot drift apart.
pub const LIVE_PLAYLIST_FILE_NAME: &str = "live.m3u8";

/// The sibling subtitle playlist a channel worker writes only when
/// `SubtitleMode::Convert` is configured — absent under `SubtitleMode::Burn`,
/// where subtitles are burned into the video picture instead. Its absence is
/// not an error; see [`run_folders_named_by_playlists`].
pub const LIVE_SUBTITLE_PLAYLIST_FILE_NAME: &str = "live_sub.m3u8";

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
///
/// This is correct only at **exit time**, where the run that just exited IS
/// the newest folder that exists yet — see `ChannelSession::spawn`'s
/// exit-time cleanup in `ersatztv`. It is the wrong rule for a **periodic**
/// sweep: between one worker's exit and its replacement's first segment,
/// both the dead run a client's in-hand playlist still names and the new
/// run can exist at once, and "keep the newest" collects the wrong one out
/// from under that client (etv-station-262.1). A periodic sweep wants
/// [`reap_unreferenced_run_folders`] instead.
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

/// Every run-folder name referenced by the generated playlists at
/// `channel_folder`'s root — [`LIVE_PLAYLIST_FILE_NAME`] and, when present,
/// [`LIVE_SUBTITLE_PLAYLIST_FILE_NAME`].
///
/// A generated playlist's media lines are bare `<run-folder>/liveNNNNNN.ts`
/// URIs (see `ersatztv-channel`'s `PlaylistManager`), so the run folder a
/// line names is the path segment before its first `/`. Blank lines and `#`
/// tag/comment lines are skipped. A missing or unreadable playlist
/// contributes nothing and is not an error — a channel that has never run
/// has no `live.m3u8` yet, and `live_sub.m3u8` only exists under
/// `SubtitleMode::Convert`.
///
/// This is a line-shape reader, not an HLS parser: a future playlist line
/// that carries a run-folder-shaped path inside a tag attribute (e.g. an
/// `EXT-X-MAP` URI) rather than as a bare media URI would be missed. The
/// generator today emits only bare segment URIs plus standard tags, so this
/// holds; it is not a general M3U8 parser.
pub async fn run_folders_named_by_playlists(channel_folder: &Path) -> HashSet<String> {
    let mut referenced = HashSet::new();

    for file_name in [LIVE_PLAYLIST_FILE_NAME, LIVE_SUBTITLE_PLAYLIST_FILE_NAME] {
        let Ok(contents) = tokio::fs::read_to_string(channel_folder.join(file_name)).await else {
            continue;
        };
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let run_folder_name = line.split('/').next().unwrap_or(line);
            if is_run_folder_name(run_folder_name) {
                referenced.insert(run_folder_name.to_owned());
            }
        }
    }

    referenced
}

/// Remove run folders under `channel_folder` that no served playlist names.
///
/// This is the periodic sweep's reap primitive — see [`reap_run_folders`]'s
/// doc for why that one is wrong here. A run folder is collectable when
/// nothing a client could currently be polling names it: `live.m3u8` and
/// `live_sub.m3u8` at the channel root are that authority, since every
/// segment URI they carry leads with its run folder. `keep_newest` still
/// protects the single newest folder on top of that — a just-spawned
/// worker's run folder is named by no playlist yet (its `PlaylistManager`
/// starts with an empty deque), and only `keep_newest` keeps it alive until
/// it has written its first segment. Returns how many run folders were
/// removed.
///
/// Residual, accepted rather than engineered away: a client holding a
/// playlist copy fetched before the current one can still name a folder the
/// current playlist no longer does. That is bounded by one poll interval —
/// about four seconds, the target segment duration — against a sweep that
/// runs every sixty, so it is not the failure mode this function exists to
/// close. No count and no age threshold are used to widen the margin further
/// — both are guesses about how many runs can stack up or how long a grace
/// period should be, and etv-station-262.1 explicitly rules both out.
pub async fn reap_unreferenced_run_folders(
    channel_folder: &Path,
    keep_newest: bool,
) -> Result<usize, std::io::Error> {
    let mut folders = list_run_folders(channel_folder).await?;
    if keep_newest {
        folders.pop();
    }

    let referenced = run_folders_named_by_playlists(channel_folder).await;

    let mut removed = 0;
    for folder in folders {
        let Some(name) = folder.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if referenced.contains(name) {
            continue;
        }
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
    use super::{
        is_run_folder_name, list_run_folders, new_run_folder_name, reap_run_folders,
        reap_unreferenced_run_folders, run_folders_named_by_playlists,
    };

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

    #[tokio::test]
    async fn playlist_references_ignore_tag_and_comment_lines() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        tokio::fs::write(
            root.join("live.m3u8"),
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:4.0,\nr0000000000001-0000/live000000.ts\n",
        )
        .await
        .unwrap();

        let referenced = run_folders_named_by_playlists(root).await;

        assert_eq!(referenced.len(), 1);
        assert!(referenced.contains("r0000000000001-0000"));
    }

    #[tokio::test]
    async fn playlist_references_ignore_non_run_folder_shaped_lines() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        tokio::fs::write(root.join("live.m3u8"), "not-a-run-folder/live000000.ts\n")
            .await
            .unwrap();

        let referenced = run_folders_named_by_playlists(root).await;

        assert!(referenced.is_empty());
    }

    #[tokio::test]
    async fn playlist_references_include_the_subtitle_playlist() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        tokio::fs::write(
            root.join("live.m3u8"),
            "r0000000000001-0000/live000000.ts\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            root.join("live_sub.m3u8"),
            "r0000000000002-0000/live000000.vtt\n",
        )
        .await
        .unwrap();

        let referenced = run_folders_named_by_playlists(root).await;

        assert_eq!(referenced.len(), 2);
        assert!(referenced.contains("r0000000000001-0000"));
        assert!(referenced.contains("r0000000000002-0000"));
    }

    #[tokio::test]
    async fn a_missing_playlist_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let referenced = run_folders_named_by_playlists(root).await;

        assert!(referenced.is_empty());
    }

    #[tokio::test]
    async fn reap_unreferenced_keeps_a_folder_a_playlist_still_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let referenced_older = root.join("r0000000000001-0000");
        let unreferenced_newer = root.join("r0000000000002-0000");
        tokio::fs::create_dir(&referenced_older).await.unwrap();
        tokio::fs::create_dir(&unreferenced_newer).await.unwrap();
        tokio::fs::write(
            root.join("live.m3u8"),
            "r0000000000001-0000/live000000.ts\n",
        )
        .await
        .unwrap();

        // keep_newest = true, so the newest folder (unreferenced) is also
        // protected here; this test's point is the OLDER, referenced one.
        let removed = reap_unreferenced_run_folders(root, true).await.unwrap();

        assert_eq!(removed, 0);
        assert!(
            referenced_older.exists(),
            "a folder a playlist still names must survive"
        );
        assert!(unreferenced_newer.exists());
    }

    #[tokio::test]
    async fn reap_unreferenced_removes_a_folder_no_playlist_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let unreferenced_older = root.join("r0000000000001-0000");
        let referenced_newer = root.join("r0000000000002-0000");
        tokio::fs::create_dir(&unreferenced_older).await.unwrap();
        tokio::fs::create_dir(&referenced_newer).await.unwrap();
        tokio::fs::write(
            root.join("live.m3u8"),
            "r0000000000002-0000/live000000.ts\n",
        )
        .await
        .unwrap();

        let removed = reap_unreferenced_run_folders(root, true).await.unwrap();

        assert_eq!(removed, 1);
        assert!(
            !unreferenced_older.exists(),
            "a folder no playlist names must be reaped"
        );
        assert!(referenced_newer.exists());
    }

    #[tokio::test]
    async fn reap_unreferenced_keep_newest_protects_a_brand_new_worker_with_no_segments_yet() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let dead_run = root.join("r0000000000001-0000");
        let brand_new_run = root.join("r0000000000002-0000");
        tokio::fs::create_dir(&dead_run).await.unwrap();
        tokio::fs::create_dir(&brand_new_run).await.unwrap();
        // live.m3u8 still names the dead run — the new worker has not
        // written its first segment yet, so no playlist names brand_new_run.
        tokio::fs::write(
            root.join("live.m3u8"),
            "r0000000000001-0000/live000000.ts\n",
        )
        .await
        .unwrap();

        let removed = reap_unreferenced_run_folders(root, true).await.unwrap();

        assert_eq!(removed, 0);
        assert!(dead_run.exists(), "still named by the current playlist");
        assert!(
            brand_new_run.exists(),
            "keep_newest must protect a worker that has not produced a segment yet"
        );
    }
}

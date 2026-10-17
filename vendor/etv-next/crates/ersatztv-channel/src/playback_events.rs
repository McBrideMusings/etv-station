//! The channel worker's playback log: what it is airing, as JSON lines.
//!
//! The worker appends to [`PLAYBACK_EVENTS_FILE_NAME`] in the playout folder.
//! A reader that wants playback signals tails that file instead of inferring
//! them from `.heartbeat` mtimes. Every write is best effort: a worker that
//! cannot write its log still streams.
//!
//! `ItemStarted` is written when the worker begins encoding an item, which
//! leads what a viewer sees by the worker's transcode lead. `start` and
//! `finish` are the item's scheduled airing times.

use std::io::Write;
use std::path::Path;

use ersatztv_core::PLAYBACK_EVENTS_FILE_NAME;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Past this size the log is dropped when a worker run starts, so it cannot
/// grow without bound across weeks of sessions.
const MAX_LOG_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PlaybackEvent {
    /// A worker run began: a viewer tuned in.
    SessionStarted {
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
        channel: String,
    },
    /// The worker began encoding a scheduled item.
    ItemStarted {
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
        channel: String,
        item_id: String,
        #[serde(with = "time::serde::rfc3339")]
        start: OffsetDateTime,
        #[serde(with = "time::serde::rfc3339")]
        finish: OffsetDateTime,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// The worker run ended: the last viewer left, or the run failed. A worker
    /// killed by a signal writes nothing.
    SessionStopped {
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
        channel: String,
    },
}

/// Append `event` as one line to the log in `folder`. One `write_all` of a
/// whole line on an `O_APPEND` file, so the worker and a concurrent reader
/// never see half a line.
pub fn append(folder: &Path, event: &PlaybackEvent) {
    let path = folder.join(PLAYBACK_EVENTS_FILE_NAME);
    if matches!(event, PlaybackEvent::SessionStarted { .. })
        && std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG_BYTES)
    {
        let _ = std::fs::remove_file(&path);
    }
    let mut line = match serde_json::to_string(event) {
        Ok(line) => line,
        Err(e) => {
            log::warn!("failed to encode playback event: {e}");
            return;
        }
    };
    line.push('\n');
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
    if let Err(e) = written {
        log::warn!("failed to append playback event to {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    fn item(id: &str) -> PlaybackEvent {
        PlaybackEvent::ItemStarted {
            at: datetime!(2026-01-01 12:00:00 UTC),
            channel: "7".into(),
            item_id: id.into(),
            start: datetime!(2026-01-01 12:00:00 UTC),
            finish: datetime!(2026-01-01 12:30:00 UTC),
            title: Some("Cheers".into()),
        }
    }

    #[test]
    fn events_append_as_one_json_line_each_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &item("a"));
        append(dir.path(), &item("b"));
        let text = std::fs::read_to_string(dir.path().join(PLAYBACK_EVENTS_FILE_NAME)).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with(r#"{"event":"item_started","at":"2026-01-01T12:00:00Z""#));
        let back: PlaybackEvent = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(back, item("b"));
    }

    #[test]
    fn a_session_start_drops_an_oversized_log_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PLAYBACK_EVENTS_FILE_NAME);
        std::fs::write(&path, vec![b'x'; MAX_LOG_BYTES as usize + 1]).unwrap();
        append(
            dir.path(),
            &PlaybackEvent::SessionStarted {
                at: datetime!(2026-01-01 12:00:00 UTC),
                channel: "7".into(),
            },
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.starts_with(r#"{"event":"session_started""#));
    }
}

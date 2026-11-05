//! What the daemon's last config reload came to, left in a file beside the
//! pid file so another process (the web dashboard) can read it.
//!
//! A SIGHUP reload has three outcomes the daemon logs at three different
//! places: the new config prepared (`config.reload`, possibly with channels the
//! loader dropped because their files failed to load), the new config failed to
//! parse and the old one kept running (`config.reload_failed`), or it parsed
//! but would not prepare and the last-known-good config came back
//! (`config.reload_reverted`). Each one writes here, with a sequence number
//! that rises on every reload, so a caller that signalled the daemon can tell
//! its own reload from an earlier one.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// A channel config the loader left out of the lineup on this reload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroppedChannel {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The new config is running. `dropped` lists channels it left out.
    Applied,
    /// The new config did not parse; the previous one is still running.
    Failed,
    /// The new config parsed but would not prepare; the last-known-good
    /// config is running.
    Reverted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReloadStatus {
    /// Rises on every recorded reload, across daemon restarts.
    pub seq: u64,
    pub at_unix: u64,
    pub outcome: Outcome,
    #[serde(default)]
    pub dropped: Vec<DroppedChannel>,
    #[serde(default)]
    pub error: Option<String>,
}

/// The file the status lives in, next to the daemon's pid file.
pub fn status_path(output_base: &Path) -> PathBuf {
    output_base.join(".etv-station.reload.json")
}

/// The last recorded reload, or `None` before the first one (or when the file
/// is unreadable).
pub fn read(output_base: &Path) -> Option<ReloadStatus> {
    let text = std::fs::read_to_string(status_path(output_base)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Record a reload's outcome as the next sequence number.
pub fn record(
    output_base: &Path,
    outcome: Outcome,
    dropped: Vec<DroppedChannel>,
    error: Option<String>,
) -> Result<ReloadStatus, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let status = ReloadStatus {
        // Never lower than the clock in milliseconds, so a status file that
        // was lost or corrupted cannot restart the count below a sequence
        // number a caller is already waiting past.
        seq: read(output_base)
            .map_or(0, |s| s.seq + 1)
            .max(now.as_millis() as u64),
        at_unix: now.as_secs(),
        outcome,
        dropped,
        error,
    };
    let bytes = serde_json::to_vec_pretty(&status).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(output_base).map_err(|e| format!("{}: {e}", output_base.display()))?;
    crate::atomic::atomic_write_bytes_sync(&status_path(output_base), &bytes)
        .map_err(|e| e.to_string())?;
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_recorded_before_the_first_reload() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()), None);
    }

    #[test]
    fn each_reload_gets_the_next_sequence_number_and_the_last_one_is_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let first = record(dir.path(), Outcome::Applied, vec![], None).unwrap();
        let dropped = vec![DroppedChannel {
            path: "channels/a/channel.yaml".into(),
            reason: "unknown source".into(),
        }];
        let second = record(dir.path(), Outcome::Applied, dropped.clone(), None).unwrap();
        assert!(second.seq > first.seq, "{} then {}", first.seq, second.seq);
        let read_back = read(dir.path()).unwrap();
        assert_eq!(read_back.seq, second.seq);
        assert_eq!(read_back.dropped, dropped);
    }

    #[test]
    fn a_failed_reload_keeps_its_error() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), Outcome::Failed, vec![], Some("bad yaml".into())).unwrap();
        let status = read(dir.path()).unwrap();
        assert_eq!(status.outcome, Outcome::Failed);
        assert_eq!(status.error.as_deref(), Some("bad yaml"));
    }
}

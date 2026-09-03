use std::path::{Path, PathBuf};
use std::time::Duration;

use ersatztv_channel::error::ChannelError;
use tokio::process::Command;

pub struct PtsTime {
    pub duration: Duration,
}

pub struct PtsScanner {
    segment_folder: PathBuf,
}

impl PtsScanner {
    /// Scans `segment_folder` — a worker run's own segment subfolder, not the
    /// channel's output folder — for the newest `.ts` file, to continue PTS
    /// numbering within a run. A fresh run folder is empty on the first
    /// transcode of a run, which is exactly the state a resumed session needs:
    /// there is nothing from a previous run to continue from, because that
    /// previous run's segments live in its own, different folder.
    pub fn new(segment_folder: &Path) -> PtsScanner {
        PtsScanner {
            segment_folder: segment_folder.to_owned(),
        }
    }

    pub async fn get_last_pts(&self) -> Result<PtsTime, ChannelError> {
        let mut pts_time = PtsTime {
            duration: Duration::ZERO,
        };

        // find last segment file in this run's segment folder
        let mut entries = Vec::new();
        let mut dir = tokio::fs::read_dir(&self.segment_folder).await?;
        while let Ok(Some(entry)) = dir.next_entry().await {
            if entry
                .path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("ts"))
            {
                entries.push(entry);
            }
        }
        entries.sort_by_key(|a| std::cmp::Reverse(a.file_name()));
        if let Some(last_segment) = entries.first() {
            // call ffprobe
            let path = last_segment
                .path()
                .into_os_string()
                .into_string()
                .map_err(|_| ChannelError::PtsScannerFailure)?;

            let output = Command::new("ffprobe")
                .args([
                    "-v",
                    "-0",
                    "-show_entries",
                    "packet=pts_time,duration_time",
                    "-of",
                    "compact=p=0:nk=1",
                    &path,
                ])
                .output()
                .await
                .map_err(|_| ChannelError::PtsScannerFailure)?;

            // parse output line by line for largest pts time
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                let split: Vec<&str> = line.trim().split('|').collect();
                if let Ok(seconds) = split[0].parse::<f64>() {
                    let mut total_seconds = seconds;
                    if let Ok(seconds) = split[1].parse::<f64>() {
                        total_seconds += seconds;
                    }

                    let duration = Duration::from_secs_f64(total_seconds);
                    if duration > pts_time.duration {
                        pts_time.duration = duration
                    }
                }
            }
        }

        Ok(pts_time)
    }
}

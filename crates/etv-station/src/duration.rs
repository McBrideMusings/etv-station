use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::atomic::atomic_write_json;
use crate::config::SourceConfig;
use crate::errors::StationError;
use crate::resolve::ResolvedItem;

const SIDECAR_NAME: &str = ".durations.json";

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DurationCache {
    entries: HashMap<PathBuf, CacheEntry>,
    #[serde(skip)]
    dirty: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    mtime_secs: i64,
    duration_secs: f64,
}

#[derive(Debug, Default)]
pub struct ProbeStats {
    pub from_cache: usize,
    pub from_probe: usize,
    pub from_config: usize,
}

impl DurationCache {
    pub async fn load(output_folder: &Path) -> Result<Self, StationError> {
        let path = output_folder.join(SIDECAR_NAME);
        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let entries: HashMap<PathBuf, CacheEntry> = serde_json::from_slice(&bytes)
                    .map_err(|source| StationError::SidecarCorrupt {
                        path: path.clone(),
                        source,
                    })?;
                Ok(DurationCache {
                    entries,
                    dirty: false,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(StationError::Io { path, source }),
        }
    }

    pub async fn save(&mut self, output_folder: &Path) -> Result<(), StationError> {
        if !self.dirty {
            return Ok(());
        }
        ensure_dir(output_folder).await?;
        atomic_write_json(&output_folder.join(SIDECAR_NAME), &self.entries).await?;
        self.dirty = false;
        Ok(())
    }

    /// Resolve durations for every item in `items` against this cache, probing
    /// where required. Updates the cache in place; caller must `save` afterward.
    pub async fn resolve_all(
        &mut self,
        items: &[ResolvedItem],
    ) -> Result<(Vec<Duration>, ProbeStats), StationError> {
        let mut durations = Vec::with_capacity(items.len());
        let mut stats = ProbeStats::default();
        for item in items {
            durations.push(self.duration_for(item, &mut stats).await?);
        }
        Ok((durations, stats))
    }

    async fn duration_for(
        &mut self,
        item: &ResolvedItem,
        stats: &mut ProbeStats,
    ) -> Result<Duration, StationError> {
        match &item.source {
            // Lavfi and HTTP sources don't have a probable duration; we trust
            // the in_point/out_point declared in config.
            SourceConfig::Lavfi { .. } | SourceConfig::Http { .. } => {
                stats.from_config += 1;
                config_duration(item)
            }
            SourceConfig::Local { path } => {
                let local_path = PathBuf::from(path);
                let metadata = tokio::fs::metadata(&local_path).await.map_err(|_| {
                    StationError::MissingLocalFile {
                        id: item.id.clone(),
                        path: local_path.clone(),
                    }
                })?;
                let mtime_secs = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);

                if let Some(entry) = self.entries.get(&local_path)
                    && entry.mtime_secs == mtime_secs
                {
                    stats.from_cache += 1;
                    let d = clamp_to_config(Duration::from_secs_f64(entry.duration_secs), item);
                    return Ok(d);
                }

                let probed = ffprobe_duration(&local_path).await?;
                self.entries.insert(
                    local_path,
                    CacheEntry {
                        mtime_secs,
                        duration_secs: probed.as_secs_f64(),
                    },
                );
                self.dirty = true;
                stats.from_probe += 1;
                Ok(clamp_to_config(probed, item))
            }
        }
    }
}

async fn ensure_dir(path: &Path) -> Result<(), StationError> {
    tokio::fs::create_dir_all(path)
        .await
        .map_err(|source| StationError::Io {
            path: path.to_path_buf(),
            source,
        })
}

fn config_duration(item: &ResolvedItem) -> Result<Duration, StationError> {
    let in_p = item.in_point.unwrap_or_default();
    let out_p = item.out_point.ok_or(StationError::MissingField {
        id: item.id.clone(),
        field: "out_point",
    })?;
    if out_p <= in_p {
        return Err(StationError::MissingField {
            id: item.id.clone(),
            field: "out_point > in_point",
        });
    }
    Ok(out_p - in_p)
}

fn clamp_to_config(probed: Duration, item: &ResolvedItem) -> Duration {
    let in_p = item.in_point.unwrap_or_default();
    let out_p = item.out_point.unwrap_or(probed);
    let bounded_out = out_p.min(probed);
    if bounded_out <= in_p {
        Duration::ZERO
    } else {
        bounded_out - in_p
    }
}

async fn ffprobe_duration(path: &Path) -> Result<Duration, StationError> {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-print_format", "json", "-show_format", "-i"])
        .arg(path)
        .output()
        .await
        .map_err(|e| StationError::Ffprobe {
            path: path.to_path_buf(),
            reason: format!("spawn: {e}"),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(StationError::Ffprobe {
            path: path.to_path_buf(),
            reason: format!("non-zero exit: {stderr}"),
        });
    }

    #[derive(Deserialize)]
    struct ProbeOut {
        format: ProbeFormat,
    }
    #[derive(Deserialize)]
    struct ProbeFormat {
        duration: Option<String>,
    }

    let probe: ProbeOut =
        serde_json::from_slice(&output.stdout).map_err(|e| StationError::Ffprobe {
            path: path.to_path_buf(),
            reason: format!("parse json: {e}"),
        })?;
    let secs_str = probe.format.duration.ok_or_else(|| StationError::Ffprobe {
        path: path.to_path_buf(),
        reason: "format.duration missing".into(),
    })?;
    let secs: f64 = secs_str.parse().map_err(|e| StationError::Ffprobe {
        path: path.to_path_buf(),
        reason: format!("duration not a float: {e}"),
    })?;
    Ok(Duration::from_secs_f64(secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SourceConfig;
    use crate::resolve::ResolvedItem;

    fn lavfi_item(id: &str, secs: u64) -> ResolvedItem {
        ResolvedItem {
            id: id.into(),
            source: SourceConfig::Lavfi {
                params: "testsrc".into(),
            },
            in_point: Some(Duration::ZERO),
            out_point: Some(Duration::from_secs(secs)),
            program: None,
        }
    }

    #[tokio::test]
    async fn lavfi_duration_from_config() {
        let mut cache = DurationCache::default();
        let item = lavfi_item("a", 30);
        let mut stats = ProbeStats::default();
        let d = cache.duration_for(&item, &mut stats).await.unwrap();
        assert_eq!(d, Duration::from_secs(30));
        assert_eq!(stats.from_config, 1);
    }

    #[tokio::test]
    async fn lavfi_without_out_point_errors() {
        let mut cache = DurationCache::default();
        let item = ResolvedItem {
            id: "x".into(),
            source: SourceConfig::Lavfi {
                params: "testsrc".into(),
            },
            in_point: None,
            out_point: None,
            program: None,
        };
        let mut stats = ProbeStats::default();
        let err = cache.duration_for(&item, &mut stats).await.unwrap_err();
        assert!(matches!(err, StationError::MissingField { .. }));
    }

    #[tokio::test]
    async fn missing_local_file_errors_loudly() {
        let mut cache = DurationCache::default();
        let item = ResolvedItem {
            id: "ghost".into(),
            source: SourceConfig::Local {
                path: "/no/such/path/zzz.mkv".into(),
            },
            in_point: None,
            out_point: None,
            program: None,
        };
        let mut stats = ProbeStats::default();
        let err = cache.duration_for(&item, &mut stats).await.unwrap_err();
        assert!(matches!(err, StationError::MissingLocalFile { .. }));
    }
}

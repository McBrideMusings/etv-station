use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::atomic::atomic_write_json;
use crate::config::SourceConfig;
use crate::error_card::make_error_card;
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
    /// Items whose file could not be opened but whose slot length was known, so
    /// an on-screen error card took the slot.
    pub error_cards: usize,
    /// Items whose file could not be opened AND whose length was unknown, so
    /// there was no slot to put a card in and the item was left out.
    pub dropped: usize,
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
    ///
    /// Takes the list by value and hands back a possibly-different one: an item
    /// whose file cannot be read is replaced by an on-screen error card of the
    /// same length, or — when its length is unknown too — left out entirely. A
    /// channel is never failed for one unreadable file, because a library with
    /// one snapped tape in it is still a channel.
    pub async fn resolve_all(
        &mut self,
        items: Vec<ResolvedItem>,
    ) -> Result<(Vec<ResolvedItem>, Vec<Duration>, ProbeStats), StationError> {
        // Every local path this generation asked for, captured BEFORE the loop.
        // Pruning against the surviving items instead would be wrong in exactly
        // the case that matters: an item that turned into an error card is no
        // longer a `Local` source, so its cached duration would be evicted. A
        // dropped network mount fails every file at once, which would empty the
        // whole sidecar in one pass and force a re-probe of the entire library
        // when the share comes back.
        let requested: Vec<PathBuf> = items
            .iter()
            .filter_map(|item| match &item.source {
                SourceConfig::Local { path } => Some(PathBuf::from(path)),
                SourceConfig::Lavfi { .. } | SourceConfig::Http { .. } => None,
            })
            .collect();

        let mut kept = Vec::with_capacity(items.len());
        let mut durations = Vec::with_capacity(items.len());
        let mut stats = ProbeStats::default();
        for mut item in items {
            match self.duration_for(&item, &mut stats).await {
                Ok(d) => {
                    kept.push(item);
                    durations.push(d);
                }
                Err(err) => {
                    let Some((path, reason)) = err.unreadable_media() else {
                        return Err(err);
                    };
                    let path = path.to_path_buf();
                    let reason = reason.to_string();
                    match item.catalog_duration {
                        Some(slot) => {
                            tracing::warn!(
                                event = "item.error_card",
                                item = %item.id,
                                path = %path.display(),
                                reason = %reason,
                                slot_secs = slot.as_secs(),
                                "file could not be read; airing an on-screen error for its slot",
                            );
                            make_error_card(&mut item, &path, &reason, slot);
                            kept.push(item);
                            durations.push(slot);
                            stats.error_cards += 1;
                        }
                        None => {
                            tracing::warn!(
                                event = "item.dropped",
                                item = %item.id,
                                path = %path.display(),
                                reason = %reason,
                                "file could not be read and its length is unknown; leaving it out",
                            );
                            stats.dropped += 1;
                        }
                    }
                }
            }
        }
        self.prune_to_paths(&requested);
        Ok((kept, durations, stats))
    }

    /// Drop cache entries whose path is no longer referenced by any current
    /// `Local` item, so the per-channel sidecar doesn't accumulate stale paths
    /// as items are removed or their source paths change. Marks the cache dirty
    /// if anything was removed, so the next `save` writes the trimmed sidecar.
    fn prune_to_paths(&mut self, paths: &[PathBuf]) {
        let keep: HashSet<&PathBuf> = paths.iter().collect();
        let before = self.entries.len();
        self.entries.retain(|path, _| keep.contains(path));
        if self.entries.len() != before {
            self.dirty = true;
        }
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
                let metadata = tokio::fs::metadata(&local_path).await.map_err(|e| {
                    // Keep the kind. A deleted file and a dropped network mount
                    // both fail here, and the difference is the whole diagnosis
                    // — one wants a re-download, the other wants the share
                    // remounting.
                    StationError::MissingLocalFile {
                        id: item.id.clone(),
                        path: local_path.clone(),
                        reason: match e.kind() {
                            std::io::ErrorKind::NotFound => "file not found".to_string(),
                            std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
                            other => format!("{other:?}"),
                        },
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
        .map_err(|e| StationError::FfprobeUnavailable {
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
            block: 0,
            id: id.into(),
            source: SourceConfig::Lavfi {
                params: "testsrc".into(),
            },
            in_point: Some(Duration::ZERO),
            out_point: Some(Duration::from_secs(secs)),
            program: None,
            catalog_duration: None,
            error_card: false,
            metadata: None,
            guide: None,
            guide_fields: crate::guide::GuideFields::default(),
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
            block: 0,
            id: "x".into(),
            source: SourceConfig::Lavfi {
                params: "testsrc".into(),
            },
            in_point: None,
            out_point: None,
            program: None,
            catalog_duration: None,
            error_card: false,
            metadata: None,
            guide: None,
            guide_fields: crate::guide::GuideFields::default(),
        };
        let mut stats = ProbeStats::default();
        let err = cache.duration_for(&item, &mut stats).await.unwrap_err();
        assert!(matches!(err, StationError::MissingField { .. }));
    }

    #[tokio::test]
    async fn missing_local_file_errors_loudly() {
        let mut cache = DurationCache::default();
        let item = ResolvedItem {
            block: 0,
            id: "ghost".into(),
            source: SourceConfig::Local {
                path: "/no/such/path/zzz.mkv".into(),
            },
            in_point: None,
            out_point: None,
            program: None,
            catalog_duration: None,
            error_card: false,
            metadata: None,
            guide: None,
            guide_fields: crate::guide::GuideFields::default(),
        };
        let mut stats = ProbeStats::default();
        let err = cache.duration_for(&item, &mut stats).await.unwrap_err();
        assert!(matches!(err, StationError::MissingLocalFile { .. }));
    }

    fn local_item(id: &str, path: &str) -> ResolvedItem {
        ResolvedItem {
            block: 0,
            id: id.into(),
            source: SourceConfig::Local { path: path.into() },
            in_point: None,
            out_point: None,
            program: None,
            catalog_duration: None,
            error_card: false,
            metadata: None,
            guide: None,
            guide_fields: crate::guide::GuideFields::default(),
        }
    }

    // A file that cannot be opened does NOT fail the channel: it keeps its slot
    // and says on screen why it is not playing. This is the whole point — a
    // library with one snapped tape is still a channel.
    #[tokio::test]
    async fn unreadable_file_becomes_an_error_card_of_its_catalog_length() {
        let mut cache = DurationCache::default();
        let mut broken = local_item("tingler", "/no/such/path/The.Tingler.1959.mp4");
        broken.catalog_duration = Some(Duration::from_secs(4891));
        let good = lavfi_item("bars", 30);

        let (items, durations, stats) = cache.resolve_all(vec![broken, good]).await.unwrap();

        assert_eq!(
            items.len(),
            2,
            "the broken item keeps its place in the list"
        );
        assert_eq!(stats.error_cards, 1);
        assert_eq!(stats.dropped, 0);
        assert_eq!(
            durations[0],
            Duration::from_secs(4891),
            "the card runs exactly as long as the film would have"
        );
        match &items[0].source {
            SourceConfig::Lavfi { params } => {
                assert!(params.contains("PLAYBACK ERROR"), "{params}");
                assert!(params.contains("file not found"), "{params}");
                assert!(
                    params.contains("[out0]") && params.contains("[out1]"),
                    "{params}"
                );
            }
            other => panic!("expected a lavfi error card, got {other:?}"),
        }
    }

    // With no length on record there is no slot to fill, so the item is left out
    // rather than guessed at — and the rest of the list still plays.
    #[tokio::test]
    async fn unreadable_file_with_no_known_length_is_dropped() {
        let mut cache = DurationCache::default();
        let broken = local_item("ghost", "/no/such/path/zzz.mkv");
        let good = lavfi_item("bars", 30);

        let (items, durations, stats) = cache.resolve_all(vec![broken, good]).await.unwrap();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "bars");
        assert_eq!(durations, vec![Duration::from_secs(30)]);
        assert_eq!(stats.dropped, 1);
        assert_eq!(stats.error_cards, 0);
    }

    // A genuinely misconfigured item is still an error. Only unreadable MEDIA is
    // survivable; a lavfi item with no out_point cannot be given a slot length
    // by anyone, and silently dropping it would hide a config mistake.
    #[tokio::test]
    async fn a_config_error_still_fails() {
        let mut cache = DurationCache::default();
        let bad = ResolvedItem {
            block: 0,
            id: "x".into(),
            source: SourceConfig::Lavfi {
                params: "testsrc".into(),
            },
            in_point: None,
            out_point: None,
            program: None,
            catalog_duration: None,
            error_card: false,
            metadata: None,
            guide: None,
            guide_fields: crate::guide::GuideFields::default(),
        };
        let err = cache.resolve_all(vec![bad]).await.unwrap_err();
        assert!(matches!(err, StationError::MissingField { .. }));
    }

    fn seed_entry(cache: &mut DurationCache, path: &str) {
        cache.entries.insert(
            PathBuf::from(path),
            CacheEntry {
                mtime_secs: 1,
                duration_secs: 10.0,
            },
        );
    }

    #[test]
    fn prune_drops_paths_not_in_current_items() {
        let mut cache = DurationCache::default();
        seed_entry(&mut cache, "/keep.mkv");
        seed_entry(&mut cache, "/stale.mkv");
        cache.dirty = false;

        // This generation asked for /keep.mkv only.
        cache.prune_to_paths(&[PathBuf::from("/keep.mkv")]);

        assert!(cache.entries.contains_key(Path::new("/keep.mkv")));
        assert!(!cache.entries.contains_key(Path::new("/stale.mkv")));
        assert!(cache.dirty, "removing a stale entry marks the cache dirty");
    }

    #[test]
    fn prune_is_noop_when_nothing_stale() {
        let mut cache = DurationCache::default();
        seed_entry(&mut cache, "/keep.mkv");
        cache.dirty = false;

        cache.prune_to_paths(&[PathBuf::from("/keep.mkv")]);

        assert!(cache.entries.contains_key(Path::new("/keep.mkv")));
        assert!(!cache.dirty, "no removals leaves the cache clean");
    }

    // A file that failed this pass keeps its cached duration. Otherwise a
    // dropped network mount — which fails every file at once — would empty the
    // whole sidecar in one generation and force a re-probe of the entire
    // library when the share came back.
    #[tokio::test]
    async fn an_error_card_does_not_evict_its_own_cached_duration() {
        let mut cache = DurationCache::default();
        seed_entry(&mut cache, "/gone.mkv");
        cache.dirty = false;

        let mut broken = local_item("gone", "/gone.mkv");
        broken.catalog_duration = Some(Duration::from_secs(600));
        let (items, _, stats) = cache.resolve_all(vec![broken]).await.unwrap();

        assert_eq!(stats.error_cards, 1);
        assert!(matches!(items[0].source, SourceConfig::Lavfi { .. }));
        assert!(
            cache.entries.contains_key(Path::new("/gone.mkv")),
            "the cached duration survives the file being unreadable this pass"
        );
    }

    // A broken ffprobe install is not a broken library. If it were treated as
    // unreadable media, every item on every channel would become an error card
    // and the daemon would report itself healthy.
    #[tokio::test]
    async fn ffprobe_that_cannot_be_run_is_not_unreadable_media() {
        let err = StationError::FfprobeUnavailable {
            reason: "spawn: No such file or directory".into(),
        };
        assert!(err.unreadable_media().is_none());
    }
}

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time_tz::Tz;

use crate::atomic::atomic_write_json;
use crate::config::ItemConfig;
use crate::errors::StationError;
use crate::tz as tzmod;

const SIDECAR_NAME: &str = ".anchor";

#[derive(Debug, Serialize, Deserialize)]
struct AnchorFile {
    #[serde(with = "time::serde::rfc3339")]
    anchor: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    item_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AnchorState {
    pub anchor_utc: OffsetDateTime,
    pub item_ids: Vec<String>,
    pub initialized_now: bool,
    pub re_anchored: bool,
}

pub async fn load_or_initialize(
    output_folder: &Path,
    items: &[ItemConfig],
    now_utc: OffsetDateTime,
    tz: &'static Tz,
) -> Result<AnchorState, StationError> {
    let path = sidecar_path(output_folder);
    let current_ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();

    let existing = match tokio::fs::read(&path).await {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => return Err(StationError::Io { path, source }),
    };

    if let Some(bytes) = existing {
        let parsed: AnchorFile =
            serde_json::from_slice(&bytes).map_err(|source| StationError::SidecarCorrupt {
                path: path.clone(),
                source,
            })?;
        if parsed.item_ids == current_ids {
            return Ok(AnchorState {
                anchor_utc: parsed.anchor,
                item_ids: parsed.item_ids,
                initialized_now: false,
                re_anchored: false,
            });
        }
        // Items changed: re-anchor.
        let new_anchor = tzmod::local_midnight_at_or_before(now_utc, tz);
        write_sidecar(output_folder, new_anchor, now_utc, &current_ids).await?;
        return Ok(AnchorState {
            anchor_utc: new_anchor,
            item_ids: current_ids,
            initialized_now: false,
            re_anchored: true,
        });
    }

    // No sidecar yet: first run.
    let new_anchor = tzmod::local_midnight_at_or_before(now_utc, tz);
    write_sidecar(output_folder, new_anchor, now_utc, &current_ids).await?;
    Ok(AnchorState {
        anchor_utc: new_anchor,
        item_ids: current_ids,
        initialized_now: true,
        re_anchored: false,
    })
}

async fn write_sidecar(
    output_folder: &Path,
    anchor: OffsetDateTime,
    now_utc: OffsetDateTime,
    item_ids: &[String],
) -> Result<(), StationError> {
    tokio::fs::create_dir_all(output_folder)
        .await
        .map_err(|source| StationError::Io {
            path: output_folder.to_path_buf(),
            source,
        })?;
    let payload = AnchorFile {
        anchor,
        created_at: now_utc,
        item_ids: item_ids.to_vec(),
    };
    atomic_write_json(&sidecar_path(output_folder), &payload).await?;
    Ok(())
}

fn sidecar_path(output_folder: &Path) -> PathBuf {
    output_folder.join(SIDECAR_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SourceConfig;
    use std::time::Duration;
    use tempfile::tempdir;
    use time::macros::datetime;

    fn lavfi(id: &str) -> ItemConfig {
        ItemConfig {
            id: id.into(),
            source: SourceConfig::Lavfi { params: "x".into() },
            in_point: Some(Duration::ZERO),
            out_point: Some(Duration::from_secs(30)),
            program: None,
        }
    }

    #[tokio::test]
    async fn first_run_creates_sidecar() {
        let dir = tempdir().unwrap();
        let now = datetime!(2026-04-13 12:00 UTC);
        let tz = tzmod::parse("UTC").unwrap();
        let items = vec![lavfi("a"), lavfi("b")];
        let st = load_or_initialize(dir.path(), &items, now, tz)
            .await
            .unwrap();
        assert!(st.initialized_now);
        assert_eq!(st.item_ids, vec!["a", "b"]);
        assert!(dir.path().join(".anchor").exists());
    }

    #[tokio::test]
    async fn second_run_reuses_anchor_when_items_match() {
        let dir = tempdir().unwrap();
        let now1 = datetime!(2026-04-13 12:00 UTC);
        let now2 = datetime!(2026-04-15 18:00 UTC);
        let tz = tzmod::parse("UTC").unwrap();
        let items = vec![lavfi("a"), lavfi("b")];
        let first = load_or_initialize(dir.path(), &items, now1, tz)
            .await
            .unwrap();
        let second = load_or_initialize(dir.path(), &items, now2, tz)
            .await
            .unwrap();
        assert!(!second.initialized_now);
        assert!(!second.re_anchored);
        assert_eq!(first.anchor_utc, second.anchor_utc);
    }

    #[tokio::test]
    async fn re_anchors_when_items_change() {
        let dir = tempdir().unwrap();
        let now = datetime!(2026-04-13 12:00 UTC);
        let tz = tzmod::parse("UTC").unwrap();
        let items_v1 = vec![lavfi("a"), lavfi("b")];
        load_or_initialize(dir.path(), &items_v1, now, tz)
            .await
            .unwrap();
        let items_v2 = vec![lavfi("b"), lavfi("a")]; // reordered
        let st = load_or_initialize(dir.path(), &items_v2, now, tz)
            .await
            .unwrap();
        assert!(st.re_anchored);
        assert_eq!(st.item_ids, vec!["b", "a"]);
    }
}

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::errors::AtomicWriteError;

pub async fn atomic_write_json<T>(path: &Path, value: &T) -> Result<(), AtomicWriteError>
where
    T: Serialize,
{
    atomic_write_bytes(path, &serde_json::to_vec_pretty(value)?).await
}

/// Write raw bytes through the same temp-file-then-rename path
/// [`atomic_write_json`] uses, for content that is already encoded — the
/// play-history ledger (#70) is newline-delimited JSON, not one JSON value.
pub async fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<(), AtomicWriteError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("playout");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let temp = parent.join(format!(
        ".{file_name}.tmp.{pid}.{nonce}",
        pid = std::process::id(),
    ));

    if let Err(source) = write_and_sync(&temp, bytes).await {
        let _ = fs::remove_file(&temp).await;
        return Err(AtomicWriteError::Io { path: temp, source });
    }

    if let Err(source) = fs::rename(&temp, path).await {
        let _ = fs::remove_file(&temp).await;
        return Err(AtomicWriteError::Io {
            path: path.to_path_buf(),
            source,
        });
    }

    Ok(())
}

async fn write_and_sync(temp: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = fs::File::create(temp).await?;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use tempfile::tempdir;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Sample {
        name: String,
        n: u32,
    }

    #[tokio::test]
    async fn writes_and_overwrites() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("playout.json");

        let v1 = Sample {
            name: "first".into(),
            n: 1,
        };
        atomic_write_json(&path, &v1).await.unwrap();
        let parsed: Sample =
            serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
        assert_eq!(parsed, v1);

        let v2 = Sample {
            name: "second".into(),
            n: 2,
        };
        atomic_write_json(&path, &v2).await.unwrap();
        let parsed: Sample =
            serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
        assert_eq!(parsed, v2);
    }

    #[tokio::test]
    async fn cleans_up_temp_on_failure() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent_subdir").join("playout.json");

        let err = atomic_write_json(
            &path,
            &Sample {
                name: "x".into(),
                n: 0,
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(err, AtomicWriteError::Io { .. }));

        let mut leftovers = Vec::new();
        let mut entries = tokio::fs::read_dir(dir.path()).await.unwrap();
        while let Ok(Some(entry)) = entries.next_entry().await {
            leftovers.push(entry.file_name());
        }
        assert!(
            leftovers
                .iter()
                .all(|n| !n.to_string_lossy().contains(".tmp.")),
            "leftover temp files: {leftovers:?}",
        );
    }
}

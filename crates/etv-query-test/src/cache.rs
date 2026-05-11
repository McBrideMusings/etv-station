use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::normalize::NormalizedItem;

const DEFAULT_TTL_SECS: u64 = 3600;

#[derive(Serialize, Deserialize)]
struct CacheFile {
    fetched_at: u64,
    items: Vec<NormalizedItem>,
}

pub fn path(key: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/cache")
        .join(format!("{key}.json"))
}

pub fn load(key: &str, ttl_secs: Option<u64>) -> Option<Vec<NormalizedItem>> {
    let ttl = ttl_secs.unwrap_or(DEFAULT_TTL_SECS);
    let path = path(key);
    let raw = std::fs::read_to_string(&path).ok()?;
    let parsed: CacheFile = serde_json::from_str(&raw).ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    if now.saturating_sub(parsed.fetched_at) > ttl {
        return None;
    }
    Some(parsed.items)
}

pub fn store(key: &str, items: &[NormalizedItem]) -> Result<(), String> {
    let path = path(key);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("clock: {e}"))?
        .as_secs();
    let file = CacheFile {
        fetched_at: now,
        items: items.to_vec(),
    };
    let json = serde_json::to_string(&file).map_err(|e| format!("serialize: {e}"))?;
    std::fs::write(&path, json).map_err(|e| format!("write: {e}"))?;
    Ok(())
}

#[allow(dead_code)]
pub fn invalidate(key: &str) {
    let _ = std::fs::remove_file(path(key));
}

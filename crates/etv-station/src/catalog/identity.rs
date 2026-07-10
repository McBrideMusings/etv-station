//! Deterministic, opaque `entry_id` derivation (#47 locked).
//!
//! An `entry_id` is derived first-hit-wins:
//! 1. Strongest external GUID present (`imdb` → `tmdb` → `tvdb` → `plex`),
//!    formatted `"{namespace}:{value}"`, e.g. `imdb:tt1375666`.
//! 2. *(ingest-time, not here)* path-match inherit — a local-FS file whose
//!    canonical path already appears in `entry_sources` reuses that
//!    `entry_id`. This needs a catalog lookup, so it lives in the FS ingester;
//!    [`derive_entry_id`] covers only the pure GUID → path-hash rule.
//! 3. Path-derived fallback for GUID-less FS content:
//!    `"fs:" + fnv1a_hex(canonical_path)`.

use super::model::ExternalNs;

/// Derive an `entry_id` from whatever external GUIDs an item carries, falling
/// back to a hash of its canonical path.
///
/// `external_ids` may be in any order and contain duplicates/unknowns; only the
/// strongest recognised namespace is used. `canonical_path` must already be
/// canonicalised (see [`canonical_path`]).
pub fn derive_entry_id(external_ids: &[(ExternalNs, String)], canonical_path: &str) -> String {
    for ns in ExternalNs::PRIORITY {
        if let Some((_, value)) = external_ids.iter().find(|(n, _)| *n == ns) {
            return format!("{}:{}", ns.as_str(), value);
        }
    }
    format!("fs:{:016x}", fnv1a_64(canonical_path))
}

/// Canonicalise a raw filesystem path for identity/dedup: strip the configured
/// source-root prefix (so the same file is one identity regardless of which
/// mount root a given process sees it under) and normalise separators to `/`.
///
/// The realpath/symlink-resolution half of the locked canonical-path rule is a
/// filesystem operation and belongs to the ingester; this function is the pure,
/// deterministic string half that identity derivation and tests depend on. The
/// first matching root in `source_roots` wins.
pub fn canonical_path(raw: &str, source_roots: &[&str]) -> String {
    let normalized = raw.replace('\\', "/");
    // Longest root first, so an overlapping shorter root (`/Volumes` vs
    // `/Volumes/media`) can't shadow the more specific match and leave a stray
    // segment in the "canonical" path.
    let mut roots: Vec<String> = source_roots
        .iter()
        .map(|r| {
            let r = r.replace('\\', "/");
            r.strip_suffix('/').unwrap_or(&r).to_string()
        })
        .collect();
    roots.sort_by_key(|r| std::cmp::Reverse(r.len()));
    for root in &roots {
        // Only accept a match at a path boundary: `/Volumes/media` must not
        // strip the prefix of a sibling like `/Volumes/mediacache/…`, which
        // would reparent an unrelated file and risk an `entry_id` collision.
        if let Some(rest) = normalized.strip_prefix(root.as_str()) {
            if rest.is_empty() {
                return String::new();
            }
            if let Some(under_root) = rest.strip_prefix('/') {
                return under_root.to_string();
            }
        }
    }
    normalized
}

/// FNV-1a 64-bit. Chosen over `std::hash::DefaultHasher` because it is a fixed,
/// documented algorithm — an `entry_id` is persisted in `catalog.db` and must
/// stay stable across Rust toolchain upgrades, which `DefaultHasher` does not
/// guarantee.
fn fnv1a_64(s: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in s.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(pairs: &[(ExternalNs, &str)], path: &str) -> String {
        let owned: Vec<(ExternalNs, String)> =
            pairs.iter().map(|(n, v)| (*n, (*v).to_string())).collect();
        derive_entry_id(&owned, path)
    }

    #[test]
    fn imdb_beats_every_other_guid() {
        let got = id(
            &[
                (ExternalNs::Plex, "abc"),
                (ExternalNs::Tvdb, "111"),
                (ExternalNs::Imdb, "tt1375666"),
                (ExternalNs::Tmdb, "27205"),
            ],
            "/movies/x.mkv",
        );
        assert_eq!(got, "imdb:tt1375666");
    }

    #[test]
    fn priority_falls_through_to_plex() {
        assert_eq!(id(&[(ExternalNs::Plex, "k9")], "/x"), "plex:k9");
        assert_eq!(
            id(&[(ExternalNs::Tvdb, "5"), (ExternalNs::Plex, "k")], "/x"),
            "tvdb:5"
        );
    }

    #[test]
    fn no_guid_hashes_canonical_path() {
        let got = id(&[], "movies/bumper.mkv");
        assert!(got.starts_with("fs:"));
        assert_eq!(got.len(), "fs:".len() + 16);
        // Deterministic across calls.
        assert_eq!(got, id(&[], "movies/bumper.mkv"));
        // Different path → different id.
        assert_ne!(got, id(&[], "movies/other.mkv"));
    }

    #[test]
    fn canonical_path_strips_root_and_normalizes_separators() {
        let roots = ["/Volumes/media", "/mnt/media/"];
        assert_eq!(
            canonical_path("/Volumes/media/movies/a.mkv", &roots),
            "movies/a.mkv"
        );
        assert_eq!(
            canonical_path("/mnt/media/movies/a.mkv", &roots),
            "movies/a.mkv"
        );
        // Same logical file under two roots canonicalises identically → one identity.
        assert_eq!(
            canonical_path("/Volumes/media/movies/a.mkv", &roots),
            canonical_path("/mnt/media/movies/a.mkv", &roots),
        );
        // Unmatched path is only separator-normalised.
        assert_eq!(canonical_path("C:\\clips\\a.mkv", &roots), "C:/clips/a.mkv");
    }

    #[test]
    fn canonical_path_only_matches_at_a_path_boundary() {
        // A sibling dir whose name merely starts with the root must NOT strip —
        // else it reparents to `cache/x.mkv` and can collide with a real file.
        let roots = ["/Volumes/media"];
        assert_eq!(
            canonical_path("/Volumes/mediacache/x.mkv", &roots),
            "/Volumes/mediacache/x.mkv",
        );
        // Exact-root match with nothing under it canonicalises to empty.
        assert_eq!(canonical_path("/Volumes/media", &roots), "");
    }

    #[test]
    fn canonical_path_prefers_the_longest_matching_root() {
        // Overlapping roots: the more specific one must win regardless of order.
        let roots = ["/Volumes", "/Volumes/media"];
        assert_eq!(canonical_path("/Volumes/media/x.mkv", &roots), "x.mkv");
        let reversed = ["/Volumes/media", "/Volumes"];
        assert_eq!(canonical_path("/Volumes/media/x.mkv", &reversed), "x.mkv");
    }

    #[test]
    fn path_match_inherit_gives_same_id_for_same_canonical_path() {
        let roots = ["/Volumes/media", "/mnt/media"];
        let a = canonical_path("/Volumes/media/x.mkv", &roots);
        let b = canonical_path("/mnt/media/x.mkv", &roots);
        assert_eq!(id(&[], &a), id(&[], &b));
    }
}

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use serde_json::Value;
use simple_expand_tilde::expand_tilde;
use thiserror::Error;

/// Raised when a resolved path cannot be written back into the JSON `Value`
/// it is stored in without lossy UTF-8 substitution.
///
/// A `serde_json::Value::String` is a Rust `String`, which is UTF-8 by
/// definition — so a path containing invalid UTF-8 bytes cannot be stored
/// there under any implementation of this function. Reporting that fact,
/// rather than silently substituting U+FFFD and writing a string that no
/// longer names the real directory, is what this type exists for.
#[derive(Error, Debug)]
pub enum PathResolveError {
    #[error("path is not valid UTF-8 and cannot be stored in config field {pointer}: {path:?}")]
    NonUtf8 { pointer: String, path: PathBuf },
}

pub fn resolve_relative_paths(
    value: &mut Value,
    base_dir: &Path,
    pointers: &[&str],
) -> Result<(), PathResolveError> {
    for pointer in pointers {
        if let Some(Value::String(s)) = value.pointer_mut(pointer)
            && !s.is_empty()
        {
            let expanded = expand_tilde(&s).unwrap_or(Path::new(s).to_path_buf());

            let resolved: PathBuf = if expanded.is_relative() {
                base_dir
                    .join(&expanded)
                    .canonicalize()
                    .unwrap_or_else(|_| base_dir.join(&expanded))
            } else {
                expanded
            };

            match resolved.to_string_lossy() {
                Cow::Borrowed(valid) => *s = valid.to_string(),
                Cow::Owned(_) => {
                    return Err(PathResolveError::NonUtf8 {
                        pointer: pointer.to_string(),
                        path: resolved,
                    });
                }
            }
        }
    }

    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    #[cfg(target_os = "linux")]
    use std::ffi::OsStr;
    #[cfg(target_os = "linux")]
    use std::os::unix::ffi::OsStrExt;

    use super::*;

    /// The happy path must resolve byte-identically to before: no behaviour
    /// change for ordinary, valid-UTF-8 paths.
    #[test]
    fn valid_utf8_relative_path_resolves_normally() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("media")).unwrap();

        let mut value = serde_json::json!({ "playout": { "folder": "media" } });
        resolve_relative_paths(&mut value, dir.path(), &["/playout/folder"]).unwrap();

        let resolved = value
            .pointer("/playout/folder")
            .and_then(Value::as_str)
            .unwrap();
        assert_eq!(
            resolved,
            dir.path().join("media").canonicalize().unwrap().to_str().unwrap()
        );
    }

    /// A base_dir containing invalid UTF-8 bytes forces `to_string_lossy`
    /// into its `Cow::Owned` (substituted) arm, which must surface as
    /// `PathResolveError::NonUtf8` naming the offending pointer rather than
    /// silently writing a U+FFFD-corrupted string back into the JSON value.
    ///
    /// Gated to Linux, not just `unix`: ext4 (and this daemon's Linux Docker
    /// production target) stores filenames as arbitrary byte sequences, but
    /// macOS's APFS validates UTF-8 at the filesystem layer and refuses to
    /// create a directory named with an invalid byte sequence in the first
    /// place (`EILSEQ`) — before `resolve_relative_paths` ever runs.
    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_base_dir_is_reported_not_silently_corrupted() {
        let dir = tempfile::tempdir().unwrap();
        let bad_dir = dir.path().join(OsStr::from_bytes(b"bad\xFFname"));
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::create_dir_all(bad_dir.join("media")).unwrap();

        let mut value = serde_json::json!({ "playout": { "folder": "media" } });
        let err = resolve_relative_paths(&mut value, &bad_dir, &["/playout/folder"]).unwrap_err();

        match err {
            PathResolveError::NonUtf8 { pointer, .. } => {
                assert_eq!(pointer, "/playout/folder");
            }
        }
    }
}

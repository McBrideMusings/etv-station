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

/// Raised by [`resolve_path`] when a path cannot be resolved.
#[derive(Error, Debug)]
pub enum ResolveError {
    /// The path starts with a bare `~` component but the home directory is
    /// unknown, so the tilde cannot be expanded.
    #[error("unable to expand tilde in path {0}")]
    TildeExpand(String),
}

/// Expands a leading `~` and joins a relative result onto `base_dir`,
/// canonicalizing it when the target exists.
///
/// Fails only when a bare leading `~` cannot be expanded. A component that
/// merely begins with `~` (`~otheruser`, `~backup`) is never expanded and is
/// not an error: it is an ordinary directory name.
pub fn resolve_path(base_dir: &Path, raw: &str) -> Result<PathBuf, ResolveError> {
    let expanded = expand_tilde(raw).ok_or_else(|| ResolveError::TildeExpand(raw.to_string()))?;

    Ok(if expanded.is_relative() {
        base_dir
            .join(&expanded)
            .canonicalize()
            .unwrap_or_else(|_| base_dir.join(&expanded))
    } else {
        expanded
    })
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
            // A tilde that cannot be expanded keeps the raw value as a plain
            // relative path; callers that must refuse it use `resolve_path`.
            let resolved = match resolve_path(base_dir, s) {
                Ok(resolved) => resolved,
                Err(ResolveError::TildeExpand(_)) => {
                    let raw = Path::new(s.as_str());
                    base_dir
                        .join(raw)
                        .canonicalize()
                        .unwrap_or_else(|_| base_dir.join(raw))
                }
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
    use std::ffi::OsStr;
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
            dir.path()
                .join("media")
                .canonicalize()
                .unwrap()
                .to_str()
                .unwrap()
        );
    }

    /// A bare `~` expands to the home directory; a component that only begins
    /// with `~` is an ordinary relative name joined onto `base_dir`.
    #[test]
    fn resolve_path_expands_bare_tilde_but_not_tilde_prefixed_names() {
        let dir = tempfile::tempdir().unwrap();

        let home = resolve_path(dir.path(), "~/x").unwrap();
        assert!(home.is_absolute());
        assert!(!home.starts_with(dir.path()));
        assert!(home.ends_with("x"));

        let named = resolve_path(dir.path(), "~backup/x").unwrap();
        assert_eq!(named, dir.path().join("~backup/x"));
    }

    /// A base_dir containing invalid UTF-8 bytes forces `to_string_lossy`
    /// into its `Cow::Owned` (substituted) arm, which must surface as
    /// `PathResolveError::NonUtf8` naming the offending pointer rather than
    /// silently writing a U+FFFD-corrupted string back into the JSON value.
    ///
    /// Does not require creating a directory with invalid UTF-8: `canonicalize()`
    /// fails on the non-existent path, falling back to `base_dir.join(expanded)`,
    /// which still carries the invalid bytes and hits `to_string_lossy`'s
    /// `Cow::Owned` arm on the same line whether the directory exists or not.
    #[test]
    fn non_utf8_base_dir_is_reported_not_silently_corrupted() {
        let dir = tempfile::tempdir().unwrap();
        let bad_dir = dir.path().join(OsStr::from_bytes(b"bad\xFFname"));

        let mut value = serde_json::json!({ "playout": { "folder": "media" } });
        let err = resolve_relative_paths(&mut value, &bad_dir, &["/playout/folder"]).unwrap_err();

        match err {
            PathResolveError::NonUtf8 { pointer, .. } => {
                assert_eq!(pointer, "/playout/folder");
            }
        }
    }
}

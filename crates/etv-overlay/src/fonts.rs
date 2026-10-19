//! The fonts an overlay registers beyond the OS's own: the files a station
//! lists under `fonts:` in `station.yaml`, each a font file or a directory of
//! them. The station checks the list at load with [`expand`]; the overlay
//! process expands it again to read the bytes, so both agree on what counts.
use std::path::{Path, PathBuf};

/// A listed path that names no usable font.
#[derive(Debug, thiserror::Error)]
pub enum FontError {
    #[error("font path {} does not exist", .0.display())]
    Missing(PathBuf),
    #[error("font path {} is not a .ttf, .otf or .ttc font file: {reason}", path.display())]
    NotAFont { path: PathBuf, reason: String },
    #[error("cannot read font path {}: {source}", path.display())]
    Unreadable {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Every font file the listed paths name: a file stands for itself, a
/// directory for each `.ttf`, `.otf` or `.ttc` directly inside it, in name
/// order. A path that does not exist, or a file that is not a font, is an
/// error naming that path.
pub fn expand(paths: &[PathBuf]) -> Result<Vec<PathBuf>, FontError> {
    let mut files = Vec::new();
    for path in paths {
        let meta = std::fs::metadata(path).map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => FontError::Missing(path.clone()),
            _ => FontError::Unreadable {
                path: path.clone(),
                source,
            },
        })?;
        if meta.is_dir() {
            let mut inside = Vec::new();
            let entries = std::fs::read_dir(path).map_err(|source| FontError::Unreadable {
                path: path.clone(),
                source,
            })?;
            for entry in entries {
                let entry = entry.map_err(|source| FontError::Unreadable {
                    path: path.clone(),
                    source,
                })?;
                let file = entry.path();
                if file.is_file() && has_font_extension(&file) {
                    inside.push(file);
                }
            }
            inside.sort();
            files.extend(inside);
        } else {
            files.push(path.clone());
        }
    }
    for file in &files {
        check_is_font(file)?;
    }
    Ok(files)
}

fn has_font_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| matches!(e.to_ascii_lowercase().as_str(), "ttf" | "otf" | "ttc"))
        .unwrap_or(false)
}

/// The first four bytes a TrueType, OpenType-CFF or collection file starts with.
fn check_is_font(path: &Path) -> Result<(), FontError> {
    let not_a_font = |reason: &str| FontError::NotAFont {
        path: path.to_path_buf(),
        reason: reason.to_string(),
    };
    if !has_font_extension(path) {
        return Err(not_a_font("the extension is not .ttf, .otf or .ttc"));
    }
    let mut magic = [0u8; 4];
    let read =
        std::fs::File::open(path).and_then(|mut f| std::io::Read::read_exact(&mut f, &mut magic));
    read.map_err(|source| FontError::Unreadable {
        path: path.to_path_buf(),
        source,
    })?;
    match &magic {
        [0, 1, 0, 0] | b"OTTO" | b"true" | b"ttcf" => Ok(()),
        _ => Err(not_a_font("the file does not start like a font")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_path_errors_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone.ttf");
        let err = expand(std::slice::from_ref(&gone)).unwrap_err();
        assert!(matches!(err, FontError::Missing(_)));
        assert!(err.to_string().contains(&gone.display().to_string()));
    }

    #[test]
    fn a_non_font_file_errors_and_a_directory_keeps_only_fonts() {
        let dir = tempfile::tempdir().unwrap();
        let ttf = dir.path().join("a.ttf");
        std::fs::write(&ttf, [0u8, 1, 0, 0, 9, 9]).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "hi").unwrap();
        let fake = dir.path().join("b.ttf");
        std::fs::write(&fake, "not a font").unwrap();

        let err = expand(std::slice::from_ref(&fake)).unwrap_err();
        assert!(matches!(err, FontError::NotAFont { .. }));
        assert!(err.to_string().contains(&fake.display().to_string()));

        std::fs::remove_file(&fake).unwrap();
        let files = expand(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(files, vec![ttf]);
    }
}

use std::fmt;

/// A render that did not finish, and the part of the chapter that stopped it.
///
/// `layer` is what a person reads to know where to look: `hook 2 (song-b)
/// vocals`, `cut 14 (song-c)`, `dependency demucs`, `file`. It is stored in the
/// job's state file and served by `/api/health/renders` exactly as written.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderError {
    pub layer: String,
    pub message: String,
}

impl RenderError {
    pub fn new(layer: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            layer: layer.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.layer, self.message)
    }
}

impl std::error::Error for RenderError {}

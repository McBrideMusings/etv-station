use std::path::PathBuf;

/// Output geometry. 1080p regardless of the sources; ETV-next normalizes again
/// on air, so this only has to be the largest picture the channel can use.
pub const WIDTH: u32 = 1920;
pub const HEIGHT: u32 = 1080;
pub const FPS: u32 = 30;

/// What the renderer runs and where it keeps what it makes.
///
/// Every program is located through configuration, never a fixed path: the
/// environment variables below name them, and the defaults are the bare names a
/// `PATH` lookup resolves. The renderer starts without any of them; what is
/// missing is reported by [`crate::deps::detect`] and named in the failure of
/// the render that needed it.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Stems and scratch files live under here. Rendered chapters land where
    /// each plan's `output` says.
    pub cache_dir: PathBuf,
    /// `ETV_RENDER_FFMPEG`: the ffmpeg to run.
    pub ffmpeg: String,
    /// `ETV_RENDER_FFPROBE`: the ffprobe to run.
    pub ffprobe: String,
    /// `ETV_RENDER_PYTHON`: the interpreter Demucs is installed into.
    pub python: String,
    /// `ETV_ACCEL`: the hardware backend the station's channels encode with;
    /// the renderer uses the same one when this ffmpeg has its encoder.
    pub accel: Option<String>,
    /// `ETV_VAAPI_DEVICE`: the render node a VAAPI encode opens.
    pub vaapi_device: Option<String>,
}

impl Settings {
    pub fn from_env(cache_dir: PathBuf) -> Self {
        Self::from_lookup(cache_dir, |k| std::env::var(k).ok())
    }

    pub fn from_lookup(cache_dir: PathBuf, lookup: impl Fn(&str) -> Option<String>) -> Self {
        let var = |k: &str| lookup(k).filter(|v| !v.is_empty());
        Self {
            cache_dir,
            ffmpeg: var("ETV_RENDER_FFMPEG").unwrap_or_else(|| "ffmpeg".into()),
            ffprobe: var("ETV_RENDER_FFPROBE").unwrap_or_else(|| "ffprobe".into()),
            python: var("ETV_RENDER_PYTHON").unwrap_or_else(|| "python3".into()),
            accel: var("ETV_ACCEL"),
            vaapi_device: var("ETV_VAAPI_DEVICE"),
        }
    }
}

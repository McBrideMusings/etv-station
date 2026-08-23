use std::path::{Path, PathBuf};

use ersatztv::config::ChannelConfig;
use ersatztv::error::LineupError;
use serde::Deserialize;

#[derive(Deserialize, Default)]
struct ConfigHint {
    #[serde(default)]
    normalization: NormalizationHint,
}

#[derive(Deserialize, Default)]
struct NormalizationHint {
    #[serde(default)]
    video: BitrateHint,
    #[serde(default)]
    audio: BitrateHint,
    #[serde(default)]
    subtitle: SubtitleHint,
}

#[derive(Deserialize, Default)]
struct BitrateHint {
    bitrate_kbps: Option<u32>,
}

/// The channel's subtitle settings as this crate needs to read them. The
/// authoritative definitions live in `ersatztv-channel`, which this crate does
/// not depend on, so the shape is mirrored here the same way the bitrate hint is.
#[derive(Deserialize, Default)]
struct SubtitleHint {
    #[serde(default)]
    mode: SubtitleModeHint,
    #[serde(default)]
    language: SubtitleLanguageHint,
}

#[derive(Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
enum SubtitleModeHint {
    #[default]
    Burn,
    Convert,
}

#[derive(Deserialize)]
struct SubtitleLanguageHint {
    #[serde(default = "default_language_tag")]
    tag: String,
    #[serde(default = "default_language_name")]
    name: String,
}

fn default_language_tag() -> String {
    String::from("en")
}

fn default_language_name() -> String {
    String::from("English")
}

impl Default for SubtitleLanguageHint {
    fn default() -> Self {
        SubtitleLanguageHint {
            tag: default_language_tag(),
            name: default_language_name(),
        }
    }
}

pub struct ChannelModel {
    number: String,
    name: String,
    config_path: PathBuf,
    overlay_paths: Vec<PathBuf>,
    output_folder: PathBuf,
    tvg_id: String,
    playout_folder: PathBuf,
    logo: Option<String>,
    group: Option<String>,
    bandwidth_bps: u32,
    subtitle: SubtitleHint,
}

#[derive(Deserialize)]
struct ChannelConfigPlayoutOnly {
    playout: PlayoutFolder,
}

#[derive(Deserialize)]
struct PlayoutFolder {
    folder: String,
}

impl ChannelModel {
    pub async fn new(
        config_path: &Path,
        output_folder: &Path,
        channel: ChannelConfig,
    ) -> Result<ChannelModel, LineupError> {
        let parent = config_path
            .parent()
            .ok_or(LineupError::LineupConfigNoParent)?;

        let channel_config = ersatztv::validate_config_path(parent, &channel.config).await?;

        let mut overlay_paths: Vec<PathBuf> = Vec::new();
        for overlay in &channel.overlays {
            let overlay_path = ersatztv::validate_config_path(parent, overlay).await?;
            overlay_paths.push(overlay_path);
        }

        let config_base = tokio::fs::read_to_string(&channel_config)
            .await
            .map_err(|e| LineupError::ChannelConfigRead {
                path: channel_config.display().to_string(),
                source: e,
            })?;

        let mut merged: serde_json::Value = serde_json::Value::Null;
        if let Ok(value) = serde_json::from_str(&config_base) {
            merged = value;
        }

        for overlay_path in &overlay_paths {
            if let Ok(overlay_str) = tokio::fs::read_to_string(overlay_path).await
                && let Ok(overlay_value) = serde_json::from_str(&overlay_str)
            {
                ersatztv_core::deep_merge(&mut merged, overlay_value);
            }
        }

        let hint = serde_json::from_value::<ConfigHint>(merged).unwrap_or_default();

        let video = hint.normalization.video.bitrate_kbps.unwrap_or(4000);
        let audio = hint.normalization.audio.bitrate_kbps.unwrap_or(192);
        let bandwidth_bps = (video + audio) * 1100; // kbps => bps + 10% for hls overhead

        let subtitle = hint.normalization.subtitle;

        let playout_folder = read_playout_folder(&channel_config, &config_base).await?;

        Ok(ChannelModel {
            number: channel.number.clone(),
            name: channel.name.clone(),
            config_path: channel_config,
            overlay_paths,
            output_folder: output_folder.join(&channel.number),
            tvg_id: channel
                .tvg_id
                .unwrap_or_else(|| format!("ersatztv.{}", channel.number)),
            playout_folder,
            logo: channel.logo.clone(),
            group: channel.group.clone(),
            bandwidth_bps,
            subtitle,
        })
    }

    pub fn number(&self) -> &str {
        self.number.as_str()
    }

    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    pub fn config_path(&self) -> &Path {
        self.config_path.as_path()
    }

    pub fn overlay_paths(&self) -> &[PathBuf] {
        self.overlay_paths.as_ref()
    }

    pub fn output_folder(&self) -> &Path {
        self.output_folder.as_path()
    }

    pub fn tvg_id(&self) -> &str {
        self.tvg_id.as_str()
    }

    pub fn playout_folder(&self) -> &Path {
        self.playout_folder.as_path()
    }

    pub fn logo(&self) -> Option<&str> {
        self.logo.as_deref()
    }

    pub fn group(&self) -> Option<&str> {
        self.group.as_deref()
    }

    pub fn bandwidth_bps(&self) -> u32 {
        self.bandwidth_bps
    }

    /// Whether this channel serves subtitles as their own selectable track.
    /// Burned-in subtitles are part of the video picture, so there is nothing to
    /// announce and no subtitle playlist to serve.
    pub fn has_subtitle_track(&self) -> bool {
        self.subtitle.mode == SubtitleModeHint::Convert
    }

    pub fn subtitle_language_tag(&self) -> &str {
        self.subtitle.language.tag.as_str()
    }

    pub fn subtitle_language_name(&self) -> &str {
        self.subtitle.language.name.as_str()
    }
}

/// Resolves `playout.folder` the same way `ersatztv-channel` does: through
/// `ersatztv_core::resolve_relative_paths`, which owns tilde-expansion and
/// relative-path joining for both crates. This function does not reimplement
/// any of that — it only turns the shared resolver's outcome into the errors
/// below. `resolve_relative_paths` can itself fail, when the resolved path
/// is not valid UTF-8; that failure is propagated as `LineupError::PathResolve`
/// rather than silently writing a corrupted path back into the config.
async fn read_playout_folder(
    channel_config_path: &Path,
    body: &str,
) -> Result<PathBuf, LineupError> {
    let parsed: ChannelConfigPlayoutOnly =
        serde_json::from_str(body).map_err(|e| LineupError::ChannelConfigParse {
            path: channel_config_path.display().to_string(),
            source: e,
        })?;

    let parent = channel_config_path
        .parent()
        .ok_or(LineupError::LineupConfigFailure(String::from(
            "failed to find parent of channel config",
        )))?;

    let mut value = serde_json::json!({ "playout": { "folder": parsed.playout.folder } });
    ersatztv_core::resolve_relative_paths(&mut value, parent, &["/playout/folder"])?;

    let resolved = value
        .pointer("/playout/folder")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let resolved_path = PathBuf::from(resolved);

    // A resolved folder whose first component is still a bare `~` means the
    // shared resolver's tilde-expansion silently gave up — its only failure
    // mode is that the home directory can't be determined — and fell back to
    // treating that segment as a plain path piece rather than erroring.
    //
    // The component must be *exactly* `~`, matching what expansion actually
    // attempts: `simple_expand_tilde::expand_tilde` tests `Path::starts_with("~")`,
    // which compares whole components. A directory legitimately named
    // `~backup` is therefore never expanded, never fails, and must not be
    // read as a failure just because its name opens with a tilde.
    let raw_first_component = Path::new(&parsed.playout.folder).components().next();
    let tilde_expand_failed = raw_first_component.is_some_and(|first| {
        first.as_os_str() == "~" && resolved_path.components().any(|c| c == first)
    });
    if tilde_expand_failed {
        return Err(LineupError::PlayoutFolderExpand(parsed.playout.folder));
    }

    tokio::fs::canonicalize(&resolved_path)
        .await
        .map_err(|e| LineupError::PlayoutFolderResolve {
            path: resolved_path.display().to_string(),
            source: e,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn write_channel_config(dir: &Path, folder: &str) -> (PathBuf, String) {
        let cfg_path = dir.join("channel.json");
        let body = serde_json::json!({ "playout": { "folder": folder } }).to_string();
        tokio::fs::write(&cfg_path, &body).await.unwrap();
        (cfg_path, body)
    }

    #[tokio::test]
    async fn missing_playout_folder_raises_resolve_error() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg_path, body) = write_channel_config(dir.path(), "does-not-exist").await;

        let err = read_playout_folder(&cfg_path, &body).await.unwrap_err();

        match err {
            LineupError::PlayoutFolderResolve { path, .. } => {
                assert!(
                    path.ends_with("does-not-exist"),
                    "resolve error should carry the folder that could not be canonicalized, got {path}"
                );
            }
            other => panic!("expected PlayoutFolderResolve, got: {other}"),
        }
    }

    // A leading component that merely *begins* with `~` — `~otheruser`,
    // `~backup` — is never a candidate for expansion: the shared resolver's
    // crate tests `Path::starts_with("~")`, which compares whole components,
    // so only a bare `~` is ever replaced. Such a component surviving into
    // the resolved path is therefore not evidence that expansion failed; it
    // is just the directory's name.
    //
    // Reading it as a failure is a real bug that shipped and stopped the
    // server from starting on a lineup whose only channel pointed at a real
    // directory named `~backup` (etv-station#239, fixed in 0f837a8). What
    // must happen instead is an ordinary resolve failure naming the path
    // that was actually looked for. #247 tracks deleting the inference that
    // makes this delicate.
    #[tokio::test]
    async fn a_name_that_merely_starts_with_a_tilde_is_not_a_failed_expansion() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg_path, body) = write_channel_config(dir.path(), "~otheruser/movies").await;

        let err = read_playout_folder(&cfg_path, &body).await.unwrap_err();

        match err {
            LineupError::PlayoutFolderResolve { path, .. } => {
                assert!(
                    path.ends_with("~otheruser/movies"),
                    "resolve error should name the path actually looked for, got {path}"
                );
            }
            other => panic!("expected PlayoutFolderResolve, got: {other}"),
        }
    }

    // The same shape, but for a directory that exists: it must load, not be
    // rejected. This is the case that broke in production.
    #[tokio::test]
    async fn a_real_directory_named_with_a_leading_tilde_resolves() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(dir.path().join("~backup/media"))
            .await
            .unwrap();
        let (cfg_path, body) = write_channel_config(dir.path(), "~backup/media").await;

        let resolved = read_playout_folder(&cfg_path, &body)
            .await
            .expect("a real directory whose name starts with ~ must resolve");

        assert!(resolved.ends_with("~backup/media"), "got {resolved:?}");
    }

    // A bare `~` is the one leading component the shared resolver's
    // tilde-expansion crate does replace, with the real home directory —
    // which every machine running this test has, so canonicalizing it
    // always succeeds. This must never be misdiagnosed as a failed expand;
    // it is the other half of the regression guard #247 is about.
    #[tokio::test]
    async fn expandable_tilde_does_not_raise_expand_error() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg_path, body) = write_channel_config(dir.path(), "~").await;

        let resolved = read_playout_folder(&cfg_path, &body)
            .await
            .expect("a bare ~ should expand to the real home directory and canonicalize");

        assert_ne!(
            resolved.as_os_str(),
            "~",
            "tilde should have been expanded, not passed through"
        );
    }

    // A literal directory named `~` that appears somewhere other than the
    // first path component must never be mistaken for a failed tilde-expansion.
    // The guard only checks whether the raw first component is exactly `~`
    // — if it is, and the resolved path still contains a bare `~` component,
    // then expansion failed. But a path like `movies/~/reruns` has a raw
    // first component of `movies`, so the guard never fires, and the path
    // should resolve normally as long as the directory exists.
    //
    // This test catches regressions that would loosen the guard to match any
    // `~` component in the resolved path, rather than comparing only against
    // the raw value's own first component.
    #[tokio::test]
    async fn a_literal_tilde_directory_in_the_middle_resolves() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(dir.path().join("movies/~/reruns"))
            .await
            .unwrap();
        let (cfg_path, body) = write_channel_config(dir.path(), "movies/~/reruns").await;

        let resolved = read_playout_folder(&cfg_path, &body)
            .await
            .expect("a real directory with a literal ~ in the middle must resolve");

        assert!(resolved.ends_with("movies/~/reruns"), "got {resolved:?}");
    }

    // A channel config directory (base_dir for the resolver) whose name is not
    // valid UTF-8 must surface as LineupError::PathResolve, not the ordinary
    // PlayoutFolderResolve "no such file" — the latter would misdiagnose a
    // lossy-conversion corruption as a missing directory (#246). The JSON
    // folder string itself is necessarily valid UTF-8 (serde_json::Value::String
    // guarantees it), so the invalid bytes have to come from the directory the
    // channel config lives in, not the folder field.
    //
    // Gated to Linux, not just `unix`: ext4 (and this daemon's Linux Docker
    // production target) stores filenames as arbitrary byte sequences, but
    // macOS's APFS validates UTF-8 at the filesystem layer and refuses to
    // create a directory named with an invalid byte sequence in the first
    // place (`EILSEQ`) — before the resolver under test ever runs.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn non_utf8_base_dir_raises_path_resolve_error() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        let bad_dir = dir.path().join(OsStr::from_bytes(b"bad\xFFname"));
        tokio::fs::create_dir_all(bad_dir.join("media"))
            .await
            .unwrap();
        let (cfg_path, body) = write_channel_config(&bad_dir, "media").await;

        let err = read_playout_folder(&cfg_path, &body).await.unwrap_err();

        match err {
            LineupError::PathResolve(_) => {}
            other => panic!("expected PathResolve, got: {other}"),
        }
    }
}

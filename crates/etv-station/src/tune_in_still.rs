//! A tune-in sample as a picture, for taste-lab's Review view: the frame of
//! the airing on screen at that moment, scaled onto the channel's canvas, with
//! the channel's resolved overlay drawn over it by [`etv_overlay::still`] — the
//! engine and renderer the channel's overlay process runs, not a second one.
//!
//! The overlay frame reaches ffmpeg the way the running overlay reaches
//! ETV-next's: raw frame bytes on an input in the spec's pixel format, laid
//! over the program with the `overlay` filter.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use etv_overlay::{OverlayKind, OverlaySpec, ProgramContext};
use sha2::{Digest, Sha256};
use time::Duration;

use crate::simulate::SimulatedAiring;

/// Everything that decides one tune-in still.
#[derive(Debug, Clone)]
pub struct TuneInStill {
    /// The airing's media file, as this machine sees it.
    pub media: PathBuf,
    /// Seconds into the media file the sample lands.
    pub at_secs: f64,
    /// The channel's output frame size; the media is scaled into it.
    pub canvas: (u32, u32),
    /// What the overlay process draws during the airing's block, or `None`.
    pub overlay: Option<OverlaySpec>,
    pub program: ProgramContext,
}

/// What [`TuneInStill::render`] drew.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedStill {
    /// The overlay script time the frame was rendered at, `None` with no
    /// overlay.
    pub overlay_time: Option<f64>,
    /// Whether that time is one where the overlay names the next title.
    pub shows_next: bool,
}

impl TuneInStill {
    /// A hex digest of every input above plus every file the overlay reads
    /// (its script and its image layers) and the media file's size and
    /// modification time, so a cache keyed on it is never stale: an edit to
    /// the channel, its overlay, its script or a logo, a replaced media file,
    /// or a sample that moves, changes the key.
    pub fn digest(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.media.to_string_lossy().as_bytes());
        if let Ok(meta) = std::fs::metadata(&self.media) {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            h.update(format!("|{}|{mtime}", meta.len()));
        }
        h.update(format!(
            "|{}|{}x{}|",
            self.at_secs, self.canvas.0, self.canvas.1
        ));
        h.update(format!("{:?}", self.program));
        if let Some(spec) = &self.overlay {
            h.update(serde_json::to_vec(spec).unwrap_or_default());
            let images = spec.layers.iter().filter_map(|l| match l {
                OverlayKind::Image { path, .. } => Some(path),
                _ => None,
            });
            for file in spec.script.iter().chain(images) {
                h.update(std::fs::read(file).unwrap_or_default());
            }
        }
        h.finalize()
            .iter()
            .take(12)
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// Grab the frame at [`Self::at_secs`], lay the overlay over it, and write
    /// a JPEG to `out`. The overlay is rendered at the first moment its script
    /// names the next title (a Now/Next snipe's NEXT card), or at time zero
    /// when it never does.
    pub fn render(&self, ffmpeg: &Path, out: &Path) -> Result<RenderedStill, String> {
        let (w, h) = self.canvas;
        let mut overlay_frame = None;
        let mut rendered = RenderedStill {
            overlay_time: None,
            shows_next: false,
        };
        if let Some(spec) = &self.overlay {
            let moment = etv_overlay::still::lookahead_moment(spec, &self.program)
                .map_err(|e| format!("overlay script: {e}"))?;
            let t = moment.unwrap_or(0.0);
            let rgba = etv_overlay::still::render_rgba(spec, t, &self.program)
                .map_err(|e| format!("overlay render: {e}"))?;
            overlay_frame = Some((spec, rgba));
            rendered = RenderedStill {
                overlay_time: Some(t),
                shows_next: moment.is_some(),
            };
        }

        let fit = format!(
            "[0:v]scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1"
        );
        let mut cmd = Command::new(ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-ss"])
            .arg(format!("{:.3}", self.at_secs))
            .arg("-i")
            .arg(&self.media);
        let filter = match &overlay_frame {
            Some((spec, _)) => {
                cmd.args(["-f", "rawvideo", "-pixel_format"])
                    .arg(spec.pixel_format.ffmpeg_arg())
                    .arg("-video_size")
                    .arg(format!("{}x{}", spec.width, spec.height))
                    .args(["-i", "pipe:0"]);
                format!("{fit}[b];[b][1:v]overlay=x=0:y=0:format=auto[v]")
            }
            None => format!("{fit}[v]"),
        };
        cmd.args(["-filter_complex", &filter, "-map", "[v]", "-frames:v", "1"])
            .args(["-q:v", "3"])
            .arg(out)
            .stdin(if overlay_frame.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn {}: {e}", ffmpeg.display()))?;
        // The frame is larger than a pipe buffer, so it is fed from its own
        // thread while this one drains stderr; a closed pipe just means
        // ffmpeg stopped early, which its exit status reports.
        let feeder = match (overlay_frame, child.stdin.take()) {
            (Some((_, rgba)), Some(mut stdin)) => Some(std::thread::spawn(move || {
                let _ = stdin.write_all(&rgba);
            })),
            _ => None,
        };
        let output = child
            .wait_with_output()
            .map_err(|e| format!("wait for ffmpeg: {e}"))?;
        if let Some(feeder) = feeder {
            let _ = feeder.join();
        }
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("ffmpeg {}: {}", output.status, stderr.trim()));
        }
        Ok(rendered)
    }
}

/// The overlay's view of `now`, `into` it, with `next` airing after: the
/// context the running overlay would read off the two playout items, built by
/// the same reader ([`ProgramContext::from_playout_programs`]) from the
/// programs the simulation rendered.
pub fn program_context(
    now: &SimulatedAiring,
    next: Option<&SimulatedAiring>,
    into: Duration,
) -> ProgramContext {
    ProgramContext::from_playout_programs(
        now.program.as_ref(),
        next.and_then(|n| n.program.as_ref()),
        into.as_seconds_f64(),
        (now.duration - into).as_seconds_f64(),
        now.metadata.clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn airing(program: Value) -> SimulatedAiring {
        SimulatedAiring {
            id: "id".into(),
            block: 0,
            show_id: None,
            start: time::OffsetDateTime::UNIX_EPOCH,
            duration: Duration::minutes(42),
            metadata: Some(json!({ "pool": "series" })),
            program: Some(program),
        }
    }

    #[test]
    fn the_context_reads_the_rendered_programs_of_both_airings() {
        let now =
            airing(json!({ "title": "Heroes", "sub_title": "Genesis", "season": 1, "episode": 2 }));
        let next = airing(json!({ "title": "SyFy Presents: Alien", "year": 1979 }));
        let ctx = program_context(&now, Some(&next), Duration::minutes(10));
        assert_eq!(
            (ctx.title.as_str(), ctx.sub_title.as_str()),
            ("Heroes", "Genesis")
        );
        assert_eq!((ctx.season, ctx.episode), (1, 2));
        assert_eq!(ctx.next_title, "SyFy Presents: Alien");
        assert_eq!((ctx.next_season, ctx.next_year), (-1, 1979));
        assert_eq!((ctx.item_elapsed, ctx.item_remaining), (600.0, 1920.0));
        assert_eq!(ctx.metadata, json!({ "pool": "series" }));
    }

    #[test]
    fn replacing_a_logo_changes_the_digest() {
        let dir = tempfile::tempdir().unwrap();
        let logo = dir.path().join("logo.png");
        std::fs::write(&logo, b"one").unwrap();
        let spec = OverlaySpec::from_yaml_str(&format!(
            "width: 64\nheight: 36\nframerate: 30\nlayers:\n  - type: image\n    path: {}\n    corner: bottom_right\n",
            logo.display()
        ))
        .unwrap();
        let still = TuneInStill {
            media: dir.path().join("missing.mkv"),
            at_secs: 1.0,
            canvas: (64, 36),
            overlay: Some(spec),
            program: ProgramContext::unknown(),
        };
        let before = still.digest();
        std::fs::write(&logo, b"two").unwrap();
        assert_ne!(still.digest(), before);
    }
}

//! A tune-in sample as a picture, for the web UI's Review view: the frame of
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

use ersatztv_channel::config::VideoNormalizationConfig;
use etv_overlay::{OverlayKind, OverlaySpec, ProgramContext};
use sha2::{Digest, Sha256};
use time::Duration;

use crate::catalog::EntrySource;
use crate::config::ChannelOverlays;
use crate::simulate::SimulatedAiring;

/// Why a tune-in sample has no still, as the Review page shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Unavailable {
    pub why: String,
    /// The media path that was looked for, when the sample got that far.
    pub path: Option<PathBuf>,
}

/// Where a channel's media paths land on this machine: the
/// `MEDIA_PATH_FROM`/`MEDIA_PATH_TO` prefix remap the catalog ingester reads.
#[derive(Debug, Clone, Default)]
pub struct MediaRemap {
    pub from: String,
    pub to: String,
}

impl MediaRemap {
    pub fn from_env() -> Self {
        Self {
            from: std::env::var("MEDIA_PATH_FROM").unwrap_or_default(),
            to: std::env::var("MEDIA_PATH_TO").unwrap_or_default(),
        }
    }
}

/// Distinguishes the scratch files of two renders of one still running at
/// once, so neither renames the other's half-written file into place.
static RENDERS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
    /// The station's `fonts:` entries, registered for the overlay render.
    pub fonts: Vec<PathBuf>,
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

/// What [`TuneInStill::render_cached`] served.
#[derive(Debug, Clone, PartialEq)]
pub enum StillJpeg {
    /// An earlier request rendered it.
    Cached(Vec<u8>),
    /// This request rendered it.
    Rendered(Vec<u8>, RenderedStill),
}

/// The airing a still is taken from and the two beside it: the overlay reads
/// the one before as `prev_metadata` and the one after as its lookahead.
#[derive(Clone, Copy)]
pub struct Neighbours<'a> {
    pub prev: Option<&'a SimulatedAiring>,
    pub airing: &'a SimulatedAiring,
    pub next: Option<&'a SimulatedAiring>,
}

impl TuneInStill {
    /// The still for `at.airing`, sampled `into` it, or why there is none.
    /// `sources` is what the catalog returned for the airing; `video` is the
    /// station's normalization, whose size is the channel's canvas when it
    /// names one.
    pub fn offer<E: std::fmt::Display>(
        sources: Result<Vec<EntrySource>, E>,
        remap: &MediaRemap,
        overlays: &ChannelOverlays,
        video: &VideoNormalizationConfig,
        fonts: &[PathBuf],
        at: Neighbours<'_>,
        into: Duration,
    ) -> Result<Self, Unavailable> {
        let Neighbours { prev, airing, next } = at;
        let unavailable = |why: &str, path: Option<PathBuf>| Unavailable {
            why: why.to_string(),
            path,
        };
        let sources = sources.map_err(|e| unavailable(&format!("Catalog error: {e}"), None))?;
        let source = crate::resolve::pick_playback_source(&sources)
            .ok_or_else(|| unavailable("No media file in the catalog", None))?;
        let media = PathBuf::from(crate::catalog::ingest::plex::translate_path(
            &source.playback_path,
            &remap.from,
            &remap.to,
        ));
        if !media.exists() {
            // With no remap there is no share to be unmounted, only a path
            // that names nothing.
            let unmounted = !remap.to.is_empty() && !Path::new(&remap.to).is_dir();
            let why = if unmounted {
                "Media not mounted"
            } else {
                "Media file not found"
            };
            return Err(unavailable(why, Some(media)));
        }

        let overlay = overlays.for_block(airing.block).cloned();
        let canvas = match (video.width, video.height, &overlay) {
            (Some(w), Some(h), _) => (w, h),
            (_, _, Some(spec)) => (spec.width, spec.height),
            _ => (1280, 720),
        };
        Ok(Self {
            media,
            at_secs: into.as_seconds_f64(),
            canvas,
            overlay,
            program: program_context(prev, airing, next, into),
            fonts: fonts.to_vec(),
        })
    }

    /// The JPEG for this still, read from `dir/id` when an earlier call
    /// rendered it, else rendered there with `ffmpeg`. A render goes to a
    /// scratch file renamed into place, so a reader never sees a half-written
    /// JPEG and a failed render leaves nothing behind.
    pub fn render_cached(&self, ffmpeg: &Path, dir: &Path, id: &str) -> Result<StillJpeg, String> {
        let path = dir.join(id);
        if let Ok(jpeg) = std::fs::read(&path) {
            return Ok(StillJpeg::Cached(jpeg));
        }
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let n = RENDERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_extension(format!("{n}.partial.jpg"));
        let rendered = self
            .render(ffmpeg, &tmp)
            .and_then(|r| {
                std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
                Ok(r)
            })
            .inspect_err(|_| {
                let _ = std::fs::remove_file(&tmp);
            })?;
        let jpeg = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(StillJpeg::Rendered(jpeg, rendered))
    }

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
        for font in etv_overlay::fonts::expand(&self.fonts).unwrap_or_default() {
            h.update(font.to_string_lossy().as_bytes());
            if let Ok(meta) = std::fs::metadata(&font) {
                h.update(format!("|{}", meta.len()));
            }
        }
        if let Some(spec) = &self.overlay {
            h.update(serde_json::to_vec(spec).unwrap_or_default());
            let scripts = spec.graphics.iter().filter_map(|g| g.script.as_ref());
            let images = spec
                .graphics
                .iter()
                .flat_map(|g| &g.layers)
                .filter_map(|l| match l {
                    OverlayKind::Image { path, .. } => Some(path),
                    _ => None,
                });
            for file in scripts.chain(images) {
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
            let rgba = etv_overlay::still::render_rgba(spec, t, &self.program, &self.fonts)
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
                let (ow, oh) = contain((spec.width, spec.height), self.canvas);
                format!(
                    "{fit}[b];[1:v]scale={ow}:{oh}:flags=fast_bilinear,setsar=1[o];[b][o]overlay=x=0:y=0:format=auto[v]"
                )
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
/// programs the simulation rendered. `prev` is the airing before `now`, whose
/// metadata the overlay reads as `prev_metadata`.
pub fn program_context(
    prev: Option<&SimulatedAiring>,
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
        prev.and_then(|p| p.metadata.clone()),
    )
}

/// The size ETV-next scales a `size` overlay to on a `canvas` output: the
/// largest aspect-preserving fit, each side rounded to even and capped at the
/// canvas, then laid unpadded at the top-left. This is ffpipeline's
/// `ScalingMode::ScaleAndPad` on the overlay input
/// (`FrameSize::square_pixel_size_contain`).
fn contain(size: (u32, u32), canvas: (u32, u32)) -> (u32, u32) {
    let (sw, sh) = (f64::from(size.0), f64::from(size.1));
    let k = f64::min(f64::from(canvas.0) / sw, f64::from(canvas.1) / sh);
    let even = |v: f64| (v.round_ties_even() as u32).next_multiple_of(2);
    (even(sw * k).min(canvas.0), even(sh * k).min(canvas.1))
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
        let ctx = program_context(None, &now, Some(&next), Duration::minutes(10));
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

    /// A ProgramMetadata serialized the way `simulate` stores it reads back
    /// through the overlay's own playout reader.
    #[test]
    fn the_context_reads_a_serialized_program_metadata() {
        use ersatztv_playout::playout::ProgramMetadata;
        let meta = |title: &str, season, episode| {
            serde_json::to_value(ProgramMetadata {
                title: Some(title.into()),
                sub_title: Some("Pilot".into()),
                season,
                episode,
                year: Some(2006),
                ..Default::default()
            })
            .unwrap()
        };
        let now = airing(meta("Heroes", Some(1), Some(1)));
        let next = airing(meta("Eureka", Some(2), Some(5)));
        let ctx = program_context(None, &now, Some(&next), Duration::minutes(1));
        assert_eq!(
            (ctx.title.as_str(), ctx.season, ctx.episode),
            ("Heroes", 1, 1)
        );
        assert_eq!(
            (ctx.next_title.as_str(), ctx.next_season, ctx.next_episode),
            ("Eureka", 2, 5)
        );
        assert_eq!(ctx.year, 2006);
    }

    fn source(path: &str, missing: bool) -> EntrySource {
        EntrySource {
            source: crate::catalog::Source::LocalFs,
            source_id: path.into(),
            entry_id: "id".into(),
            playback_path: path.into(),
            last_seen: None,
            missing_since: missing.then(|| "2026-01-01T00:00:00Z".into()),
            unairable_reason: None,
        }
    }

    fn video(size: Option<(u32, u32)>) -> VideoNormalizationConfig {
        let json = match size {
            Some((w, h)) => json!({ "width": w, "height": h }),
            None => json!({}),
        };
        serde_json::from_value(json).unwrap()
    }

    /// A 64x36 overlay that draws a white 16px box in its top-left corner.
    fn box_spec() -> OverlaySpec {
        OverlaySpec::from_yaml_str(
            "width: 64\nheight: 36\nframerate: 30\ngraphics:\n  - layers:\n      - type: watermark\n        corner: top_left\n        margin: 0\n        box_size: 16\n        color: [255, 255, 255, 255]\n",
        )
        .unwrap()
    }

    fn offer_with(
        sources: Result<Vec<EntrySource>, &str>,
        remap: &MediaRemap,
        overlays: &ChannelOverlays,
        size: Option<(u32, u32)>,
    ) -> Result<TuneInStill, Unavailable> {
        let now = airing(json!({ "title": "Heroes" }));
        TuneInStill::offer(
            sources,
            remap,
            overlays,
            &video(size),
            &[],
            Neighbours {
                prev: None,
                airing: &now,
                next: None,
            },
            Duration::minutes(3),
        )
    }

    #[test]
    fn a_catalog_error_is_named() {
        let err = offer_with(
            Err("disk I/O"),
            &MediaRemap::default(),
            &ChannelOverlays::default(),
            None,
        )
        .unwrap_err();
        assert_eq!(err.why, "Catalog error: disk I/O");
        assert_eq!(err.path, None);
    }

    #[test]
    fn an_airing_with_no_catalog_source_has_no_still() {
        let err = offer_with(
            Ok(vec![]),
            &MediaRemap::default(),
            &ChannelOverlays::default(),
            None,
        )
        .unwrap_err();
        assert_eq!(err.why, "No media file in the catalog");
    }

    #[test]
    fn a_remap_onto_an_absent_share_reads_as_not_mounted() {
        let dir = tempfile::tempdir().unwrap();
        let share = dir.path().join("media");
        let remap = MediaRemap {
            from: "/data".into(),
            to: share.display().to_string(),
        };
        let err = offer_with(
            Ok(vec![source("/data/film.mkv", false)]),
            &remap,
            &ChannelOverlays::default(),
            None,
        )
        .unwrap_err();
        assert_eq!(err.why, "Media not mounted");
        assert_eq!(err.path, Some(share.join("film.mkv")));
    }

    #[test]
    fn a_missing_file_on_a_mounted_share_reads_as_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let remap = MediaRemap {
            from: "/data".into(),
            to: dir.path().display().to_string(),
        };
        let err = offer_with(
            Ok(vec![source("/data/film.mkv", false)]),
            &remap,
            &ChannelOverlays::default(),
            None,
        )
        .unwrap_err();
        assert_eq!(err.why, "Media file not found");
        assert_eq!(err.path, Some(dir.path().join("film.mkv")));
    }

    #[test]
    fn the_live_source_is_the_one_sampled() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("live.mkv");
        std::fs::write(&live, b"x").unwrap();
        let still = offer_with(
            Ok(vec![
                source(&dir.path().join("gone.mkv").display().to_string(), true),
                source(&live.display().to_string(), false),
            ]),
            &MediaRemap::default(),
            &ChannelOverlays::default(),
            Some((1920, 1080)),
        )
        .unwrap();
        assert_eq!(still.media, live);
        assert_eq!(still.at_secs, 180.0);
        assert_eq!(still.canvas, (1920, 1080));
        assert_eq!(still.overlay, None);
        assert_eq!(still.program.title, "Heroes");
    }

    #[test]
    fn the_canvas_falls_back_to_the_overlay_then_to_720p() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("a.mkv");
        std::fs::write(&media, b"x").unwrap();
        let sources = || Ok(vec![source(&media.display().to_string(), false)]);
        let with_overlay = ChannelOverlays {
            base: Some(box_spec()),
            blocks: vec![],
        };
        let still = offer_with(sources(), &MediaRemap::default(), &with_overlay, None).unwrap();
        assert_eq!(still.canvas, (64, 36));
        assert_eq!(still.overlay, Some(box_spec()));
        let still = offer_with(
            sources(),
            &MediaRemap::default(),
            &ChannelOverlays::default(),
            None,
        )
        .unwrap();
        assert_eq!(still.canvas, (1280, 720));
    }

    #[test]
    fn a_block_that_draws_nothing_gets_no_overlay() {
        let mut spec = box_spec();
        spec.graphics.clear();
        let overlays = ChannelOverlays {
            base: Some(box_spec()),
            blocks: vec![Some(spec.clone()), None],
        };
        assert_eq!(overlays.for_block(0), Some(&spec));
        assert_eq!(overlays.for_block(1), None);
        assert_eq!(ChannelOverlays::default().for_block(0), None);
    }

    /// Two seconds of black at 320x180, the media a render test samples. The
    /// render tests need ffmpeg and ffprobe on PATH and fail without them.
    fn black_fixture(dir: &Path) -> PathBuf {
        let path = dir.join("black.mkv");
        let status = Command::new("ffmpeg")
            .args(["-nostdin", "-y", "-hide_banner", "-loglevel", "error"])
            .args(["-f", "lavfi", "-i"])
            .arg("color=c=black:s=320x180:d=2:r=10")
            .args(["-c:v", "mpeg4"])
            .arg(&path)
            .status()
            .expect("ffmpeg on PATH");
        assert!(status.success());
        path
    }

    /// `jpeg` decoded to RGB rows, with its width.
    fn decode(jpeg: &Path) -> (Vec<u8>, usize) {
        let out = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(jpeg)
            .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
            .output()
            .unwrap();
        assert!(out.status.success());
        let probe = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
            .args(["stream=width", "-of", "csv=p=0"])
            .arg(jpeg)
            .output()
            .expect("ffprobe on PATH");
        assert!(probe.status.success());
        let width = String::from_utf8_lossy(&probe.stdout)
            .trim()
            .parse()
            .unwrap();
        (out.stdout, width)
    }

    fn luma(rgb: &[u8], width: usize, x: usize, y: usize) -> u8 {
        rgb[(y * width + x) * 3 + 1]
    }

    fn still_of(media: PathBuf, canvas: (u32, u32), overlay: Option<OverlaySpec>) -> TuneInStill {
        TuneInStill {
            media,
            at_secs: 1.0,
            canvas,
            overlay,
            program: ProgramContext::unknown(),
            fonts: Vec::new(),
        }
    }

    #[test]
    fn a_render_lays_the_overlay_over_the_fitted_frame() {
        let dir = tempfile::tempdir().unwrap();
        let media = black_fixture(dir.path());
        let out = dir.path().join("still.jpg");

        let still = still_of(media.clone(), (64, 36), Some(box_spec()));
        let rendered = still.render(Path::new("ffmpeg"), &out).unwrap();
        assert_eq!(
            rendered,
            RenderedStill {
                overlay_time: Some(0.0),
                shows_next: false
            }
        );
        let (rgb, width) = decode(&out);
        assert_eq!((width, rgb.len()), (64, 64 * 36 * 3));
        assert!(luma(&rgb, width, 6, 6) > 200, "inside the box");
        assert!(luma(&rgb, width, 50, 28) < 40, "outside the box");

        let bare = still_of(media, (64, 36), None);
        assert_eq!(
            bare.render(Path::new("ffmpeg"), &out).unwrap().overlay_time,
            None
        );
        let (rgb, width) = decode(&out);
        assert!(luma(&rgb, width, 6, 6) < 40, "no overlay, no box");
    }

    #[test]
    fn a_smaller_overlay_is_scaled_up_to_the_canvas() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("still.jpg");
        let still = still_of(black_fixture(dir.path()), (128, 72), Some(box_spec()));
        still.render(Path::new("ffmpeg"), &out).unwrap();
        let (rgb, width) = decode(&out);
        assert_eq!(width, 128);
        assert!(luma(&rgb, width, 24, 24) > 200, "inside the doubled box");
        assert!(luma(&rgb, width, 40, 40) < 40, "outside the doubled box");

        // A square canvas fits the 16:9 overlay by width and leaves it at the
        // top, unpadded: the box spans 0..32 down from y=0, not from y=28.
        let still = still_of(black_fixture(dir.path()), (128, 128), Some(box_spec()));
        still.render(Path::new("ffmpeg"), &out).unwrap();
        let (rgb, width) = decode(&out);
        assert!(luma(&rgb, width, 10, 10) > 200, "box at the top");
        assert!(luma(&rgb, width, 10, 50) < 40, "nothing below it");
    }

    #[test]
    fn the_overlay_fit_matches_ffpipeline() {
        assert_eq!(contain((1280, 720), (1280, 720)), (1280, 720));
        assert_eq!(contain((64, 36), (128, 128)), (128, 72));
        assert_eq!(contain((1920, 1080), (1280, 720)), (1280, 720));
        assert_eq!(contain((64, 36), (100, 100)), (100, 56));
        assert_eq!(contain((1280, 720), (1440, 1080)), (1440, 810));
    }

    #[test]
    fn a_failed_render_names_ffmpeg_and_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let still = still_of(dir.path().join("absent.mkv"), (64, 36), None);
        let err = still
            .render_cached(Path::new("ffmpeg"), &dir.path().join("cache"), "x.jpg")
            .unwrap_err();
        assert!(err.starts_with("ffmpeg "), "{err}");
        let left: Vec<_> = std::fs::read_dir(dir.path().join("cache"))
            .unwrap()
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn a_cached_still_is_read_back_without_rendering() {
        let dir = tempfile::tempdir().unwrap();
        let media = black_fixture(dir.path());
        let cache = dir.path().join("cache");
        let still = still_of(media, (64, 36), None);

        let StillJpeg::Rendered(first, _) = still
            .render_cached(Path::new("ffmpeg"), &cache, "s.jpg")
            .unwrap()
        else {
            panic!("the first request renders");
        };
        // A cache hit never runs ffmpeg, so a binary that cannot exist proves it.
        let second = still
            .render_cached(Path::new("/nonexistent/ffmpeg"), &cache, "s.jpg")
            .unwrap();
        assert_eq!(second, StillJpeg::Cached(first));
    }

    #[test]
    fn replacing_a_logo_changes_the_digest() {
        let dir = tempfile::tempdir().unwrap();
        let logo = dir.path().join("logo.png");
        std::fs::write(&logo, b"one").unwrap();
        let spec = OverlaySpec::from_yaml_str(&format!(
            "width: 64\nheight: 36\nframerate: 30\ngraphics:\n  - layers:\n      - type: image\n        path: {}\n        corner: bottom_right\n",
            logo.display()
        ))
        .unwrap();
        let still = TuneInStill {
            media: dir.path().join("missing.mkv"),
            at_secs: 1.0,
            canvas: (64, 36),
            overlay: Some(spec),
            program: ProgramContext::unknown(),
            fonts: Vec::new(),
        };
        let before = still.digest();
        std::fs::write(&logo, b"two").unwrap();
        assert_ne!(still.digest(), before);
    }
}

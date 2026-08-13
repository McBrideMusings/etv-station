use std::path::Path;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::{Iso8601, iso8601};

use crate::error::PlayoutError;

const DATE_CONFIG: iso8601::EncodedConfig =
    iso8601::Config::DEFAULT.set_use_separators(false).encode();

pub const DATE_FORMAT: Iso8601<DATE_CONFIG> = Iso8601::<DATE_CONFIG>;

pub const SUPPORTED_SCHEMA: SchemaVersion = SchemaVersion {
    breaking: 0,
    compatible: 3,
};
const VERSION_URI_PREFIX: &str = "https://ersatztv.org/playout/version/0.";

// TODO: support major version post-1.0
#[derive(Debug, Clone, Copy)]
pub struct SchemaVersion {
    pub breaking: u32,
    pub compatible: u32,
}

impl SchemaVersion {
    pub fn parse(uri: &str) -> Option<SchemaVersion> {
        let rest = uri.strip_prefix(VERSION_URI_PREFIX)?;
        let (b, a) = rest.split_once('.')?;
        Some(SchemaVersion {
            breaking: b.parse().ok()?,
            compatible: a.parse().ok()?,
        })
    }
}

/// A playout schedule for a single time window.
///
/// Files should be named `{start}_{finish}.json` using compact ISO 8601
/// (no separators), e.g. `20260413T000000.000000000-0500_20260414T002131.620000000-0500.json`,
/// so that the channel can locate the correct file for the current time.
#[derive(Debug, Deserialize, Serialize)]
pub struct Playout {
    /// URI identifying the schema version, e.g. "https://ersatztv.org/playout/version/0.0.1"
    pub version: String,
    pub items: Vec<PlayoutItem>,
}

impl Playout {
    pub fn new(items: Vec<PlayoutItem>) -> Self {
        Playout {
            version: format!(
                "{}{}.{}",
                VERSION_URI_PREFIX, SUPPORTED_SCHEMA.breaking, SUPPORTED_SCHEMA.compatible
            ),
            items,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PlayoutItem {
    pub id: String,
    /// RFC3339 formatted date/time, e.g. 2026-04-13T00:24:21.527-05:00
    #[serde(with = "time::serde::rfc3339")]
    pub start: OffsetDateTime,
    /// RFC3339 formatted date/time, e.g. 2026-04-13T00:24:21.527-05:00
    #[serde(with = "time::serde::rfc3339")]
    pub finish: OffsetDateTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<PlayoutItemSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracks: Option<PlayoutItemTracks>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watermark: Option<Watermark>,
    /// Optional EPG metadata for this item. Whatever is present here is emitted in
    /// the XMLTV guide; missing fields are simply omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program: Option<ProgramMetadata>,
    /// Optional overlay spec — when present, the channel reads RGBA frames from
    /// `fifo_path` and composites them on top of the video. The producer of the
    /// playout JSON is responsible for arranging for something to be writing to
    /// that fifo while this item is playing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay: Option<OverlaySpec>,
    /// Arbitrary per-airing data attached by whatever produced this playout
    /// JSON, carried through untouched.
    ///
    /// **Nothing in this repo reads it.** It is deserialized, held, and
    /// re-serialized; no key is reserved, no shape is validated, and an
    /// unrecognised key is not an error. It exists because the producer and
    /// the overlay process on the other side of `overlay.fifo_path` both read
    /// this file and need a channel between them that is per-item — the
    /// overlay spec is per-item too, but its fields are all geometry, and
    /// widening them for one producer's vocabulary would put that producer's
    /// concepts into this schema.
    ///
    /// A producer that attaches nothing leaves this absent, which is the
    /// common case and costs nothing on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Spec for a live overlay source feeding the secondary input of an ffmpeg
/// `overlay` filter. The channel worker opens the fifo itself, on a deadline,
/// and hands ffmpeg the already-open descriptor — ffmpeg's own `open()` on a
/// fifo with no writer never returns, which would wedge the channel forever.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct OverlaySpec {
    /// Filesystem path the channel will open for reading. Must exist as a fifo
    /// (or regular file) by the time the channel starts the item.
    pub fifo_path: String,
    /// Output pixel format of the overlay frames (default "rgba").
    #[serde(default = "default_overlay_pixel_format")]
    pub pixel_format: String,
    /// Width of the overlay raster in pixels.
    pub width: u32,
    /// Height of the overlay raster in pixels.
    pub height: u32,
    /// Framerate of the rawvideo stream produced by the overlay process.
    pub framerate: u32,
    /// Overlay top-left x relative to the main video (default 0).
    #[serde(default)]
    pub x: i32,
    /// Overlay top-left y relative to the main video (default 0).
    #[serde(default)]
    pub y: i32,
}

fn default_overlay_pixel_format() -> String {
    String::from("rgba")
}

/// Program metadata used to populate the XMLTV EPG. Every field is optional; the
/// server emits whatever is provided and skips the rest. Producers of playout JSON
/// are responsible for filling these in — this repo never sources them externally.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct ProgramMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub season: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub categories: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_rating: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artwork_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<u32>,
    /// Cast and crew credits, grouped by role. Emitted as `<credits>` with
    /// children in XMLTV's required order (director, actor, writer)
    /// regardless of the order populated here.
    ///
    /// Boxed because `Credits` (two name lists plus a cast list) is by far
    /// the largest field here, and `ProgramMetadata` is embedded by value in
    /// every type that carries it. Most programmes have no credits, so the
    /// pointer keeps those types small in the common case.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credits: Option<Box<Credits>>,
    /// Country (or countries) of origin, e.g. `["United States"]`. Emitted
    /// as one `<country>` element per entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<Vec<String>>,
    /// Pre-formatted star rating, e.g. `"4 / 5"` per the XMLTV `star-rating`
    /// convention (`N / M`, whitespace around the slash is ignored).
    /// Emitted verbatim as `<star-rating><value>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub star_rating: Option<String>,
}

/// Cast and crew credits for a programme, grouped by XMLTV role. Each role
/// list is independently optional; an absent role emits no elements for it.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Credits {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub director: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actor: Vec<Actor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writer: Vec<String>,
}

/// A single actor credit, with an optional character/role name — XMLTV's
/// `<actor role="...">`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Actor {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

impl PlayoutItem {
    pub fn new(
        id: String,
        start: OffsetDateTime,
        finish: OffsetDateTime,
        in_point: Option<std::time::Duration>,
        out_point: Option<std::time::Duration>,
        path: &Path,
    ) -> Result<PlayoutItem, PlayoutError> {
        Ok(PlayoutItem {
            id,
            start,
            finish,
            source: Some(PlayoutItemSource::Local {
                path: path.to_string_lossy().to_string(),
                in_point_ms: in_point.map(|d| d.as_millis() as u64),
                out_point_ms: out_point.map(|d| d.as_millis() as u64),
                probe_hint: None,
            }),
            tracks: None,
            watermark: None,
            program: None,
            overlay: None,
            metadata: None,
        })
    }

    pub fn finish(&self) -> OffsetDateTime {
        self.finish
    }

    /// Whether this item names a subtitle track selection at all. `false`
    /// covers both "no `tracks` block" and "`tracks` present but no
    /// `subtitle` selection" — either way, nothing asked for subtitles, so
    /// nothing should be produced or advertised for it. This says nothing
    /// about whether the selected stream can actually yield cues (a
    /// picture-based Blu-ray/DVD subtitle track still resolves `true` here);
    /// that can only be known once the stream is probed.
    pub fn requests_subtitle(&self) -> bool {
        self.tracks
            .as_ref()
            .is_some_and(|tracks| tracks.subtitle.is_some())
    }

    /// Construct a scheduled item from an already-resolved source, defaulting
    /// every optional field (`tracks`/`watermark`/`program`/`overlay`/`metadata`). Callers
    /// set whichever optionals they drive afterward. New optional schema fields
    /// default here, so producers that use this constructor don't need editing
    /// when the schema grows.
    pub fn scheduled(
        id: String,
        start: OffsetDateTime,
        finish: OffsetDateTime,
        source: PlayoutItemSource,
    ) -> PlayoutItem {
        PlayoutItem {
            id,
            start,
            finish,
            source: Some(source),
            tracks: None,
            watermark: None,
            program: None,
            overlay: None,
            metadata: None,
        }
    }
}

impl PlayoutItemSource {
    /// Local file source, defaulting `probe_hint`.
    pub fn local(path: String, in_point_ms: Option<u64>, out_point_ms: Option<u64>) -> Self {
        PlayoutItemSource::Local {
            path,
            in_point_ms,
            out_point_ms,
            probe_hint: None,
        }
    }

    /// lavfi source, defaulting `probe_hint`.
    pub fn lavfi(params: String) -> Self {
        PlayoutItemSource::Lavfi {
            params,
            probe_hint: None,
        }
    }

    /// HTTP source, defaulting the ffmpeg-tuning and `probe_hint` fields a
    /// producer typically doesn't set.
    pub fn http(
        uri: String,
        in_point_ms: Option<u64>,
        out_point_ms: Option<u64>,
        headers: Option<Vec<String>>,
        user_agent: Option<String>,
    ) -> Self {
        PlayoutItemSource::Http {
            uri,
            in_point_ms,
            out_point_ms,
            headers,
            user_agent,
            is_live: None,
            timeout_us: None,
            reconnect: None,
            reconnect_delay_max: None,
            keep_alive: None,
            probe_hint: None,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PlayoutItemTracks {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<TrackSelection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video: Option<TrackSelection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<TrackSelection>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TrackSelection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<PlayoutItemSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_index: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Watermark {
    pub source: PlayoutItemSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_index: Option<u32>,
    pub location: WatermarkLocation,
    /// Scale to this percent of primary content width (0–100).
    /// Omitted = actual size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width_percent: Option<f32>,
    /// When `true`, position margins are measured from the edges of the source
    /// content rather than the padded output frame, so letterbox/pillarbox bars
    /// push the watermark inward and keep it inside the visible content. When
    /// `false`, margins are relative to the full padded frame, so a 0% margin
    /// can land inside the bars. Has no effect when the primary content fills
    /// the output (crop/stretch). Omitted = `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub within_source_content: Option<bool>,
    /// Horizontal offset from `location`, as percent of primary content width (0–100).
    /// Omitted = 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub horizontal_margin_percent: Option<f32>,
    /// Vertical offset from `location`, as percent of primary content height (0–100).
    /// Omitted = 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vertical_margin_percent: Option<f32>,
    /// Opacity as a percent (0–100). Omitted = fully opaque (100).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opacity_percent: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timing: Option<WatermarkTiming>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WatermarkLocation {
    TopLeft,
    TopCenter,
    TopRight,
    CenterLeft,
    Center,
    CenterRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "timing_type", rename_all = "snake_case")]
pub enum WatermarkTiming {
    Periodic {
        clock: PeriodicClock,
        frequency_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        phase_offset_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_after_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        fade_ms: Option<u64>,
        hold_ms: u64,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PeriodicClock {
    Wall,
    Content,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(tag = "source_type", rename_all = "snake_case")]
pub enum PlayoutItemSource {
    Local {
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        in_point_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        out_point_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        probe_hint: Option<ProbeHint>,
    },
    Lavfi {
        params: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        probe_hint: Option<ProbeHint>,
    },
    Http {
        /// URI template, e.g. "https://example.com/file.mkv?token={{MY_SECRET}}"
        uri: String,
        /// Whether the content is live and therefore cannot seek or work
        /// ahead (default: false)
        #[serde(skip_serializing_if = "Option::is_none")]
        is_live: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        in_point_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        out_point_ms: Option<u64>,
        /// Custom HTTP headers, e.g. ["Authorization: Bearer {{TOKEN}}"]
        #[serde(skip_serializing_if = "Option::is_none")]
        headers: Option<Vec<String>>,
        /// Custom user-agent string
        #[serde(skip_serializing_if = "Option::is_none")]
        user_agent: Option<String>,
        /// Socket timeout in microseconds
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout_us: Option<u64>,
        /// Enable reconnect on failure (default: true)
        #[serde(skip_serializing_if = "Option::is_none")]
        reconnect: Option<bool>,
        /// Max reconnect delay in seconds
        /// Maps directly to the reconnect_delay_max ffmpeg option
        #[serde(skip_serializing_if = "Option::is_none")]
        reconnect_delay_max: Option<u32>,
        /// Enable persistent connections in ffmpeg (default: false)
        #[serde(skip_serializing_if = "Option::is_none")]
        keep_alive: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        probe_hint: Option<ProbeHint>,
    },
    Rtsp {
        uri: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout_us: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        probe_hint: Option<ProbeHint>,
    },
    Script {
        /// Command that writes an MPEG-TS stream to its stdout
        command: String,
        /// Optional arguments for the command
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        /// Whether the content is live and therefore cannot work ahead (default: false)
        #[serde(skip_serializing_if = "Option::is_none")]
        is_live: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        probe_hint: Option<ProbeHint>,
    },
    Dynamic {
        /// URI template, e.g. "https://example.com/file.mkv?token={{MY_SECRET}}"
        uri: String,
        /// Custom HTTP headers, e.g. ["Authorization: Bearer {{TOKEN}}"]
        #[serde(skip_serializing_if = "Option::is_none")]
        headers: Option<Vec<String>>,
        /// Custom user-agent string
        #[serde(skip_serializing_if = "Option::is_none")]
        user_agent: Option<String>,
        /// Socket timeout in microseconds
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout_us: Option<u64>,
    },
}

impl PlayoutItemSource {
    pub fn probe_hint(&self) -> Option<&ProbeHint> {
        match self {
            PlayoutItemSource::Local { probe_hint, .. }
            | PlayoutItemSource::Lavfi { probe_hint, .. }
            | PlayoutItemSource::Http { probe_hint, .. }
            | PlayoutItemSource::Rtsp { probe_hint, .. }
            | PlayoutItemSource::Script { probe_hint, .. } => probe_hint.as_ref(),
            PlayoutItemSource::Dynamic { .. } => None,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct ProbeHint {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub video: Vec<VideoHint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio: Vec<AudioHint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subtitle: Vec<SubtitleHint>,
    pub format_name: Option<String>,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone, PartialEq)]
pub struct VideoHint {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub pix_fmt: String,
    pub stream_index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field_order: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_aspect_ratio: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_aspect_ratio: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_range: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_space: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_transfer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_primaries: Option<String>,
}

impl VideoHint {
    pub fn new(codec: String, width: u32, height: u32, pix_fmt: String) -> VideoHint {
        VideoHint {
            stream_index: 0,
            codec,
            width,
            height,
            pix_fmt,
            ..Default::default()
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct AudioHint {
    pub codec: String,
    pub channels: u32,
    pub stream_index: u32,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct SubtitleHint {
    pub codec: String,
    pub stream_index: u32,
}

pub struct PlayoutLoadResult {
    pub playout: Playout,
    // TODO: start, finish
}

pub async fn from_file(path: &str) -> Result<PlayoutLoadResult, PlayoutError> {
    #[derive(Deserialize)]
    struct PlayoutVersion {
        version: String,
    }

    let contents = tokio::fs::read_to_string(path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            PlayoutError::PlayoutJsonDoesNotExist
        } else {
            PlayoutError::PlayoutJsonLoadError(e.to_string())
        }
    })?;

    let version_only: PlayoutVersion = serde_json::from_str(&contents)
        .map_err(|e| PlayoutError::PlayoutJsonLoadError(e.to_string()))?;

    let found = SchemaVersion::parse(&version_only.version)
        .ok_or_else(|| PlayoutError::UnrecognizedSchemaVersion(version_only.version.clone()))?;

    if found.breaking != SUPPORTED_SCHEMA.breaking || found.compatible > SUPPORTED_SCHEMA.compatible
    {
        return Err(PlayoutError::UnsupportedSchemaVersion(
            version_only.version,
            format!(
                "{}{}.{}",
                VERSION_URI_PREFIX, SUPPORTED_SCHEMA.breaking, SUPPORTED_SCHEMA.compatible
            ),
        ));
    }

    let playout: Playout = serde_json::from_str(&contents)
        .map_err(|e| PlayoutError::PlayoutJsonLoadError(e.to_string()))?;

    Ok(PlayoutLoadResult { playout })
}

/// Whether any playout chunk file (`{start}_{finish}.json`) currently sitting
/// in `folder` contains an item that names a subtitle track selection.
///
/// This is the shared fact both the master-playlist decision (`ersatztv`,
/// which channel/HTTP-request the client's player at) and the channel's own
/// subtitle production (`ersatztv-channel`, the worker that actually plays
/// the schedule) can independently derive from the exact same source of
/// truth — the playout JSON directory — without either process having to
/// signal the other. A folder that can't be read, or contains no files that
/// parse, reports `false` rather than erroring, matching how a channel with
/// nothing scheduled yet behaves: nothing has asked for subtitles.
pub async fn schedule_requests_subtitle(folder: &Path) -> bool {
    let mut entries = match tokio::fs::read_dir(folder).await {
        Ok(entries) => entries,
        Err(_) => return false,
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }

        let Some(path_str) = path.to_str() else {
            continue;
        };

        if let Ok(loaded) = from_file(path_str).await
            && loaded
                .playout
                .items
                .iter()
                .any(PlayoutItem::requests_subtitle)
        {
            return true;
        }
    }

    false
}

pub fn parse_playout_filename(file_stem: &str) -> Option<(OffsetDateTime, OffsetDateTime)> {
    let split: Vec<&str> = file_stem.split("_").collect();
    if split.len() == 2 {
        let maybe_start = OffsetDateTime::parse(split[0], &DATE_FORMAT)
            .ok()
            .or_else(|| parse_unix_timestamp(split[0]));

        let maybe_finish = OffsetDateTime::parse(split[1], &DATE_FORMAT)
            .ok()
            .or_else(|| parse_unix_timestamp(split[1]));

        return match (maybe_start, maybe_finish) {
            (Some(start), Some(finish)) => Some((start, finish)),
            _ => None,
        };
    }

    None
}

fn parse_unix_timestamp(timestamp: &str) -> Option<OffsetDateTime> {
    let maybe_epoch = timestamp
        .parse::<i64>()
        .map(|i| if timestamp.len() > 10 { i / 1000 } else { i });

    if let Ok(epoch) = maybe_epoch {
        OffsetDateTime::from_unix_timestamp(epoch).ok()
    } else {
        None
    }
}

#[cfg(test)]
mod schedule_requests_subtitle_tests {
    use super::*;

    fn item(id: &str, wants_subtitle: bool) -> PlayoutItem {
        let start = OffsetDateTime::now_utc();
        let finish = start + time::Duration::minutes(5);
        let mut playout_item = PlayoutItem::scheduled(
            id.to_string(),
            start,
            finish,
            PlayoutItemSource::local("/media/file.mkv".to_string(), None, None),
        );
        if wants_subtitle {
            playout_item.tracks = Some(PlayoutItemTracks {
                audio: None,
                video: None,
                subtitle: Some(TrackSelection {
                    source: None,
                    stream_index: Some(2),
                }),
            });
        }
        playout_item
    }

    fn write_chunk(dir: &Path, name: &str, items: Vec<PlayoutItem>) {
        let playout = Playout::new(items);
        std::fs::write(dir.join(name), serde_json::to_string(&playout).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn true_when_any_chunk_file_has_an_item_requesting_subtitles() {
        let dir = tempfile::tempdir().unwrap();
        write_chunk(
            dir.path(),
            "chunk.json",
            vec![item("no-subs", false), item("wants-subs", true)],
        );

        assert!(schedule_requests_subtitle(dir.path()).await);
    }

    #[tokio::test]
    async fn false_when_no_item_in_any_chunk_file_requests_subtitles() {
        let dir = tempfile::tempdir().unwrap();
        write_chunk(dir.path(), "chunk.json", vec![item("no-subs", false)]);

        assert!(!schedule_requests_subtitle(dir.path()).await);
    }

    #[tokio::test]
    async fn false_when_folder_is_empty_or_missing() {
        let dir = tempfile::tempdir().unwrap();

        assert!(!schedule_requests_subtitle(dir.path()).await);
        assert!(!schedule_requests_subtitle(&dir.path().join("does-not-exist")).await);
    }
}

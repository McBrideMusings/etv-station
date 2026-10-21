use std::collections::BTreeMap;
use std::path::PathBuf;

use ersatztv_channel::config::{FfmpegConfig, NormalizationConfig, VideoFormat};
use serde::{Deserialize, Serialize};

use super::overlay::OverlayDecl;

#[derive(Debug, Deserialize, Serialize)]
pub struct StationConfig {
    #[serde(default = "default_tz")]
    pub tz: String,

    /// How Plex tells this tuner apart from every other one it has seen. Plex
    /// keys a DVR's entire channel mapping on it, so a value that changes
    /// between restarts reads as a brand-new tuner and all of the mapping has
    /// to be redone by hand.
    ///
    /// Leave it unset and one is minted on first run and kept in
    /// `{station-config-dir}/.device_id` — beside the station config rather
    /// than on the data volume, because `/data` is disposable (playout JSON
    /// is rewritten every roll, `catalog.db` is a rebuildable cache) and
    /// clearing it to force a catalog rebuild would otherwise mint a new id
    /// and drop Plex's channel mapping. Set it here and that value wins —
    /// the file is not consulted.
    ///
    /// The generated id cannot live in the rendered `lineup.json`: the
    /// container regenerates that file from this config at every start, so a
    /// value written there would be discarded on the next boot.
    #[serde(default)]
    pub device_id: Option<String>,

    /// Base directory every channel's output folder is derived under. A
    /// channel writing playout JSON to `{output_base}/{identity}`, where
    /// `identity` is the channel's `name` override or its config file stem.
    /// Used verbatim relative to the process CWD (the same way the daemon
    /// writes), not resolved against the station config's directory.
    pub output_base: PathBuf,

    /// Channel config references. Each entry is either a literal path or a
    /// glob pattern (e.g. `channels/*.yaml`), resolved relative to the station
    /// config's directory. Globs expand to every matching file; a literal path
    /// that doesn't exist is an error, and a glob matching nothing is an error.
    pub channels: Vec<String>,

    /// Category files (#170): literal paths or globs, resolved like `channels`.
    /// Each file declares named keyword sets that any channel's pool
    /// expression references as `category("name")`. A name declared twice, or
    /// referenced but never declared, fails the load.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,

    /// Chronology files (#164): literal paths or globs, resolved like
    /// `channels`. Each file declares one named, ordered list of item
    /// references that any channel's block plays with `kind: chronology`. A
    /// name declared twice, two entries on one position, or a name referenced
    /// but never declared fails the load.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chronologies: Vec<String>,

    /// Filesystem directories the daemon walks to populate the catalog when
    /// `catalog_path` is set — an operational choice about which parts of this
    /// deployment's disk the daemon is allowed to read. Empty (the default)
    /// skips the filesystem scan entirely; a Plex-only station, or one that
    /// hasn't set `catalog_path` at all, leaves it empty and the scan costs
    /// nothing at startup.
    ///
    /// **Not used for identity.** See [`Self::identity_roots`] for the setting
    /// that canonicalises a local item's path — the two used to be one field,
    /// and setting it to fix identity started an unwanted filesystem walk
    /// (#243).
    #[serde(default)]
    pub source_roots: Vec<String>,

    /// Media mount roots, in the daemon's own filesystem view. Used to
    /// canonicalise a local item's path when deriving its identity (see
    /// [`crate::catalog::identity::canonical_path`]) so the same file collapses
    /// to one identity regardless of which mount root it is reached under. May
    /// be empty — an empty list only skips root-stripping, leaving identity as a
    /// hash of the separator-normalised full path, which is portable only while
    /// every consumer of that identity mounts the disk at the same path.
    ///
    /// **Pure string manipulation — touches no disk.** Unlike
    /// [`Self::source_roots`], setting this triggers no filesystem access, so it
    /// is safe to fill in with the real media root even when a full-library scan
    /// is unaffordable at startup. `ETV_STATION_IDENTITY_ROOTS` (colon-separated)
    /// overrides at runtime, matching `source_roots`' own override convention.
    #[serde(default)]
    pub identity_roots: Vec<String>,

    /// Path to the sqlite catalog the daemon opens and ingests at startup, so
    /// `query` entries and non-`manual` order resolve and manual items path-match
    /// onto catalog identities. Unset (or blank) keeps the catalog-free behavior:
    /// only inline-item, `manual`-order channels resolve. `ETV_STATION_CATALOG`
    /// overrides at runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_path: Option<String>,

    /// Directory Plex artwork is cached to at ingest time and ETV-next serves
    /// from over its own HTTP surface (#187) — the only acceptable source for
    /// `<icon src>` in the generated guide, since a Plex artwork URL carries
    /// `X-Plex-Token` as a working credential. Unset (the default) disables
    /// artwork caching entirely: no fetch happens and `<icon>` is omitted from
    /// every programme, the same safe-by-default behavior as before this
    /// existed. `ETV_STATION_ARTWORK_CACHE` overrides at runtime, matching
    /// `catalog_path`'s own override convention.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork_cache_dir: Option<String>,

    /// How long a freshly ingested catalog is trusted without contacting Plex at
    /// all, in seconds. Two things read it, both meaning the same thing:
    ///
    /// - **At startup**, a restart inside this window reuses the sqlite file as
    ///   it stands — the common case when iterating on channel configs, where
    ///   the daemon may be restarted many times an hour and the library has not
    ///   changed.
    /// - **While running**, it is the interval of the catalog refresh: the
    ///   daemon re-ingests this often, so a long-lived process cannot fall out
    ///   of the freshness window and stay there. A rename or a new title shows
    ///   up within one interval instead of at the next restart. The
    ///   reconciliation sweep runs on the same tick, patching playout JSON that
    ///   was written before the change.
    ///
    /// `0` disables both: no skip at startup, and no periodic refresh.
    #[serde(default = "default_catalog_refresh_secs")]
    pub catalog_refresh_secs: u64,

    /// How long before a delta ingest is escalated to a full re-read, in
    /// seconds. A delta asks Plex only for records touched since the last pass,
    /// which cannot express a deletion — an item removed from the library simply
    /// stops being mentioned. Only a full pass notices those, so one is forced
    /// this often. `0` disables delta ingest entirely: every pass is full.
    #[serde(default = "default_full_sweep_after_secs")]
    pub full_sweep_after_secs: u64,

    /// The station-wide overlay default — the least specific of the three
    /// cascade levels (station → channel → block; see
    /// [`crate::config::OverlayDecl`]).
    ///
    /// This is where a house watermark belongs: every channel that says
    /// nothing inherits it, and a channel that wants its own writes one, with
    /// no third place to look. Unset means channels start from nothing, which
    /// is what every station config meant before this key existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay: Option<OverlayDecl>,

    /// The station-wide seed — the station level of the station → channel →
    /// pool cascade, fixed or rotating (see [`crate::config::variety`]).
    /// Every channel that sets no `seed:` of its own inherits it at load time.
    ///
    /// **Channels do not receive a number verbatim.** Nothing in the
    /// SplitMix64 chain mixes in anything channel-specific — `seeded_shuffle`
    /// consumes the seed as raw state, and `RollKey::u64_at` mixes only
    /// `(seed, cycle, step, nonce)` — so handing two channels the same number
    /// would make any two with the same candidate multiset shuffle
    /// *identically*. [`derive_channel_seed`] salts each number (a fixed seed,
    /// or every entry of a `cycle`) with the channel's declared `name` first;
    /// a derived rotation is salted when it is drawn.
    ///
    /// Unset means [`crate::variety::default_seed`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<crate::config::SeedSpec>,

    /// The station-wide `temperature:`, inherited by every channel that sets
    /// none. Unset means 0: every plugin draws its own ranking in order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<crate::config::TemperatureSpec>,

    /// Station-level track-selection rules (#261): the least specific level of
    /// the station → channel → block → pool → item cascade. Unset means the
    /// built-in floor alone — any text subtitle, and the file's default audio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracks: Option<crate::config::TrackRules>,

    /// Event hooks (ADR 0021): code run at station events — today `cron` on an
    /// interval. A hook cannot change the schedule.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<crate::config::HookDecl>,

    /// ETV-next's `ffmpeg:` block, deserialized through its own type so a
    /// renamed field fails this build instead of being silently dropped.
    /// Every field defaults, so `ffmpeg:` can be omitted entirely.
    #[serde(default = "default_ffmpeg")]
    pub ffmpeg: FfmpegConfig,

    /// ETV-next's `normalization:` block — the playback profile handed to
    /// every rendered `channelN.json`. The block itself is required: a station
    /// with no playback profile should fail to load rather than silently emit
    /// an empty one that ETV-next then rejects (`audio`/`video` are required
    /// there too). Inside `video`, a field left unset takes the station
    /// default picture (see [`fill_default_picture`]).
    #[serde(deserialize_with = "deserialize_normalization")]
    pub normalization: NormalizationConfig,

    /// Named encoding pictures (#260). `active_profile` picks the one every
    /// channel gets unless its own `ffmpeg_profile` names another. Empty means
    /// `normalization.video` is used as written.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ffmpeg_profiles: BTreeMap<String, FfmpegProfile>,

    /// The key of `ffmpeg_profiles` every channel encodes with by default.
    /// Unset applies no profile. A key `ffmpeg_profiles` lacks fails the load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_profile: Option<String>,

    /// Station-wide guide settings; a channel's `guide:` overrides them.
    #[serde(default)]
    pub guide: StationGuide,

    /// Seconds a channel's worker stays warm after its last viewer leaves, so
    /// channel-surfing back skips the cold start. Written to every rendered
    /// `channelN.json`. Unset means ETV-next's own 90s, which is also the
    /// floor: a smaller value is raised to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_grace_seconds: Option<u64>,

    /// Fonts overlay text may name, beyond the OS's own: font files, or
    /// directories whose `.ttf`/`.otf`/`.ttc` files all count, resolved
    /// relative to the station config's directory. Station-wide — every
    /// overlay process registers every listed font. A family no listed font
    /// provides renders in the OS sans-serif. A path that does not exist or is
    /// not a font fails the load. No font ships in the binary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fonts: Vec<PathBuf>,
}

/// One named encoding picture from `ffmpeg_profiles`. Each field set replaces
/// the matching `normalization.video` field of every channel using the profile;
/// a field left unset keeps the station's value.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FfmpegProfile {
    /// `WIDTHxHEIGHT`, e.g. `1920x1080`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    /// Target video bitrate in kbps; the buffer is set to twice this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_bitrate: Option<u32>,
    /// `h264` or `hevc`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u8>,
}

impl FfmpegProfile {
    /// Checks every field parses; the error names the field.
    pub fn validate(&self) -> Result<(), String> {
        self.parsed_resolution()?;
        self.parsed_codec()?;
        if let Some(depth) = self.bit_depth
            && depth != 8
            && depth != 10
        {
            return Err(format!("bit_depth {depth} must be 8 or 10"));
        }
        Ok(())
    }

    fn parsed_resolution(&self) -> Result<Option<(u32, u32)>, String> {
        let Some(raw) = self.resolution.as_deref() else {
            return Ok(None);
        };
        let bad = || format!("resolution {raw:?} must look like 1920x1080");
        let (w, h) = raw.split_once('x').ok_or_else(bad)?;
        let w: u32 = w.trim().parse().map_err(|_| bad())?;
        let h: u32 = h.trim().parse().map_err(|_| bad())?;
        if w == 0 || h == 0 {
            return Err(bad());
        }
        Ok(Some((w, h)))
    }

    fn parsed_codec(&self) -> Result<Option<VideoFormat>, String> {
        match self.video_codec.as_deref() {
            None => Ok(None),
            Some("h264") => Ok(Some(VideoFormat::H264)),
            Some("hevc") => Ok(Some(VideoFormat::Hevc)),
            Some(other) => Err(format!("video_codec {other:?} must be h264 or hevc")),
        }
    }

    /// Writes this profile's fields over a `video` block. A profile that
    /// failed [`Self::validate`] changes nothing it could not parse.
    pub fn apply(&self, video: &mut ersatztv_channel::config::VideoNormalizationConfig) {
        if let Ok(Some((w, h))) = self.parsed_resolution() {
            video.width = Some(w);
            video.height = Some(h);
        }
        if let Some(kbps) = self.video_bitrate {
            video.bitrate_kbps = Some(kbps);
            video.buffer_kbps = Some(kbps.saturating_mul(2));
        }
        if let Ok(Some(format)) = self.parsed_codec() {
            video.format = Some(format);
        }
        if let Some(depth) = self.bit_depth {
            video.bit_depth = Some(depth);
        }
    }
}

/// The station-level `guide:` block.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct StationGuide {
    /// List bumper items as their own guide rows. Off, each bumper folds into
    /// the programme before it.
    #[serde(default)]
    pub show_bumpers: bool,
}

/// Mix a station [`seed`](StationConfig::seed) with one channel's declared
/// `name` into the seed that channel actually uses.
///
/// The salt is the **name**, not the channel number: the name is the channel's
/// identity (#414), while the number is a presentation detail (#263 makes it
/// separately declarable), and renumbering a channel or renaming its folder
/// must not reshuffle it.
///
/// Fixed algorithms end to end — FNV-1a over the name, then the SplitMix64
/// finalizer — rather than `DefaultHasher`, whose output is explicitly not
/// stable across builds. A pinned station seed therefore reproduces the same
/// per-channel seed on every machine and every release, which is the whole
/// point of pinning one.
pub fn derive_channel_seed(station_seed: u64, channel_name: &str) -> u64 {
    let mut hash: u64 = 0xCBF2_9CE4_8422_2325;
    for byte in channel_name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    let mut z = station_seed
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(hash);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// 15 minutes: long enough that a restart-heavy editing session pays the ingest
/// once, short enough that a library change shows up without thinking about it.
fn default_catalog_refresh_secs() -> u64 {
    900
}

/// 24 hours. Deletions are the only thing a delta misses, and they are rare and
/// rarely urgent; a daily full pass costs one slow startup.
fn default_full_sweep_after_secs() -> u64 {
    86_400
}

fn default_tz() -> String {
    "UTC".to_string()
}

/// `FfmpegConfig` (from `ersatztv_channel::config`) does not derive `Default`
/// — every field on it defaults individually instead, so an empty object
/// deserializes into the same value a hand-written `Default` impl would
/// produce, without a second copy of upstream's field list to drift from it.
pub(crate) fn default_ffmpeg() -> FfmpegConfig {
    serde_json::from_value(serde_json::json!({})).expect("every FfmpegConfig field defaults")
}

/// The station's default picture: 1920x1080, 6000 kbps, h264 High, 8-bit SDR.
/// ETV-next tone-maps an HDR source down to SDR for an 8-bit h264 target, so
/// no field here asks for HDR passthrough.
const DEFAULT_VIDEO_WIDTH: u32 = 1920;
const DEFAULT_VIDEO_HEIGHT: u32 = 1080;
const DEFAULT_VIDEO_BITRATE_KBPS: u32 = 6000;
/// Twice the bitrate, the same ratio the audio block's 192/384 uses.
const DEFAULT_VIDEO_BUFFER_KBPS: u32 = 12000;
const DEFAULT_VIDEO_BIT_DEPTH: u8 = 8;

/// Fills every unset `video` field with the station default picture. A field
/// the station file sets keeps its value.
pub(crate) fn fill_default_picture(norm: &mut NormalizationConfig) {
    let video = &mut norm.video;
    video.format.get_or_insert(VideoFormat::H264);
    video.bit_depth.get_or_insert(DEFAULT_VIDEO_BIT_DEPTH);
    video.width.get_or_insert(DEFAULT_VIDEO_WIDTH);
    video.height.get_or_insert(DEFAULT_VIDEO_HEIGHT);
    video.bitrate_kbps.get_or_insert(DEFAULT_VIDEO_BITRATE_KBPS);
    video.buffer_kbps.get_or_insert(DEFAULT_VIDEO_BUFFER_KBPS);
}

fn deserialize_normalization<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<NormalizationConfig, D::Error> {
    let mut norm = NormalizationConfig::deserialize(d)?;
    fill_default_picture(&mut norm);
    Ok(norm)
}

/// A `NormalizationConfig` with every field left unset, for tests that build a
/// `StationConfig` literal and don't care about the playback profile.
/// `audio`/`video` are required, but every field inside them is `Option`.
#[cfg(test)]
pub(crate) fn empty_normalization_for_test() -> NormalizationConfig {
    serde_json::from_value(serde_json::json!({"audio": {}, "video": {}}))
        .expect("audio/video with no fields set is a valid NormalizationConfig")
}

#[cfg(test)]
mod tests {
    use super::{NormalizationConfig, derive_channel_seed, fill_default_picture};

    /// An empty `video:` block takes the station default picture; a field the
    /// station file sets survives.
    #[test]
    fn default_picture_is_1080p_6mbps_h264_8bit() {
        let mut norm: NormalizationConfig =
            serde_json::from_value(serde_json::json!({"audio": {}, "video": {"width": 1280}}))
                .unwrap();
        fill_default_picture(&mut norm);
        let v = serde_json::to_value(&norm).unwrap()["video"].clone();
        assert_eq!(v["width"], 1280);
        assert_eq!(v["height"], 1080);
        assert_eq!(v["bitrate_kbps"], 6000);
        assert_eq!(v["buffer_kbps"], 12000);
        assert_eq!(v["format"], "h264");
        assert_eq!(v["bit_depth"], 8);

        let mut bare: NormalizationConfig =
            serde_json::from_value(serde_json::json!({"audio": {}, "video": {}})).unwrap();
        fill_default_picture(&mut bare);
        assert_eq!(bare.video.width, Some(1920));
    }

    /// Fixed algorithm, fixed output. A pinned station seed has to reproduce
    /// the same channel seed on every machine and every release, so this holds
    /// a golden value rather than only comparing two live calls.
    #[test]
    fn derivation_is_stable_across_builds() {
        assert_eq!(
            derive_channel_seed(42, "001-for-you"),
            derive_channel_seed(42, "001-for-you")
        );
        assert_eq!(
            derive_channel_seed(42, "001-for-you"),
            9_370_805_649_482_762_113
        );
    }

    /// Different channel names diverge; the same name under a different
    /// station seed diverges too.
    #[test]
    fn derivation_separates_channels_and_stations() {
        assert_ne!(
            derive_channel_seed(42, "001-for-you"),
            derive_channel_seed(42, "002-for-pierce")
        );
        assert_ne!(
            derive_channel_seed(42, "001-for-you"),
            derive_channel_seed(43, "001-for-you")
        );
    }

    /// The station seed is not passed through to any channel verbatim — that
    /// is the failure mode the salt exists to prevent.
    #[test]
    fn no_channel_receives_the_station_seed_unchanged() {
        for name in ["001-for-you", "002-for-pierce", "003-for-madi"] {
            assert_ne!(derive_channel_seed(42, name), 42);
        }
    }
}

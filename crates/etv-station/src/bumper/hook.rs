//! The bumper hook: a Rhai plugin's `build(ctx)` returns a
//! [`Timeline`] for one gap.
//!
//! ```rhai
//! fn hooks() { ["bumper"] }
//!
//! fn build(ctx) {
//!     // ctx.gap_ms  — milliseconds to fill
//!     // ctx.next    — item map for the programme after the gap, or () at
//!     //               the generation frontier (the last gap of a generation
//!     //               comes before the next programme is chosen)
//!     //               Beyond the catalog's columns it carries `start` (unix
//!     //               seconds the programme begins, on ctx.next only), `path`
//!     //               (a local file's path) and `source_ids`.
//!     // ctx.prev    — item map for the programme before the gap, or ()
//!     // ctx.upcoming — item maps of the programmes after ctx.next, nearest
//!     //               first, or [] (the ones already laid; a gap near the end
//!     //               of a generation sees few)
//!     // ctx.config  — the `config:` of the block's `fill:` entry, or #{}
//!     // ctx.channel — #{ name, number, logo, vocabulary }
//!     // ctx.rng     — seeded from (channel, next.entry_id, air_date, draw)
//!     let t = timeline();
//!     t.push(segment().black().fixed(2.0));
//!     t
//! }
//! ```
//!
//! `ctx` carries no filesystem or network handle: `build` can only see what
//! the host put in it. Five capability-gated calls reach outside it, each
//! declared in `capabilities()` and granted by the channel
//! ([`BUMPER_CAPABILITIES`]):
//!
//! - `probe` — `ctx.probe(path)` runs `ffprobe` on a media file and returns
//!   `#{ duration_ms, format, tags, streams }`, or `()` when the file cannot be
//!   probed. `tags` is the container's metadata (ComfyUI writes `prompt` and
//!   `workflow` there), keys lower-cased; each stream map has `index`, `kind`,
//!   `codec`, `lang`, `width`, `height`, `channels` and its own lower-cased
//!   `tags`. When the catalog holds audio facts for the file, the map also
//!   has `audio` (`bpm`, `beat_grid`, `downbeat_phase`, `landing_bar`, `key`,
//!   `scale`, `camelot`, `key_strength`, `beat_confidence`, `bar_energy`,
//!   `rms_mean`; see `tools/audio-analyze.py`); it is absent otherwise.
//! - `read` — `ctx.read(path)` returns a UTF-8 text file from the plugin's own
//!   folder (up to [`READ_LIMIT_BYTES`]), or `()` when it does not exist. A path
//!   that resolves outside that folder is an error.
//! - `catalog_read` — `ctx.catalog(expr)` returns the live catalog entries a CEL
//!   expression matches (the language a pool's `expr` uses) as item maps, the
//!   ones `ctx.sets` carries, each with `path` (the file it plays from) and
//!   `audio` (its stored audio facts, `()` when not analyzed), up to
//!   [`CATALOG_LIMIT`] of them. It reads the catalog and nothing else.
//! - `http` — `ctx.http(url, method, body)` makes one HTTP call through
//!   [`crate::plugin_http::call`] and returns the response body as text.
//!   `{{PLEX_URL}}` and `{{PLEX_TOKEN}}` in the URL are filled from the
//!   station's environment, so the script reaches Plex without holding the
//!   token. A fourth argument, a map of header names to values, adds request
//!   headers: `ctx.http(url, "GET", "", #{ Accept: "application/json" })`. A
//!   4xx/5xx, a transport failure, or no response within
//!   [`crate::plugin_http::GENERATION_TIMEOUT`] is an error. `parse_json(text)`
//!   turns the body into maps and arrays and needs no grant.
//!
//!   `ctx.http` and `ctx.exec` share one budget per `build()` call,
//!   [`crate::plugin_http::GENERATION_BUDGET`]: each call's timeout is capped at
//!   what is left, and once it is spent every further call errors at once,
//!   naming the budget.
//! - `exec` — `ctx.exec(program, args)` runs one program with no stdin through
//!   [`crate::hooks::exec_call`] and returns `#{ code, stdout, stderr }`. A
//!   spawn failure, or a run that outlives
//!   [`crate::plugin_http::GENERATION_TIMEOUT`] (the child is killed), is an
//!   error; a nonzero `code` is the script's to read.
//!
//! `local_time(unix_secs)` converts an instant to the station's time zone and
//! returns `#{ weekday, hour, minute }`, as it does for a sequencer; with
//! `ctx.next.start` it lets a script word "Tonight" or "Tomorrow". It needs no
//! grant.
//!
//! `ctx.airings(filter)` reads the airings ledger (`history.db`) and needs no
//! grant: it is in-process and read-only. It returns an array of maps
//! `#{ channel, entry_id, show_id, start_ms, kind, source, in_ms, out_ms,
//! produced_by, error_card }`, past and scheduled rows alike, plus the bumper
//! rows earlier gaps of the same generation pass have laid on this channel but
//! not yet recorded. The filter keys are listed on [`airing_filter`]; with no
//! `kind` it returns programmes and bumper segments together.
//!
//! The builder (`timeline()`, `segment()`, `clip(..)`) constructs the same
//! [`Timeline`] structs a later declarative format would deserialize into.
//! Every builder method counts its calls; [`BuiltTimeline::usage`] returns
//! the counts so the host can log which calls real scripts reach for.
//!
//! The builder takes seconds (`fixed(2.0)`, `flex(4.0, 10.0, 2)`); the `_ms`
//! variants take milliseconds. `looped()` sets [`Segment::loop_`] because
//! `loop` is a Rhai keyword and cannot name a method.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rhai::{Array, Dynamic, EvalAltResult, Map, Scope};
use time::OffsetDateTime;

use crate::ffprobe::PROBE_TIMEOUT;
use crate::history::{AiringFilter, AiringKind, HistoryDb, LedgerRow, PlayRecord};

use super::timeline::{
    AudioSource, BedSource, Duration, OverlayVars, Segment, SegmentAudio, SegmentSource, Timeline,
};

/// Capability names a bumper plugin may declare: each gates one `ctx` call.
pub const BUMPER_CAPABILITIES: &[&str] = &["probe", "read", "catalog_read", "http", "exec"];

/// The most entries one `ctx.catalog` call returns.
pub const CATALOG_LIMIT: usize = 5000;

/// The largest file `ctx.read` returns.
pub const READ_LIMIT_BYTES: u64 = 1 << 20;

/// Why a bumper script produced no timeline.
#[derive(Debug, thiserror::Error)]
pub enum BumperError {
    #[error("bumper plugin {path}: {message}")]
    Script { path: String, message: String },
    #[error("bumper plugin {path}: build() returned an empty timeline")]
    Empty { path: String },
}

/// What the host knows about the channel a bumper airs on.
#[derive(Debug, Clone, Default)]
pub struct ChannelInfo {
    pub name: String,
    pub number: i64,
    pub logo: Option<String>,
    pub vocabulary: Vec<String>,
    /// The station's time zone, which `local_time()` reads in; `None` is UTC.
    pub tz: Option<&'static time_tz::Tz>,
}

/// Everything `ctx` is built from.
#[derive(Debug, Clone)]
pub struct BumperInput {
    pub gap_ms: u64,
    /// The item after the gap, as the item map a scorer's `ctx.sets` carries
    /// ([`crate::score::load_items`]); `None` at the generation frontier.
    pub next: Option<Map>,
    pub prev: Option<Map>,
    /// The programmes after `next` in the schedule being laid, nearest first,
    /// as item maps; empty at the frontier or when `next` is the last one.
    pub upcoming: Vec<Map>,
    /// The `config:` the block's `fill:` entry gave this script, verbatim;
    /// `None` becomes an empty map in `ctx.config`.
    pub config: Option<serde_json::Value>,
    pub channel: ChannelInfo,
    /// `YYYY-MM-DD` the gap airs on, in the station's time zone.
    pub air_date: String,
    /// Which draw of a chained gap this is, 0 for the first. Mixed into
    /// `ctx.rng`'s seed from the second draw on, so back-to-back bumpers from
    /// one script do not repeat each other's random picks.
    pub draw: u32,
    /// Capabilities the channel granted this script; `ctx.probe`,
    /// `ctx.read`, `ctx.http` and `ctx.exec` error unless their name is here.
    pub granted: Vec<String>,
    /// The airings ledger `ctx.airings` reads; `None` answers every query
    /// with an empty array.
    pub history: Option<Arc<HistoryDb>>,
    /// Bumper rows this generation pass has already laid for the channel but
    /// not yet recorded in the ledger; `ctx.airings` answers with them
    /// beside the ledger's own rows.
    pub pass: Vec<PlayRecord>,
    /// The catalog `ctx.probe` reads a file's stored `shots` from; `None`
    /// leaves `shots` out of every probe result.
    pub catalog_path: Option<PathBuf>,
}

/// A script's timeline plus how it built it.
#[derive(Debug, Clone, PartialEq)]
pub struct BuiltTimeline {
    pub timeline: Timeline,
    /// Builder method name to number of calls in this `build`.
    pub usage: BTreeMap<&'static str, u64>,
}

/// Check that `script` can serve as a bumper: it declares the `bumper` hook,
/// defines `build`, and its declared capabilities equal `granted`, each one
/// among [`BUMPER_CAPABILITIES`].
pub fn validate_script(script: &Path, granted: &[String]) -> Result<(), String> {
    let ast = crate::score::compile_plugin(script)?;
    let hooks = crate::score::declared_hooks_from_ast(&ast, script)?;
    if !hooks.iter().any(|h| h == "bumper") {
        return Err(format!(
            "plugin {} must declare the `bumper` hook, but it only declares: {}",
            script.display(),
            hooks.join(", ")
        ));
    }
    let missing = crate::score::missing_required_fns_from_ast(&ast, &hooks);
    if let Some(req) = missing.first() {
        return Err(format!(
            "plugin {} declares the `{}` hook but implements no `{}` — {}",
            script.display(),
            req.hook,
            req.signature,
            req.why
        ));
    }
    let declared: Vec<String> = crate::score::declared_capabilities(script, "bumper")?
        .iter()
        .map(|c| c.name().to_string())
        .collect();
    for cap in &declared {
        if !granted.contains(cap) {
            return Err(format!(
                "plugin {} declares capability {cap:?} that its channel does not grant",
                script.display()
            ));
        }
    }
    if let Some(cap) = granted.iter().find(|g| !declared.contains(g)) {
        return Err(format!(
            "channel grants capability {cap:?} that plugin {} never declares — \
             a grant nobody asked for is a typo or a stale config",
            script.display()
        ));
    }
    Ok(())
}

/// Run `script`'s `build(ctx)` and return its timeline.
pub fn build(script: &Path, input: &BumperInput) -> Result<BuiltTimeline, BumperError> {
    let path = script.display().to_string();
    let fail = |message: String| BumperError::Script {
        path: path.clone(),
        message,
    };
    let ast = crate::score::compile_plugin(script).map_err(&fail)?;
    let engine = bumper_engine(Host {
        dir: script
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
        granted: input.granted.clone(),
        history: input.history.clone(),
        pass: input.pass.clone(),
        catalog_path: input.catalog_path.clone(),
        channel: input.channel.name.clone(),
        budget: crate::plugin_http::Budget::new(crate::plugin_http::GENERATION_BUDGET),
        tz: input.channel.tz.unwrap_or(time_tz::timezones::db::UTC),
    });
    let mut scope = Scope::new();
    let value: Dynamic = engine
        .call_fn(&mut scope, &ast, "build", (ctx_map(input).map_err(&fail)?,))
        .map_err(|e| fail(format!("build(): {e}")))?;
    let type_name = value.type_name();
    let builder = value
        .try_cast::<TimelineBuilder>()
        .ok_or_else(|| fail(format!("build() must return a timeline(), got {type_name}")))?;
    if builder.segments.is_empty() {
        return Err(BumperError::Empty { path });
    }
    for seg in &builder.segments {
        if !matches!(seg.audio, SegmentAudio::Ducked { .. }) {
            continue;
        }
        if builder.bed.is_none() {
            return Err(fail(format!(
                "segment \"{}\": ducked_bed() needs the timeline to have an audio_bed()",
                seg.name
            )));
        }
        if !matches!(
            seg.source,
            SegmentSource::Clip { .. } | SegmentSource::Render { .. }
        ) {
            return Err(fail(format!(
                "segment \"{}\": ducked_bed() needs a clip or render source",
                seg.name
            )));
        }
    }
    let usage = builder.usage.snapshot();
    Ok(BuiltTimeline {
        timeline: Timeline {
            segments: builder.segments,
            bed: builder.bed,
        },
        usage,
    })
}

/// [`build`], with the host's fallback: a script that fails or returns an
/// empty timeline is replaced by one black segment covering the gap, and the
/// failure is logged. The second value is the error that caused the
/// substitution.
pub fn build_or_black(script: &Path, input: &BumperInput) -> (BuiltTimeline, Option<BumperError>) {
    match build(script, input) {
        Ok(built) => {
            tracing::info!(
                event = "bumper.built",
                channel = %input.channel.name,
                script = %script.display(),
                gap_ms = input.gap_ms,
                segments = built.timeline.segments.len(),
                usage = ?built.usage,
                "bumper script built a timeline"
            );
            (built, None)
        }
        Err(e) => {
            tracing::warn!(
                event = "bumper.fallback_black",
                channel = %input.channel.name,
                script = %script.display(),
                gap_ms = input.gap_ms,
                error = %e,
                "bumper script produced no timeline; substituting black"
            );
            let black = Segment {
                name: "black".to_string(),
                source: SegmentSource::black(),
                duration: Duration::Fixed(input.gap_ms),
                loop_: false,
                optional: false,
                priority: 0,
                overlay: None,
                audio: SegmentAudio::Mute,
            };
            (
                BuiltTimeline {
                    timeline: Timeline {
                        segments: vec![black],
                        bed: None,
                    },
                    usage: BTreeMap::new(),
                },
                Some(e),
            )
        }
    }
}

/// The seed behind `ctx.rng`: stable across builds, and equal for the same
/// channel, following item and air date. A frontier gap (`next_entry_id` of
/// `None`) hashes the empty id.
pub fn rng_seed(channel: &str, next_entry_id: Option<&str>, air_date: &str) -> u64 {
    use crate::catalog::identity::fnv1a_64;
    let a = crate::pattern::mix_seed(fnv1a_64(channel), fnv1a_64(next_entry_id.unwrap_or("")));
    crate::pattern::mix_seed(a, fnv1a_64(air_date))
}

fn ctx_map(input: &BumperInput) -> Result<Dynamic, String> {
    let entry_id = match &input.next {
        Some(next) => Some(
            next.get("entry_id")
                .and_then(|v| v.clone().into_string().ok())
                .ok_or("ctx.next has no string `entry_id`")?,
        ),
        None => None,
    };
    let item = |m: &Option<Map>| m.clone().map(Dynamic::from_map).unwrap_or(Dynamic::UNIT);

    let mut channel = Map::new();
    channel.insert("name".into(), input.channel.name.clone().into());
    channel.insert("number".into(), input.channel.number.into());
    channel.insert(
        "logo".into(),
        input
            .channel
            .logo
            .clone()
            .map(Dynamic::from)
            .unwrap_or(Dynamic::UNIT),
    );
    channel.insert(
        "vocabulary".into(),
        input
            .channel
            .vocabulary
            .iter()
            .cloned()
            .map(Dynamic::from)
            .collect::<Vec<_>>()
            .into(),
    );

    let mut seed = rng_seed(&input.channel.name, entry_id.as_deref(), &input.air_date);
    if input.draw > 0 {
        seed = crate::pattern::mix_seed(seed, u64::from(input.draw));
    }
    let rng = Rng::new(seed);
    let mut ctx = Map::new();
    ctx.insert("gap_ms".into(), (input.gap_ms as i64).into());
    ctx.insert("next".into(), item(&input.next));
    ctx.insert("prev".into(), item(&input.prev));
    ctx.insert(
        "upcoming".into(),
        input
            .upcoming
            .iter()
            .cloned()
            .map(Dynamic::from_map)
            .collect::<Vec<_>>()
            .into(),
    );
    ctx.insert(
        "config".into(),
        match &input.config {
            Some(value) => rhai::serde::to_dynamic(value)
                .map_err(|e| format!("the script's config is not representable in Rhai: {e}"))?,
            None => Dynamic::from_map(Map::new()),
        },
    );
    ctx.insert("channel".into(), Dynamic::from_map(channel));
    ctx.insert("rng".into(), Dynamic::from(rng));
    Ok(Dynamic::from_map(ctx))
}

/// `ctx.rng`: a deterministic stream. Clones share one position, so two
/// reads of `ctx.rng` in a script continue the same sequence.
#[derive(Debug, Clone)]
pub struct Rng {
    seed: u64,
    drawn: Arc<Mutex<u64>>,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            drawn: Arc::new(Mutex::new(0)),
        }
    }

    fn next_u64(&self) -> u64 {
        let mut drawn = self.drawn.lock().expect("rng lock poisoned");
        let n = *drawn;
        *drawn += 1;
        crate::pattern::mix_seed(self.seed, n)
    }

    /// An integer in `lo..=hi`; `lo` when the range is empty.
    fn int(&mut self, lo: i64, hi: i64) -> i64 {
        if hi <= lo {
            return lo;
        }
        let span = (hi as i128 - lo as i128 + 1) as u128;
        lo.wrapping_add((self.next_u64() as u128 % span) as i64)
    }

    /// A float in `[0, 1)`.
    fn float(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn chance(&mut self, p: f64) -> bool {
        self.float() < p
    }

    /// One element of `items`, or `()` when it is empty.
    fn pick(&mut self, items: rhai::Array) -> Dynamic {
        if items.is_empty() {
            return Dynamic::UNIT;
        }
        let i = self.int(0, items.len() as i64 - 1) as usize;
        items[i].clone()
    }
}

/// Per-`build` tally of builder calls.
#[derive(Debug, Clone, Default)]
struct Usage(Arc<Mutex<BTreeMap<&'static str, u64>>>);

impl Usage {
    fn bump(&self, method: &'static str) {
        *self
            .0
            .lock()
            .expect("usage lock poisoned")
            .entry(method)
            .or_insert(0) += 1;
    }

    fn snapshot(&self) -> BTreeMap<&'static str, u64> {
        self.0.lock().expect("usage lock poisoned").clone()
    }
}

type RhaiResult<T> = Result<T, Box<EvalAltResult>>;

fn err<T>(message: impl Into<String>) -> RhaiResult<T> {
    Err(message.into().into())
}

fn secs_to_ms(secs: f64, what: &str) -> RhaiResult<u64> {
    if !secs.is_finite() || secs < 0.0 {
        return err(format!("{what}: {secs} is not a non-negative number"));
    }
    Ok((secs * 1000.0).round() as u64)
}

/// An int or float argument as `f64`, so mixed calls like `flex(2.5, 10, 1)` work.
fn number(value: &Dynamic, what: &str) -> RhaiResult<f64> {
    value
        .as_float()
        .or_else(|_| value.as_int().map(|i| i as f64))
        .or_else(|_| err(format!("{what}: {} is not a number", value.type_name())))
}

fn int_to_ms(ms: i64, what: &str) -> RhaiResult<u64> {
    u64::try_from(ms).or_else(|_| err(format!("{what}: {ms} is negative")))
}

/// A clip reference: where to cut from, and the range it may flex across.
#[derive(Debug, Clone)]
struct ClipRef {
    path: PathBuf,
    in_ms: u64,
    out_ms: u64,
    min_ms: u64,
    max_ms: u64,
    usage: Usage,
}

/// A segment under construction. `push` checks it is complete.
#[derive(Debug, Clone)]
struct SegmentBuilder {
    name: Option<String>,
    source: Option<SegmentSource>,
    duration: Option<Duration>,
    loop_: bool,
    optional: bool,
    priority: u8,
    overlay: Option<OverlayVars>,
    audio: SegmentAudio,
    usage: Usage,
}

impl SegmentBuilder {
    fn new(usage: Usage) -> Self {
        usage.bump("segment");
        Self {
            name: None,
            source: None,
            duration: None,
            loop_: false,
            optional: false,
            priority: 0,
            overlay: None,
            audio: SegmentAudio::Own,
            usage,
        }
    }

    fn set_source(&mut self, method: &'static str, source: SegmentSource) -> RhaiResult<Self> {
        self.usage.bump(method);
        if self.source.is_some() {
            return err(format!(
                "segment().{method}(): the segment already has a source"
            ));
        }
        self.source = Some(source);
        Ok(self.clone())
    }

    fn set_duration(&mut self, method: &'static str, duration: Duration) -> RhaiResult<Self> {
        self.usage.bump(method);
        if self.duration.is_some() {
            return err(format!(
                "segment().{method}(): the segment already has a duration"
            ));
        }
        self.duration = Some(duration);
        Ok(self.clone())
    }

    fn flex_ms(&mut self, method: &'static str, min: u64, max: u64, grow: f64) -> RhaiResult<Self> {
        if min > max {
            return err(format!(
                "segment().{method}(): min {min}ms exceeds max {max}ms"
            ));
        }
        if !grow.is_finite() || grow < 0.0 {
            return err(format!(
                "segment().{method}(): grow {grow} is not a number >= 0"
            ));
        }
        self.set_duration(
            method,
            Duration::Flex {
                min_ms: min,
                max_ms: max,
                grow: grow as f32,
            },
        )
    }

    fn finish(self, index: usize) -> RhaiResult<Segment> {
        let source = self
            .source
            .ok_or("push(): the segment has no source — call black(), color(), clip(), still() or render()")?;
        let duration = self
            .duration
            .ok_or("push(): the segment has no duration — call fixed() or flex()")?;
        let name = self.name.unwrap_or_else(|| {
            let kind = match &source {
                SegmentSource::Clip { .. } => "clip",
                SegmentSource::Lavfi { .. } => "lavfi",
                SegmentSource::Still { .. } => "still",
                SegmentSource::Render { .. } => "render",
                SegmentSource::Pending { .. } => "pending",
                SegmentSource::Http { .. } => "http",
            };
            format!("{kind}{index}")
        });
        Ok(Segment {
            name,
            source,
            duration,
            loop_: self.loop_,
            optional: self.optional,
            priority: self.priority,
            overlay: self.overlay,
            audio: self.audio,
        })
    }
}

/// A timeline under construction; what `timeline()` returns.
#[derive(Debug, Clone)]
struct TimelineBuilder {
    segments: Vec<Segment>,
    bed: Option<AudioSource>,
    usage: Usage,
}

fn parse_color(hex: &str) -> RhaiResult<String> {
    let digits = hex.strip_prefix('#').unwrap_or(hex);
    if digits.len() != 6 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return err(format!("color(): {hex:?} is not #rrggbb"));
    }
    Ok(format!("color=c=0x{}", digits.to_ascii_uppercase()))
}

fn overlay_vars(script: String, vars: Map) -> RhaiResult<OverlayVars> {
    let mut out = BTreeMap::new();
    for (key, value) in vars {
        let json = rhai::serde::from_dynamic::<serde_json::Value>(&value)
            .map_err(|e| format!("overlay(): var {key:?} is not plain data: {e}"))?;
        out.insert(key.to_string(), json);
    }
    Ok(OverlayVars { script, vars: out })
}

fn bed_source(source: Dynamic, opts: Map) -> RhaiResult<AudioSource> {
    let source = if source.is_string() {
        BedSource::File(PathBuf::from(source.into_string().unwrap_or_default()))
    } else if let Some(m) = source.try_cast::<Map>() {
        let text = |key: &str| m.get(key).and_then(|v| v.clone().into_string().ok());
        match (text("path"), text("lavfi")) {
            (Some(path), None) => BedSource::File(PathBuf::from(path)),
            (None, Some(filters)) => BedSource::Lavfi(filters),
            _ => {
                return err(
                    "audio_bed(): the source map needs exactly one of a string `path` or `lavfi`",
                );
            }
        }
    } else {
        return err(
            "audio_bed(): the source must be a path string or a map with `path` or `lavfi`",
        );
    };
    let num = |key: &str| -> RhaiResult<f64> {
        match opts.get(key) {
            None => Ok(0.0),
            Some(v) => v
                .as_float()
                .or_else(|_| v.as_int().map(|i| i as f64))
                .or_else(|_| err(format!("audio_bed(): {key} is not a number"))),
        }
    };
    Ok(AudioSource {
        source,
        gain_db: num("gain_db")? as f32,
        fade_in_ms: secs_to_ms(num("fade_in")?, "audio_bed() fade_in")?,
        fade_out_ms: secs_to_ms(num("fade_out")?, "audio_bed() fade_out")?,
    })
}

/// What the gated `ctx` calls need from the host.
#[derive(Debug, Clone)]
struct Host {
    /// The plugin's own folder: the only place `ctx.read` reaches.
    dir: PathBuf,
    granted: Vec<String>,
    history: Option<Arc<HistoryDb>>,
    pass: Vec<PlayRecord>,
    catalog_path: Option<PathBuf>,
    /// The channel `ctx.airings` queries unless the filter names another.
    channel: String,
    /// What this `build()` call has left for `ctx.http` and `ctx.exec`.
    budget: crate::plugin_http::Budget,
    tz: &'static time_tz::Tz,
}

impl Host {
    fn require(&self, capability: &str) -> RhaiResult<()> {
        if self.granted.iter().any(|g| g == capability) {
            Ok(())
        } else {
            err(format!("capability `{capability}` not declared"))
        }
    }

    fn probe(&self, path: &str) -> RhaiResult<Dynamic> {
        self.require("probe")?;
        let mut probed = probe_media(Path::new(path)).map_err(|e| format!("ctx.probe(): {e}"))?;
        let shots = match (&probed, &self.catalog_path) {
            (Some(_), Some(catalog)) => stored_shots(catalog, path),
            _ => None,
        };
        let audio = match (&probed, &self.catalog_path) {
            (Some(_), Some(catalog)) => crate::audio::facts_for_path(catalog, path),
            _ => None,
        };
        if let (Some(serde_json::Value::Object(map)), Some(audio)) = (&mut probed, &audio) {
            map.insert("audio".into(), audio.clone());
        }
        if let (Some(serde_json::Value::Object(map)), Some(shots)) = (&mut probed, &shots) {
            map.insert("shots".into(), shots.clone());
        }
        tracing::info!(
            event = "bumper.probe",
            path,
            found = probed.is_some(),
            shots = shots.as_ref().and_then(|s| s.as_array()).map(Vec::len),
            "bumper script probed a file"
        );
        match probed {
            Some(value) => rhai::serde::to_dynamic(value),
            None => Ok(Dynamic::UNIT),
        }
    }

    /// `ctx.catalog(expr)`: the live catalog entries a CEL expression matches
    /// (the language a pool's `expr` uses), as item maps, in `entry_id` order.
    /// Each map is the one `ctx.sets` carries, plus `path` (the file the entry
    /// plays from, `()` when it has no source) and `audio` (the stored audio
    /// facts, `()` when the entry is not analyzed). At most [`CATALOG_LIMIT`]
    /// entries come back.
    fn catalog(&self, expr: &str) -> RhaiResult<Dynamic> {
        self.require("catalog_read")?;
        let Some(path) = &self.catalog_path else {
            tracing::warn!(
                event = "bumper.catalog_unavailable",
                channel = %self.channel,
                "ctx.catalog has no catalog to read; returning no entries"
            );
            return Ok(Array::new().into());
        };
        let found = (|| -> Result<(Array, usize), String> {
            let catalog =
                crate::catalog::Catalog::open_readonly(path).map_err(|e| e.to_string())?;
            let mut ids = catalog.resolve_query(expr).map_err(|e| e.to_string())?;
            let matched = ids.len();
            if matched > CATALOG_LIMIT {
                tracing::warn!(
                    event = "bumper.catalog_truncated",
                    channel = %self.channel,
                    matched,
                    limit = CATALOG_LIMIT,
                    "ctx.catalog matched more entries than it returns"
                );
                ids.truncate(CATALOG_LIMIT);
            }
            let mut items = crate::score::load_items(&catalog, &ids)?;
            let mut analyzed = 0usize;
            for item in &mut items {
                let mut map = std::mem::take(item).cast::<Map>();
                let id = map
                    .get("entry_id")
                    .and_then(|v| v.clone().into_string().ok())
                    .unwrap_or_default();
                let sources = catalog.sources_for(&id).map_err(|e| e.to_string())?;
                let playing = crate::resolve::pick_playback_source(&sources)
                    .map(|s| Dynamic::from(s.playback_path.clone()))
                    .unwrap_or(Dynamic::UNIT);
                map.insert("path".into(), playing);
                let facts = catalog
                    .audio_fact(&id)
                    .map_err(|e| e.to_string())?
                    .and_then(|row| row.facts);
                let audio = match facts {
                    Some(v) => {
                        analyzed += 1;
                        rhai::serde::to_dynamic(v).map_err(|e| e.to_string())?
                    }
                    None => Dynamic::UNIT,
                };
                map.insert("audio".into(), audio);
                *item = Dynamic::from_map(map);
            }
            Ok((items, analyzed))
        })()
        .map_err(|e| format!("ctx.catalog(): {e}"))?;
        let (found, analyzed) = found;
        tracing::info!(
            event = "bumper.catalog",
            channel = %self.channel,
            expr,
            entries = found.len(),
            analyzed,
            "bumper script queried the catalog"
        );
        Ok(found.into())
    }

    /// `ctx.airings(filter)`: ledger rows, past and scheduled, as maps.
    fn airings(&self, filter: Map) -> RhaiResult<Dynamic> {
        let parsed = airing_filter(&filter, &self.channel)?;
        let mut rows = match &self.history {
            Some(db) => db
                .airings(&parsed)
                .map_err(|e| format!("ctx.airings(): {e}"))?,
            None => Vec::new(),
        };
        let planned: Vec<LedgerRow> = self
            .pass
            .iter()
            .filter(|r| pass_row_matches(r, &self.channel, &parsed))
            .map(|r| LedgerRow {
                channel: self.channel.clone(),
                record: r.clone(),
            })
            .collect();
        if !planned.is_empty() {
            rows.extend(planned);
            rows.sort_by_key(|r| r.record.start);
            if parsed.newest_first {
                rows.reverse();
            }
            rows.truncate(parsed.limit);
        }
        tracing::info!(
            event = "bumper.airings",
            channel = %self.channel,
            filter = ?parsed,
            rows = rows.len(),
            "bumper script queried the airings ledger"
        );
        Ok(rows
            .into_iter()
            .map(|row| Dynamic::from_map(airing_map(row)))
            .collect::<Vec<_>>()
            .into())
    }

    fn read(&self, path: &str) -> RhaiResult<Dynamic> {
        self.require("read")?;
        let text = read_in_dir(&self.dir, path).map_err(|e| format!("ctx.read(): {e}"))?;
        tracing::info!(
            event = "bumper.read",
            path,
            bytes = text.as_ref().map(String::len),
            "bumper script read a plugin file"
        );
        Ok(text.map(Dynamic::from).unwrap_or(Dynamic::UNIT))
    }

    fn http(&self, url: &str, method: &str, body: &str, headers: Map) -> RhaiResult<String> {
        self.require("http")?;
        let headers = headers
            .into_iter()
            .map(|(k, v)| match v.into_string() {
                Ok(v) => Ok((k.to_string(), v)),
                Err(_) => err(format!("ctx.http(): header {k:?} must be a string")),
            })
            .collect::<RhaiResult<Vec<_>>>()?;
        Ok(self.budget.within("ctx.http()", |timeout| {
            crate::plugin_http::call_with(url, method, body, &headers, timeout)
        })?)
    }

    fn exec(&self, program: &str, args: rhai::Array) -> RhaiResult<Map> {
        self.require("exec")?;
        let started = std::time::Instant::now();
        let result = self.budget.within("ctx.exec()", |timeout| {
            crate::hooks::exec_call(program, args, timeout)
        });
        tracing::info!(
            event = "bumper.exec",
            program,
            ok = result.is_ok(),
            code = result
                .as_ref()
                .ok()
                .and_then(|m| m.get("code"))
                .map(|c| c.to_string()),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "bumper script ran a program"
        );
        Ok(result?)
    }
}

/// The default and largest `limit` of a `ctx.airings` call.
const AIRINGS_DEFAULT_LIMIT: usize = 100;
const AIRINGS_MAX_LIMIT: usize = 1000;

/// A `ctx.airings` filter map as an [`AiringFilter`]. Keys: `channel` (default
/// the bumper's own; `"*"` for every channel), `kind` (`"programme"` or
/// `"bumper"`), `entry_id`, `show_id`, `source`, `produced_by`, `from_ms` and
/// `to_ms` (epoch milliseconds bounding the start, inclusive then exclusive),
/// `order` (`"asc"` or `"desc"`, default `"asc"`) and `limit` (default 100,
/// at most 1000). An unknown key is an error, so a typo never widens a query.
fn airing_filter(filter: &Map, own_channel: &str) -> RhaiResult<AiringFilter> {
    let text = |key: &str, v: &Dynamic| -> RhaiResult<String> {
        v.clone()
            .into_string()
            .map_err(|_| format!("ctx.airings(): `{key}` must be a string").into())
    };
    let ms = |key: &str, v: &Dynamic| -> RhaiResult<OffsetDateTime> {
        let n = v
            .as_int()
            .map_err(|_| format!("ctx.airings(): `{key}` must be an integer of milliseconds"))?;
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(n) * 1_000_000)
            .map_err(|_| format!("ctx.airings(): `{key}` is out of range").into())
    };
    let mut out = AiringFilter {
        channel: Some(own_channel.to_string()),
        limit: AIRINGS_DEFAULT_LIMIT,
        ..Default::default()
    };
    for (key, value) in filter {
        match key.as_str() {
            "channel" => {
                let c = text("channel", value)?;
                out.channel = (c != "*").then_some(c);
            }
            "kind" => {
                let k = text("kind", value)?;
                out.kind = Some(AiringKind::parse(&k).ok_or_else(|| {
                    format!("ctx.airings(): `kind` must be \"programme\" or \"bumper\", got {k:?}")
                })?);
            }
            "entry_id" => out.entry_id = Some(text("entry_id", value)?),
            "show_id" => out.show_id = Some(text("show_id", value)?),
            "source" => out.source = Some(text("source", value)?),
            "produced_by" => out.produced_by = Some(text("produced_by", value)?),
            "from_ms" => out.from = Some(ms("from_ms", value)?),
            "to_ms" => out.to = Some(ms("to_ms", value)?),
            "order" => {
                out.newest_first = match text("order", value)?.as_str() {
                    "asc" => false,
                    "desc" => true,
                    other => {
                        return err(format!(
                            "ctx.airings(): `order` must be \"asc\" or \"desc\", got {other:?}"
                        ));
                    }
                }
            }
            "limit" => {
                let n = value
                    .as_int()
                    .map_err(|_| "ctx.airings(): `limit` must be an integer".to_string())?;
                out.limit = usize::try_from(n)
                    .map_err(|_| "ctx.airings(): `limit` must not be negative".to_string())?
                    .min(AIRINGS_MAX_LIMIT);
            }
            other => return err(format!("ctx.airings(): unknown filter key `{other}`")),
        }
    }
    Ok(out)
}

/// Whether a row laid earlier in this pass on `channel` satisfies `f`, as the
/// ledger query would have judged it.
fn pass_row_matches(r: &PlayRecord, channel: &str, f: &AiringFilter) -> bool {
    f.channel.as_deref().is_none_or(|c| c == channel)
        && f.kind.is_none_or(|k| k == r.kind)
        && f.entry_id.as_ref().is_none_or(|v| *v == r.entry_id)
        && f.show_id
            .as_ref()
            .is_none_or(|v| r.show_id.as_ref() == Some(v))
        && f.source
            .as_ref()
            .is_none_or(|v| r.source.as_ref() == Some(v))
        && f.produced_by
            .as_ref()
            .is_none_or(|v| r.produced_by.as_ref() == Some(v))
        && f.from.is_none_or(|t| r.start >= t)
        && f.to.is_none_or(|t| r.start < t)
}

fn airing_map(row: LedgerRow) -> Map {
    let LedgerRow { channel, record } = row;
    let opt = |v: Option<String>| v.map(Dynamic::from).unwrap_or(Dynamic::UNIT);
    let opt_ms = |v: Option<u64>| v.map(|n| Dynamic::from(n as i64)).unwrap_or(Dynamic::UNIT);
    let mut m = Map::new();
    m.insert("channel".into(), channel.into());
    m.insert("entry_id".into(), record.entry_id.into());
    m.insert("show_id".into(), opt(record.show_id));
    m.insert(
        "start_ms".into(),
        ((record.start.unix_timestamp_nanos() / 1_000_000) as i64).into(),
    );
    m.insert("kind".into(), record.kind.as_str().to_string().into());
    m.insert("source".into(), opt(record.source));
    m.insert("in_ms".into(), opt_ms(record.in_ms));
    m.insert("out_ms".into(), opt_ms(record.out_ms));
    m.insert("produced_by".into(), opt(record.produced_by));
    m.insert("error_card".into(), record.error_card.into());
    m
}

/// `path` read as text, when it resolves inside `dir`. `Ok(None)` for a file
/// that does not exist; `Err` for one that escapes `dir`, is too large, or is
/// not UTF-8.
fn read_in_dir(dir: &Path, path: &str) -> Result<Option<String>, String> {
    let root = dir
        .canonicalize()
        .map_err(|e| format!("plugin folder {}: {e}", dir.display()))?;
    let target = match root.join(path).canonicalize() {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{path:?}: {e}")),
    };
    if !target.starts_with(&root) {
        return Err(format!(
            "{path:?} is outside the plugin folder {}",
            root.display()
        ));
    }
    let meta = std::fs::metadata(&target).map_err(|e| format!("{path:?}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{path:?} is not a file"));
    }
    if meta.len() > READ_LIMIT_BYTES {
        return Err(format!(
            "{path:?} is {} bytes, over the {READ_LIMIT_BYTES}-byte limit",
            meta.len()
        ));
    }
    let bytes = std::fs::read(&target).map_err(|e| format!("{path:?}: {e}"))?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| format!("{path:?} is not UTF-8 text"))
}

/// The shot list the catalog at `catalog` holds for the entry playing `path`,
/// as the array `ctx.probe(path).shots` returns; `None` when there is none.
fn stored_shots(catalog: &Path, path: &str) -> Option<serde_json::Value> {
    let found = crate::catalog::Catalog::open_readonly(catalog)
        .and_then(|c| c.shots_for_path(path))
        .unwrap_or_else(|e| {
            tracing::warn!(
                event = "bumper.probe_shots_failed",
                path,
                error = %e,
                "could not read the stored shots; ctx.probe leaves `shots` out"
            );
            None
        })?;
    serde_json::to_value(found).ok()
}

/// `ffprobe` output for `path` as the map `ctx.probe` returns. Only the `file`
/// protocol is allowed, so a path cannot name a URL. `Ok(None)` when
/// ffprobe runs but rejects the file; `Err` when ffprobe cannot be run.
fn probe_media(path: &Path) -> Result<Option<serde_json::Value>, String> {
    use serde_json::{Map as JsonMap, Value, json};

    let args: Vec<std::ffi::OsString> = [
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_format",
        "-show_streams",
        "-protocol_whitelist",
        "file",
        "-i",
    ]
    .iter()
    .map(Into::into)
    .chain([path.as_os_str().to_owned()])
    .collect();
    let stdout = match crate::ffprobe::run_bounded(&args, PROBE_TIMEOUT)
        .map_err(|e| format!("cannot run ffprobe: {e}"))?
    {
        crate::ffprobe::Bounded::Done {
            success: true,
            stdout,
            ..
        } => stdout,
        crate::ffprobe::Bounded::Done { .. } => return Ok(None),
        crate::ffprobe::Bounded::TimedOut => {
            tracing::warn!(
                event = "bumper.probe_timeout",
                path = %path.display(),
                timeout_secs = PROBE_TIMEOUT.as_secs(),
                "ffprobe did not finish; ctx.probe returns ()"
            );
            return Ok(None);
        }
    };
    let raw: Value = serde_json::from_slice(&stdout)
        .map_err(|e| format!("ffprobe printed invalid JSON: {e}"))?;

    let tags = |v: &Value| -> Value {
        let mut out = JsonMap::new();
        if let Some(obj) = v.get("tags").and_then(Value::as_object) {
            for (k, val) in obj {
                if let Some(s) = val.as_str() {
                    out.insert(k.to_ascii_lowercase(), json!(s));
                }
            }
        }
        Value::Object(out)
    };
    let duration_ms = raw["format"]["duration"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|secs| (secs * 1000.0).round() as i64);
    let streams: Vec<Value> = raw["streams"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|s| {
            let stream_tags = tags(s);
            json!({
                "index": s["index"].as_i64(),
                "kind": s["codec_type"].as_str().unwrap_or("other"),
                "codec": s["codec_name"].as_str().unwrap_or(""),
                "lang": stream_tags["language"].as_str().filter(|l| *l != "und"),
                "width": s["width"].as_i64(),
                "height": s["height"].as_i64(),
                "channels": s["channels"].as_i64(),
                "tags": stream_tags,
            })
        })
        .collect();
    Ok(Some(json!({
        "duration_ms": duration_ms,
        "format": raw["format"]["format_name"].as_str().unwrap_or(""),
        "tags": tags(&raw["format"]),
        "streams": streams,
    })))
}

/// The station's plugin engine plus the bumper builder, `ctx.rng` and the
/// gated `ctx.probe` / `ctx.read` / `ctx.http`.
fn bumper_engine(host: Host) -> rhai::Engine {
    let usage = Usage::default();
    let mut engine = crate::score::engine();

    let tz = host.tz;
    engine.register_fn("local_time", move |secs: i64| {
        crate::sequence::local_time_map(secs, tz)
    });
    let h = host.clone();
    engine.register_fn("probe", move |_ctx: &mut Map, path: &str| h.probe(path));
    let h = host.clone();
    engine.register_fn("catalog", move |_ctx: &mut Map, expr: &str| h.catalog(expr));
    let h = host.clone();
    engine.register_fn("airings", move |_ctx: &mut Map, filter: Map| {
        h.airings(filter)
    });
    let h = host.clone();
    engine.register_fn("read", move |_ctx: &mut Map, path: &str| h.read(path));
    let h = host.clone();
    engine.register_fn(
        "http",
        move |_ctx: &mut Map, url: &str, method: &str, body: &str| {
            h.http(url, method, body, Map::new())
        },
    );
    let h = host.clone();
    engine.register_fn(
        "http",
        move |_ctx: &mut Map, url: &str, method: &str, body: &str, headers: Map| {
            h.http(url, method, body, headers)
        },
    );
    engine.register_fn(
        "exec",
        move |_ctx: &mut Map, program: &str, args: rhai::Array| host.exec(program, args),
    );

    engine
        .register_type_with_name::<Rng>("Rng")
        .register_fn("int", Rng::int)
        .register_fn("float", Rng::float)
        .register_fn("chance", Rng::chance)
        .register_fn("pick", Rng::pick)
        .register_fn("random", |items: rhai::Array, mut rng: Rng| rng.pick(items));

    let u = usage.clone();
    engine.register_fn("timeline", move || {
        u.bump("timeline");
        TimelineBuilder {
            segments: Vec::new(),
            bed: None,
            usage: u.clone(),
        }
    });
    let u = usage.clone();
    engine.register_fn("segment", move || SegmentBuilder::new(u.clone()));
    let u = usage;
    engine.register_fn(
        "clip",
        move |path: &str, in_ms: i64, out_ms: i64| -> RhaiResult<ClipRef> {
            u.bump("clip");
            let (in_ms, out_ms) = (
                int_to_ms(in_ms, "clip() in_ms")?,
                int_to_ms(out_ms, "clip() out_ms")?,
            );
            if out_ms <= in_ms {
                return err(format!(
                    "clip(): out_ms {out_ms} is not after in_ms {in_ms}"
                ));
            }
            Ok(ClipRef {
                path: PathBuf::from(path),
                in_ms,
                out_ms,
                min_ms: out_ms - in_ms,
                max_ms: out_ms - in_ms,
                usage: u.clone(),
            })
        },
    );

    engine
        .register_type_with_name::<ClipRef>("Clip")
        .register_get("min_ms", |c: &mut ClipRef| c.min_ms as i64)
        .register_get("max_ms", |c: &mut ClipRef| c.max_ms as i64)
        .register_fn(
            "with_flex",
            |c: &mut ClipRef, min_ms: i64, max_ms: i64| -> RhaiResult<ClipRef> {
                c.usage.bump("with_flex");
                let (min, max) = (
                    int_to_ms(min_ms, "with_flex() min_ms")?,
                    int_to_ms(max_ms, "with_flex() max_ms")?,
                );
                if min > max {
                    return err(format!("with_flex(): min {min}ms exceeds max {max}ms"));
                }
                c.min_ms = min;
                c.max_ms = max;
                Ok(c.clone())
            },
        );

    engine
        .register_type_with_name::<TimelineBuilder>("Timeline")
        .register_fn(
            "push",
            |t: &mut TimelineBuilder, s: SegmentBuilder| -> RhaiResult<()> {
                t.usage.bump("push");
                let index = t.segments.len();
                t.segments.push(s.finish(index)?);
                Ok(())
            },
        )
        .register_fn(
            "audio_bed",
            |t: &mut TimelineBuilder, source: Dynamic, opts: Map| -> RhaiResult<()> {
                t.usage.bump("audio_bed");
                t.bed = Some(bed_source(source, opts)?);
                Ok(())
            },
        )
        .register_fn(
            "audio_bed",
            |t: &mut TimelineBuilder, source: Dynamic| -> RhaiResult<()> {
                t.usage.bump("audio_bed");
                t.bed = Some(bed_source(source, Map::new())?);
                Ok(())
            },
        );

    engine
        .register_type_with_name::<SegmentBuilder>("Segment")
        .register_fn("name", |s: &mut SegmentBuilder, name: &str| {
            s.usage.bump("name");
            s.name = Some(name.to_string());
            s.clone()
        })
        .register_fn("black", |s: &mut SegmentBuilder| {
            s.set_source("black", SegmentSource::black())
        })
        .register_fn("color", |s: &mut SegmentBuilder, hex: &str| {
            let params = parse_color(hex)?;
            s.set_source("color", SegmentSource::Lavfi { params })
        })
        .register_fn("clip", |s: &mut SegmentBuilder, c: ClipRef| {
            s.usage.bump("clip_ref");
            let source = SegmentSource::Clip {
                path: c.path,
                in_ms: c.in_ms,
                out_ms: c.out_ms,
            };
            s.set_source("clip", source)
        })
        .register_fn("still", |s: &mut SegmentBuilder, path: &str| {
            s.set_source(
                "still",
                SegmentSource::Still {
                    image_path: PathBuf::from(path),
                },
            )
        })
        .register_fn(
            "http",
            |s: &mut SegmentBuilder,
             url: &str,
             in_ms: i64,
             out_ms: i64|
             -> RhaiResult<SegmentBuilder> {
                let (in_ms, out_ms) = (
                    int_to_ms(in_ms, "http() in_ms")?,
                    int_to_ms(out_ms, "http() out_ms")?,
                );
                if out_ms <= in_ms {
                    return err(format!(
                        "http(): out_ms {out_ms} is not after in_ms {in_ms}"
                    ));
                }
                s.set_source(
                    "http",
                    SegmentSource::Http {
                        url: url.to_string(),
                        in_ms,
                        out_ms,
                    },
                )
            },
        )
        .register_fn("render", |s: &mut SegmentBuilder, path: &str| {
            s.set_source(
                "render",
                SegmentSource::Render {
                    profile: String::new(),
                    path: PathBuf::from(path),
                },
            )
        })
        .register_fn(
            "pending",
            |s: &mut SegmentBuilder, path: &str, stand_in: &str| {
                s.set_source(
                    "pending",
                    SegmentSource::Pending {
                        path: PathBuf::from(path),
                        stand_in: PathBuf::from(stand_in),
                    },
                )
            },
        )
        .register_fn("fixed", |s: &mut SegmentBuilder, secs: f64| {
            let ms = secs_to_ms(secs, "fixed()")?;
            s.set_duration("fixed", Duration::Fixed(ms))
        })
        .register_fn("fixed", |s: &mut SegmentBuilder, secs: i64| {
            let ms = secs_to_ms(secs as f64, "fixed()")?;
            s.set_duration("fixed", Duration::Fixed(ms))
        })
        .register_fn("fixed_ms", |s: &mut SegmentBuilder, ms: i64| {
            let ms = int_to_ms(ms, "fixed_ms()")?;
            s.set_duration("fixed_ms", Duration::Fixed(ms))
        })
        .register_fn(
            "flex",
            |s: &mut SegmentBuilder, min: Dynamic, max: Dynamic, grow: Dynamic| {
                let (min, max) = (
                    secs_to_ms(number(&min, "flex() min")?, "flex() min")?,
                    secs_to_ms(number(&max, "flex() max")?, "flex() max")?,
                );
                s.flex_ms("flex", min, max, number(&grow, "flex() grow")?)
            },
        )
        .register_fn(
            "flex_ms",
            |s: &mut SegmentBuilder, min: i64, max: i64, grow: i64| {
                let (min, max) = (
                    int_to_ms(min, "flex_ms() min")?,
                    int_to_ms(max, "flex_ms() max")?,
                );
                s.flex_ms("flex_ms", min, max, grow as f64)
            },
        )
        .register_fn(
            "flex_ms",
            |s: &mut SegmentBuilder, min: i64, max: i64, grow: f64| {
                let (min, max) = (
                    int_to_ms(min, "flex_ms() min")?,
                    int_to_ms(max, "flex_ms() max")?,
                );
                s.flex_ms("flex_ms", min, max, grow)
            },
        )
        .register_fn("looped", |s: &mut SegmentBuilder| {
            s.usage.bump("looped");
            s.loop_ = true;
            s.clone()
        })
        .register_fn("optional", |s: &mut SegmentBuilder| {
            s.usage.bump("optional");
            s.optional = true;
            s.clone()
        })
        .register_fn(
            "priority",
            |s: &mut SegmentBuilder, n: i64| -> RhaiResult<SegmentBuilder> {
                s.usage.bump("priority");
                s.priority =
                    u8::try_from(n).or_else(|_| err(format!("priority(): {n} is not 0..=255")))?;
                Ok(s.clone())
            },
        )
        .register_fn(
            "overlay",
            |s: &mut SegmentBuilder, script: &str, vars: Map| -> RhaiResult<SegmentBuilder> {
                s.usage.bump("overlay");
                s.overlay = Some(overlay_vars(script.to_string(), vars)?);
                Ok(s.clone())
            },
        )
        .register_fn("overlay", |s: &mut SegmentBuilder, script: &str| {
            s.usage.bump("overlay");
            s.overlay = Some(OverlayVars {
                script: script.to_string(),
                vars: BTreeMap::new(),
            });
            s.clone()
        })
        .register_fn("own_audio", |s: &mut SegmentBuilder| {
            s.usage.bump("own_audio");
            s.audio = SegmentAudio::Own;
            s.clone()
        })
        .register_fn("bed_audio", |s: &mut SegmentBuilder| {
            s.usage.bump("bed_audio");
            s.audio = SegmentAudio::Bed;
            s.clone()
        })
        .register_fn("mute", |s: &mut SegmentBuilder| {
            s.usage.bump("mute");
            s.audio = SegmentAudio::Mute;
            s.clone()
        })
        .register_fn(
            "ducked_bed",
            |s: &mut SegmentBuilder, gain_db: Dynamic| -> RhaiResult<SegmentBuilder> {
                s.usage.bump("ducked_bed");
                let gain_db = number(&gain_db, "ducked_bed() gain_db")?;
                if !gain_db.is_finite() {
                    return err(format!("ducked_bed(): {gain_db} is not a finite number"));
                }
                s.audio = SegmentAudio::Ducked { gain_db };
                Ok(s.clone())
            },
        );

    engine
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(dir: &tempfile::TempDir, body: &str) -> PathBuf {
        let path = dir.path().join("b.rhai");
        std::fs::write(&path, body).unwrap();
        path
    }

    fn input(next: Option<&str>) -> BumperInput {
        let item = |id: &str| {
            let mut m = Map::new();
            m.insert("entry_id".into(), id.to_string().into());
            m
        };
        BumperInput {
            gap_ms: 30_000,
            next: next.map(item),
            prev: Some(item("prev-1")),
            channel: ChannelInfo {
                name: "Night".into(),
                number: 7,
                logo: None,
                vocabulary: vec!["alpha".into(), "beta".into(), "gamma".into()],
                tz: None,
            },
            air_date: "2026-10-17".into(),
            draw: 0,
            granted: Vec::new(),
            upcoming: Vec::new(),
            config: None,
            history: None,
            pass: Vec::new(),
            catalog_path: None,
        }
    }

    const CARD: &str = r##"
fn hooks() { ["bumper"] }
fn build(ctx) {
    let t = timeline();
    t.push(segment().color("#ff0000").fixed(2.0));
    t.push(segment().still("a.png").flex(4.0, 10.0, 2).optional().priority(3)
        .overlay("shared/card.rhai", #{ title: ctx.channel.name, n: ctx.channel.number }));
    let c = clip("m.mkv", 1000, 9000).with_flex(4000, 8000);
    t.push(segment().clip(c).flex_ms(c.min_ms, c.max_ms, 1).looped().mute());
    t
}
"##;

    #[test]
    fn build_returns_the_scripts_timeline_and_counts_builder_calls() {
        let dir = tempfile::tempdir().unwrap();
        let built = build(&script(&dir, CARD), &input(Some("m-1"))).unwrap();
        let segs = &built.timeline.segments;
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0].duration, Duration::Fixed(2000));
        assert_eq!(
            segs[0].source,
            SegmentSource::Lavfi {
                params: "color=c=0xFF0000".into()
            }
        );
        assert_eq!(
            segs[1].duration,
            Duration::Flex {
                min_ms: 4000,
                max_ms: 10_000,
                grow: 2.0
            }
        );
        assert!(segs[1].optional && segs[1].priority == 3);
        let overlay = segs[1].overlay.as_ref().unwrap();
        assert_eq!(overlay.vars["title"], serde_json::json!("Night"));
        assert_eq!(overlay.vars["n"], serde_json::json!(7));
        assert!(segs[2].loop_ && segs[2].audio == SegmentAudio::Mute);
        assert_eq!(built.usage["segment"], 3);
        assert_eq!(built.usage["push"], 3);
        assert_eq!(built.usage["timeline"], 1);
        assert_eq!(built.usage["with_flex"], 1);
        assert_eq!(built.usage["looped"], 1);
    }

    #[test]
    fn pending_sets_a_stand_in_source_and_counts_the_call() {
        let dir = tempfile::tempdir().unwrap();
        let script_text = r#"
fn hooks() { ["bumper"] }
fn build(ctx) {
    let t = timeline();
    t.push(segment().pending("/r/real.mp4", "/r/stand.mp4").fixed(6.0).looped().mute());
    t
}
"#;
        let built = build(&script(&dir, script_text), &input(None)).unwrap();
        assert_eq!(
            built.timeline.segments[0].source,
            SegmentSource::Pending {
                path: "/r/real.mp4".into(),
                stand_in: "/r/stand.mp4".into()
            }
        );
        assert_eq!(built.timeline.segments[0].name, "pending0");
        assert_eq!(built.usage["pending"], 1);
    }

    #[test]
    fn the_shipped_example_builds_at_the_frontier() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/plugins/text-card-bumper.rhai");
        validate_script(&path, &[]).unwrap();
        let built = build(&path, &input(None)).unwrap();
        assert_eq!(built.timeline.segments.len(), 2);
    }

    const DRAW: &str = r#"
fn hooks() { ["bumper"] }
fn build(ctx) {
    let t = timeline();
    let word = ctx.channel.vocabulary.random(ctx.rng);
    t.push(segment().black().fixed_ms(ctx.rng.int(1000, 1_000_000)).name(word));
    t
}
"#;

    #[test]
    fn the_same_airing_resolves_identically_and_another_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(&dir, DRAW);
        let draw = |next: Option<&str>| build(&path, &input(next)).unwrap().timeline;
        assert_eq!(draw(Some("m-1")), draw(Some("m-1")));
        let distinct = (0..20)
            .map(|n| {
                let id = format!("m-{n}");
                format!("{:?}", draw(Some(&id)))
            })
            .collect::<std::collections::HashSet<_>>()
            .len();
        assert!(distinct > 1, "twenty airings drew one timeline");
    }

    #[test]
    fn a_frontier_gap_hands_the_script_unit_for_next() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(
            &dir,
            r#"fn hooks() { ["bumper"] }
fn build(ctx) {
    let t = timeline();
    let secs = if ctx.next == () { 3 } else { 9 };
    t.push(segment().black().fixed(secs));
    t
}"#,
        );
        let built = build(&path, &input(None)).unwrap();
        assert_eq!(built.timeline.segments[0].duration, Duration::Fixed(3000));
    }

    #[test]
    fn an_empty_timeline_is_an_error_and_the_host_substitutes_black() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(
            &dir,
            "fn hooks() { [\"bumper\"] }\nfn build(ctx) { timeline() }",
        );
        assert!(matches!(
            build(&path, &input(Some("m-1"))),
            Err(BumperError::Empty { .. })
        ));
        let (built, cause) = build_or_black(&path, &input(Some("m-1")));
        assert!(matches!(cause, Some(BumperError::Empty { .. })));
        let segs = built.timeline.segments;
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].source, SegmentSource::black());
        assert_eq!(segs[0].duration, Duration::Fixed(30_000));
    }

    #[test]
    fn audio_bed_takes_a_lavfi_graph_or_a_path_and_refuses_both_or_neither() {
        let dir = tempfile::tempdir().unwrap();
        let bed_of = |source: &str| {
            let path = script(
                &dir,
                &format!(
                    "fn hooks() {{ [\"bumper\"] }}\nfn build(ctx) {{ let t = timeline(); \
                     t.audio_bed({source}, #{{ gain_db: -20.0 }}); \
                     t.push(segment().black().fixed(1).bed_audio()); t }}"
                ),
            );
            build(&path, &input(None)).map(|b| b.timeline.bed)
        };
        let lavfi = bed_of(r##"#{ lavfi: "anoisesrc=color=brown" }"##)
            .unwrap()
            .unwrap();
        assert_eq!(
            lavfi.source,
            BedSource::Lavfi("anoisesrc=color=brown".into())
        );
        assert_eq!(lavfi.gain_db, -20.0);
        let file = bed_of(r#"#{ path: "/b/rain.wav" }"#).unwrap().unwrap();
        assert_eq!(file.source, BedSource::File("/b/rain.wav".into()));
        for bad in [r#"#{ path: "/b.wav", lavfi: "anullsrc" }"#, "#{}"] {
            let msg = bed_of(bad).unwrap_err().to_string();
            assert!(msg.contains("exactly one of"), "{bad}: {msg}");
        }
    }

    #[test]
    fn ducked_bed_sets_the_gain_and_needs_a_bed_naming_the_segment() {
        let dir = tempfile::tempdir().unwrap();
        let ok = script(
            &dir,
            "fn hooks() { [\"bumper\"] }\nfn build(ctx) { let t = timeline(); \
             t.audio_bed(\"/b/rain.wav\"); \
             t.push(segment().name(\"talk\").clip(clip(\"/m/a.mkv\", 0, 1000)).fixed(1).ducked_bed(-12)); t }",
        );
        let built = build(&ok, &input(None)).unwrap();
        assert_eq!(
            built.timeline.segments[0].audio,
            SegmentAudio::Ducked { gain_db: -12.0 }
        );
        let no_bed = script(
            &dir,
            "fn hooks() { [\"bumper\"] }\nfn build(ctx) { let t = timeline(); \
             t.push(segment().name(\"talk\").clip(clip(\"/m/a.mkv\", 0, 1000)).fixed(1).ducked_bed(-12)); t }",
        );
        let msg = build(&no_bed, &input(None)).unwrap_err().to_string();
        assert!(
            msg.contains("\"talk\"") && msg.contains("audio_bed"),
            "{msg}"
        );
    }

    #[test]
    fn validation_requires_the_hook_and_both_way_capability_agreement() {
        let dir = tempfile::tempdir().unwrap();
        let ok = script(&dir, CARD);
        validate_script(&ok, &[]).unwrap();

        let msg = validate_script(&ok, &["teleport".into()]).unwrap_err();
        assert!(msg.contains("never declares"), "{msg}");

        let declares = script(
            &dir,
            "fn hooks() { [\"bumper\"] }\nfn capabilities() { [\"teleport\"] }\n\
             fn build(ctx) { timeline() }",
        );
        let msg = validate_script(&declares, &["teleport".into()]).unwrap_err();
        assert!(msg.contains("unknown capability"), "{msg}");

        let wrong_hook = script(
            &dir,
            "fn hooks() { [\"sequencer\"] }\nfn build(ctx) { timeline() }",
        );
        let msg = validate_script(&wrong_hook, &[]).unwrap_err();
        assert!(msg.contains("`bumper` hook"), "{msg}");

        let no_build = script(&dir, "fn hooks() { [\"bumper\"] }");
        let msg = validate_script(&no_build, &[]).unwrap_err();
        assert!(msg.contains("build(ctx)"), "{msg}");
    }

    fn granted(names: &[&str]) -> BumperInput {
        BumperInput {
            granted: names.iter().map(|n| n.to_string()).collect(),
            ..input(None)
        }
    }

    #[test]
    fn read_returns_a_plugin_file_and_unit_for_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("words.txt"), "alpha beta").unwrap();
        let path = script(
            &dir,
            r#"fn hooks() { ["bumper"] }
fn capabilities() { ["read"] }
fn build(ctx) {
    let t = timeline();
    let text = ctx.read("words.txt");
    if ctx.read("nope.txt") != () { throw "missing file must be ()"; }
    t.push(segment().black().fixed(1).name(text));
    t
}"#,
        );
        validate_script(&path, &["read".into()]).unwrap();
        let built = build(&path, &granted(&["read"])).unwrap();
        assert_eq!(built.timeline.segments[0].name, "alpha beta");
    }

    #[test]
    fn read_refuses_paths_outside_the_plugin_folder() {
        let outer = tempfile::tempdir().unwrap();
        std::fs::write(outer.path().join("secret.txt"), "s").unwrap();
        let plugin = outer.path().join("plugin");
        std::fs::create_dir(&plugin).unwrap();
        std::os::unix::fs::symlink(outer.path().join("secret.txt"), plugin.join("link.txt"))
            .unwrap();
        for target in [
            "../secret.txt".to_string(),
            "link.txt".to_string(),
            outer.path().join("secret.txt").display().to_string(),
        ] {
            let err = read_in_dir(&plugin, &target).unwrap_err();
            assert!(err.contains("outside the plugin folder"), "{target}: {err}");
        }
    }

    #[test]
    fn read_refuses_a_file_over_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("big.txt"),
            vec![b'x'; READ_LIMIT_BYTES as usize + 1],
        )
        .unwrap();
        let err = read_in_dir(dir.path(), "big.txt").unwrap_err();
        assert!(err.contains("over the"), "{err}");
    }

    #[test]
    fn probe_read_and_http_error_unless_granted() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(
            &dir,
            "fn hooks() { [\"bumper\"] }\nfn build(ctx) { ctx.probe(\"x.mp4\"); timeline() }",
        );
        let msg = build(&path, &granted(&[])).unwrap_err().to_string();
        assert!(msg.contains("capability `probe` not declared"), "{msg}");
        let path = script(
            &dir,
            "fn hooks() { [\"bumper\"] }\nfn build(ctx) { ctx.read(\"x\"); timeline() }",
        );
        let msg = build(&path, &granted(&["probe"])).unwrap_err().to_string();
        assert!(msg.contains("capability `read` not declared"), "{msg}");
        let path = script(
            &dir,
            "fn hooks() { [\"bumper\"] }\n\
             fn build(ctx) { ctx.http(\"http://127.0.0.1:9/\", \"GET\", \"\"); timeline() }",
        );
        let msg = build(&path, &granted(&["read"])).unwrap_err().to_string();
        assert!(msg.contains("capability `http` not declared"), "{msg}");
    }

    #[test]
    fn exec_errors_unless_granted_and_returns_code_and_output() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(
            &dir,
            r#"fn hooks() { ["bumper"] }
fn capabilities() { ["exec"] }
fn build(ctx) {
    let r = ctx.exec("/bin/sh", ["-c", "printf out; printf err >&2; exit 3"]);
    let t = timeline();
    t.push(segment().black().fixed(1).name(`${r.code}:${r.stdout}:${r.stderr}`));
    t
}"#,
        );
        validate_script(&path, &["exec".into()]).unwrap();
        let built = build(&path, &granted(&["exec"])).unwrap();
        assert_eq!(built.timeline.segments[0].name, "3:out:err");
        let msg = build(&path, &granted(&[])).unwrap_err().to_string();
        assert!(msg.contains("capability `exec` not declared"), "{msg}");
    }

    #[test]
    fn http_returns_the_body_and_parse_json_reads_it() {
        let (url, server) =
            crate::plugin_http::tests::serve_once("200 OK", r#"{"title":"Up Next"}"#);
        let dir = tempfile::tempdir().unwrap();
        let path = script(
            &dir,
            &format!(
                r#"fn hooks() {{ ["bumper"] }}
fn capabilities() {{ ["http"] }}
fn build(ctx) {{
    let reply = parse_json(ctx.http("{url}/next", "GET", ""));
    let t = timeline();
    t.push(segment().black().fixed(1).name(reply.title));
    t
}}"#
            ),
        );
        validate_script(&path, &["http".into()]).unwrap();
        let built = build(&path, &granted(&["http"])).unwrap();
        assert_eq!(server.join().unwrap(), "GET /next HTTP/1.1");
        assert_eq!(built.timeline.segments[0].name, "Up Next");
    }

    #[test]
    fn probe_reports_duration_streams_and_container_tags() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("m.mkv");
        let made = std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=64x48:d=1.5:r=10",
            ])
            .args(["-f", "lavfi", "-i", "anullsrc=r=44100:cl=mono", "-t", "1.5"])
            .args([
                "-metadata",
                "Prompt={\"a\":1}",
                "-metadata:s:a:0",
                "language=eng",
            ])
            .arg(&media)
            .status()
            .unwrap();
        assert!(made.success());
        let path = script(
            &dir,
            &format!(
                r#"fn hooks() {{ ["bumper"] }}
fn capabilities() {{ ["probe"] }}
fn build(ctx) {{
    let p = ctx.probe("{m}");
    if ctx.probe("{m}.absent") != () {{ throw "unprobeable file must be ()"; }}
    let t = timeline();
    let audio = p.streams.filter(|s| s.kind == "audio")[0];
    t.push(segment().black().fixed_ms(p.duration_ms)
        .name(`${{p.tags.prompt}}|${{p.streams.len()}}|${{p.streams[0].width}}x${{p.streams[0].height}}|${{audio.lang}}`));
    t
}}"#,
                m = media.display()
            ),
        );
        let built = build(&path, &granted(&["probe"])).unwrap();
        let seg = &built.timeline.segments[0];
        assert_eq!(seg.name, "{\"a\":1}|2|64x48|eng");
        // The container pads a 1.5s encode by a few ms.
        let Duration::Fixed(ms) = seg.duration else {
            panic!("probe duration became {:?}", seg.duration)
        };
        assert!((1500..1520).contains(&ms), "{ms}");
    }

    #[test]
    fn probe_carries_the_catalog_shots_of_the_file_it_probes() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("m.mkv");
        let made = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-f", "lavfi", "-i"])
            .arg("color=c=red:s=64x48:d=1.5:r=10")
            .arg(&media)
            .status()
            .unwrap();
        assert!(made.success());
        let db = dir.path().join("catalog.db");
        {
            let c = crate::catalog::Catalog::open(&db).unwrap();
            let id = "imdb:tt1";
            c.upsert_entry(&crate::catalog::Entry::new(
                id,
                "movie",
                "Red",
                crate::catalog::Source::LocalFs,
            ))
            .unwrap();
            c.add_source(&crate::catalog::EntrySource {
                source: crate::catalog::Source::LocalFs,
                source_id: "fs-1".into(),
                entry_id: id.into(),
                playback_path: media.display().to_string(),
                last_seen: None,
                missing_since: None,
                unairable_reason: None,
            })
            .unwrap();
            let shots = [
                crate::shots::Shot {
                    start: 0.0,
                    end: 0.5,
                    luma: 0.01,
                    motion: 0.0,
                    card: true,
                },
                crate::shots::Shot {
                    start: 0.5,
                    end: 1.5,
                    luma: 0.3,
                    motion: 0.12,
                    card: false,
                },
            ];
            c.set_shots(id, &media.display().to_string(), &shots, None)
                .unwrap();
        }
        let path = script(
            &dir,
            &format!(
                r#"fn hooks() {{ ["bumper"] }}
fn capabilities() {{ ["probe"] }}
fn build(ctx) {{
    let s = ctx.probe("{m}").shots;
    let t = timeline();
    t.push(segment().black().fixed_ms(1000)
        .name(`${{s.len()}}|${{s[0].card}}|${{s[1].card}}|${{s[1].end}}|${{s[1].motion}}`));
    t
}}"#,
                m = media.display()
            ),
        );
        let mut input = granted(&["probe"]);
        input.catalog_path = Some(db);
        let built = build(&path, &input).unwrap();
        assert_eq!(built.timeline.segments[0].name, "2|true|false|1.5|0.12");

        // Without a catalog the key is absent, not an empty list.
        let path = script(
            &dir,
            &format!(
                r#"fn hooks() {{ ["bumper"] }}
fn capabilities() {{ ["probe"] }}
fn build(ctx) {{
    let t = timeline();
    t.push(segment().black().fixed_ms(1000).name(`${{"shots" in ctx.probe("{m}")}}`));
    t
}}"#,
                m = media.display()
            ),
        );
        let built = build(&path, &granted(&["probe"])).unwrap();
        assert_eq!(built.timeline.segments[0].name, "false");
    }

    /// `ctx.probe(path).audio` reads the facts the catalog holds for the entry
    /// that plays from `path`; a probed file with no facts has no `audio`.
    #[test]
    fn probe_carries_the_catalogs_audio_facts_when_the_file_was_analyzed() {
        use crate::catalog::{Catalog, Entry, EntrySource, Source};
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("song.mkv");
        let made = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-f", "lavfi", "-i"])
            .arg("anullsrc=r=44100:cl=mono")
            .args(["-t", "1"])
            .arg(&media)
            .status()
            .unwrap();
        assert!(made.success());
        let plain = dir.path().join("plain.mkv");
        std::fs::copy(&media, &plain).unwrap();

        let catalog_path = dir.path().join("catalog.db");
        let cat = Catalog::open(&catalog_path).unwrap();
        cat.upsert_entry(&Entry::new("song", "movie", "Song", Source::LocalFs))
            .unwrap();
        cat.add_source(&EntrySource {
            source: Source::LocalFs,
            source_id: "song".into(),
            entry_id: "song".into(),
            playback_path: media.display().to_string(),
            last_seen: None,
            missing_since: None,
            unairable_reason: None,
        })
        .unwrap();
        cat.set_audio_fact(
            "song",
            &media.display().to_string(),
            "2026-01-01T00:00:00Z",
            Ok(&serde_json::json!({"bpm": 128.5, "beat_grid": {"beats": 64}})),
        )
        .unwrap();

        let path = script(
            &dir,
            &format!(
                r#"fn hooks() {{ ["bumper"] }}
fn capabilities() {{ ["probe"] }}
fn build(ctx) {{
    let a = ctx.probe("{m}");
    let b = ctx.probe("{p}");
    if "audio" in b {{ throw "an unanalyzed file must have no audio"; }}
    let t = timeline();
    t.push(segment().black().fixed_ms(1000).name(`${{a.audio.bpm}}|${{a.audio.beat_grid.beats}}`));
    t
}}"#,
                m = media.display(),
                p = plain.display()
            ),
        );
        let mut input = granted(&["probe"]);
        input.catalog_path = Some(catalog_path);
        let built = build(&path, &input).unwrap();
        assert_eq!(built.timeline.segments[0].name, "128.5|64");
    }

    fn ledger_with_a_programme_and_two_bumpers() -> Arc<HistoryDb> {
        use crate::history::PlayRecord;
        use time::macros::datetime;
        let db = HistoryDb::open_in_memory().unwrap();
        let t0 = datetime!(2026-10-17 20:00 UTC);
        let bumper = |name: &str, secs: i64, source: &str| PlayRecord {
            entry_id: name.into(),
            start: t0 + time::Duration::seconds(secs),
            played_at: t0,
            kind: AiringKind::Bumper,
            source: Some(source.into()),
            in_ms: Some(1_000),
            out_ms: Some(4_000),
            produced_by: Some("tease.rhai".into()),
            ..Default::default()
        };
        let programme = PlayRecord {
            entry_id: "film-1".into(),
            start: t0 + time::Duration::seconds(60),
            played_at: t0,
            ..Default::default()
        };
        db.record(
            "Night",
            &[
                bumper("a", 0, "/m/a.mkv"),
                programme,
                bumper("b", 120, "/m/b.mkv"),
            ],
        )
        .unwrap();
        db.record("Other", &[bumper("c", 0, "/m/c.mkv")]).unwrap();
        Arc::new(db)
    }

    #[test]
    fn airings_returns_this_channels_rows_past_and_scheduled() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(
            &dir,
            r#"fn hooks() { ["bumper"] }
fn build(ctx) {
    let all = ctx.airings(#{});
    let bumpers = ctx.airings(#{ kind: "bumper", order: "desc", limit: 1 });
    let by_source = ctx.airings(#{ source: "/m/a.mkv" });
    let elsewhere = ctx.airings(#{ channel: "*", kind: "bumper" });
    let t = timeline();
    t.push(segment().black().fixed(1.0));
    t.push(segment().black().fixed_ms(all.len()));
    t.push(segment().black().fixed_ms(bumpers[0].start_ms - 1792267200000));
    t.push(segment().black().fixed_ms(by_source[0].out_ms));
    t.push(segment().black().fixed_ms(elsewhere.len()));
    t
}"#,
        );
        let input = BumperInput {
            upcoming: Vec::new(),
            config: None,
            history: Some(ledger_with_a_programme_and_two_bumpers()),
            ..input(None)
        };
        let built = build(&path, &input).unwrap();
        let ms: Vec<u64> = built
            .timeline
            .segments
            .iter()
            .map(|s| match s.duration {
                Duration::Fixed(ms) => ms,
                other => panic!("{other:?}"),
            })
            .collect();
        // 3 own rows; newest bumper "b" starts 120s after 20:00 UTC
        // (the script subtracts 20:00 UTC, 1792267200000 ms); the source
        // filter finds "a" with out_ms 4000; 3 bumpers across channels.
        assert_eq!(ms, vec![1_000, 3, 120_000, 4_000, 3]);
    }

    #[test]
    fn airings_rejects_an_unknown_filter_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(
            &dir,
            "fn hooks() { [\"bumper\"] }\nfn build(ctx) { ctx.airings(#{ kinds: \"bumper\" }); timeline() }",
        );
        let msg = build(&path, &input(None)).unwrap_err().to_string();
        assert!(msg.contains("unknown filter key `kinds`"), "{msg}");
    }

    #[test]
    fn airings_without_a_ledger_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(
            &dir,
            r#"fn hooks() { ["bumper"] }
fn build(ctx) {
    let t = timeline();
    t.push(segment().black().fixed_ms(ctx.airings(#{}).len() + 1000));
    t
}"#,
        );
        let built = build(&path, &input(None)).unwrap();
        assert_eq!(built.timeline.segments[0].duration, Duration::Fixed(1_000));
    }

    /// One Plex stand-in for every trailer test, on a free local port. It
    /// answers a film's extras by rating key (any other path is a 404), and
    /// `PLEX_URL` / `PLEX_TOKEN` point at it, as they would in the container.
    fn plex() -> &'static str {
        use std::io::{Read, Write};
        static PLEX: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        PLEX.get_or_init(|| {
            let routes: Vec<(&str, String)> = vec![
                (
                    "/library/metadata/11960/extras",
                    extras(
                        "trailer",
                        "/services/iva/assets/7788/video.mp4?fmt=4&bitrate=750",
                        88_000,
                    ),
                ),
                ("/library/metadata/1/extras", extras("clip", "/c.mp4", 90_000)),
                ("/library/metadata/2/extras", extras("trailer", "/t.mp4", 3_000)),
                ("/library/metadata/21/extras", extras("trailer", "/a.mp4", 60_000)),
                ("/library/metadata/22/extras", extras("trailer", "/b.mp4", 60_000)),
                ("/library/metadata/23/extras", extras("trailer", "/c.mp4", 60_000)),
            ];
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut sock) = stream else { return };
                    let mut buf = [0u8; 4096];
                    let n = sock.read(&mut buf).unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let path = head
                        .split_whitespace()
                        .nth(1)
                        .and_then(|p| p.split('?').next())
                        .unwrap_or("")
                        .to_string();
                    let (status, body) = match routes.iter().find(|(p, _)| *p == path) {
                        Some((_, body)) => ("200 OK", body.clone()),
                        None => ("404 Not Found", String::new()),
                    };
                    let _ = write!(
                        sock,
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                }
            });
            // Only these tests read these two names, and they run once.
            unsafe {
                std::env::set_var("PLEX_URL", &url);
                std::env::set_var("PLEX_TOKEN", "t0k");
            }
            url
        })
    }

    fn extras(subtype: &str, part_key: &str, duration_ms: i64) -> String {
        serde_json::json!({ "MediaContainer": { "Metadata": [{
            "subtype": subtype,
            "duration": duration_ms,
            "Media": [{ "duration": duration_ms, "Part": [{ "key": part_key }] }],
        }] } })
        .to_string()
    }

    fn movie(id: &str, plex_key: Option<&str>) -> Map {
        let mut m = Map::new();
        m.insert("entry_id".into(), id.to_string().into());
        m.insert("title".into(), format!("Film {id}").into());
        m.insert("type".into(), "movie".into());
        if let Some(key) = plex_key {
            let mut ids = Map::new();
            ids.insert("plex".into(), key.to_string().into());
            m.insert("source_ids".into(), Dynamic::from_map(ids));
        }
        m
    }

    fn trailer_input(mode: &str, gap_ms: u64, next: Map, upcoming: Vec<Map>) -> BumperInput {
        plex();
        BumperInput {
            gap_ms,
            next: Some(next),
            upcoming,
            config: Some(serde_json::json!({ "mode": mode })),
            granted: vec!["http".into()],
            ..input(None)
        }
    }

    fn trailer_script() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins/trailer-bumper.rhai")
    }

    #[test]
    fn a_trailer_bumper_in_next_mode_airs_the_nexts_trailer_over_an_up_next_card() {
        validate_script(&trailer_script(), &["http".into()]).unwrap();
        let input = trailer_input("next", 120_000, movie("rush", Some("11960")), vec![]);
        let built = build(&trailer_script(), &input).unwrap();
        let first = &built.timeline.segments[0];
        // The url keeps its placeholders: the token is in no playout file.
        assert_eq!(
            first.source,
            SegmentSource::Http {
                url: "{{PLEX_URL}}/services/iva/assets/7788/video.mp4?fmt=4&bitrate=750&X-Plex-Token={{PLEX_TOKEN}}".into(),
                in_ms: 0,
                out_ms: 88_000,
            }
        );
        let card = first.overlay.as_ref().unwrap();
        assert_eq!(card.vars["title"], serde_json::json!("Film rush"));
        assert_eq!(card.vars["word"], serde_json::json!("Up next"));

        let resolved = super::super::timeline::fit(&built.timeline, 120_000, 30.0).unwrap();
        let items = super::super::emit::emit_timeline(
            &resolved,
            &super::super::emit::EmitContext {
                channel: "t",
                fps: 30.0,
                overlay_root: Path::new("/o"),
                media_len_ms: &|_| None,
            },
            OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
        );
        let json = serde_json::to_value(&items[0]).unwrap();
        assert_eq!(json["source"]["source_type"], "http");
        assert_eq!(json["source"]["out_point_ms"], 88_000);
        assert!(
            !json.to_string().contains("t0k"),
            "the token reached the playout"
        );
        let span: i128 = items
            .iter()
            .map(|i| (i.finish - i.start).whole_milliseconds())
            .sum();
        assert_eq!(span, 120_000);
    }

    #[test]
    fn a_trailer_bumper_returns_nothing_when_there_is_no_trailer() {
        let empty = |next: Map| {
            matches!(
                build(
                    &trailer_script(),
                    &trailer_input("next", 120_000, next, vec![])
                ),
                Err(BumperError::Empty { .. })
            )
        };
        assert!(
            empty(movie("only-a-clip", Some("1"))),
            "a clip is not a trailer"
        );
        assert!(
            empty(movie("too-short", Some("2"))),
            "a 3s trailer is under the minimum"
        );
        assert!(
            empty(movie("unknown-to-plex", Some("99"))),
            "a 404 from Plex"
        );
        assert!(empty(movie("no-plex-id", None)), "an entry with no Plex id");
    }

    #[test]
    fn a_trailer_bumper_in_upcoming_mode_airs_each_film_once_nearest_first() {
        let upcoming = vec![
            movie("b", Some("22")),
            movie("a", Some("21")),
            movie("c", Some("23")),
        ];
        let urls = |gap_ms: u64| -> Vec<String> {
            let input = trailer_input("upcoming", gap_ms, movie("a", Some("21")), upcoming.clone());
            let built = build(&trailer_script(), &input).unwrap();
            let resolved = super::super::timeline::fit(&built.timeline, gap_ms, 30.0).unwrap();
            assert_eq!(resolved.total_ms(), gap_ms);
            built
                .timeline
                .segments
                .iter()
                .filter_map(|s| match &s.source {
                    SegmentSource::Http { url, .. } => Some(
                        url.trim_start_matches("{{PLEX_URL}}")
                            .split('?')
                            .next()
                            .unwrap()
                            .to_string(),
                    ),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(urls(400_000), ["/a.mp4", "/b.mp4", "/c.mp4"]);
        // A 70s gap holds one whole trailer and the start of the next.
        assert_eq!(urls(70_000), ["/a.mp4", "/b.mp4"]);
    }

    fn up_next_script() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins/bumpers/up_next.rhai")
    }

    /// 2026-10-17 20:00 UTC, the hour the film after the gap starts.
    const FILM_START: i64 = 1_792_267_200;

    fn film(rating: Option<&str>, start: i64) -> Map {
        let mut m = movie("heat", Some("555"));
        m.insert("path".into(), "/media/heat.mkv".into());
        m.insert("start".into(), start.into());
        if let Some(r) = rating {
            m.insert("content_rating".into(), r.to_string().into());
        }
        m
    }

    fn up_next_input(config: serde_json::Value, next: Map, granted: &[&str]) -> BumperInput {
        BumperInput {
            gap_ms: 120_000,
            next: Some(next),
            config: Some(config),
            granted: granted.iter().map(|g| g.to_string()).collect(),
            ..input(None)
        }
    }

    const PICKS: &str = r#"{ "heat": { "path": "/picks/heat.mkv", "in": 120, "out": 135 } }"#;

    fn clip_of(built: &BuiltTimeline) -> (String, u64, u64) {
        match &built.timeline.segments[0].source {
            SegmentSource::Clip {
                path,
                in_ms,
                out_ms,
            } => (path.display().to_string(), *in_ms, *out_ms),
            other => panic!("{other:?}"),
        }
    }

    /// A plex-db-ex explorer stand-in: answers `/api/search/run` with `reply`
    /// and returns its URL plus a channel carrying each request body.
    fn explorer(reply: &'static str) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut sock) = stream else { return };
                // Read through the body: closing on an unread body resets the client.
                let mut head = String::new();
                let mut buf = [0u8; 8192];
                loop {
                    let n = sock.read(&mut buf).unwrap_or(0);
                    head.push_str(&String::from_utf8_lossy(&buf[..n]));
                    let Some((top, body)) = head.split_once("\r\n\r\n") else {
                        if n == 0 {
                            break;
                        }
                        continue;
                    };
                    let want = top
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if body.len() >= want || n == 0 {
                        break;
                    }
                }
                let body = head.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
                let _ = tx.send(format!("{}\n{body}", head.lines().next().unwrap_or("")));
                let _ = write!(
                    sock,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        (url, rx)
    }

    #[test]
    fn up_next_airs_a_hand_picked_clip_with_title_time_and_channel_over_it() {
        validate_script(&up_next_script(), &["http".into()]).unwrap();
        let config = serde_json::json!({ "picks": serde_json::from_str::<serde_json::Value>(PICKS).unwrap() });
        let built = build(
            &up_next_script(),
            &up_next_input(config, film(Some("PG"), FILM_START), &["http"]),
        )
        .unwrap();
        assert_eq!(
            clip_of(&built),
            ("/picks/heat.mkv".into(), 120_000, 135_000)
        );
        let card = built.timeline.segments[0].overlay.as_ref().unwrap();
        assert_eq!(
            card.vars["lines"],
            serde_json::json!(["Up next", "Film heat", "Tonight 8:00 PM", "Night"])
        );
    }

    #[test]
    fn up_next_words_a_film_on_a_later_day_as_tomorrow() {
        let config = serde_json::json!({ "picks": serde_json::from_str::<serde_json::Value>(PICKS).unwrap() });
        let next = film(Some("PG"), FILM_START + 3_600 * 4 + 60);
        let built = build(&up_next_script(), &up_next_input(config, next, &[])).unwrap();
        let card = built.timeline.segments[0].overlay.as_ref().unwrap();
        assert_eq!(
            card.vars["lines"][2],
            serde_json::json!("Tomorrow 12:01 AM")
        );
    }

    #[test]
    fn up_next_skips_a_pick_above_the_slots_rating_for_broll() {
        let config = |slot: &str| {
            serde_json::json!({
                "picks": serde_json::from_str::<serde_json::Value>(PICKS).unwrap(),
                "slot_rating": slot,
                "broll_generic": [{ "path": "/broll/a.mp4", "in": 0, "out": 20 }],
            })
        };
        let aired = |rating, slot: &str| {
            let built = build(
                &up_next_script(),
                &up_next_input(config(slot), film(rating, FILM_START), &[]),
            )
            .unwrap();
            clip_of(&built).0
        };
        assert_eq!(aired(Some("R"), "PG-13"), "/broll/a.mp4");
        assert_eq!(aired(None, "PG-13"), "/broll/a.mp4");
        assert_eq!(aired(Some("PG-13"), "PG-13"), "/picks/heat.mkv");
        assert_eq!(aired(Some("TV-MA"), "R"), "/picks/heat.mkv");
    }

    #[test]
    fn up_next_plays_the_top_shot_plex_db_ex_finds_in_the_film_itself() {
        let (url, requests) =
            explorer(r#"{"shots":[{"item_id":"555","in_ms":2000000,"out_ms":2012000,"drawn":1}]}"#);
        let config =
            serde_json::json!({ "plexdb_url": url, "phrase": "a car chase", "slot_rating": "R" });
        let built = build(
            &up_next_script(),
            &up_next_input(config, film(Some("R"), FILM_START), &["http"]),
        )
        .unwrap();
        assert_eq!(
            clip_of(&built),
            ("/media/heat.mkv".into(), 2_000_000, 2_012_000)
        );
        let request = requests.recv().unwrap();
        let (line, body) = request.split_once('\n').unwrap();
        assert!(line.starts_with("POST /api/search/run"), "{line}");
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["pool"], serde_json::json!(["555"]));
        assert_eq!(body["phrase"], "a car chase");
        assert_eq!(body["draw"]["n"], 1);
    }

    #[test]
    fn up_next_drops_a_searched_shot_above_the_slots_rating() {
        let (url, _) = explorer(r#"{"shots":[{"item_id":"555","in_ms":1000,"out_ms":9000}]}"#);
        let config = serde_json::json!({
            "plexdb_url": url, "phrase": "x", "slot_rating": "PG",
            "broll_generic": [{ "path": "/broll/a.mp4", "in": 0, "out": 20 }],
        });
        let built = build(
            &up_next_script(),
            &up_next_input(config, film(Some("R"), FILM_START), &["http"]),
        )
        .unwrap();
        assert_eq!(clip_of(&built).0, "/broll/a.mp4");
    }

    #[test]
    fn up_next_falls_to_broll_when_search_is_ungranted_empty_or_unreachable() {
        let (empty, _) = explorer(r#"{"shots":[]}"#);
        let broll = serde_json::json!([{ "path": "/broll/a.mp4", "in": 5, "out": 25 }]);
        for (url, granted) in [
            (empty.clone(), vec![]),
            (empty, vec!["http"]),
            ("http://127.0.0.1:9".to_string(), vec!["http"]),
        ] {
            let config =
                serde_json::json!({ "plexdb_url": url, "phrase": "x", "broll_generic": broll });
            let built = build(
                &up_next_script(),
                &up_next_input(config, film(Some("R"), FILM_START), &granted),
            )
            .unwrap();
            assert_eq!(clip_of(&built), ("/broll/a.mp4".into(), 5_000, 25_000));
        }
    }

    #[test]
    fn up_next_with_no_clip_anywhere_returns_an_empty_timeline() {
        let built = build(
            &up_next_script(),
            &up_next_input(serde_json::json!({}), film(None, FILM_START), &[]),
        );
        assert!(matches!(built, Err(BumperError::Empty { .. })));
    }

    fn archival_dir(manifest: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("archival.rhai");
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../examples/plugins/bumpers/archival.rhai"),
            &script,
        )
        .unwrap();
        std::fs::write(dir.path().join("archival.json"), manifest).unwrap();
        (dir, script)
    }

    fn fixture(name: &str) -> String {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/etv-query-test/fixtures/bumpers")
            .join(name)
            .display()
            .to_string()
    }

    fn archival_manifest(clips: &[(&str, &[&str], bool, Option<i64>)]) -> String {
        let clips: Vec<_> = clips
            .iter()
            .map(|(name, tags, burned, minutes)| {
                serde_json::json!({
                    "path": fixture(name), "tags": tags,
                    "burned_in_text": burned, "minutes": minutes,
                })
            })
            .collect();
        serde_json::json!({ "clips": clips }).to_string()
    }

    fn archival_input(gap_ms: u64, config: serde_json::Value) -> BumperInput {
        BumperInput {
            gap_ms,
            config: Some(config),
            granted: vec!["probe".into(), "read".into()],
            ..input(None)
        }
    }

    #[test]
    fn archival_trims_two_frames_off_both_ends_and_leaves_burned_in_reels_bare() {
        let (_dir, script) = archival_dir(&archival_manifest(&[(
            "ident-45s.mp4",
            &["ident"],
            true,
            None,
        )]));
        let built = build(&script, &archival_input(120_000, serde_json::json!({}))).unwrap();
        let duration = probe_media(Path::new(&fixture("ident-45s.mp4")))
            .unwrap()
            .unwrap()["duration_ms"]
            .as_u64()
            .unwrap();
        // Two frames at 30 fps is 67 ms.
        assert_eq!(
            clip_of(&built),
            (fixture("ident-45s.mp4"), 67, duration - 67)
        );
        assert_eq!(
            built.timeline.segments[0].duration,
            Duration::Fixed(duration - 134)
        );
        assert!(built.timeline.segments[0].overlay.is_none());
    }

    #[test]
    fn archival_cuts_a_reel_longer_than_the_gap_at_the_gap() {
        let (_dir, script) = archival_dir(&archival_manifest(&[(
            "ident-45s.mp4",
            &["ident"],
            true,
            None,
        )]));
        let built = build(&script, &archival_input(10_000, serde_json::json!({}))).unwrap();
        assert_eq!(clip_of(&built).1..clip_of(&built).2, 67..10_067);
        assert_eq!(built.timeline.segments[0].duration, Duration::Fixed(10_000));
    }

    #[test]
    fn archival_rejects_a_frame_rate_of_zero() {
        let (_dir, script) = archival_dir(&archival_manifest(&[(
            "ident-45s.mp4",
            &["ident"],
            true,
            None,
        )]));
        let msg = build(
            &script,
            &archival_input(10_000, serde_json::json!({ "fps": 0 })),
        )
        .unwrap_err()
        .to_string();
        assert!(msg.contains("config.fps"), "{msg}");
    }

    #[test]
    fn archival_trims_by_the_configured_frame_rate() {
        let (_dir, script) = archival_dir(&archival_manifest(&[(
            "ident-45s.mp4",
            &["ident"],
            true,
            None,
        )]));
        let built = build(
            &script,
            &archival_input(120_000, serde_json::json!({ "fps": 25 })),
        )
        .unwrap();
        assert_eq!(clip_of(&built).1, 80);
    }

    #[test]
    fn archival_draws_the_card_over_a_reel_without_burned_in_text() {
        let (_dir, script) = archival_dir(&archival_manifest(&[(
            "promo-22s.mp4",
            &["promo"],
            false,
            None,
        )]));
        let built = build(&script, &archival_input(120_000, serde_json::json!({}))).unwrap();
        let card = built.timeline.segments[0].overlay.as_ref().unwrap();
        assert_eq!(card.vars["lines"], serde_json::json!(["Night"]));
    }

    #[test]
    fn archival_finds_clips_by_tag() {
        let (_dir, script) = archival_dir(&archival_manifest(&[
            ("ident-45s.mp4", &["ident"], true, None),
            ("promo-22s.mp4", &["promo", "bumper"], true, None),
        ]));
        let drawn = |tags: serde_json::Value| {
            let built = build(
                &script,
                &archival_input(120_000, serde_json::json!({ "tags": tags })),
            );
            built.map(|b| clip_of(&b).0)
        };
        assert_eq!(
            drawn(serde_json::json!(["promo"])).unwrap(),
            fixture("promo-22s.mp4")
        );
        assert_eq!(
            drawn(serde_json::json!(["bumper"])).unwrap(),
            fixture("promo-22s.mp4")
        );
        assert_eq!(
            drawn(serde_json::json!(["ident"])).unwrap(),
            fixture("ident-45s.mp4")
        );
        assert!(matches!(
            drawn(serde_json::json!(["snipe"])),
            Err(BumperError::Empty { .. })
        ));
    }

    #[test]
    fn archival_draws_a_countdown_clip_only_for_a_gap_matching_its_minutes() {
        let (_dir, script) = archival_dir(&archival_manifest(&[(
            "station-bumper-18s.mp4",
            &["countdown"],
            true,
            Some(5),
        )]));
        let at = |gap_ms| build(&script, &archival_input(gap_ms, serde_json::json!({})));
        assert_eq!(
            clip_of(&at(300_000).unwrap()).0,
            fixture("station-bumper-18s.mp4")
        );
        assert_eq!(
            clip_of(&at(290_000).unwrap()).0,
            fixture("station-bumper-18s.mp4")
        );
        assert!(matches!(at(120_000), Err(BumperError::Empty { .. })));
        assert!(matches!(at(600_000), Err(BumperError::Empty { .. })));
    }

    #[test]
    fn archival_draws_the_least_recently_aired_clip() {
        use crate::history::PlayRecord;
        use time::macros::datetime;
        let names = ["ident-45s.mp4", "promo-22s.mp4", "station-bumper-12s.mp4"];
        let (_dir, script) = archival_dir(&archival_manifest(
            &names.map(|n| (n, &["ident"][..], true, None)),
        ));
        let t0 = datetime!(2026-10-17 20:00 UTC);
        let aired = |name: &str, secs: i64| PlayRecord {
            entry_id: "archival".into(),
            start: t0 + time::Duration::seconds(secs),
            played_at: t0,
            kind: AiringKind::Bumper,
            source: Some(fixture(name)),
            ..Default::default()
        };
        let drawn = |rows: Vec<PlayRecord>| {
            let db = HistoryDb::open_in_memory().unwrap();
            db.record("Night", &rows).unwrap();
            let input = BumperInput {
                history: Some(Arc::new(db)),
                ..archival_input(120_000, serde_json::json!({}))
            };
            clip_of(&build(&script, &input).unwrap()).0
        };
        // ident aired last, promo before it: the 12s bumper never aired.
        assert_eq!(
            drawn(vec![aired(names[0], 600), aired(names[1], 300)]),
            fixture(names[2])
        );
        // All three aired: promo is the oldest.
        assert_eq!(
            drawn(vec![
                aired(names[0], 600),
                aired(names[1], 100),
                aired(names[2], 300),
                aired(names[1], 50),
            ]),
            fixture(names[1])
        );
    }

    #[test]
    fn archival_names_the_manifest_when_it_is_malformed() {
        for bad in [
            "{ not json",
            "{\"clips\": 3}",
            "{\"clips\": [{\"path\": \"x\"}]}",
            "{\"clips\": [{\"path\": \"x\", \"tags\": [\"countdown\"], \"burned_in_text\": true, \"minutes\": \"5\"}]}",
            "{\"clips\": [{\"path\": \"x\", \"tags\": [3], \"burned_in_text\": true}]}",
        ] {
            let (_dir, script) = archival_dir(bad);
            let msg = build(&script, &archival_input(120_000, serde_json::json!({})))
                .unwrap_err()
                .to_string();
            assert!(msg.contains("manifest archival.json"), "{bad}: {msg}");
        }
        let (dir, script) = archival_dir("{}");
        std::fs::remove_file(dir.path().join("archival.json")).unwrap();
        let msg = build(&script, &archival_input(120_000, serde_json::json!({})))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("manifest archival.json not found"), "{msg}");
    }

    #[test]
    fn local_time_reads_in_the_channels_time_zone() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(
            &dir,
            "fn hooks() { [\"bumper\"] }\nfn build(ctx) { let t = timeline(); \
             t.push(segment().black().fixed(local_time(1792267200).hour)); t }",
        );
        let mut input = input(None);
        input.channel.tz = Some(crate::tz::parse("America/New_York").unwrap());
        let built = build(&path, &input).unwrap();
        // 20:00 UTC is 16:00 EDT.
        assert_eq!(built.timeline.segments[0].duration, Duration::Fixed(16_000));
    }
}

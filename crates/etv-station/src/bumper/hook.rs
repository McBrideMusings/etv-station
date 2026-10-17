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
//!     // ctx.prev    — item map for the programme before the gap, or ()
//!     // ctx.channel — #{ name, number, logo, vocabulary }
//!     // ctx.rng     — seeded from (channel, next.entry_id, air_date)
//!     let t = timeline();
//!     t.push(segment().black().fixed(2.0));
//!     t
//! }
//! ```
//!
//! `ctx` carries no filesystem or network handle: `build` can only see what
//! the host put in it. Phase 1 offers no capability-gated field, so a bumper
//! plugin declares and is granted none ([`BUMPER_CAPABILITIES`]).
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

use rhai::{Dynamic, EvalAltResult, Map, Scope};

use super::timeline::{
    AudioSource, Duration, OverlayVars, Segment, SegmentAudio, SegmentSource, Timeline,
};

/// Capability names a bumper plugin may declare. Empty in Phase 1: `ctx`
/// has no gated field yet, so a declared capability would name a field the
/// script cannot read.
pub const BUMPER_CAPABILITIES: &[&str] = &[];

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
}

/// Everything `ctx` is built from.
#[derive(Debug, Clone)]
pub struct BumperInput {
    pub gap_ms: u64,
    /// The item after the gap, as the item map a scorer's `ctx.sets` carries
    /// ([`crate::score::load_items`]); `None` at the generation frontier.
    pub next: Option<Map>,
    pub prev: Option<Map>,
    pub channel: ChannelInfo,
    /// `YYYY-MM-DD` the gap airs on, in the station's time zone.
    pub air_date: String,
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
    let declared: Vec<String> = crate::score::declared_capabilities(script)?
        .iter()
        .map(|c| c.name().to_string())
        .collect();
    for cap in &declared {
        if !BUMPER_CAPABILITIES.contains(&cap.as_str()) {
            return Err(format!(
                "plugin {} declares capability {cap:?}, which the bumper hook does not offer \
                 (it offers: {})",
                script.display(),
                if BUMPER_CAPABILITIES.is_empty() {
                    "none".to_string()
                } else {
                    BUMPER_CAPABILITIES.join(", ")
                }
            ));
        }
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
    let engine = bumper_engine();
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

    let rng = Rng::new(rng_seed(
        &input.channel.name,
        entry_id.as_deref(),
        &input.air_date,
    ));
    let mut ctx = Map::new();
    ctx.insert("gap_ms".into(), (input.gap_ms as i64).into());
    ctx.insert("next".into(), item(&input.next));
    ctx.insert("prev".into(), item(&input.prev));
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
    let path = if source.is_string() {
        source.into_string().unwrap_or_default()
    } else if let Some(m) = source.try_cast::<Map>() {
        m.get("path")
            .and_then(|p| p.clone().into_string().ok())
            .ok_or("audio_bed(): the source map has no string `path`")?
    } else {
        return err("audio_bed(): the source must be a path string or a map with `path`");
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
        path: PathBuf::from(path),
        gain_db: num("gain_db")? as f32,
        fade_in_ms: secs_to_ms(num("fade_in")?, "audio_bed() fade_in")?,
        fade_out_ms: secs_to_ms(num("fade_out")?, "audio_bed() fade_out")?,
    })
}

/// The station's plugin engine plus the bumper builder and `ctx.rng`.
fn bumper_engine() -> rhai::Engine {
    let usage = Usage::default();
    let mut engine = crate::score::engine();

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
        .register_fn("render", |s: &mut SegmentBuilder, path: &str| {
            s.set_source(
                "render",
                SegmentSource::Render {
                    profile: String::new(),
                    path: PathBuf::from(path),
                },
            )
        })
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
            |s: &mut SegmentBuilder, _gain_db: f64| -> RhaiResult<SegmentBuilder> {
                s.usage.bump("ducked_bed");
                err("ducked_bed() is not implemented: clip audio under a bed needs an amix graph")
            },
        )
        .register_fn(
            "ducked_bed",
            |s: &mut SegmentBuilder| -> RhaiResult<SegmentBuilder> {
                s.usage.bump("ducked_bed");
                err("ducked_bed() is not implemented: clip audio under a bed needs an amix graph")
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
            },
            air_date: "2026-10-17".into(),
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
    fn ducked_bed_errors_until_implemented() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(
            &dir,
            "fn hooks() { [\"bumper\"] }\nfn build(ctx) { let t = timeline(); \
             t.push(segment().black().fixed(1).ducked_bed(-12.0)); t }",
        );
        let msg = build(&path, &input(None)).unwrap_err().to_string();
        assert!(msg.contains("ducked_bed() is not implemented"), "{msg}");
    }

    #[test]
    fn validation_requires_the_hook_and_both_way_capability_agreement() {
        let dir = tempfile::tempdir().unwrap();
        let ok = script(&dir, CARD);
        validate_script(&ok, &[]).unwrap();

        let msg = validate_script(&ok, &["catalog_read".into()]).unwrap_err();
        assert!(msg.contains("never declares"), "{msg}");

        let declares = script(
            &dir,
            "fn hooks() { [\"bumper\"] }\nfn capabilities() { [\"catalog_read\"] }\n\
             fn build(ctx) { timeline() }",
        );
        let msg = validate_script(&declares, &["catalog_read".into()]).unwrap_err();
        assert!(msg.contains("does not offer"), "{msg}");

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
}

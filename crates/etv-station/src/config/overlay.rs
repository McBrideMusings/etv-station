//! The `overlay:` key, and the station → channel → block cascade behind it
//! (#48).
//!
//! # Replace by default, append when asked
//!
//! A level either declares a *complete* overlay config, adds layers to the one
//! above it, or says nothing. There is still no field-by-field merge and no
//! layer-`id` matching — unlike `guide:`'s cascade
//! ([`crate::guide::GuideConfig`]), which merges per field, because a guide
//! config is a bag of independent strings while an overlay config is one
//! composed picture, and half of one picture over half of another is not a
//! picture.
//!
//! So the rule is four lines long:
//!
//! - no `overlay:` key at a level — carry the ancestor's resolution forward
//! - `overlay: clear` — nothing is drawn, whatever the ancestor said
//! - a `file:` reference or an inline spec — that config, and only that config
//! - `overlay: {extend: …}` — the ancestor's config with these layers appended
//!
//! Appending is the one form of composition allowed, and it is deliberately
//! not a merge: an extend can only add layers on top, name a different script,
//! or replace `config:` wholesale. It cannot reach into an inherited layer, and
//! it carries no geometry, so the four fields a running overlay process bakes
//! into its fifo (ADR 0007) can only ever come from the level that declared the
//! complete spec.
//!
//! `extend` exists because replacement alone cannot express the station's
//! actual shape: graphics every channel shares, plus one thing only the channel
//! knows (its logo, at its own size). Under replacement each channel had to
//! restate the shared half, so one graphic lived as 60 copies. See ADR 0008.
//!
//! # What `clear` means
//!
//! One thing, at every level: **nothing is drawn**. At channel level that also
//! means no overlay process and no fifo, because there is nothing for either to
//! carry. At block level the process stays up — it owns a fifo ETV-next's
//! ffmpeg is reading, and tearing that down mid-stream is what ADR 0007 exists
//! to avoid — and renders transparent frames for the block's duration. Both are
//! the same visible outcome; only the plumbing differs.
//!
//! # Where paths resolve against
//!
//! A `file:` reference, an inline spec's `script:`, and an inline layer's logo
//! `path:` are all relative to **the config file's own directory** — the
//! station YAML's for a station-level declaration, the channel YAML's for a
//! channel-level *or block-level* one. A block declaration authored in a
//! separate block file still resolves against the channel that includes it,
//! matching what a pool's `plugin:` already does: the block file is a fragment
//! spliced into a channel, not a root of its own.

use std::path::{Path, PathBuf};

use etv_overlay::overlay_spec::OverlaySpec;
use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

/// One level's `overlay:` value.
///
/// Untagged by shape rather than by a `kind:` discriminator: `file` is not a
/// field [`OverlaySpec`] has, and `width`/`height`/`framerate` are required on
/// it, so the two map forms cannot be confused for each other and an author
/// writes no word that carries no information.
#[derive(Debug, Clone, PartialEq)]
pub enum OverlayDecl {
    /// `overlay: clear` — this level draws nothing.
    Clear,
    /// `overlay: {file: "overlay.yaml"}` — reuse a spec file by path, the same
    /// path-reuse idiom a pool's `plugin:` uses.
    File(PathBuf),
    /// `overlay: {width: …, height: …, layers: [...]}` — the spec written out
    /// where it is used, for a config small enough that a second file would be
    /// the bigger cost.
    Inline(Box<OverlaySpec>),
    /// `overlay: {extend: {layers: [...]}}` — keep whatever the level above
    /// resolved to and add to it, rather than restating it.
    ///
    /// This exists because replacement alone cannot express the one shape the
    /// station actually has: graphics every channel shares (a Now/Next snipe,
    /// its scrim) plus one thing only the channel knows (its own logo, at its
    /// own size). Under replacement a channel wanting its own bug had to
    /// restate the shared half too, so the same block was copied into 60
    /// channel files and every edit to it meant editing 60 files.
    ///
    /// An extend carries no geometry, so it cannot reach the four fields a
    /// running overlay process bakes into its fifo (ADR 0007) — the geometry
    /// check below has nothing new to catch.
    Extend(Box<OverlayExtend>),
}

/// The additive half of a cascade level: what an [`OverlayDecl::Extend`] adds
/// to the spec it inherits.
///
/// Layers append rather than merge by index. Index-merging would make a
/// channel's file depend on the *order* of layers in a file it does not own,
/// so inserting a layer at the station level would silently re-target every
/// channel's override.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayExtend {
    /// Appended after the inherited layers, so they draw on top.
    #[serde(default)]
    pub layers: Vec<etv_overlay::overlay_spec::OverlayKind>,
    /// Replaces the inherited script when set. A level adding layers a script
    /// has to drive needs a way to name that script; absent keeps the parent's.
    #[serde(default)]
    pub script: Option<PathBuf>,
    /// Replaces the inherited `config:` wholesale when set — not a per-key
    /// merge, matching how the rest of the cascade treats a value it owns.
    #[serde(
        default,
        deserialize_with = "etv_overlay::config_carrier::deserialize_config",
        skip_serializing_if = "Option::is_none"
    )]
    pub config: Option<serde_json::Value>,
}

/// The wire shape [`OverlayDecl`] is read through. Separate from the public
/// enum so `Clear` can be the bare string `clear` while the other two are maps,
/// which a derived untagged enum cannot express on a unit variant directly.
#[derive(Deserialize)]
#[serde(untagged)]
enum DeclRepr {
    Keyword(Keyword),
    File { file: PathBuf },
    // Before `Inline`: an extend is a map, and `OverlaySpec`'s required
    // width/height/framerate are what stop the two from matching each other,
    // but only if the narrower shape is tried first.
    Extend { extend: Box<OverlayExtend> },
    Inline(Box<OverlaySpec>),
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Keyword {
    Clear,
}

impl<'de> Deserialize<'de> for OverlayDecl {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Untagged's own error is "data did not match any variant", which names
        // none of the three shapes an author could have meant. Replacing it
        // costs one match arm and is the difference between a usable error and
        // a hunt through the file.
        DeclRepr::deserialize(deserializer)
            .map(|repr| match repr {
                DeclRepr::Keyword(Keyword::Clear) => OverlayDecl::Clear,
                DeclRepr::File { file } => OverlayDecl::File(file),
                DeclRepr::Extend { extend } => OverlayDecl::Extend(extend),
                DeclRepr::Inline(spec) => OverlayDecl::Inline(spec),
            })
            .map_err(|_| {
                de::Error::custom(
                    "`overlay:` must be `clear`, a `{file: <path>}` reference, an \
                     `{extend: {layers: [...]}}` addition to the level above, or an inline \
                     spec with at least `width`, `height` and `framerate`",
                )
            })
    }
}

impl Serialize for OverlayDecl {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            OverlayDecl::Clear => serializer.serialize_str("clear"),
            OverlayDecl::File(path) => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("file", path)?;
                map.end()
            }
            OverlayDecl::Inline(spec) => spec.serialize(serializer),
            OverlayDecl::Extend(extend) => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("extend", extend)?;
                map.end()
            }
        }
    }
}

/// One cascade level: a declaration, paired with the directory its paths
/// resolve against. The pair travels together because the winner and the base
/// directory come from the same file, and a station-level declaration surviving
/// into a channel must still resolve its paths against the station's directory.
pub type Level<'a> = (&'a OverlayDecl, &'a Path);

/// Which level's declaration governs, walking station → channel → block.
///
/// Returns the winning declaration with its base directory and nothing about
/// which level it came from, because nothing downstream needs to know: the
/// resolution IS the answer. A `None` return means nothing is drawn — either no
/// level declared an overlay, or the deepest one that spoke said `clear`.
pub fn resolve_decl<'a>(
    station: Option<Level<'a>>,
    channel: Option<Level<'a>>,
    block: Option<Level<'a>>,
) -> Vec<Level<'a>> {
    let mut chain: Vec<Level<'a>> = Vec::new();
    for level in [station, channel, block] {
        match level {
            // Silence carries the ancestors' resolution forward untouched.
            None => {}
            // `clear` means nothing is drawn, so it drops the whole chain and
            // not just the level above — a deeper `extend` after it then has
            // nothing to extend, which `load_chain` reports.
            Some((OverlayDecl::Clear, _)) => chain.clear(),
            // A complete spec replaces everything before it, exactly as it did
            // before extend existed.
            Some(level @ (OverlayDecl::File(_) | OverlayDecl::Inline(_), _)) => {
                chain.clear();
                chain.push(level);
            }
            // The only additive case: keep what is there and stack on top.
            Some(level @ (OverlayDecl::Extend(_), _)) => chain.push(level),
        }
    }
    chain
}

/// Fold a [`resolve_decl`] chain into one spec, loading each level against its
/// own base directory.
///
/// Each level's paths are re-rooted before its layers are appended, which is
/// the whole reason a level travels with its directory: a station-level layer
/// naming `shared/logo.png` and a channel-level layer naming `logo.png` are
/// both correct, relative to different files, and end up in one spec.
pub fn load_chain(chain: &[Level<'_>]) -> Result<Option<OverlaySpec>, String> {
    let mut resolved: Option<OverlaySpec> = None;
    for (decl, dir) in chain {
        match decl {
            OverlayDecl::Clear => {
                unreachable!("`clear` empties the chain; see `resolve_decl`")
            }
            OverlayDecl::File(_) | OverlayDecl::Inline(_) => {
                resolved = Some(load_decl(decl, dir)?);
            }
            OverlayDecl::Extend(extend) => {
                let Some(base) = resolved.as_mut() else {
                    return Err(
                        "`overlay: {extend: …}` has nothing to extend — no level above this one \
                         declared a complete overlay config (or the nearest one said `clear`). \
                         Declare the base config at the station or channel level, or write this \
                         level as a full spec with `width`, `height` and `framerate`."
                            .to_string(),
                    );
                };
                // Re-root this level's own paths, then append. `with_paths_relative_to`
                // works on a whole spec, so the layers ride through a scratch
                // one rather than growing a second path-rewriting routine that
                // could disagree with it.
                let scratch = OverlaySpec {
                    script: extend.script.clone(),
                    layers: extend.layers.clone(),
                    ..base.clone()
                }
                .with_paths_relative_to(Some(dir));
                base.layers.extend(scratch.layers);
                if extend.script.is_some() {
                    base.script = scratch.script;
                }
                if extend.config.is_some() {
                    base.config = extend.config.clone();
                }
            }
        }
    }
    Ok(resolved)
}

/// Load a declaration into a spec with every path made absolute against
/// `base_dir` — the directory of the config file the declaration was authored
/// in (see the module docs).
///
/// [`OverlayDecl::Clear`] has no spec to load and is not a valid input here;
/// [`resolve_decl`] has already turned it into `None`.
///
/// The error is a bare message rather than a typed one because both callers —
/// config validation and the daemon's startup parse — already wrap it with the
/// channel and path they were working on, and a second copy of that context
/// inside the message reads as a stutter.
pub fn load_decl(decl: &OverlayDecl, base_dir: &Path) -> Result<OverlaySpec, String> {
    match decl {
        OverlayDecl::Clear => unreachable!("`clear` resolves to no spec; see `resolve_decl`"),
        OverlayDecl::Extend(_) => {
            unreachable!("an extend is folded onto its base by `load_chain`, never loaded alone")
        }
        OverlayDecl::File(path) => {
            let full = if path.is_absolute() {
                path.clone()
            } else {
                base_dir.join(path)
            };
            OverlaySpec::from_path(&full).map_err(|e| e.to_string())
        }
        // An inline spec was parsed with the channel file, so nothing has
        // re-rooted its `script:` and logo paths yet — `from_path` does that
        // for a file reference, and this is the other half of the same job.
        OverlayDecl::Inline(spec) => {
            Ok(spec.as_ref().clone().with_paths_relative_to(Some(base_dir)))
        }
    }
}

/// Everything one channel's overlay cascade resolves to, loaded and with every
/// path absolute — computed once at config load, because a cascade evaluated
/// per generation is a cascade that can disagree with itself between two.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ChannelOverlays {
    /// The station → channel resolution: the channel's **spawn config**. It
    /// sizes the canvas and the fifo, and its geometry is what reaches
    /// ETV-next in `channelN.json`. `None` means this channel has no overlay
    /// process at all.
    pub base: Option<OverlaySpec>,
    /// The station → channel → block resolution, one entry per
    /// `rule.blocks` index in order. An entry is `None` when nothing is drawn
    /// for that block.
    pub blocks: Vec<Option<OverlaySpec>>,
}

impl ChannelOverlays {
    /// Whether any block draws something the channel's own resolution does
    /// not — i.e. whether the running process ever needs a config swap. A
    /// channel where this is false can skip writing an overlay timeline
    /// entirely; the spawn config already says everything.
    pub fn varies_by_block(&self) -> bool {
        self.blocks.iter().any(|b| b.as_ref() != self.base.as_ref())
    }
}

/// Resolve one channel's whole overlay cascade, loading each winning
/// declaration and checking the one thing a running process cannot absorb.
///
/// The geometry check is here, at load, rather than at the swap: `width`,
/// `height`, `framerate` and `pixel_format` are baked into the fifo's byte
/// layout and into the `-video_size`/`-framerate` arguments ETV-next's ffmpeg
/// was started with, so a block that disagrees about them can never be applied
/// (ADR 0007). Catching it while reading the file turns a config mistake into a
/// startup error naming the block, instead of a log line at 3am and a block
/// that silently keeps the previous overlay.
pub fn resolve_channel(
    station: Option<&OverlayDecl>,
    station_dir: &Path,
    channel: &super::channel::ChannelConfig,
    channel_dir: &Path,
) -> Result<ChannelOverlays, String> {
    let station_level = station.map(|d| (d, station_dir));
    let channel_level = channel.overlay.as_ref().map(|d| (d, channel_dir));

    let base = load_chain(&resolve_decl(station_level, channel_level, None))?;

    let mut blocks = Vec::with_capacity(channel.rule.blocks.len());
    for (idx, include) in channel.rule.blocks.iter().enumerate() {
        let block_level = include.overlay.as_ref().map(|d| (d, channel_dir));
        let resolved = load_chain(&resolve_decl(station_level, channel_level, block_level))
            .map_err(|e| format!("block #{idx}: overlay: {e}"))?;

        if let Some(spec) = resolved.as_ref() {
            let Some(base) = base.as_ref() else {
                // Nothing to swap the config into: the fifo and the subprocess
                // both come from the channel-level resolution, and there is no
                // sensible geometry to invent for a channel that never asked
                // for an overlay.
                return Err(format!(
                    "block #{idx} declares an `overlay:` but the channel resolves to none. \
                     A block-level overlay swaps the config of the channel's running overlay \
                     process, so the channel (or the station) has to declare one for it to \
                     swap into — declare the base config there, or drop this key."
                ));
            };
            if spec.geometry() != base.geometry() {
                return Err(format!(
                    "block #{idx}'s overlay is {}, but the channel's is {}. A block-level \
                     overlay is loaded into the channel's already-running overlay process, \
                     whose canvas, fifo and ffmpeg arguments were all sized at spawn — so \
                     width, height, framerate and pixel_format have to match the channel's. \
                     Only the script and layers may differ per block.",
                    spec.geometry(),
                    base.geometry(),
                ));
            }
        }
        blocks.push(resolved);
    }

    Ok(ChannelOverlays { base, blocks })
}

#[cfg(test)]
mod tests {
    use super::*;
    use etv_overlay::overlay_spec::{Corner, OverlayKind, PixelFormat};

    fn decl(yaml: &str) -> OverlayDecl {
        #[derive(Deserialize)]
        struct Holder {
            overlay: OverlayDecl,
        }
        serde_norway::from_str::<Holder>(yaml).unwrap().overlay
    }

    fn spec(width: u32) -> OverlaySpec {
        OverlaySpec {
            width,
            height: 720,
            framerate: 30,
            pixel_format: PixelFormat::Rgba8,
            script: None,
            config: None,
            layers: vec![],
        }
    }

    #[test]
    fn the_bare_word_clear_parses_as_clear() {
        assert_eq!(decl("overlay: clear\n"), OverlayDecl::Clear);
    }

    #[test]
    fn a_file_key_parses_as_a_reference() {
        assert_eq!(
            decl("overlay:\n  file: ../../shared/pierce-overlay.yaml\n"),
            OverlayDecl::File(PathBuf::from("../../shared/pierce-overlay.yaml")),
        );
    }

    /// The inline form is the same shape an `overlay.yaml` file holds — that is
    /// what makes moving a config between the two a copy-paste rather than a
    /// translation.
    #[test]
    fn an_inline_spec_parses_as_a_spec() {
        let parsed = decl(
            "overlay:\n  width: 1280\n  height: 720\n  framerate: 30\n  layers:\n    - type: logo\n      path: logo.png\n      corner: bottom_right\n      height: 56\n",
        );
        let OverlayDecl::Inline(spec) = parsed else {
            panic!("expected an inline spec, got {parsed:?}");
        };
        assert_eq!(spec.width, 1280);
        assert_eq!(spec.layers.len(), 1);
        assert!(matches!(
            spec.layers[0],
            OverlayKind::Logo {
                corner: Corner::BottomRight,
                height: 56,
                ..
            }
        ));
    }

    /// The three shapes are distinguishable, so a wrong one gets an error that
    /// names all three rather than serde's "data did not match any variant".
    #[test]
    fn an_unrecognised_shape_names_what_the_key_accepts() {
        #[derive(Debug, Deserialize)]
        struct Holder {
            #[allow(dead_code)]
            overlay: OverlayDecl,
        }
        let err = serde_norway::from_str::<Holder>("overlay: disabled\n")
            .expect_err("`disabled` is not one of the three forms")
            .to_string();
        assert!(err.contains("clear"), "{err}");
        assert!(err.contains("file"), "{err}");
    }

    // ---- the cascade -------------------------------------------------------

    /// The station and channel directories differ in every real deployment, so
    /// the tests below distinguish them: a station-level declaration winning
    /// must carry the station's directory forward, not the channel's.
    const STATION_DIR: &str = "/etc/etv";
    const CHANNEL_DIR: &str = "/etc/etv/channels/085-hbo";

    fn at<'a>(decl: &'a OverlayDecl, dir: &'a str) -> Option<Level<'a>> {
        Some((decl, Path::new(dir)))
    }

    /// `resolve_decl` returns the chain to fold, so a test asserting "this one
    /// level governs" is asserting a one-element chain.
    fn only<'a>(decl: &'a OverlayDecl, dir: &'a str) -> Vec<Level<'a>> {
        vec![(decl, Path::new(dir))]
    }

    #[test]
    fn nothing_declared_anywhere_draws_nothing() {
        assert!(resolve_decl(None, None, None).is_empty());
    }

    #[test]
    fn a_level_that_says_nothing_carries_its_ancestor_forward() {
        let station = OverlayDecl::File("station.yaml".into());
        assert_eq!(
            resolve_decl(at(&station, STATION_DIR), None, None),
            only(&station, STATION_DIR),
            "a channel and block that say nothing inherit the station's, \
             including the directory its paths resolve against"
        );
    }

    /// The whole point of "deepest wins outright": a block's declaration is the
    /// config, not a patch over the channel's.
    #[test]
    fn the_deepest_declaration_replaces_the_whole_config() {
        let station = OverlayDecl::File("station.yaml".into());
        let channel = OverlayDecl::File("channel.yaml".into());
        let block = OverlayDecl::Inline(Box::new(spec(1920)));
        assert_eq!(
            resolve_decl(
                at(&station, STATION_DIR),
                at(&channel, CHANNEL_DIR),
                at(&block, CHANNEL_DIR),
            ),
            only(&block, CHANNEL_DIR),
        );
        assert_eq!(
            resolve_decl(at(&station, STATION_DIR), at(&channel, CHANNEL_DIR), None),
            only(&channel, CHANNEL_DIR),
        );
    }

    #[test]
    fn clear_at_any_level_beats_every_ancestor() {
        let station = OverlayDecl::File("station.yaml".into());
        let channel = OverlayDecl::File("channel.yaml".into());
        assert!(
            resolve_decl(
                at(&station, STATION_DIR),
                at(&OverlayDecl::Clear, CHANNEL_DIR),
                None,
            )
            .is_empty()
        );
        assert!(
            resolve_decl(
                at(&station, STATION_DIR),
                at(&channel, CHANNEL_DIR),
                at(&OverlayDecl::Clear, CHANNEL_DIR),
            )
            .is_empty()
        );
    }

    /// `clear` is not permanent — it is one level's answer, and a deeper level
    /// still gets to give its own.
    #[test]
    fn a_block_can_declare_an_overlay_under_a_cleared_channel() {
        let block = OverlayDecl::Inline(Box::new(spec(1280)));
        assert_eq!(
            resolve_decl(
                at(&OverlayDecl::File("station.yaml".into()), STATION_DIR),
                at(&OverlayDecl::Clear, CHANNEL_DIR),
                at(&block, CHANNEL_DIR),
            ),
            only(&block, CHANNEL_DIR),
        );
    }

    /// The shape the whole feature exists for: shared graphics declared once at
    /// the station, one channel-specific layer added per channel, both drawn.
    #[test]
    fn an_extend_stacks_onto_its_ancestor_instead_of_replacing_it() {
        // A station layer to inherit, so the assertion below distinguishes
        // "appended" from "replaced" — `spec()` alone carries none.
        let station = OverlayDecl::Inline(Box::new(OverlaySpec {
            layers: vec![OverlayKind::Watermark {
                corner: Corner::TopLeft,
                margin: 8,
                box_size: 16,
                color: [1, 2, 3, 4],
            }],
            ..spec(1280)
        }));
        let channel = decl(
            "overlay:\n  extend:\n    layers:\n      - type: logo\n        path: logo.png\n        corner: bottom_right\n",
        );
        let chain = resolve_decl(at(&station, STATION_DIR), at(&channel, CHANNEL_DIR), None);
        assert_eq!(
            chain.len(),
            2,
            "both levels contribute, neither is discarded"
        );

        let resolved = load_chain(&chain).unwrap().unwrap();
        assert_eq!(resolved.layers.len(), 2, "the station's layer survives");
        assert!(matches!(resolved.layers[0], OverlayKind::Watermark { .. }));
        match &resolved.layers[1] {
            // Appended last so it draws on top, and re-rooted against the
            // channel's own directory rather than the station's.
            OverlayKind::Logo { path, .. } => {
                assert_eq!(path, Path::new(CHANNEL_DIR).join("logo.png").as_path())
            }
            other => panic!("expected the channel's logo, got {other:?}"),
        }
    }

    /// An extend is a patch, so it needs something to patch. Reaching the
    /// renderer with no geometry would be a crash at spawn; this is the error
    /// that stops it at config load instead.
    #[test]
    fn an_extend_with_no_ancestor_is_a_load_error() {
        let channel = decl("overlay:\n  extend:\n    layers: []\n");
        let chain = resolve_decl(None, at(&channel, CHANNEL_DIR), None);
        let err = load_chain(&chain).unwrap_err();
        assert!(
            err.contains("nothing to extend"),
            "error should name the actual problem, got: {err}"
        );
    }

    /// `clear` drops the chain rather than just the level above it, so a
    /// channel that cleared and a block that extends do not silently resurrect
    /// the station's config.
    #[test]
    fn an_extend_under_a_cleared_ancestor_has_nothing_to_extend() {
        let station = OverlayDecl::Inline(Box::new(spec(1280)));
        let block = decl("overlay:\n  extend:\n    layers: []\n");
        let chain = resolve_decl(
            at(&station, STATION_DIR),
            at(&OverlayDecl::Clear, CHANNEL_DIR),
            at(&block, CHANNEL_DIR),
        );
        assert!(load_chain(&chain).is_err());
    }

    /// A level adding layers a script has to drive needs to be able to name
    /// that script; a level that says nothing about it keeps its ancestor's.
    #[test]
    fn an_extend_replaces_the_script_only_when_it_names_one() {
        let station = OverlayDecl::Inline(Box::new(OverlaySpec {
            script: Some("station.rhai".into()),
            ..spec(1280)
        }));

        let silent = decl("overlay:\n  extend:\n    layers: []\n");
        let kept = load_chain(&resolve_decl(
            at(&station, STATION_DIR),
            at(&silent, CHANNEL_DIR),
            None,
        ))
        .unwrap()
        .unwrap();
        assert_eq!(
            kept.script,
            Some(Path::new(STATION_DIR).join("station.rhai")),
            "an extend that names no script keeps the station's, still rooted at the station"
        );

        let overriding = decl("overlay:\n  extend:\n    script: channel.rhai\n    layers: []\n");
        let replaced = load_chain(&resolve_decl(
            at(&station, STATION_DIR),
            at(&overriding, CHANNEL_DIR),
            None,
        ))
        .unwrap()
        .unwrap();
        assert_eq!(
            replaced.script,
            Some(Path::new(CHANNEL_DIR).join("channel.rhai")),
            "an extend naming a script wins, rooted at its own directory"
        );
    }

    /// An extend cannot carry geometry, so the four fields a running overlay
    /// process bakes into its fifo (ADR 0007) survive the fold untouched.
    #[test]
    fn an_extend_cannot_change_geometry() {
        let station = OverlayDecl::Inline(Box::new(spec(1280)));
        let channel = decl("overlay:\n  extend:\n    layers: []\n");
        let resolved = load_chain(&resolve_decl(
            at(&station, STATION_DIR),
            at(&channel, CHANNEL_DIR),
            None,
        ))
        .unwrap()
        .unwrap();
        assert_eq!(resolved.geometry(), spec(1280).geometry());
    }

    // ---- loading -----------------------------------------------------------

    #[test]
    fn a_file_reference_resolves_against_the_config_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("shared")).unwrap();
        std::fs::write(
            dir.path().join("shared/mark.yaml"),
            "width: 1280\nheight: 720\nframerate: 30\nlayers:\n  - type: logo\n    path: logo.png\n    corner: bottom_right\n",
        )
        .unwrap();

        let loaded = load_decl(&OverlayDecl::File("shared/mark.yaml".into()), dir.path()).unwrap();

        // The logo path is relative to the SPEC file's own directory, which is
        // `shared/`, not the directory the reference was written in.
        let OverlayKind::Logo { path, .. } = &loaded.layers[0] else {
            panic!("expected a logo layer");
        };
        assert_eq!(path.as_path(), dir.path().join("shared/logo.png"));
    }

    /// An inline spec has no file of its own, so its paths hang off the config
    /// that carries it — which is the only reading that lets an author move a
    /// three-line overlay into their channel YAML without moving the artwork.
    #[test]
    fn an_inline_specs_paths_resolve_against_the_carrying_config() {
        let mut inline = spec(1280);
        inline.layers = vec![OverlayKind::Logo {
            path: "logo.png".into(),
            corner: Corner::BottomRight,
            margin: 24,
            height: 56,
        }];
        let loaded = load_decl(
            &OverlayDecl::Inline(Box::new(inline)),
            Path::new("/etc/etv/channels/085-hbo"),
        )
        .unwrap();
        let OverlayKind::Logo { path, .. } = &loaded.layers[0] else {
            panic!("expected a logo layer");
        };
        assert_eq!(path, Path::new("/etc/etv/channels/085-hbo/logo.png"));
    }
}

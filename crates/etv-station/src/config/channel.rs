use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::overlay::OverlayDecl;
use super::pool::ShowGroup;
use super::rule::RuleConfig;
use crate::guide::GuideConfig;
use crate::tautulli::HistoryScope;

/// The per-channel subtitle settings ETV-next reads from `channelN.json`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ChannelSubtitle {
    /// Announce this channel's subtitle rendition as `DEFAULT=YES`, so players
    /// start with subtitles showing. The viewer can still switch them off.
    /// `None` leaves the station's `normalization.subtitle.default_on`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_on: Option<bool>,
}

/// A channel's `annotate:` entry (ADR 0017): the script and the capabilities
/// the channel grants it. Same two keys a pool uses (`plugin:`,
/// `capabilities:`), checked the same way at load (ADR 0015): the grant must
/// equal what the script's `capabilities()` declares.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AnnotateConfig {
    /// The script, relative to the channel config's directory.
    pub plugin: PathBuf,
    /// Capabilities this channel grants the script; `http` is the only one
    /// the annotate hook offers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
}

/// A channel's `guide:` block: the text defaults every item inherits, plus the
/// one setting that only exists at channel level.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ChannelGuide {
    #[serde(flatten)]
    pub text: GuideConfig,
    /// List bumper items as their own guide rows instead of folding each into
    /// the programme before it. `None` leaves the station's
    /// `guide.show_bumpers`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_bumpers: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ChannelConfig {
    /// This channel's dial number (#263) — required, and no longer derived
    /// from the channel's position in the sorted folder list. That derivation
    /// was the bug: adding, renaming, or removing any channel renumbered
    /// every channel after it, silently invalidating every saved favourite,
    /// remembered number, and Plex's own cached lineup.
    ///
    /// A channel whose config omits this fails to load; two channels
    /// declaring the same number both fail to load. Neither check can live on
    /// this type alone (a collision is a cross-channel property) — see
    /// `config::load`. Numbers need not be contiguous: gaps are legal and not
    /// warned about.
    pub number: i64,

    /// This channel's identity (#414) — required, and never derived from the
    /// config's file or folder name. It is the key every `airings` row and
    /// every series resume cursor in `history.db` is filed under, the salt on
    /// an inherited station seed, the log label, the overlay handshake name,
    /// and the output folder leaf under the station's `output_base`. Deriving
    /// it from the folder was the bug: renaming a channel's directory silently
    /// orphaned its whole play history.
    ///
    /// A channel whose config omits this fails to load; two channels
    /// declaring the same name both fail to load — see `config::load`. Must
    /// not be empty or contain path separators or control characters.
    pub name: String,

    /// The name shown to viewers — in the XMLTV guide's `<display-name>` and
    /// the lineup's channel name — as distinct from [`name`](Self::name),
    /// which drives the identity (log label, output folder, overlay
    /// handshake). `None` falls back to the identity, same as before this
    /// field existed. Replaces `presentation.json`'s per-channel `name` key
    /// (#158 decision #5): that file lived outside the channel config and
    /// outside git, which is why a channel's guide-facing name could only be
    /// changed by hand-editing a file nobody would think to look in. There is
    /// no dual support — `presentation.json` is no longer read at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,

    /// Channel-level `guide:` defaults (#158) — the least specific of the
    /// three cascade levels (channel → block → item, most specific wins).
    /// `None` (no `guide:` block at all) is not "no guide text": every item
    /// still gets the series-title convention and genre-tag categories for
    /// free, with no config needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guide: Option<ChannelGuide>,

    /// Channel-level track-selection rules (#261), under the station's and
    /// over nothing: blocks, pools and items refine them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracks: Option<crate::config::TrackRules>,

    /// This channel's override of ETV-next's `normalization.subtitle`
    /// settings, written into its `channelN.json` (#290). Only the settings
    /// that belong to one channel appear here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<ChannelSubtitle>,

    /// How far ahead of live the schedule is materialized. This is also what
    /// bounds a single generation, whichever shape the channel is: a pattern
    /// whose pools would take years to drain stops at the visit boundary that
    /// covers this span (#140), and a flat `entries` list longer than the span
    /// stops at the item that covers it and resumes there next time (#118). So
    /// an edit to the channel file shows up on air within roughly this long.
    #[serde(default = "default_window_days")]
    pub window_days: u32,

    #[serde(default = "default_chunk_hours")]
    pub chunk_hours: u32,

    #[serde(default = "default_roll_interval", with = "humantime_serde")]
    pub roll_interval: Duration,

    #[serde(default = "default_retention_days")]
    pub retention_days: u32,

    /// The seed every random choice on this channel is computed from — a
    /// fixed number, or one that rotates on a cadence (see
    /// [`crate::config::variety`]). Unset inherits the station's, salted with
    /// this channel's `name`; unset at both means
    /// [`crate::variety::default_seed`]. `config::load` writes the effective
    /// value back here, so after load this is the only field to read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<crate::config::SeedSpec>,

    /// How far a plugin's draw may stray from its own ranking, handed to the
    /// script as `ctx.temperature` — fixed, or rotating like `seed`. Unset
    /// inherits the station's; unset at both is 0. `config::load` writes the
    /// effective value back here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<crate::config::TemperatureSpec>,

    /// An instant this channel is treated as having been broadcasting since.
    /// Only affects the **first** generation of a channel with nothing written
    /// yet: instead of starting at item 0, it joins its list where elapsed time
    /// since the anchor says it should be, so a newly-added channel feels like
    /// it has been running all along rather than beginning at the top.
    ///
    /// After that first generation the written timeline carries the phase, and
    /// the anchor is not consulted again — it is a starting position, not a
    /// repeating phase origin. Unset means start at the top.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub anchor: Option<time::OffsetDateTime>,

    /// Tuning for pools that draw from a scorer plugin (#74). Absent on every
    /// channel that uses none, which is why it is optional rather than a
    /// defaulted struct — an unused knob in a config file invites tuning that
    /// does nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scoring: Option<ScoringConfig>,

    pub rule: RuleConfig,

    /// A plugin that attaches metadata to every item on this channel's
    /// already-built schedule, without selecting or ordering anything (ADR
    /// 0017). Sibling to [`rule`](Self::rule) rather than nested inside it —
    /// annotation runs once per item regardless of which block or pool put
    /// that item there. Channel-scoped only: there is no pool-level or
    /// block-level equivalent, since the schedule it walks does not exist
    /// until the whole channel has resolved.
    ///
    /// At config load, the named script must declare the `annotate` hook
    /// (`hooks()`, the same declaration `pool_provider`/`sequencer` already
    /// use) — a script that doesn't fails load with a named error — and the
    /// entry's `capabilities` must equal the script's declared set. `None`
    /// (the default) means every item keeps whatever metadata its source and
    /// blocks already gave it, unchanged from before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotate: Option<AnnotateConfig>,

    /// Event hooks (ADR 0021) scoped to this channel; same shape as the
    /// station's `hooks:`. A relative `run` resolves against this file's
    /// directory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<crate::config::HookDecl>,

    /// Named show groups (#165), declared once and referenced by name from
    /// any pool's `groups:` field — see [`ShowGroup`]. Empty on every channel
    /// written before this, which is what an absent `groups:` key means.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<ShowGroup>,

    /// This channel's live overlay — the middle of the three cascade levels
    /// (station → channel → block, deepest wins outright; see
    /// [`crate::config::OverlayDecl`]).
    ///
    /// Whatever this resolves to with the station's is the channel's **spawn
    /// config**: it sizes the canvas and the fifo, the station daemon
    /// supervises an `etv-overlay` subprocess against it, and the rendered
    /// `channelN.json` carries its geometry so ETV-next's ffmpeg reads frames
    /// of the right shape. A block-level declaration then swaps script and
    /// layers inside that running process, which is why it may not disagree
    /// about geometry (#48, ADR 0007).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay: Option<OverlayDecl>,
}

impl ChannelConfig {
    /// Whether any block interleaves pools via a pattern (#72).
    pub fn is_pattern(&self) -> bool {
        self.rule.all_blocks().any(|b| b.is_pattern())
    }

    /// Whether any pool on this channel draws its items from a scorer plugin
    /// (#74) rather than from a CEL expression.
    ///
    /// A `plugin:` is the only path by which anything reads the server's watch
    /// history: the station hands it to a script as `ctx.history` and nothing
    /// else consumes it. So a station of channels that all answer `false` here
    /// has no reader for a watch history at all, and the daemon skips fetching
    /// one (#131).
    pub fn uses_scorer_plugin(&self) -> bool {
        self.rule
            .all_blocks()
            .any(|b| b.pools.iter().any(|p| p.plugin.is_some()))
    }

    /// Whether this channel shows a "why this was picked" line in the guide
    /// (`scoring.explain`).
    pub fn explains_why(&self) -> bool {
        self.scoring.as_ref().is_some_and(|s| s.explain)
    }

    /// Whose watch history this channel ranks against (#112).
    ///
    /// Also the key its history is cached under for the tick, so two channels
    /// naming the same person share one `get_history` call and one catalog join
    /// rather than each paying for their own — the same sharing #126 gave the
    /// server-wide history, now per distinct scope instead of per station.
    ///
    /// A channel with no `scoring:` block at all is [`HistoryScope::AllUsers`]:
    /// saying nothing has always meant the server-wide pool, and #112 does not
    /// change what an existing config means.
    pub fn history_scope(&self) -> HistoryScope {
        match self.scoring.as_ref() {
            Some(s) => match (s.taste_scope, s.user.as_deref()) {
                // Trimmed, because the surrounding whitespace would otherwise be
                // sent as part of the name: `user: " carol "` reaches Tautulli
                // as `user=+carol+`, matches no account, and returns an empty
                // history that is indistinguishable from a quiet week.
                // `validate_taste_scope` only rejects an *entirely* blank value,
                // so trimming has to happen here too.
                (TasteScope::SingleUser, Some(u)) => HistoryScope::User(u.trim().to_string()),
                // `single_user` with no `user` is rejected by `config::validate`,
                // so this arm is unreachable through a loaded config. Falling
                // back to the server-wide pool rather than panicking keeps a
                // hand-built `ScoringConfig` in a test from being a crash.
                _ => HistoryScope::AllUsers,
            },
            None => HistoryScope::AllUsers,
        }
    }

    /// How far back this channel's widest adjacency constraint reaches, in
    /// positions — how much aired history the constraint pass needs to enforce
    /// it across a generation seam (#73). `0` when nothing is constrained.
    ///
    /// Derived rather than a fixed cap: a constraint the config declares has to
    /// hold at the seam too, and truncating the history to some constant would
    /// make a wide `separate_min_gap` hold inside a generation and quietly lapse
    /// between two.
    pub fn adjacency_reach(&self) -> usize {
        // The channel-wide rule already counts channel positions, so it needs
        // no conversion — only the widest of it and every block's own reach.
        let channel = self.rule.no_repeat_within.map_or(0, |gap| gap.reach());
        self.rule
            .all_blocks()
            .map(|b| b.adjacency_reach())
            .max()
            .unwrap_or(0)
            .max(channel)
    }
}

/// What a scorer plugin is told about, per channel.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScoringConfig {
    /// How many recently-aired entries the plugin sees in `ctx.recent`. This is
    /// the window a script suppresses repeats over, so it belongs to the
    /// channel's taste, not to the daemon: a channel with a deep library wants
    /// a long memory, a narrow one would starve on the same setting.
    #[serde(default = "default_recent_depth")]
    pub recent_depth: usize,

    /// Nominal seconds per item: how long an item on this channel typically
    /// runs. A channel of half-hour episodes and one of three-hour films need
    /// very different numbers.
    ///
    /// Two readers, the same question asked twice. It turns a generation's span
    /// into the `ctx.target_count` hint a scorer plugin is given, and it is the
    /// stand-in length used to cut a flat `entries` list at the window (#118)
    /// for an item nothing has measured yet — an authored file with no in/out
    /// points and no catalog row behind it. Every item that *is* measured
    /// counts at its real length, and an unmeasured one falls back to the mean
    /// of the measured ones first, so this only bites a channel where nothing
    /// at all is known.
    #[serde(default = "default_nominal_item_secs")]
    pub nominal_item_secs: u32,

    /// Whose watch history this channel's scorer ranks against (#112).
    ///
    /// Defaults to [`TasteScope::AllUsers`], the server-wide aggregate #74
    /// shipped: every Tautulli user's recent history pooled with no user
    /// dimension. `single_user` narrows it to the one account named in
    /// [`user`](Self::user).
    #[serde(default)]
    pub taste_scope: TasteScope,

    /// The one account `taste_scope: single_user` ranks against — a Tautulli
    /// username (`"bob"`) or a numeric user id (`"1234567"`). Which one it is
    /// is inferred, not configured: a value that parses as an integer is sent as
    /// `user_id`, anything else as `user`.
    ///
    /// Required when `taste_scope` is `single_user` and rejected otherwise; both
    /// halves are enforced in `config::validate` so a typo fails at load rather
    /// than silently ranking against the whole server.
    ///
    /// `single_user` also requires `PLEX_URL` and `PLEX_TOKEN` on the daemon,
    /// independently of how the catalog is ingested and of any scorer plugin's
    /// `plexdb_reader` datastore: Tautulli's user id must be translated to a
    /// Plex account id, and only Plex can confirm that mapping (the two ids
    /// differ for the server owner and nobody else). `config::validate`
    /// rejects the channel at load when either variable is missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,

    /// Show a one-line "why this was picked" explanation in the guide, built
    /// from each item's own `metadata.audit` trail (ADR 0011).
    ///
    /// Off by default and opt-in: a
    /// plugin's `guide` sentence or a built-in stage's `verdict` is written
    /// for a different reader (an operator running `admin audit`, or the
    /// plugin author), so surfacing it to every viewer is a call the person
    /// running the server makes per channel, in writing.
    #[serde(default)]
    pub explain: bool,
}

/// Whose watch history a channel's scorer plugin sees (#112).
///
/// This is a property of the *channel*, not of the station: two channels on one
/// station can rank against different people, which is the whole point of a
/// personal For You channel sitting next to the house one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TasteScope {
    /// Every user's history, pooled with no user dimension (#74). The default,
    /// because it is what a station gets without saying anything.
    #[default]
    AllUsers,
    /// One named account's history, and nobody else's.
    SingleUser,
}

impl Default for ScoringConfig {
    fn default() -> Self {
        Self {
            recent_depth: default_recent_depth(),
            nominal_item_secs: default_nominal_item_secs(),
            taste_scope: TasteScope::default(),
            user: None,
            explain: false,
        }
    }
}

fn default_recent_depth() -> usize {
    200
}

fn default_nominal_item_secs() -> u32 {
    1800
}

/// One day. The window is how long an edit to a channel file waits before it
/// reaches the screen, so the default is the shortest span that still keeps a
/// full day of schedule standing between the playhead and dead air. Every
/// channel and sample in this repo already writes `window_days: 1`; the old
/// default of 30 only ever applied to a file that forgot the line, and it
/// bought that file a month of un-editable schedule.
fn default_window_days() -> u32 {
    1
}
fn default_chunk_hours() -> u32 {
    6
}
fn default_roll_interval() -> Duration {
    Duration::from_secs(3600)
}
fn default_retention_days() -> u32 {
    7
}

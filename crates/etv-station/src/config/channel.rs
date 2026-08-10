use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::pool::ShowGroup;
use super::rule::RuleConfig;
use crate::tautulli::HistoryScope;

#[derive(Debug, Deserialize, Serialize)]
pub struct ChannelConfig {
    /// Optional channel identity override. When unset, the channel's identity
    /// is its config file's stem (e.g. `diehard.yaml` -> `diehard`). The
    /// identity drives the log label, the overlay handshake name, and the
    /// output folder leaf under the station's `output_base`. Must not contain
    /// path separators.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// How far ahead of live the schedule is materialized. This is also what
    /// bounds a single generation, whichever shape the channel is: a pattern
    /// whose pools would take years to drain stops at the cycle boundary that
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

    /// Channel-level random seed. Only meaningful when a block uses
    /// `order = "random"`; unset means a fresh (non-reproducible) shuffle per
    /// generation, set means a pinned shuffle (#46 locked decision). Omit on
    /// channels with no random ordering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,

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

    /// Named show groups (#165), declared once and referenced by name from
    /// any pool's `groups:` field — see [`ShowGroup`]. Empty on every channel
    /// written before this, which is what an absent `groups:` key means.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<ShowGroup>,

    /// Optional live overlay. When set, the station daemon supervises an
    /// `etv-overlay` subprocess that writes RGBA frames to a fifo per
    /// channel; the emitted playout JSON carries an `overlay` field
    /// pointing etv-next at that fifo.
    #[serde(default)]
    pub overlay: Option<ChannelOverlayConfig>,
}

impl ChannelConfig {
    /// Whether any block interleaves pools via a pattern (#72).
    pub fn is_pattern(&self) -> bool {
        self.rule.blocks.iter().any(|b| b.is_pattern())
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
            .blocks
            .iter()
            .any(|b| b.pools.iter().any(|p| p.plugin.is_some()))
    }

    /// Whether this channel names recent watchers on its items (#113).
    pub fn attributes_watchers(&self) -> bool {
        self.scoring.as_ref().is_some_and(|s| s.attribution)
    }

    /// Whether anything on this channel reads the server's watch history.
    ///
    /// Two readers, not one: a scorer plugin ranks with it (#74), and
    /// `attribution` turns it into guide and on-screen text (#113). A channel
    /// that does neither must never trigger a fetch — that is #131's rule, and
    /// widening the reader set without widening this check is how a station
    /// silently starts calling Tautulli for results nothing consumes.
    pub fn reads_watch_history(&self) -> bool {
        self.uses_scorer_plugin() || self.attributes_watchers()
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
        self.rule
            .blocks
            .iter()
            .map(|b| b.adjacency_reach())
            .max()
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChannelOverlayConfig {
    /// Path to the etv-overlay TOML config (relative to the channel config
    /// directory or absolute). The config supplies width / height / framerate
    /// and the rendering script.
    pub config: PathBuf,

    /// Path to the fifo the channel + overlay process share. If omitted,
    /// defaults to `{output_folder}/overlay.fifo`.
    #[serde(default)]
    pub fifo_path: Option<PathBuf>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,

    /// Name on screen and in the guide who has been watching each item (#113).
    ///
    /// Off by default, and deliberately opt-in rather than derived: turning it
    /// on publishes every viewer's activity to everyone else watching the
    /// channel, which is a call the person running the server makes once, per
    /// channel, in writing.
    ///
    /// Costs nothing when off — the line is only built for a channel that asked
    /// for it.
    #[serde(default)]
    pub attribution: bool,
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
            attribution: false,
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

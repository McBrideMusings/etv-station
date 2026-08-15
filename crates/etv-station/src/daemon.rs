use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ersatztv_playout::playout::OverlaySpec as PlayoutOverlaySpec;
use ersatztv_playout::playout::Playout;
use time::OffsetDateTime;
use time_tz::Tz;
use tokio::select;
use tokio::sync::Notify;
use tracing::Instrument;

use crate::catalog::Catalog;
use crate::catalog::ingest::plex::{PlexAccount, PlexEnv};
use crate::config::{ChannelConfig, LoadedChannel, ScoringConfig, Station};
use crate::duration::DurationCache;
use crate::emit::emit_window;
use crate::errors::{ConfigError, StationError};
use crate::history::HistoryDb;
use crate::overlay_supervisor;
use crate::scan;
use crate::tautulli::{HistoryRow, HistoryScope};
use crate::tz as tzmod;

/// The station-wide history database's filename, under `output_base` — a
/// sibling of every channel's own output folder rather than inside one of
/// them, since it is shared by all of them (#111).
const HISTORY_DB_NAME: &str = "history.db";

pub async fn run(station: Station) -> Result<(), StationError> {
    let config_path = station.config_path.clone();
    let shutdown = Arc::new(Notify::new());
    let reload = Arc::new(Notify::new());
    spawn_signal_listener(shutdown.clone(), reload.clone());

    // Each pass of this loop runs one "generation" of channel + overlay tasks
    // against the current config. SIGHUP re-reads the config from disk and
    // starts a fresh generation; SIGTERM/SIGINT stops the current generation
    // and exits. The first generation runs the config passed in at startup.
    let mut station = Arc::new(station);
    // Open + populate the station-wide catalog once, before the first generation,
    // so it survives reloads (#96). A catalog-free station keeps working: query /
    // non-`manual` channels just error in `resolve_channel` as before.
    // The one read of `PLEX_URL`/`PLEX_TOKEN` in the daemon; everything below
    // takes the connection as an argument, so a test supplies `None` rather than
    // deleting the variables out of the process (#132).
    let plex = PlexEnv::from_env();
    let catalog = open_and_ingest_catalog(&station, plex.as_ref()).await?;
    // What the catalog was opened against — the catalog is opened once and NOT
    // reopened on reload (#96), so a later reload that changes these diverges.
    let opened_catalog_path = station.station.catalog_path.clone();
    let opened_source_roots = station.station.source_roots.clone();
    let opened_identity_roots = station.station.identity_roots.clone();
    // The station-wide play-history database (#111): one file under
    // `output_base`, shared by every channel and distinguished by a `channel`
    // column, opened once for the same reason the catalog is — every channel
    // task below gets a reference to this one handle rather than each
    // managing its own.
    let history_db = Arc::new(HistoryDb::open(
        station.station.output_base.join(HISTORY_DB_NAME),
    )?);
    // The last config that prepared cleanly. A `prepare_generation` failure on a
    // reload reverts to this instead of killing a daemon that's streaming fine.
    // See #90 and docs/adr/0001-reload-generation-revert.md.
    let mut last_good: Option<Arc<Station>> = None;
    loop {
        let tz = match prepare_generation(&station).await {
            Ok(tz) => {
                last_good = Some(station.clone());
                tz
            }
            // First generation (nothing good yet) → fail loud at startup. A
            // reload whose config won't prepare reverts to the last-known-good
            // and re-spawns it. If the config we reverted TO also fails to
            // prepare (same Arc), the environment is unrecoverable → exit.
            Err(e) => match &last_good {
                Some(good) if !Arc::ptr_eq(good, &station) => {
                    tracing::error!(
                        event = "config.reload_reverted",
                        error = %e,
                        "generation failed to prepare on reload; reverting to last-known-good config",
                    );
                    station = good.clone();
                    continue;
                }
                _ => return Err(e),
            },
        };

        // Spawn the generation, run it until a signal, then tear it down. The
        // whole generation is joined before we return here, which is what lets
        // the `station` Arc be swapped safely below (no task still reads it).
        let (do_reload, first_err) = run_generation(
            &station,
            tz,
            catalog.as_ref(),
            &history_db,
            plex.as_ref(),
            &shutdown,
            &reload,
        )
        .await;

        // On shutdown a channel that failed on its own becomes the daemon's exit
        // error. `channel_loop` only returns `Err` from its startup section
        // (duration probing, sidecar load, catch-up emit); roll-tick errors are
        // logged and retried, never returned. On reload we do NOT treat that
        // error as fatal — the failing channel gets another startup attempt next
        // generation, so a transient probe/media error must not tear down an
        // otherwise-healthy daemon (it was already logged in `run_generation`).
        if !do_reload {
            return first_err.map_or(Ok(()), Err);
        }

        // SIGHUP: re-read the config from disk. A malformed edit that won't even
        // parse keeps the previous config running. A config that parses but
        // can't be prepared (bad tz/overlay, uncreatable folder) is caught the
        // next iteration by `prepare_generation`, which reverts — so the
        // runnable-check lives in exactly one place.
        match crate::config::load(&config_path) {
            Ok(s) => {
                if catalog.is_some()
                    && (s.station.catalog_path != opened_catalog_path
                        || s.station.source_roots != opened_source_roots
                        || s.station.identity_roots != opened_identity_roots)
                {
                    tracing::warn!(
                        event = "config.reload_catalog_divergent",
                        "reload changes catalog_path/source_roots/identity_roots, but the catalog is opened once at startup and is not reopened; the running catalog and its path index still reflect the config it was opened with — restart to apply",
                    );
                }
                tracing::info!(event = "config.reload", config = %config_path.display(), "configuration reloaded");
                station = Arc::new(s);
            }
            Err(e) => {
                tracing::error!(
                    event = "config.reload_failed",
                    error = %e,
                    config = %config_path.display(),
                    "configuration reload failed to parse; keeping previous config running",
                );
            }
        }
    }
}

/// What the station knows about the catalog after ingest: where the file is,
/// and the canonical-path → `entry_id` index built **once** here so channels
/// don't each rebuild it.
///
/// Deliberately *not* a shared `Catalog`. It used to be one `Catalog` behind a
/// `Mutex`, because `rusqlite::Connection` is `Send` but `!Sync` and sixteen
/// channel tasks cannot share one. That satisfied the type system and created a
/// station-wide chokepoint: a channel whose scorer plugin took six minutes to
/// rank the library held the lock for all six, and every other channel's resolve
/// queued behind it — three channels dark for 6m34s over nine seconds of work.
///
/// SQLite has no such limitation. The file is in WAL mode, which exists so many
/// readers can work at once, and nothing writes it after ingest. So each channel
/// opens its own read-only handle from `path` (see [`Catalog::open_readonly`])
/// and there is no shared lock left to contend on.
struct CatalogInfo {
    path: PathBuf,
    path_index: HashMap<String, String>,
}

/// Everything a channel loop needs that belongs to the *station* rather than to
/// the channel: the parsed time zone, the media roots, the shared catalog, and
/// the shared watch history.
///
/// One value per generation, handed whole to every channel task. Threading these
/// individually meant each new station-wide resource added a parameter to three
/// nested functions — which is how #126's history ended up fetched per channel
/// in the first place: the cheap change was to fetch it where it was used.
#[derive(Clone, Copy)]
struct StationContext<'a> {
    tz: &'static Tz,
    identity_roots: &'a [String],
    catalog: Option<&'a CatalogInfo>,
    history: &'a SharedHistory,
    /// The station-wide play-history database (#111). Named distinctly from
    /// `history` above — that field is the *watch* history a scorer plugin
    /// reads (`ctx.history`); this one is the play-history ledger every
    /// channel appends to and projects its resume cursor from.
    history_db: &'a HistoryDb,
}

/// The station's copy of "what has been watched lately" — one per distinct
/// audience, not one per channel (#126, #112).
///
/// Before #126, an N-channel station issued N identical `get_history` calls of a
/// thousand rows and ran N identical catalog joins on every tick to produce N
/// identical `Vec<WatchEvent>`. #126 collapsed that to a single cached copy,
/// which was correct while history had no user dimension. #112 gives it one: a
/// channel can now rank against one named person, so two channels no longer
/// always want the same rows.
///
/// So the cache is keyed by [`HistoryScope`] rather than reverted to a
/// per-channel fetch. A station running the house For You channel plus one each
/// for two people makes three requests per refresh window and no more — however
/// many channels are pointed at those three audiences. Sharing survives; it is
/// just sharing among the channels that actually want the same rows.
///
/// Channels tick on their own `roll_interval`s and there is no station-wide
/// clock to hang a single fetch off, so this stays a cache the channels pull
/// from rather than a task that pushes to them: the first channel into a refresh
/// window for a given scope pays for that scope's fetch, and for the join when
/// it returned rows, while every other channel on that scope inside that window
/// gets the same `Arc` back.
///
/// Contention is the point, not a cost: the one mutex is held across the fetch,
/// so channels that tick together queue behind one request instead of racing N.
/// Keeping a single mutex over the whole map rather than one per scope also caps
/// how hard a many-channel station can hit Tautulli at once — scopes serialize
/// against each other, which is the behaviour worth having when the alternative
/// is three simultaneous thousand-row requests.
///
/// A `single_user` scope's fetch also resolves that account's **Plex** id
/// (#278, #281), cached alongside its events and read back through
/// [`Self::account_id`] — a scorer plugin needs the id, not the events, to
/// rank against one person's [`plexdb_reader::Reader::taste_vector_for`]
/// rather than the pooled vector. It is Plex's own id, not Tautulli's: the two
/// disagree for exactly one account, the server's owner (plex-db-ex
/// ADR-0010), so [`Self::resolve_account_id`] translates through Plex's own
/// `/accounts` rather than handing a scorer plugin the raw Tautulli id
/// [`crate::tautulli::resolve_account_id`] finds.
struct SharedHistory {
    /// What to join fetched rows against, and — because it is `None` whenever
    /// this generation has no reader for the result — the switch that decides
    /// whether a fetch happens at all. `None` means no request is ever made and
    /// no join event is ever logged.
    ///
    /// Two things put it there: a station with no `catalog_path`, which has
    /// nothing to join rows against; and a station whose channels name no
    /// scorer plugin, which has nobody to hand the joined events to. See
    /// [`history_catalog`] (#131).
    catalog: Option<Arc<CatalogInfo>>,
    /// The Tautulli `(url, api key)` to fetch against, resolved from the
    /// environment once by the caller. `None` — an unconfigured Tautulli —
    /// makes no request and produces no rows, which then takes the same
    /// skip-the-join path an unreachable or idle server takes (#141). Holding
    /// the pair here rather than reading the environment down in the fetch is
    /// what lets a test exercise that path without mutating the process
    /// (#132).
    tautulli: Option<(String, String)>,
    /// The Plex connection to resolve a `single_user` scope's account id
    /// against (#281), resolved from the environment once by the caller — the
    /// same [`PlexEnv`] the catalog ingest uses. `None` — an unconfigured
    /// Plex — makes a `single_user` channel's scorer plugin fail its
    /// generation loudly naming the user, the same as an account Plex's own
    /// `/accounts` does not recognise: there is no pooled-vector fallback to
    /// degrade to.
    plex: Option<PlexEnv>,
    /// How long a fetched history is reused before the next channel to ask
    /// refetches. Set to the shortest `roll_interval` on the station, so no
    /// channel ever sees a history older than one of its own ticks.
    refresh_after: Duration,
    /// One entry per audience any channel has asked for so far, created on first
    /// ask. Scopes come from the channel list, which is fixed for the life of a
    /// generation, so this cannot grow without bound under a running station.
    state: tokio::sync::Mutex<HashMap<HistoryScope, HistoryCache>>,
}

struct HistoryCache {
    /// `None` until the first fetch. Tokio's clock, so tests can advance it.
    fetched_at: Option<tokio::time::Instant>,
    events: Arc<[crate::score::WatchEvent]>,
    /// The **Plex** account id this scope's fetch resolved (#278, #281),
    /// cached alongside `events` since both come from the same fetch and age
    /// together. `Err` names the user when nothing resolved — a Tautulli
    /// account [`crate::tautulli::resolve_account_id`] could not find, no
    /// Plex configured to translate it, or a Tautulli account Plex's own
    /// `/accounts` does not recognise; see [`SharedHistory::resolve_account_id`].
    /// `Ok(None)` before the first fetch, the same as an unset field would
    /// default to, since a scope with no catalog to fetch against never
    /// resolves to anything more interesting than "nothing to report" anyway.
    account_id: Result<Option<i64>, String>,
}

impl Default for HistoryCache {
    fn default() -> Self {
        Self {
            fetched_at: None,
            events: Arc::from(Vec::new()),
            account_id: Ok(None),
        }
    }
}

impl SharedHistory {
    fn new(
        catalog: Option<Arc<CatalogInfo>>,
        tautulli: Option<(String, String)>,
        plex: Option<PlexEnv>,
        refresh_after: Duration,
    ) -> Self {
        Self {
            catalog,
            tautulli,
            plex,
            refresh_after,
            state: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// The history to generate `scope`'s channels against right now, fetching it
    /// first if that scope's cached copy has aged past `refresh_after`.
    ///
    /// Each scope ages independently: a channel ranking against one person does
    /// not refresh the server-wide pool, and vice versa.
    ///
    /// Empty when Tautulli is unset or unreachable, which degrades a scorer's
    /// ranking rather than failing the tick (#74).
    async fn current(&self, scope: &HistoryScope) -> Arc<[crate::score::WatchEvent]> {
        self.refresh(scope).await.0
    }

    /// The **Plex** account id `scope` resolves to right now (#278, #281),
    /// from the same fetch [`Self::current`] uses — read back rather than
    /// fetched a second time.
    ///
    /// `Ok(None)` for [`HistoryScope::AllUsers`]. `Err` names the user when a
    /// `single_user` scope could not be resolved all the way to a Plex
    /// account id — a Tautulli account Tautulli itself does not recognise, no
    /// `PLEX_URL`/`PLEX_TOKEN` configured to translate it, or a Tautulli
    /// account Plex's own `/accounts` does not recognise. Either way there is
    /// no id to hand a scorer plugin, and the caller must fail the generation
    /// loudly rather than let `ctx.account_id` arrive as unit and read like a
    /// pooled channel. Only worth calling for a channel that names a scorer
    /// plugin — nothing else reads the result.
    async fn account_id(&self, scope: &HistoryScope) -> Result<Option<i64>, String> {
        self.refresh(scope).await.1
    }

    /// The shared fetch-or-cache-hit both [`Self::current`] and
    /// [`Self::account_id`] read from, so the two can never observe two
    /// different fetches for what is supposed to be one scope's one refresh
    /// window.
    async fn refresh(
        &self,
        scope: &HistoryScope,
    ) -> (Arc<[crate::score::WatchEvent]>, Result<Option<i64>, String>) {
        let Some(sc) = self.catalog.as_ref() else {
            // Nothing to join rows against, so no Tautulli fetch runs here —
            // unchanged from before #281. A `single_user` scope's account id
            // still has to be resolved (#278), and #281 needs a live Plex
            // `/accounts` fetch to do it, run with no rows to fall back on: a
            // scope whose config value doesn't directly match a Plex account
            // id/name fails loudly here, exactly as a name-scope already did
            // in this configuration before #281.
            let account_id = self.resolve_account_id(scope, &[]).await;
            return (Arc::from(Vec::new()), account_id);
        };
        let mut state = self.state.lock().await;
        let cache = state.entry(scope.clone()).or_default();
        if let Some(at) = cache.fetched_at
            && at.elapsed() < self.refresh_after
        {
            return (Arc::clone(&cache.events), cache.account_id.clone());
        }
        // The HTTP half runs on a blocking thread — `ureq` is synchronous and
        // would otherwise stall this runtime worker for the request timeout,
        // exactly as the Plex ingest above avoids. The catalog join is local
        // work and stays here.
        //
        // With no Tautulli configured there is nothing to request, so no
        // request is made and the row set is empty before the join is even
        // considered.
        let rows = match self.tautulli.clone() {
            Some((url, key)) => {
                let scope = scope.clone();
                tokio::task::spawn_blocking(move || crate::tautulli::fetch_rows(&url, &key, &scope))
                    .await
                    .unwrap_or_default()
            }
            None => Vec::new(),
        };
        // Resolved from `rows` before they are moved into the join below
        // (#278, #281) — the only place on this side a username maps to an
        // id, and a digit-authored scope that matches a Plex account id
        // directly never even looks at `rows` to answer it.
        let account_id = self.resolve_account_id(scope, &rows).await;
        let events = if rows.is_empty() {
            // No rows is the ordinary state, not a corner: a station with no
            // `TAUTULLI_URL`/`TAUTULLI_API_KEY` never asks for any, and a
            // configured server that is unreachable or that nobody has watched
            // lately returns none. A join over an empty row set can only
            // produce an empty result, so running it costs a SQLite open and a
            // `tautulli.join` INFO reading `rows=0, keys=0, matched=0` to
            // arrive back where it started. That line is worse than wasted:
            // "joined watch history to the catalog" on an unconfigured station
            // reads as a wired-up Tautulli whose server is idle, which is a
            // different problem entirely (#141). Skip both, and let
            // `tautulli.join` mean a join actually happened.
            //
            // The empty result is still cached and still stamped below, so the
            // rest of this refresh window is served from the cache rather than
            // repeating the fetch on every channel's tick.
            Vec::new()
        } else {
            // A reader of its own, opened for this join and dropped with it.
            // Once per refresh window for the whole station, so the open costs
            // nothing next to the HTTP request that just happened; keeping a
            // handle alive between fetches would buy nothing. A failure to open
            // degrades the ranking rather than failing the tick, same as an
            // unreachable Tautulli.
            match Catalog::open_readonly(&sc.path) {
                Ok(reader) => crate::tautulli::join(&reader, rows),
                Err(e) => {
                    tracing::warn!(
                        event = "tautulli.catalog_unavailable",
                        error = %e,
                        "could not open a catalog reader to join watch history; generating without it",
                    );
                    Vec::new()
                }
            }
        };
        cache.events = Arc::from(events);
        cache.account_id = account_id;
        cache.fetched_at = Some(tokio::time::Instant::now());
        (Arc::clone(&cache.events), cache.account_id.clone())
    }

    /// The **Plex** account id `scope` resolves to (#281) — `Ok(None)` for
    /// [`HistoryScope::AllUsers`].
    ///
    /// Starts from [`crate::tautulli::resolve_account_id`]'s Tautulli-space id
    /// (#278, unchanged — that function's job is narrowing the Tautulli
    /// fetch and validating the account exists there, not taste) and
    /// translates it into Plex's own id space, since the two only disagree
    /// for one account: the server's owner, whom Plex's own history stores
    /// under a small server-local id while Tautulli reports the same person
    /// under their much larger plex.tv id (plex-db-ex ADR-0010). A `single_user`
    /// channel's scorer plugin ranks against
    /// `plexdb_reader::Reader::taste_vector_for`, which is keyed on *Plex's*
    /// id, so handing it the raw Tautulli id is exactly #281's silent bug.
    ///
    /// Two steps, direct then fallback, so every account except the owner
    /// resolves with no Tautulli row needed at all — unchanged from before
    /// #281:
    /// 1. **Direct.** The Tautulli-resolved id, checked against Plex's own
    ///    `/accounts` by id. Every account agrees on id except the owner, so
    ///    this is the only step most accounts ever need.
    /// 2. **Fallback**, only when the direct check misses. `rows`' Tautulli
    ///    username for this scope ([`crate::tautulli::HistoryRow::username_for`])
    ///    matched against a Plex account's *name* — the one field
    ///    plex-db-ex's own name join (ADR-0010) confirms agrees between the
    ///    two systems. `rows` is what this refresh already fetched; no
    ///    second Tautulli call.
    ///
    /// `Err`, naming the user, when: `resolve_account_id` itself failed; this
    /// station has no `PLEX_URL`/`PLEX_TOKEN` to translate with; Plex was
    /// unreachable; or neither step above found a match. Never a silent
    /// `Ok(None)` for a `single_user` scope — that is what let a `single_user`
    /// channel rank against an empty vector without anything in the logs
    /// saying why (#281's whole point, same rule #278 set for the Tautulli
    /// side).
    async fn resolve_account_id(
        &self,
        scope: &HistoryScope,
        rows: &[HistoryRow],
    ) -> Result<Option<i64>, String> {
        let Some(tautulli_id) = crate::tautulli::resolve_account_id(scope, rows)? else {
            return Ok(None);
        };
        let Some(env) = self.plex.clone() else {
            return Err(format!(
                "taste_scope: single_user resolved a Tautulli account ({tautulli_id}) but this \
                 station has no PLEX_URL/PLEX_TOKEN configured to translate it into a Plex \
                 account id (#281)",
            ));
        };
        let accounts =
            tokio::task::spawn_blocking(move || crate::catalog::ingest::plex::fetch_accounts(&env))
                .await
                .map_err(|e| format!("plex accounts fetch panicked: {e}"))?
                .map_err(|e| {
                    format!("fetching plex accounts to resolve a taste_scope account id: {e}")
                })?;

        translate_tautulli_id_to_plex(tautulli_id, scope, rows, &accounts).map(Some)
    }
}

/// The pure core of [`SharedHistory::resolve_account_id`]'s Plex-side
/// translation (#281) — split out from the HTTP-fetching wrapper above so the
/// direct/fallback decision is unit-tested without a live Plex server, the
/// same "pure core, thin HTTP wrapper" split [`crate::catalog::ingest::plex`]
/// already uses.
///
/// `tautulli_id` is what [`crate::tautulli::resolve_account_id`] already
/// resolved for `scope`; `accounts` is a live `/accounts` listing. Direct
/// match by id first (every account except the owner), then a fallback
/// through `rows`' Tautulli username for `scope` matched by Plex account
/// name — see [`SharedHistory::resolve_account_id`]'s doc for why each step
/// exists.
fn translate_tautulli_id_to_plex(
    tautulli_id: i64,
    scope: &HistoryScope,
    rows: &[HistoryRow],
    accounts: &[PlexAccount],
) -> Result<i64, String> {
    // Direct: the Tautulli id already IS the Plex id for every account except
    // the owner (#281's own diagnosis).
    if accounts.iter().any(|a| a.id == tautulli_id) {
        return Ok(tautulli_id);
    }

    // Fallback: translate through the Tautulli username this scope's rows
    // report, matched against a Plex account's name.
    let Some(username) = rows.iter().find_map(|r| r.username_for(scope)) else {
        return Err(format!(
            "no Plex account has id {tautulli_id} (Tautulli's account id for this scope), and \
             no recent Tautulli row named the account to translate its username instead — \
             regenerate once the account has recent watch history, or reconfigure `user:` with \
             the account's Plex name directly",
        ));
    };
    accounts
        .iter()
        .find(|a| a.name == username)
        .map(|a| a.id)
        .ok_or_else(|| {
            format!(
                "no Plex account is named {username:?} (this station's Tautulli username for \
                 the account this scope resolved to Tautulli id {tautulli_id})",
            )
        })
}

/// The catalog this generation's [`SharedHistory`] should join watch rows
/// against — `None` when nothing in it would ever read the result (#131).
///
/// Watch history reaches two places: `ScoreInputs::history`, which a Rhai script
/// sees through a pool that names a `plugin:` (#74), and the attribution line a
/// channel with `attribution: true` stamps into its items' guide text (#113). A
/// station whose channels do neither has no reader, so fetching a thousand
/// `get_history` rows and joining them against the catalog on every tick
/// produces a `Vec<WatchEvent>` that is dropped unread.
///
/// Deciding here rather than inside [`SharedHistory::current`] keeps `current`
/// a plain cache, and costs nothing in freshness: a generation holds the whole
/// channel list already, and a station that gains a plugin pool on SIGHUP is
/// handed a brand-new `SharedHistory` by the next generation, so the answer
/// cannot go stale under a running station.
fn history_catalog(
    channels: &[LoadedChannel],
    catalog: Option<&Arc<CatalogInfo>>,
) -> Option<Arc<CatalogInfo>> {
    if channels.iter().any(|c| c.config.reads_watch_history()) {
        catalog.cloned()
    } else {
        None
    }
}

/// What a startup should do about the Plex half of the catalog.
#[derive(Debug, PartialEq, Eq)]
enum PlexIngestPlan {
    /// The catalog was ingested recently enough to trust as-is; don't contact
    /// Plex at all.
    Skip { age_secs: i64 },
    /// Ask Plex only for records touched since this unix-seconds cursor.
    Delta { since: i64 },
    /// Re-read everything.
    Full,
}

impl PlexIngestPlan {
    /// The `since` cursor to hand the ingest — `None` for a full pass.
    fn since(&self) -> Option<i64> {
        match self {
            PlexIngestPlan::Delta { since } => Some(*since),
            _ => None,
        }
    }
}

/// Decide between skipping, a delta, and a full re-read.
///
/// Pure so the three boundaries can be tested without a Plex server or a clock:
/// `last` is when the previous pass started, `now` the current unix seconds.
///
/// Ordering matters. A never-ingested catalog is always full. Then age is
/// checked against `full_sweep_after_secs` *before* the refresh window, so a
/// catalog left untouched over a long weekend gets its deletion-catching full
/// pass rather than being skipped as "recent enough" forever. A clock that went
/// backwards (NTP correction, a restored snapshot) yields a negative age, which
/// falls through to a full pass — the safe direction, since we cannot tell what
/// a delta would be relative to.
fn plex_ingest_plan(
    last: Option<i64>,
    now: i64,
    catalog_refresh_secs: u64,
    full_sweep_after_secs: u64,
) -> PlexIngestPlan {
    let Some(last) = last else {
        return PlexIngestPlan::Full;
    };
    let age = now - last;
    if age < 0 {
        return PlexIngestPlan::Full;
    }
    if full_sweep_after_secs == 0 || age >= full_sweep_after_secs as i64 {
        return PlexIngestPlan::Full;
    }
    if age < catalog_refresh_secs as i64 {
        return PlexIngestPlan::Skip { age_secs: age };
    }
    PlexIngestPlan::Delta { since: last }
}

/// Open the station catalog (if `catalog_path` is set), bring it up to date, and
/// return what the channel tasks need to open readers of their own. `None` → the
/// station is catalog-free and only inline `manual` channels resolve. Opening is
/// fatal (a broken db must not be silently ignored); an ingest failure is logged
/// and the daemon continues with whatever was written — a Plex outage or a bad
/// media root shouldn't take playout down.
///
/// Every write this process makes to the catalog happens inside this function.
/// That is what lets every reader downstream be read-only.
///
/// `plex` is the Plex connection to ingest from, or `None` for a station with no
/// Plex configured — which skips the Plex pass entirely and contacts nothing.
async fn open_and_ingest_catalog(
    station: &Station,
    plex: Option<&PlexEnv>,
) -> Result<Option<Arc<CatalogInfo>>, StationError> {
    let Some(path) = station
        .station
        .catalog_path
        .as_deref()
        .filter(|p| !p.trim().is_empty())
    else {
        return Ok(None);
    };

    let mut catalog = Catalog::open(path)?;
    let source_roots = &station.station.source_roots;
    let identity_roots = &station.station.identity_roots;

    // Artwork cache (#187): unset means the feature is off — no directory, no
    // fetch, no `<icon>` in the guide, same as before this existed. Created
    // up front, the same way `prepare_generation` creates each channel's
    // output_folder, so a fresh deploy on an empty volume has somewhere to
    // write before the first Plex pass tries to.
    let artwork_dir = station
        .station
        .artwork_cache_dir
        .as_deref()
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from);
    if let Some(dir) = &artwork_dir {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|source| StationError::Io {
                path: dir.clone(),
                source,
            })?;
    }

    // Local filesystem: scan `source_roots` (an operational choice about which
    // directories this deployment may walk); identity is canonicalised against
    // `identity_roots` instead — a property of the media layout, not the same
    // decision (#243).
    let fs_roots: Vec<PathBuf> = source_roots.iter().map(PathBuf::from).collect();
    match crate::catalog::ingest::fs::ingest_roots(&catalog, &fs_roots, identity_roots).await {
        Ok(stats) => tracing::info!(
            event = "catalog.ingest.fs",
            entries = stats.entries_written,
            sources = stats.sources_written,
            sources_pruned = stats.sources_pruned,
            entries_pruned = stats.entries_pruned,
            "local-fs catalog ingest complete",
        ),
        Err(e) => {
            tracing::error!(event = "catalog.ingest.fs_failed", error = %e, "local-fs catalog ingest failed; continuing")
        }
    }

    // Plex: only when a connection was resolved — an unconfigured Plex is
    // normal, not an error. The client is blocking (`ureq`), so run it on a
    // blocking thread (moving the catalog in and back out) rather than stalling
    // the async runtime. `spawn_blocking` works on any runtime flavor, unlike
    // `block_in_place`.
    if let Some(plex) = plex {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let last = catalog.last_plex_ingest()?;
        match plex_ingest_plan(
            last,
            now,
            station.station.catalog_refresh_secs,
            station.station.full_sweep_after_secs,
        ) {
            PlexIngestPlan::Skip { age_secs } => tracing::info!(
                event = "catalog.ingest.plex_skipped",
                age_secs = age_secs,
                refresh_secs = station.station.catalog_refresh_secs,
                "catalog is younger than catalog_refresh_secs; reusing it without contacting plex",
            ),
            plan => {
                let since = plan.since();
                // Announce the pass before it starts. A full ingest reads the
                // whole library over the network and can take minutes; without
                // this line the daemon is silent throughout, so a slow or stale
                // mount is indistinguishable from a hang.
                tracing::info!(
                    event = "catalog.ingest.plex_start",
                    mode = if since.is_some() { "delta" } else { "full" },
                    "contacting plex to ingest the catalog; a full pass reads the whole library and can take a few minutes",
                );
                let roots = identity_roots.clone();
                let conn = plex.clone();
                let artwork_dir_for_ingest = artwork_dir.clone();
                let (returned, ingested) = tokio::task::spawn_blocking(move || {
                    let result = crate::catalog::ingest::plex::ingest(
                        &catalog,
                        &roots,
                        since,
                        &conn,
                        artwork_dir_for_ingest.as_deref(),
                    );
                    (catalog, result)
                })
                .await
                .expect("plex ingest task panicked");
                catalog = returned;
                match ingested {
                    Ok(stats) => tracing::info!(
                        event = "catalog.ingest.plex",
                        mode = if since.is_some() { "delta" } else { "full" },
                        entries = stats.entries_written,
                        sources = stats.sources_written,
                        summary_missing = stats.summary_missing,
                        "plex catalog ingest complete",
                    ),
                    Err(e) => {
                        tracing::error!(event = "catalog.ingest.plex_failed", error = %e, "plex catalog ingest failed; continuing")
                    }
                }
            }
        }
    }

    // Reconcile the artwork cache against the now-fully-ingested catalog
    // (#187): removes any cached file no current entry references, which is
    // what keeps the cache bounded by the catalog rather than growing
    // forever as entries come and go. Runs every pass (not just a full
    // sweep) — the reconcile itself is a directory listing plus a set diff,
    // cheap relative to the ingest it follows.
    if let Some(dir) = &artwork_dir {
        match crate::catalog::ingest::reconcile_artwork_cache(&catalog, dir) {
            Ok(removed) if removed > 0 => tracing::info!(
                event = "catalog.artwork.reconciled",
                removed,
                "removed cached artwork files no catalog entry references",
            ),
            Ok(_) => {}
            Err(e) => tracing::error!(
                event = "catalog.artwork.reconcile_io_failed",
                error = %e,
                "could not reconcile the artwork cache directory; continuing",
            ),
        }
    }

    // Build the path-match index once, now that the catalog is fully ingested.
    let roots: Vec<&str> = identity_roots.iter().map(String::as_str).collect();
    let path_index = crate::catalog::ingest::canonical_index(&catalog, &roots)?;

    // The writable handle dies here, with the last write this process will make.
    // Everything downstream reopens the file read-only, one handle per channel.
    drop(catalog);

    Ok(Some(Arc::new(CatalogInfo {
        path: PathBuf::from(path),
        path_index,
    })))
}

/// Spawn one generation's channel + overlay tasks against `station`, run until a
/// shutdown or reload signal, then stop every task and join it. Returns
/// `(do_reload, first_err)`: whether the signal was a reload (vs. shutdown), and
/// the first channel startup error seen (logged here regardless of which). The
/// whole generation is joined before returning, so the caller can safely swap
/// the `station` Arc for the next generation with no task still reading it.
/// `run` is the sole caller and sole waiter on both signals, so `notify_one`'s
/// stored permit makes the wait race-free without an explicit `enable()`.
async fn run_generation(
    station: &Arc<Station>,
    tz: &'static Tz,
    catalog: Option<&Arc<CatalogInfo>>,
    history_db: &Arc<HistoryDb>,
    plex: Option<&PlexEnv>,
    shutdown: &Notify,
    reload: &Notify,
) -> (bool, Option<StationError>) {
    let mut handles = Vec::new();
    let mut supervisor_handles = Vec::new();
    // One watch history for the whole station (#126). Built per generation
    // because a reload can change both the roll intervals it is sized from and
    // whether any channel still reads a history at all; the catalog behind it is
    // the same one every generation shares. With no channels nothing ever asks,
    // and a zero window is the inert choice — it means "always refetch", which
    // is the pre-#126 behaviour.
    let history = Arc::new(SharedHistory::new(
        history_catalog(&station.channels, catalog),
        // The one read of `TAUTULLI_URL`/`TAUTULLI_API_KEY` in the daemon (#132).
        crate::tautulli::credentials_from_env(),
        // The same `PLEX_URL`/`PLEX_TOKEN` connection the catalog ingest used
        // at startup (#281) — not a second read of the environment.
        plex.cloned(),
        station
            .channels
            .iter()
            .map(|c| c.config.roll_interval)
            .min()
            .unwrap_or(Duration::ZERO),
    ));
    // One stop signal per spawned task. `notify_one` stores a permit if the task
    // is not yet parked, so a reload that races a slow generation startup
    // (duration probing, catch-up emit) is never lost — unlike `notify_waiters`,
    // which only wakes already-parked waiters.
    let mut stops: Vec<Arc<Notify>> = Vec::new();
    for idx in 0..station.channels.len() {
        if let Some(ctx) = build_overlay_context(&station.channels[idx]) {
            let stop = Arc::new(Notify::new());
            stops.push(stop.clone());
            let name = station.channels[idx].name.clone();
            supervisor_handles.push(tokio::spawn(async move {
                tracing::info!(
                    event = "overlay.start",
                    channel = %name,
                    config = %ctx.overlay_config.display(),
                    fifo = %ctx.fifo_path.display(),
                    "starting overlay supervisor",
                );
                overlay_supervisor::run(ctx, stop).await;
            }));
        }
        let s = station.clone();
        let cat = catalog.cloned();
        let hist = history.clone();
        let hdb = history_db.clone();
        let stop = Arc::new(Notify::new());
        stops.push(stop.clone());
        let channel_name = station.channels[idx].name.clone();
        // One span per channel wraps the whole channel loop, so every event it
        // emits (roll ticks, chunk writes, retention deletes) carries the channel
        // in its span context for correlation.
        let span = tracing::info_span!("channel", channel = %channel_name);
        handles.push(tokio::spawn(
            async move {
                let ch = &s.channels[idx];
                let ctx = StationContext {
                    tz,
                    identity_roots: &s.station.identity_roots,
                    catalog: cat.as_deref(),
                    history: &hist,
                    history_db: &hdb,
                };
                let result = supervise_channel(ch, ctx, stop).await;
                (ch.name.clone(), result)
            }
            .instrument(span),
        ));
    }

    // `biased` makes shutdown win if both signals are pending.
    let do_reload = select! {
        biased;
        _ = shutdown.notified() => false,
        _ = reload.notified() => true,
    };

    // Stop this generation and wait for every task to wind down: channel loops
    // return Ok on the stop branch; overlay supervisors kill their subprocess and
    // remove the fifo + ready marker.
    for stop in &stops {
        stop.notify_one();
    }

    let mut first_err: Option<StationError> = None;
    for h in handles {
        match h.await {
            Ok((name, Ok(()))) => {
                tracing::info!(event = "channel.exit", channel = %name, "channel loop exited cleanly");
            }
            Ok((name, Err(e))) => {
                // Already logged at the point of failure inside the task
                // (event = "channel.failed"); here we only capture the first
                // error to surface through the daemon's exit code.
                //
                // Wrap it so it can't masquerade as a daemon-level failure. A
                // channel error is isolated and can happen minutes or hours
                // before shutdown, but it only reaches a human here — at exit,
                // stripped of which channel raised it and of the fact that the
                // daemon went on serving every other channel. Reported bare, a
                // startup problem on one channel reads at 22:11 as "the daemon
                // just died", which is the opposite of what happened.
                first_err.get_or_insert_with(|| StationError::ChannelFailed {
                    channel: name.clone(),
                    reason: e.to_string(),
                });
            }
            Err(e) => {
                tracing::error!(event = "channel.panic", error = %e, "channel task panicked");
                first_err.get_or_insert_with(|| StationError::Task(format!("{e}")));
            }
        }
    }
    for h in supervisor_handles {
        if let Err(e) = h.await {
            tracing::warn!(event = "overlay.supervisor_error", error = %e, "overlay supervisor task ended with error");
        }
    }

    (do_reload, first_err)
}

/// Install the signal handlers that drive the daemon: SIGTERM/SIGINT request
/// shutdown, SIGHUP requests a config reload. Both notify the single waiter in
/// `run` via `notify_one`, so a signal delivered before `run` parks is not lost.
fn spawn_signal_listener(shutdown: Arc<Notify>, reload: Arc<Notify>) {
    use tokio::signal::unix::{SignalKind, signal};

    tokio::spawn(async move {
        // `./tools/kill-dev.sh` and most container orchestrators send SIGTERM,
        // not SIGINT — handle both so the generation's stop path always runs and
        // cleans up the etv-overlay subprocess + its fifo.
        let mut sigterm = signal(SignalKind::terminate())
            .map_err(|e| {
                tracing::error!(event = "signal.handler_failed", signal = "SIGTERM", error = %e, "failed to install SIGTERM handler; relying on SIGINT only");
            })
            .ok();
        let mut sighup = signal(SignalKind::hangup())
            .map_err(|e| {
                tracing::error!(event = "signal.handler_failed", signal = "SIGHUP", error = %e, "failed to install SIGHUP handler; config reload via signal disabled");
            })
            .ok();

        loop {
            select! {
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!(event = "signal.shutdown", signal = "SIGINT", "ctrl-c received, shutting down");
                    shutdown.notify_one();
                    return;
                }
                _ = recv_signal(sigterm.as_mut()) => {
                    tracing::info!(event = "signal.shutdown", signal = "SIGTERM", "sigterm received, shutting down");
                    shutdown.notify_one();
                    return;
                }
                _ = recv_signal(sighup.as_mut()) => {
                    tracing::info!(event = "signal.reload", signal = "SIGHUP", "sighup received, reloading config");
                    reload.notify_one();
                }
            }
        }
    });
}

/// Await one delivery of an optional Unix signal. A `None` handler (one that
/// failed to install) never fires, so the corresponding `select!` arm is inert.
async fn recv_signal(sig: Option<&mut tokio::signal::unix::Signal>) {
    match sig {
        Some(s) => {
            s.recv().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// Run every check and side effect a generation needs before its channel and
/// overlay tasks spawn: parse the station tz, validate each channel's overlay
/// spec, and create each channel's `output_folder`. The single home for the "is
/// this config runnable" gate — `run` calls it once per generation on both the
/// startup and reload paths, so a check added here can never be silently skipped
/// on one path (the split that let the #34 mkdir slip past the reload gate; see
/// #90 and docs/adr/0001-reload-generation-revert.md).
async fn prepare_generation(station: &Station) -> Result<&'static Tz, StationError> {
    let tz = tzmod::parse(&station.station.tz)?;
    validate_overlay_configs(station)?;
    // Create every channel's output_folder before any task spawns — a fresh
    // deploy on empty volumes needs it in place before etv-next's canonicalize
    // reads it and before the overlay supervisor opens its fifo underneath (#34).
    for channel in &station.channels {
        let output = &channel.output_folder;
        tokio::fs::create_dir_all(output)
            .await
            .map_err(|source| StationError::Io {
                path: output.clone(),
                source,
            })?;
    }
    Ok(tz)
}

/// Resolve the (overlay_config_path, fifo_path) pair for a channel, if it has
/// an overlay configured. Both `build_overlay_context` and
/// `load_overlay_playout_spec` need the same resolution.
fn resolve_overlay_paths(channel: &LoadedChannel) -> Option<(PathBuf, PathBuf)> {
    let cfg = channel.config.overlay.as_ref()?;
    let overlay_config =
        overlay_supervisor::resolve_overlay_config(&channel.config_path, &cfg.config);
    let fifo_path =
        overlay_supervisor::resolve_fifo_path(&channel.output_folder, cfg.fifo_path.as_deref());
    Some((overlay_config, fifo_path))
}

fn build_overlay_context(channel: &LoadedChannel) -> Option<overlay_supervisor::OverlayContext> {
    let (overlay_config, fifo_path) = resolve_overlay_paths(channel)?;
    Some(overlay_supervisor::OverlayContext {
        channel_name: channel.name.clone(),
        output_folder: channel.output_folder.clone(),
        overlay_config,
        fifo_path,
    })
}

/// Parse every channel's overlay config up front so a malformed TOML fails the
/// daemon at startup instead of silently emitting playout JSON without an
/// overlay spec while the supervisor crash-loops on the same broken file.
fn validate_overlay_configs(station: &Station) -> Result<(), StationError> {
    for channel in &station.channels {
        let Some((overlay_config_path, _)) = resolve_overlay_paths(channel) else {
            continue;
        };
        etv_overlay::overlay_spec::OverlaySpec::from_path(&overlay_config_path).map_err(|e| {
            ConfigError::Validation {
                path: overlay_config_path.clone(),
                message: format!("overlay config for channel '{}': {e}", channel.name),
            }
        })?;
    }
    Ok(())
}

fn load_overlay_playout_spec(channel: &LoadedChannel) -> Option<PlayoutOverlaySpec> {
    let (overlay_config_path, fifo_path) = resolve_overlay_paths(channel)?;
    match etv_overlay::overlay_spec::OverlaySpec::from_path(&overlay_config_path) {
        Ok(spec) => Some(PlayoutOverlaySpec {
            fifo_path: fifo_path.to_string_lossy().into_owned(),
            pixel_format: String::from(spec.pixel_format.ffmpeg_arg()),
            width: spec.width,
            height: spec.height,
            framerate: spec.framerate,
            x: 0,
            y: 0,
        }),
        Err(e) => {
            // validate_overlay_configs parsed this at startup, so we only land
            // here if the file changed between startup and channel init.
            tracing::error!(
                event = "overlay.spec_error",
                channel = %channel.name,
                error = %e,
                config = %overlay_config_path.display(),
                "overlay config re-parse failed after startup validation; emitting playout without overlay spec",
            );
            None
        }
    }
}

async fn channel_loop(
    channel: &LoadedChannel,
    ctx: StationContext<'_>,
    catalog: Option<Catalog>,
    shutdown: Arc<Notify>,
) -> Result<(), StationError> {
    forward_channel_loop(channel, ctx, catalog, shutdown).await
}

/// How long the supervisor waits before the first restart of a channel loop that
/// died, doubling per consecutive failure up to the channel's `roll_interval`.
///
/// Most causes of a dead loop are transient and outlast a few seconds but not an
/// hour: a stale SMB mount, a full disk, a sidecar someone had open. Because the
/// playout JSON already on disk keeps airing for up to `window_days`, a restart
/// that lands inside the hour is invisible to viewers — nobody sees anything at
/// all, because the written window never drains.
const CHANNEL_RESTART_BACKOFF_START: Duration = Duration::from_secs(30);

/// Consecutive failures before the channel gets a card on screen. Two failures
/// with a doubling wait between them is still a channel having a moment; a third
/// is a channel that is not coming back on its own, and by then a viewer needs to
/// be told something rather than watch the written window run out into black.
const CHANNEL_FAILURES_BEFORE_CARD: u32 = 3;

/// Keep one channel on the air across failures of its own loop.
///
/// Without this, a loop that returned `Err` was simply gone: the task handle is
/// only joined at shutdown, so the failure was logged once and the channel aired
/// whatever had already been written — up to `window_days` of normal programming
/// — and then went dark with no further output and no explanation on screen.
///
/// So the loop is restarted on a widening backoff, and once it has clearly
/// stopped coming back the channel says so on screen instead of falling silent.
/// The first error is still carried out to the caller, so the daemon's exit code
/// means exactly what it meant before.
async fn supervise_channel(
    channel: &LoadedChannel,
    ctx: StationContext<'_>,
    stop: Arc<Notify>,
) -> Result<(), StationError> {
    let attempt = || {
        let stop = stop.clone();
        async move {
            // This attempt's own read-only handle on the catalog, opened fresh
            // each time so a restart also retries a database that would not
            // open. Owned rather than borrowed because a `&Catalog` cannot cross
            // an `.await` in a spawned task — `Connection` is `Send` but `!Sync`
            // — while the `Option<Catalog>` itself moves freely.
            //
            // A handle that won't open leaves this channel catalog-free, the
            // same state a station with no `catalog_path` puts every channel in.
            // That is exactly right for the two kinds of channel: one built from
            // inline `manual` entries never reads the catalog and keeps airing,
            // and one built from a `query` has nothing to resolve against —
            // which `resolve_channel` already reports, per channel, naming the
            // query. Failing the task here instead would take a manual channel
            // off the air over a database it does not use.
            let reader = match ctx.catalog.map(|c| Catalog::open_readonly(&c.path)) {
                Some(Ok(c)) => Some(c),
                None => None,
                Some(Err(e)) => {
                    tracing::error!(
                        event = "catalog.reader_failed",
                        channel = %channel.name,
                        error = %e,
                        "could not open this channel's catalog reader; it will run \
                         catalog-free — inline entries still air, queries will error",
                    );
                    None
                }
            };
            channel_loop(channel, ctx, reader, stop).await
        }
    };
    supervise(channel, ctx.tz, stop.clone(), attempt).await
}

/// The restart-and-card policy on its own, with the thing it supervises passed
/// in — so a test can drive a channel loop that fails a chosen number of times
/// and then succeeds, and read the timeline that leaves on disk.
async fn supervise<F, Fut>(
    channel: &LoadedChannel,
    tz: &'static Tz,
    stop: Arc<Notify>,
    mut run: F,
) -> Result<(), StationError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), StationError>>,
{
    let mut first_err: Option<StationError> = None;
    // Never reset, because it cannot go stale: `forward_channel_loop` only
    // returns `Err` from its startup section — once it reaches the roll loop a
    // failed tick is logged as `roll.error` and the loop carries on. So every
    // failure counted here is a failed *start*, and three of them are three in a
    // row inside about three and a half minutes.
    let mut consecutive: u32 = 0;
    let mut backoff = CHANNEL_RESTART_BACKOFF_START;
    // Never wait longer than a roll tick: past that the channel would miss the
    // cadence it is configured to extend its window on.
    let backoff_cap = channel
        .config
        .roll_interval
        .max(CHANNEL_RESTART_BACKOFF_START);

    loop {
        // Take down any card covering the future before handing the channel back
        // to its own loop. This runs before every attempt, including the first,
        // so cards left behind by a daemon that was killed or reloaded while
        // broken are cleared too. Cards that have already started are left alone:
        // one is on screen right now, and the rest are a record of what aired.
        //
        // It has to happen *before* the loop starts, not after it succeeds: the
        // loop picks up from the end of everything written, so a card run still
        // on disk would push the real schedule out beyond it. The cost is that
        // the far end of the window is briefly uncovered while the attempt runs
        // — a failed start, which is seconds — and if the attempt fails again the
        // card run is rewritten immediately below.
        match crate::channel_card::wipe_cards_from(channel, OffsetDateTime::now_utc()).await {
            Ok(0) => {}
            Ok(dropped) => tracing::info!(
                event = "channel.cards_cleared",
                channel = %channel.name,
                dropped = dropped,
                "cleared channel error cards from the future; regenerating the real schedule",
            ),
            Err(err) => tracing::error!(
                event = "channel.card_clear_failed",
                channel = %channel.name,
                error = %err,
                "could not clear channel error cards; the real schedule will be laid after them",
            ),
        }

        let err = match run().await {
            // The only clean exit is shutdown, and it ends the supervisor too.
            Ok(()) => return first_err.map_or(Ok(()), Err),
            Err(err) => err,
        };
        consecutive += 1;
        // Logged on every failure, not just the first: a channel that keeps
        // dying must not read as healthy to whoever is watching the logs, and the
        // card below only spares the viewer, never the operator.
        tracing::error!(
            event = "channel.failed",
            channel = %channel.name,
            error = %err,
            failures = consecutive,
            backoff_secs = backoff.as_secs(),
            "channel loop exited with error; restarting after backoff",
        );
        let reason = err.to_string();
        first_err.get_or_insert(err);

        if consecutive >= CHANNEL_FAILURES_BEFORE_CARD {
            match crate::channel_card::cover_after_written(
                channel,
                tz,
                &reason,
                OffsetDateTime::now_utc(),
            )
            .await
            {
                Ok(Some(from)) => tracing::warn!(
                    event = "channel.carded",
                    channel = %channel.name,
                    failures = consecutive,
                    from = %from,
                    "channel loop keeps failing; covering the rest of its window with an on-screen card",
                ),
                Ok(None) => {}
                Err(err) => tracing::error!(
                    event = "channel.card_failed",
                    channel = %channel.name,
                    error = %err,
                    "could not write the channel error card; this channel will go dark when its written window runs out",
                ),
            }
        }

        select! {
            biased;
            _ = stop.notified() => return first_err.map_or(Ok(()), Err),
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = backoff.saturating_mul(2).min(backoff_cap);
    }
}

#[cfg(test)]
mod supervisor_tests {
    use super::*;
    use ersatztv_playout::playout::{Playout, PlayoutItem};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    fn fixture(dir: &TempDir) -> LoadedChannel {
        let config: ChannelConfig = toml::from_str(
            "window_days = 1\nchunk_hours = 6\nroll_interval = \"1h\"\n\
             [rule]\nblocks = []\n",
        )
        .expect("fixture channel config parses");
        LoadedChannel {
            name: "testch".into(),
            config_path: PathBuf::from("testch.toml"),
            output_folder: dir.path().to_path_buf(),
            config,
        }
    }

    /// Everything the channel will air, in order. An item straddling a chunk
    /// boundary is deliberately written into both neighbouring files so either
    /// side can play it, so the same airing is read twice and folded back into
    /// one here.
    async fn timeline(dir: &Path) -> Vec<PlayoutItem> {
        let mut all = Vec::new();
        for f in scan::scan_output_folder(dir).await.unwrap() {
            let bytes = tokio::fs::read(&f.path).await.unwrap();
            all.extend(serde_json::from_slice::<Playout>(&bytes).unwrap().items);
        }
        all.sort_by_key(|i| i.start);
        all.dedup_by(|a, b| a.id == b.id && a.start == b.start);
        all
    }

    fn is_card(item: &PlayoutItem) -> bool {
        item.id.starts_with("etv-station-channel-card")
    }

    /// What a healthy loop leaves behind: half-hour programmes laid end to end
    /// from the end of whatever is already written, out to the end of the window.
    async fn lay_real_schedule(channel: &LoadedChannel, tz: &'static Tz) {
        let existing = scan::scan_output_folder(&channel.output_folder)
            .await
            .unwrap();
        let now = OffsetDateTime::now_utc();
        let from = scan::highest_finish(&existing).await.unwrap_or(now).max(now);
        let slot = Duration::from_secs(1800);
        let target = now + window_duration(channel.config.window_days);
        let count = ((target - from).as_seconds_f64() / 1800.0).ceil().max(0.0) as usize;
        let items: Vec<crate::resolve::ResolvedItem> = (0..count)
            .map(|i| crate::resolve::ResolvedItem {
                id: format!("film-{i}"),
                source: crate::config::SourceConfig::Lavfi {
                    params: "color=c=blue".into(),
                },
                in_point: Some(Duration::ZERO),
                out_point: Some(slot),
                program: None,
                catalog_duration: None,
                error_card: false,
                metadata: None,
                guide: None,
                guide_fields: crate::guide::GuideFields::default(),
            })
            .collect();
        let durations = vec![slot; count];
        let rule = crate::rule::Sequential::new(&items, &durations);
        crate::emit::emit_window(
            &channel.output_folder,
            &rule,
            from,
            tz,
            channel.config.chunk_hours,
            from,
            from + rule.total_duration(),
        )
        .await
        .unwrap();
    }

    /// One bad tick is not a broken channel: the loop is restarted, the window is
    /// filled with real programmes, and no card is ever written.
    #[tokio::test(start_paused = true)]
    async fn a_loop_that_fails_once_recovers_with_no_card_and_no_gap() {
        let dir = tempfile::tempdir().unwrap();
        let ch = fixture(&dir);
        let tz = crate::tz::parse("UTC").unwrap();
        let calls = AtomicUsize::new(0);

        let result = supervise(&ch, tz, Arc::new(Notify::new()), || async {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(StationError::Task("stale mount".into()));
            }
            lay_real_schedule(&ch, tz).await;
            Ok(())
        })
        .await;

        // The first error still reaches the caller, so the daemon's exit code
        // means what it always meant.
        assert!(result.is_err());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "the loop was restarted once"
        );
        let items = timeline(dir.path()).await;
        assert!(!items.is_empty());
        assert!(!items.iter().any(is_card), "one failure must not card");
        for pair in items.windows(2) {
            assert_eq!(pair[0].finish, pair[1].start, "gap in the timeline");
        }
    }

    /// A channel that keeps dying gets its window covered with cards rather than
    /// airing out its written schedule and going dark.
    #[tokio::test(start_paused = true)]
    async fn three_consecutive_failures_put_a_card_on_screen() {
        let dir = tempfile::tempdir().unwrap();
        let ch = fixture(&dir);
        let tz = crate::tz::parse("UTC").unwrap();
        let stop = Arc::new(Notify::new());
        let calls = AtomicUsize::new(0);
        let before = OffsetDateTime::now_utc();

        let s = stop.clone();
        let result = supervise(&ch, tz, stop.clone(), || async {
            // Four failures: the third is the one that cards, the fourth proves
            // the card is refreshed rather than written once and forgotten.
            if calls.fetch_add(1, Ordering::SeqCst) >= 3 {
                s.notify_one();
            }
            Err(StationError::Task("catalog is locked".into()))
        })
        .await;

        assert!(result.is_err());
        let items = timeline(dir.path()).await;
        assert!(!items.is_empty(), "the window was left uncovered");
        assert!(items.iter().all(is_card), "everything on air is a card");
        for pair in items.windows(2) {
            assert_eq!(pair[0].finish, pair[1].start, "gap between cards");
        }
        assert!(
            items.last().unwrap().finish >= before + window_duration(ch.config.window_days),
            "cards must reach the end of the window",
        );
    }

    /// Recovery: the cards come off and the real schedule takes over, rather than
    /// being laid behind a day of black. The one card already on screen at that
    /// moment plays out its five minutes — pulling it would cut a viewer to black
    /// mid-item, which is the thing this whole path exists to avoid.
    #[tokio::test(start_paused = true)]
    async fn a_channel_that_comes_back_replaces_its_cards_with_real_items() {
        let dir = tempfile::tempdir().unwrap();
        let ch = fixture(&dir);
        let tz = crate::tz::parse("UTC").unwrap();
        let calls = AtomicUsize::new(0);
        let recovered_at = std::sync::Mutex::new(None);

        let result = supervise(&ch, tz, Arc::new(Notify::new()), || async {
            if calls.fetch_add(1, Ordering::SeqCst) < 3 {
                return Err(StationError::Task("disk is full".into()));
            }
            *recovered_at.lock().unwrap() = Some(OffsetDateTime::now_utc());
            lay_real_schedule(&ch, tz).await;
            Ok(())
        })
        .await;

        assert!(result.is_err(), "the failures still surface");
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        let recovered_at = recovered_at.lock().unwrap().expect("the loop came back");
        let items = timeline(dir.path()).await;
        assert!(!items.is_empty());
        assert!(
            items.iter().filter(|i| is_card(i)).count() <= 1,
            "only the card already airing may survive: {:?}",
            items.iter().map(|i| &i.id).collect::<Vec<_>>(),
        );
        assert!(
            !items.iter().any(|i| is_card(i) && i.start >= recovered_at),
            "nothing carded may be scheduled once the channel is back",
        );
        assert!(items.iter().any(|i| !is_card(i)), "real items took over");
        for pair in items.windows(2) {
            assert_eq!(pair[0].finish, pair[1].start, "gap after recovery");
        }
    }
}

/// Bound on how many generations one catch-up will chain before giving up, so
/// a pathological config (a sequence with no wall-clock length) can't spin.
const MAX_GENERATIONS_PER_TICK: usize = 512;

/// Delete emitted playout files that begin at or after `from`, leaving anything
/// already airing or aired in place. Returns how many were removed.
async fn wipe_playout_from(
    channel: &LoadedChannel,
    from: OffsetDateTime,
) -> Result<usize, StationError> {
    let files = scan::scan_output_folder(&channel.output_folder).await?;
    let mut removed = 0;
    for f in files.iter().filter(|f| f.start >= from) {
        match tokio::fs::remove_file(&f.path).await {
            Ok(()) => removed += 1,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(StationError::Io {
                    path: f.path.clone(),
                    source,
                });
            }
        }
    }
    Ok(removed)
}

/// The earliest instant it is safe to wipe and regenerate from, given a raw
/// `from` (a chunk boundary or checkpoint instant with no guarantee of
/// landing on an item boundary).
///
/// `emit_window` deliberately emits a boundary-straddling item whole into
/// both neighbouring chunks so either side can play across the seam (see its
/// doc comment), and `wipe_playout_from` correctly leaves the chunk file
/// immediately before `from` in place — it also holds earlier items that are
/// still valid. But the straddling item survives with it, still airing past
/// `from`, so wiping and regenerating at the raw `from` lays a fresh item
/// over one that is already scheduled there (#153). Advancing to the
/// straddling item's real finish is what keeps the two from overlapping.
/// Returns `from` unchanged when nothing straddles it.
async fn regen_floor(
    channel: &LoadedChannel,
    from: OffsetDateTime,
) -> Result<OffsetDateTime, StationError> {
    let files = scan::scan_output_folder(&channel.output_folder).await?;
    let Some(preceding) = files
        .iter()
        .filter(|f| f.start < from)
        .max_by_key(|f| f.start)
    else {
        return Ok(from);
    };
    let bytes = tokio::fs::read(&preceding.path)
        .await
        .map_err(|source| StationError::Io {
            path: preceding.path.clone(),
            source,
        })?;
    let playout: Playout =
        serde_json::from_slice(&bytes).map_err(|source| StationError::PlayoutCorrupt {
            path: preceding.path.clone(),
            source,
        })?;
    let content_finish = playout.items.last().map(|item| item.finish).unwrap_or(from);
    Ok(content_finish.max(from))
}

#[cfg(test)]
mod regen_floor_tests {
    use super::*;
    use crate::rule::Sequential;
    use tempfile::tempdir;
    use time::macros::datetime;

    fn ch(dir: &tempfile::TempDir) -> LoadedChannel {
        let config: ChannelConfig = toml::from_str(
            "window_days = 1\nchunk_hours = 6\nroll_interval = \"1h\"\n[rule]\nblocks = []\n",
        )
        .expect("fixture channel config parses");
        LoadedChannel {
            name: "testch".into(),
            config_path: PathBuf::from("testch.toml"),
            output_folder: dir.path().to_path_buf(),
            config,
        }
    }

    fn film(id: &str, secs: u64) -> crate::resolve::ResolvedItem {
        crate::resolve::ResolvedItem {
            id: id.into(),
            source: crate::config::SourceConfig::Lavfi {
                params: format!("src={id}"),
            },
            in_point: Some(Duration::ZERO),
            out_point: Some(Duration::from_secs(secs)),
            program: None,
            catalog_duration: None,
            error_card: false,
            metadata: None,
            guide: None,
            guide_fields: crate::guide::GuideFields::default(),
        }
    }

    /// The exact #153 shape: an 8-hour film starting an hour before a 6-hour
    /// chunk boundary, so it is emitted whole into the chunk ending at that
    /// boundary (per `emit_window`'s doc comment) with the boundary as its
    /// filename finish — even though the film really runs another 7 hours past
    /// it. A wipe at the boundary correctly leaves that chunk file alone, but
    /// `regen_floor` must still read past the filename to the film's real end.
    #[tokio::test]
    async fn advances_past_a_film_straddling_the_wipe_point() {
        let dir = tempdir().unwrap();
        let channel = ch(&dir);
        let tz = crate::tz::parse("UTC").unwrap();
        let boundary = datetime!(2026-01-01 06:00 UTC);
        let anchor = boundary - time::Duration::hours(1);
        let items = vec![film("fellowship", 8 * 3600)];
        let durations = vec![Duration::from_secs(8 * 3600)];
        let rule = Sequential::new(&items, &durations);
        emit_window(
            &channel.output_folder,
            &rule,
            anchor,
            tz,
            6,
            anchor,
            anchor + rule.total_duration(),
        )
        .await
        .unwrap();

        // What a #153 regeneration does before this fix: wipe everything at or
        // after the boundary, stranding the boundary-named chunk as the only
        // survivor.
        wipe_playout_from(&channel, boundary).await.unwrap();

        let floor = regen_floor(&channel, boundary).await.unwrap();
        assert_eq!(
            floor,
            anchor + time::Duration::hours(8),
            "must not land inside the still-airing film"
        );
    }

    /// A chunk whose content fills it exactly, with nothing straddling the
    /// boundary, must not be pushed forward — there is nothing to protect.
    #[tokio::test]
    async fn leaves_a_clean_boundary_untouched() {
        let dir = tempdir().unwrap();
        let channel = ch(&dir);
        let tz = crate::tz::parse("UTC").unwrap();
        let anchor = datetime!(2026-01-01 00:00 UTC);
        let items = vec![film("a", 6 * 3600)];
        let durations = vec![Duration::from_secs(6 * 3600)];
        let rule = Sequential::new(&items, &durations);
        emit_window(
            &channel.output_folder,
            &rule,
            anchor,
            tz,
            6,
            anchor,
            anchor + rule.total_duration(),
        )
        .await
        .unwrap();

        let boundary = datetime!(2026-01-01 06:00 UTC);
        assert_eq!(regen_floor(&channel, boundary).await.unwrap(), boundary);
    }

    /// Nothing written yet: regen_floor is a no-op, not an error.
    #[tokio::test]
    async fn returns_from_unchanged_with_no_existing_files() {
        let dir = tempdir().unwrap();
        let channel = ch(&dir);
        let from = datetime!(2026-01-01 06:00 UTC);
        assert_eq!(regen_floor(&channel, from).await.unwrap(), from);
    }
}

/// The emission loop for every channel: **materialize forward**.
///
/// Each pass resolves the channel, lays the resulting sequence end-to-end after
/// the last thing already written, and stores where it got to. The emitted
/// chunk JSON is the durable timeline; the `.resume` sidecar holds only the
/// seam. Nothing already written is ever rewritten — the past is a record, not
/// a rendering of the current config — so config edits arrive through the
/// checkpoint rewind below rather than a wholesale wipe (#53).
///
/// This replaced an anchor-and-loop model that resolved one list at startup and
/// repeated it forever off an `.anchor` sidecar. That model could not express a
/// channel whose list changes between generations: a pool with
/// `advance = "resume"` produces a different list by design, and `.anchor`
/// re-anchored on every change, restarting the schedule. It also could not
/// deliver what an unseeded `order = "random"` channel advertises — resolving
/// once per process meant one shuffle replayed until the daemon restarted,
/// never a fresh one per pass.
///
/// Nothing was lost by dropping it. A channel whose list happens never to change
/// resolves the same list every generation, and those laid end-to-end *are* the
/// loop. So there is one emission model rather than two.
async fn forward_channel_loop(
    channel: &LoadedChannel,
    ctx: StationContext<'_>,
    mut catalog: Option<Catalog>,
    shutdown: Arc<Notify>,
) -> Result<(), StationError> {
    let (resume, how) = crate::resume::load(&channel.output_folder).await?;
    match &how {
        crate::resume::ResumeLoad::Fresh => tracing::info!(
            event = "resume.init",
            channel = %channel.name,
            "no resume sidecar; starting every pool from the top",
        ),
        crate::resume::ResumeLoad::Loaded => tracing::info!(
            event = "resume.load",
            channel = %channel.name,
            pools = resume.pools.len(),
            "loaded resume map",
        ),
        crate::resume::ResumeLoad::Discarded(reason) => tracing::warn!(
            event = "resume.discard",
            channel = %channel.name,
            reason = %reason,
            "resume sidecar unusable; starting every pool from the top",
        ),
    }
    let mut resume = resume;

    // The play-history database (#70, promoted to sqlite by #111) — the
    // single record of what this channel has aired, and the thing each
    // series' resume position is derived from. A channel's old `.history`
    // JSONL sidecar is migrated into it exactly once; every read after that
    // is an indexed query against `ctx.history_db`, never a full-file parse.
    let migration = ctx
        .history_db
        .migrate_channel(&channel.name, &channel.output_folder)
        .await?;
    if migration.skipped > 0 {
        tracing::warn!(
            event = "history.partial",
            channel = %channel.name,
            skipped = migration.skipped,
            "skipped unparseable play-history lines while migrating; affected series resume from an earlier position",
        );
    }
    if !migration.already_done {
        tracing::info!(
            event = "history.migrated",
            channel = %channel.name,
            migrated = migration.migrated,
            skipped = migration.skipped,
            "migrated play history from the .history sidecar into the sqlite store",
        );
    }
    tracing::info!(
        event = "history.ready",
        channel = %channel.name,
        airings = ctx.history_db.count(&channel.name)?,
        "play history store ready",
    );

    // Startup: throw away the future this channel had already written and
    // generate it again from the config as it stands now.
    //
    // A wholesale wipe is not available: the output depends on where the pools
    // had advanced to, and that state is gone once consumed.
    // The checkpoint trail is what makes the same thing possible here — rewind
    // the pools to the start of the earliest unaired generation, drop exactly
    // the files from that instant on, and regenerate. What has already aired,
    // or is airing now, is untouched. Without this, a config or overlay edit
    // wouldn't reach a pattern channel until its entire written window had
    // played out (#53).
    let now = OffsetDateTime::now_utc();
    if let Some(regen_from) = resume.rewind_to_unaired(now) {
        let regen_from = regen_floor(channel, regen_from).await?;
        let removed = wipe_playout_from(channel, regen_from).await?;
        // Those airings are no longer scheduled, so they are no longer history.
        // Because the resume position is a projection of the store, dropping
        // them is also what rewinds each series — the two cannot disagree.
        ctx.history_db.truncate_from(&channel.name, regen_from)?;
        tracing::info!(
            event = "resume.rewind",
            channel = %channel.name,
            from = %regen_from,
            removed = removed,
            airings = ctx.history_db.count(&channel.name)?,
            "rewound to the earliest unaired generation; regenerating it from the current config",
        );
    }

    resume = pattern_catch_up(channel, ctx, &mut catalog, resume, "startup").await?;

    let mut interval = tokio::time::interval(channel.config.roll_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await; // consume immediate tick

    loop {
        select! {
            _ = shutdown.notified() => {
                tracing::info!(event = "channel.shutdown", channel = %channel.name, "shutdown received");
                return Ok(());
            }
            _ = interval.tick() => {
                let tick = async {
                    match pattern_catch_up(channel, ctx, &mut catalog, resume.clone(), "roll").await {
                        Ok(next) => resume = next,
                        Err(err) => tracing::error!(
                            event = "roll.error",
                            channel = %channel.name,
                            error = %err,
                            "roll tick failed; will retry next interval",
                        ),
                    }
                };
                tick.instrument(tracing::info_span!("roll_tick")).await;
            }
        }
    }
}

/// Generate and emit forward until the window through `now + window_days` is
/// covered, chaining one generation into the next through the resume map.
/// Hint a scorer plugin about how many items this generation needs.
///
/// Sized to **one chunk**, not to the whole remaining window. A generation lays
/// the plugin's returned list end-to-end, so a hint covering a 30-day window
/// would push a single generation to materialize the entire month in one pass.
/// Near the end of the window the remaining span is smaller than a chunk, and
/// that smaller span wins.
///
/// Clamped at both ends: at least one item, so a nearly-covered window still
/// asks for something rather than handing a plugin a target of zero, and capped
/// so no configuration can ask a plugin to rank an entire library.
fn target_count(config: &ChannelConfig, from: OffsetDateTime, target: OffsetDateTime) -> usize {
    const MAX: i64 = 500;
    let per_item = config
        .scoring
        .as_ref()
        .map(|s| s.nominal_item_secs)
        .unwrap_or_else(|| ScoringConfig::default().nominal_item_secs)
        .max(1) as i64;
    let chunk = i64::from(config.chunk_hours) * 3600;
    let remaining = (target - from).whole_seconds().max(0);
    remaining.min(chunk).div_euclid(per_item).clamp(1, MAX) as usize
}

/// Returns the map to carry into the next tick.
async fn pattern_catch_up(
    channel: &LoadedChannel,
    ctx: StationContext<'_>,
    catalog: &mut Option<Catalog>,
    mut resume: crate::resume::ResumeMap,
    phase: &'static str,
) -> Result<crate::resume::ResumeMap, StationError> {
    let output = &channel.output_folder;
    let now = OffsetDateTime::now_utc();
    let target = now + window_duration(channel.config.window_days);
    let overlay_spec = load_overlay_playout_spec(channel);
    let mut cache = DurationCache::load(output).await?;

    // Bound the window at both ends BEFORE reading the frontier, so a file
    // dated past what the window can reach never gets a vote on where
    // generation resumes. Order matters: `highest_finish` below is the only
    // thing that decides whether this tick generates at all, and a single
    // stranded far-future file makes it answer "already covered" forever.
    let unreachable = scan::unreachable_after(target, channel.config.chunk_hours, ctx.tz);
    let swept = scan::sweep_window(output, channel.config.retention_days, unreachable, now).await;
    if swept.unreachable > 0 {
        // The deleted spans have records: pool checkpoints in the sidecar and
        // airings in the play-history store, both dated in the span just
        // removed. Left standing, `tail()` would hand the scorer a "recently
        // aired" list from years in the future. Same two calls the
        // coverage-heal path makes below, at the same cutoff.
        resume.rewind_to(unreachable);
        ctx.history_db.truncate_from(&channel.name, unreachable)?;
        crate::resume::save(output, &resume).await?;
    }
    if swept.total() > 0 {
        tracing::info!(
            event = "window.sweep",
            channel = %channel.name,
            phase = phase,
            elapsed = swept.elapsed,
            unreachable = swept.unreachable,
            retention_days = channel.config.retention_days,
            unreachable_after = %unreachable,
            "window sweep pruned playout files",
        );
    }

    // Repair a coverage hole before extending the window. An interrupted run
    // from before the honest-naming fix — or any unforeseen cause — can leave a
    // chunk that airs black between two covered spans, and forward
    // materialization alone never revisits it (`highest_finish` only ever moves
    // the frontier forward). So read the *actual* item coverage: if a hole opens
    // before the frontier, wipe from the chunk that should contain it and let
    // the generation loop below regenerate it.
    //
    // Horizon differs by phase. Startup scans the whole window, healing damage
    // already on disk in one pass. A roll tick scans only the near future — two
    // roll intervals — which is cheap and still spots a hole at least one tick
    // before the playhead reaches it; a hole further out is caught as `now`
    // advances toward it on later ticks.
    let heal_horizon = if phase == "startup" {
        target
    } else {
        now + time::Duration::seconds_f64(channel.config.roll_interval.as_secs_f64() * 2.0)
    };
    if let Some(gap) = scan::first_coverage_gap(output, now, heal_horizon).await? {
        let boundary = tzmod::chunk_boundary_at_or_before(gap, channel.config.chunk_hours, ctx.tz);
        let regen_from = regen_floor(channel, boundary).await?;
        let removed = wipe_playout_from(channel, regen_from).await?;
        // Best-effort pool alignment: rewind to the checkpoint covering the hole
        // if it survives, else leave the pools as they are and accept a possible
        // seam glitch — either way the black is gone once the loop regenerates.
        resume.rewind_to(regen_from);
        ctx.history_db.truncate_from(&channel.name, regen_from)?;
        crate::resume::save(output, &resume).await?;
        tracing::warn!(
            event = "coverage.heal",
            channel = %channel.name,
            phase = phase,
            gap = %gap,
            regen_from = %regen_from,
            removed = removed,
            "coverage hole found; wiped the affected chunk onward to regenerate it",
        );
    }

    // Pick up exactly where the written record ends. Unlike the looping path
    // this is deliberately NOT snapped to a chunk boundary: a forward-
    // materialized channel continues from the last item's finish, so the
    // timeline stays gapless across the seam.
    let existing = scan::scan_output_folder(output).await?;
    let mut from = scan::highest_finish(&existing).await.unwrap_or(now).max(now);
    // Only the first generation of a channel with nothing written yet joins its
    // list mid-way from a past `anchor`; see the phase calculation below.
    let mut first_generation = existing.is_empty();

    // Watch history is read at most once per station tick per audience — not
    // once per generation, and not once per channel. A catch-up chains many
    // generations in a row and they all share the same "what has been watched
    // lately", and so does every other channel that ticks in the same refresh
    // window *and ranks against the same people* (#126, #112). Empty when
    // Tautulli is unset or unreachable, which degrades a scorer's ranking rather
    // than failing the tick (#74).
    //
    // Asked for only by a channel that actually reads it. `history_catalog`
    // makes this decision station-wide (#131) — nobody on the station names a
    // scorer plugin, nobody fetches — but station-wide is too coarse now that
    // scopes differ: without this check, one plugin channel anywhere would make
    // every *other* channel fetch its own scope too, for a `Vec<WatchEvent>`
    // that nothing then reads. Before #112 that cost nothing because they all
    // shared one entry.
    let history = if channel.config.reads_watch_history() {
        ctx.history.current(&channel.config.history_scope()).await
    } else {
        Arc::from(Vec::new())
    };

    // The account id a scorer plugin ranks against when this channel is
    // `single_user`-scoped (#278), resolved from the same Tautulli fetch
    // `history` above just ran. `None` on a pooled channel and on any channel
    // with no scorer plugin at all — attribution alone has no use for an id,
    // only a display name, so it must not gate on this.
    //
    // A `single_user` channel whose named user resolved to nobody fails this
    // generation loudly, naming the user, rather than letting `ctx.account_id`
    // arrive as unit and a scorer plugin quietly rank against the pooled
    // vector instead — the exact silent failure #278 exists to rule out. The
    // failure is caught here and not at config load because resolving a
    // username needs the same live Tautulli fetch `history` does; a config
    // pass has no network (see `config::validate::validate_taste_scope`).
    let account_id = if channel.config.uses_scorer_plugin() {
        ctx.history
            .account_id(&channel.config.history_scope())
            .await
            .map_err(|reason| ConfigError::Validation {
                path: channel.config_path.clone(),
                message: format!("scoring: {reason}"),
            })?
    } else {
        None
    };

    // Whether this generation names watchers, read once — it cannot change
    // inside a tick, and the check sits in the per-item loop below.
    let attribution_wanted = channel.config.attributes_watchers();

    // How deep a recently-aired tail this channel's scorer sees. Read once —
    // it cannot change inside a tick.
    let recent_depth = channel
        .config
        .scoring
        .as_ref()
        .map(|s| s.recent_depth)
        .unwrap_or_else(|| ScoringConfig::default().recent_depth);

    let mut generations = 0;
    while from < target {
        if generations >= MAX_GENERATIONS_PER_TICK {
            tracing::warn!(
                event = "pattern.generation_cap",
                channel = %channel.name,
                phase = phase,
                generations = generations,
                covered_through = %from,
                target = %target,
                "hit the per-tick generation cap; the window is only covered this far and will extend on the next roll tick",
            );
            break;
        }

        // Record the state entering this generation before anything consumes
        // it, so the span it is about to write stays regenerable while it is
        // still in the future.
        resume.checkpoint(from);

        // Where each series left off comes from the play-history store, not
        // from a cursor of the sidecar's own (#70): one table, projected on
        // demand via an indexed query, never a whole-file read.
        let state = crate::resume::GenerationState {
            resume: resume.clone(),
            cursor: ctx.history_db.series_cursor(&channel.name)?,
            tail: ctx
                .history_db
                .tail(&channel.name, channel.config.adjacency_reach())?,
        };

        // Sized to one chunk, not to the whole remaining window: the generation
        // lays whatever the plugin returns end-to-end, so asking for a month's
        // worth would make a single generation try to cover the month.
        let scoring = crate::score::ScoreInputs {
            target_count: target_count(&channel.config, from, target),
            history: Arc::clone(&history),
            recent: ctx.history_db.tail(&channel.name, recent_depth)?,
            now: now.unix_timestamp(),
            // The station's configured tz — a sequencer block (#169) reads
            // this to place its pools against the local clock (ADR 0004).
            tz: Some(ctx.tz),
            account_id,
        };

        // This channel's own reader, borrowed for the synchronous resolve and
        // released at the end of the block. Nothing else can be waiting on it —
        // it belongs to this task alone — so however long a scorer plugin takes
        // in here, no other channel is affected.
        //
        // The borrow stays inside the block deliberately: a `&Catalog` held
        // across the `.await`s further down would make this task's future
        // non-`Send` and it would not compile at the `tokio::spawn`.
        let (items, resume_out, show_ids) = {
            let reader = catalog.as_ref();
            let (items, resume_out) = crate::resolve::resolve_channel_with_resume(
                &channel.config,
                &channel.config_path,
                ctx.identity_roots,
                ctx.catalog.map(|info| &info.path_index),
                reader,
                &state,
                &scoring,
                // How much airtime is still missing, and what bounds one
                // generation. A pattern block with no authored `cycles` stops
                // once it has laid this much down rather than running its pools
                // to the end — eleven years in one pass on a 51-pool channel
                // (#140) — and a flat `entries` channel cuts its authored list
                // here and resumes at that position next time rather than
                // laying all 950 items and idling for a month (#118). The loop
                // condition guarantees this is positive.
                Some((target - from).unsigned_abs()),
                // The absolute instant this generation begins airing at — a
                // sequencer block (#169) reads it as `ctx.window.from`, so a
                // daypart script asks "what hour does this generation start"
                // rather than "what hour is it while the daemon happens to be
                // computing this".
                from,
            )?;
            // The ledger needs each airing's show, and only the catalog knows
            // it. One query for the whole generation rather than one per item.
            let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
            let show_ids = match reader {
                Some(cat) => cat.show_ids_for(&ids)?,
                None => HashMap::new(),
            };
            (items, resume_out, show_ids)
        };

        // No "the channel ran out" branch: every series loops, so a pattern
        // channel cannot play itself empty. An empty resolve means an empty
        // *set*, which `resolve_channel_with_resume` has already raised as the
        // config error it is.

        // Takes the list and hands one back: an unreadable file becomes an
        // on-screen error card of the same length, so the channel keeps its
        // shape instead of failing over one bad file.
        let (mut items, durations, probe_stats) = cache.resolve_all(items).await?;
        if probe_stats.error_cards > 0 || probe_stats.dropped > 0 {
            tracing::warn!(
                event = "generation.unreadable_media",
                channel = %channel.name,
                error_cards = probe_stats.error_cards,
                dropped = probe_stats.dropped,
                "some items could not be read; see the per-item warnings above",
            );
        }

        // Render each item's `guide:` overrides (#158) and, on a channel that
        // opted into it, name who has been watching (#113) — both need the
        // schedule (this item's own start/finish, a one-item lookahead) and
        // the watch history `history` already fetched above, neither of
        // which `resolve_channel_with_resume` had: that call happens before
        // duration-probing has assigned any item a real length. Stamped here
        // rather than inside `resolve` for the same reason attribution
        // always was — it lands on whatever the channel actually scheduled,
        // however that list was chosen: a plugin pool, a CEL query, or a
        // hand-written entry list all get the same treatment.
        render_guide_and_attribution(
            &mut items,
            &durations,
            from,
            &channel.name,
            channel.config.display_name.as_deref(),
            attribution_wanted,
            &history,
        );

        // A channel with an `anchor` in the past joins its list where elapsed
        // time says it should be, rather than at item 0 — "this station has been
        // broadcasting since 2020". Only on the very first generation: after
        // that the written timeline is the phase, and re-deriving it would fight
        // the resume map. The skipped items do not come back: joining at the
        // anchor means treating them as already aired, which is the whole claim
        // the anchor makes, and the list position the resolve just recorded has
        // moved past them.
        let (items_slice, durations_slice, seq_start) =
            match channel.config.anchor.filter(|_| first_generation) {
                Some(anchor) => {
                    let (skip, into_item) = crate::rule::phase_at(anchor, from, &durations);
                    if skip > 0 || !into_item.is_zero() {
                        tracing::info!(
                            event = "anchor.join",
                            channel = %channel.name,
                            anchor = %anchor,
                            skipped_items = skip,
                            "joined the sequence mid-list from the configured anchor",
                        );
                    }
                    (&items[skip..], &durations[skip..], from - into_item)
                }
                None => (&items[..], &durations[..], from),
            };
        first_generation = false;

        let rule = crate::rule::Sequential::new(items_slice, durations_slice)
            .with_overlay(overlay_spec.as_ref().map(clone_overlay_spec));
        let span = rule.total_duration() - (from - seq_start);
        if span <= time::Duration::ZERO {
            // Zero wall-clock length would never advance `from`. Stop rather
            // than spin, and say so — silently emitting nothing would look
            // exactly like a healthy idle channel.
            tracing::error!(
                event = "pattern.zero_length",
                channel = %channel.name,
                phase = phase,
                items = items.len(),
                "generation produced no playable duration; nothing further can be emitted",
            );
            resume.checkpoints.pop();
            break;
        }

        // Emit the generation *whole*, even when it reaches past the target.
        // Clamping to the target would drop the sequence's tail while the
        // resume map still recorded those items as played, skipping them
        // permanently. Overshooting the window by less than one generation
        // costs nothing; a hole in the schedule is unrecoverable.
        //
        // The generation is sized to the window at the other end instead — the
        // `fill` span handed to the resolve above — so "one generation" is now
        // about one window rather than however long the channel's whole list or
        // pool set happens to run (#140, #118).
        let to = from + span;
        let written = emit_window(
            output,
            &rule,
            seq_start,
            ctx.tz,
            channel.config.chunk_hours,
            from,
            to,
        )
        .await?;
        log_emission(&channel.name, phase, &written, from, to);

        // One play-history row per scheduled airing, in schedule order. The
        // times are the same walk `Sequential` just emitted: items laid end
        // to end from `from`, which is why the whole generation is emitted
        // rather than clamped — a row must correspond to something actually
        // on disk.
        //
        // `written_at` is read per generation rather than reusing the tick's
        // `now`: a catch-up can chain many generations, and stamping them all
        // with the moment the tick began would misreport when each was
        // actually scheduled.
        let written_at = OffsetDateTime::now_utc();
        let mut airing = from;
        let records: Vec<crate::history::PlayRecord> = items
            .iter()
            .zip(durations.iter())
            .map(|(item, dur)| {
                let start = airing;
                airing += time::Duration::seconds_f64(dur.as_secs_f64());
                crate::history::PlayRecord {
                    entry_id: item.id.clone(),
                    show_id: show_ids.get(&item.id).cloned(),
                    start,
                    played_at: written_at,
                    // The slot aired either way, so the series advances either
                    // way. The flag is what keeps a scorer from counting an
                    // error card as a film somebody watched.
                    error_card: item.error_card,
                }
            })
            .collect();
        ctx.history_db.record(&channel.name, &records)?;

        // `resume_out` carries only pool state; the checkpoint trail is the
        // daemon's, so it rides across rather than being replaced.
        let checkpoints = std::mem::take(&mut resume.checkpoints);
        resume = resume_out;
        resume.checkpoints = checkpoints;
        resume.prune_elapsed(now);
        crate::resume::save(output, &resume).await?;
        from += span;
        generations += 1;
    }

    if generations == 0 {
        tracing::info!(
            event = "chunk.skip",
            channel = %channel.name,
            phase = phase,
            "window already materialized through {target}",
        );
    }
    cache.save(output).await?;

    Ok(resume)
}

/// Render each item's cascaded `guide:` template (#158) and, on a channel
/// that opted in, append the #113 "watched recently by" line — the one pass
/// that needs the schedule (each item's own start/finish, a one-item
/// lookahead/lookbehind over this generation) and the watch history
/// together, which is why it runs here rather than inside `resolve` (no
/// schedule yet) or inside `Sequential` (no watch history).
///
/// `items` and `durations` are already paired 1:1 by
/// [`crate::duration::DurationCache::resolve_all`] — the same pairing the
/// play-history `records` loop right after this call relies on.
fn render_guide_and_attribution(
    items: &mut [crate::resolve::ResolvedItem],
    durations: &[Duration],
    from: OffsetDateTime,
    channel_identity: &str,
    channel_display_name: Option<&str>,
    attribution_wanted: bool,
    history: &[crate::score::WatchEvent],
) {
    let channel_name = channel_display_name.unwrap_or(channel_identity);
    let credits = attribution_wanted.then(|| crate::attribution::Attribution::build(history));

    // Snapshot every item's default title *before* any item in this pass is
    // rewritten, so item N's `{next_title}`/`{prev_title}` always reads item
    // N±1's series-convention/catalog title — never a value this same pass
    // already replaced.
    let base_titles: Vec<Option<String>> = items
        .iter()
        .map(|item| item.program.as_ref().and_then(|p| p.title.clone()))
        .collect();

    let generated_at = OffsetDateTime::now_utc();
    let mut airing = from;
    let mut stamped = 0usize;
    for (idx, dur) in durations.iter().enumerate() {
        let start = airing;
        let stop = start + time::Duration::seconds_f64(dur.as_secs_f64());
        airing = stop;

        let watched_by = credits
            .as_ref()
            .and_then(|c| c.line_for(&items[idx].id))
            .map(|line| line.to_string());

        let Some(guide) = items[idx].guide.clone() else {
            // No `guide:` override anywhere in the cascade for this item —
            // the built-in defaults from `resolve` (series-title convention,
            // genre categories) stand as written. `attribution: true` still
            // applies its line on top, exactly as before #158.
            if let Some(line) = &watched_by {
                let program = items[idx].program.get_or_insert_with(Default::default);
                program.description = Some(crate::attribution::append_to_description(
                    program.description.take(),
                    line,
                ));
                stamped += 1;
            }
            continue;
        };

        let base = items[idx].program.take().unwrap_or_default();
        // Owned copies of what the render context borrows from `base`, so
        // `base` can move into the rebuilt `program` below while `ctx` is
        // still in scope.
        let base_title = base.title.clone();
        let base_content_rating = base.content_rating.clone();
        let program_title = base_title.clone();

        let ctx = crate::guide::RenderContext {
            fields: &items[idx].guide_fields,
            base_title: base_title.as_deref(),
            base_season: base.season,
            base_episode: base.episode,
            base_year: base.year,
            base_content_rating: base_content_rating.as_deref(),
            channel_identity,
            channel_name,
            program_title: program_title.as_deref(),
            watched_by: watched_by.as_deref(),
            next_title: base_titles
                .get(idx + 1)
                .and_then(|t: &Option<String>| t.as_deref()),
            next_start: (idx + 1 < durations.len()).then_some(stop),
            prev_title: if idx > 0 {
                base_titles[idx - 1].as_deref()
            } else {
                None
            },
            start,
            stop,
            now: generated_at,
        };

        let mut program = base;
        if let Some(t) = &guide.title {
            program.title = Some(crate::guide::render(t, &ctx));
        }
        if let Some(t) = &guide.sub_title {
            let rendered = crate::guide::render(t, &ctx);
            program.sub_title = (!rendered.is_empty()).then_some(rendered);
        }
        match &guide.description {
            Some(t) => {
                let rendered = crate::guide::render(t, &ctx);
                program.description = (!rendered.is_empty()).then_some(rendered);
                // An explicit `{watched_by}` already carries the line — the
                // shorthand appending it again would duplicate it (#158
                // decision #4).
                if let Some(line) = &watched_by
                    && !crate::guide::references_watched_by(t)
                {
                    program.description = Some(crate::attribution::append_to_description(
                        program.description.take(),
                        line,
                    ));
                    stamped += 1;
                }
            }
            None => {
                if let Some(line) = &watched_by {
                    program.description = Some(crate::attribution::append_to_description(
                        program.description.take(),
                        line,
                    ));
                    stamped += 1;
                }
            }
        }
        if let Some(cats) = &guide.categories {
            let rendered = crate::guide::render_categories(cats, &ctx);
            program.categories = (!rendered.is_empty()).then_some(rendered);
        }
        items[idx].program = Some(program);
    }

    if attribution_wanted {
        tracing::info!(
            event = "attribution.stamped",
            channel = %channel_identity,
            items = items.len(),
            stamped,
            entries_with_watchers = credits.as_ref().map(|c| c.len()).unwrap_or(0),
            "named recent watchers on this generation's items",
        );
    }
}

#[cfg(test)]
mod render_guide_and_attribution_tests {
    use super::*;
    use crate::guide::{GuideConfig, GuideFields};
    use crate::resolve::ResolvedItem;
    use crate::score::WatchEvent;
    use ersatztv_playout::playout::ProgramMetadata;
    use time::macros::datetime;

    fn item_with_guide(id: &str, title: &str, guide: Option<GuideConfig>) -> ResolvedItem {
        ResolvedItem {
            id: id.into(),
            source: crate::config::SourceConfig::Lavfi {
                params: "testsrc".into(),
            },
            in_point: None,
            out_point: None,
            program: Some(ProgramMetadata {
                title: Some(title.into()),
                ..Default::default()
            }),
            catalog_duration: None,
            error_card: false,
            metadata: None,
            guide,
            guide_fields: GuideFields::default(),
        }
    }

    fn watch(entry: &str, who: &str) -> WatchEvent {
        WatchEvent {
            entry_id: entry.into(),
            watched_at: 0,
            watcher: Some(who.into()),
        }
    }

    /// An item with no `guide:` override anywhere is untouched — the
    /// defaults resolve already computed (series-title convention, genre
    /// categories) stand exactly as written.
    #[test]
    fn no_override_leaves_the_default_program_untouched() {
        let mut items = vec![item_with_guide("a", "Die Hard", None)];
        let durations = vec![Duration::from_secs(60)];
        render_guide_and_attribution(
            &mut items,
            &durations,
            datetime!(2026-08-13 00:00 UTC),
            "diehard",
            None,
            false,
            &[],
        );
        assert_eq!(
            items[0].program.as_ref().unwrap().title.as_deref(),
            Some("Die Hard")
        );
    }

    /// `{field}` substitution and the `{a|b}` fallback.
    #[test]
    fn template_substitution_and_fallback() {
        let guide = GuideConfig {
            title: Some("{channel_name}".into()),
            description: Some("{content_missing|program_title}".into()),
            sub_title: None,
            categories: None,
        };
        let mut items = vec![item_with_guide("a", "Die Hard", Some(guide))];
        let durations = vec![Duration::from_secs(60)];
        render_guide_and_attribution(
            &mut items,
            &durations,
            datetime!(2026-08-13 00:00 UTC),
            "diehard",
            Some("Die Hard 24/7"),
            false,
            &[],
        );
        let program = items[0].program.as_ref().unwrap();
        assert_eq!(program.title.as_deref(), Some("Die Hard 24/7"));
        // "content_missing" isn't a real field name, but every unresolved
        // name renders empty rather than panicking (validation is what
        // catches a real typo at load) — proving the fallback still lands on
        // the working `program_title` branch.
        assert_eq!(program.description.as_deref(), Some("Die Hard"));
    }

    /// `{genres}` in a `categories:` list fans out to one `<category>` per
    /// tag; a non-fan-out entry renders as one string.
    #[test]
    fn categories_fan_out_and_plain_entries() {
        let guide = GuideConfig {
            categories: Some(vec!["{genres}".into(), "Movie Night".into()]),
            title: None,
            sub_title: None,
            description: None,
        };
        let mut items = vec![item_with_guide("a", "Die Hard", Some(guide))];
        items[0].guide_fields.genres = vec!["Action".into(), "Thriller".into()];
        let durations = vec![Duration::from_secs(60)];
        render_guide_and_attribution(
            &mut items,
            &durations,
            datetime!(2026-08-13 00:00 UTC),
            "diehard",
            None,
            false,
            &[],
        );
        assert_eq!(
            items[0].program.as_ref().unwrap().categories.as_deref(),
            Some(
                &[
                    "Action".to_string(),
                    "Thriller".to_string(),
                    "Movie Night".to_string()
                ][..]
            )
        );
    }

    /// The schedule: `{next_title}`/`{prev_title}` read the neighbouring
    /// item's *default* title, never a title this same pass already
    /// rewrote — and the first/last item's missing neighbour renders empty.
    #[test]
    fn next_and_prev_title_read_the_original_neighbour_not_a_rewritten_one() {
        let guide = |t: &str| {
            Some(GuideConfig {
                description: Some(t.into()),
                title: None,
                sub_title: None,
                categories: None,
            })
        };
        let mut items = vec![
            item_with_guide("a", "First", guide("{next_title}")),
            item_with_guide("b", "Second", guide("{prev_title|next_title}")),
            item_with_guide("c", "Third", guide("{prev_title}")),
        ];
        let durations = vec![
            Duration::from_secs(60),
            Duration::from_secs(60),
            Duration::from_secs(60),
        ];
        render_guide_and_attribution(
            &mut items,
            &durations,
            datetime!(2026-08-13 00:00 UTC),
            "diehard",
            None,
            false,
            &[],
        );
        assert_eq!(
            items[0].program.as_ref().unwrap().description.as_deref(),
            Some("Second"),
            "item a's next is b's ORIGINAL title, not b's rewritten description"
        );
        assert_eq!(
            items[1].program.as_ref().unwrap().description.as_deref(),
            Some("First"),
            "prev branch wins since it is non-empty"
        );
        assert_eq!(
            items[2].program.as_ref().unwrap().description.as_deref(),
            Some("Second"),
            "the last item has no next, so prev is used"
        );
    }

    /// `attribution: true` still appends the #113 line when no `guide:`
    /// override is present at all.
    #[test]
    fn attribution_appends_with_no_guide_override() {
        let mut items = vec![item_with_guide("a", "Die Hard", None)];
        let durations = vec![Duration::from_secs(60)];
        let history = vec![watch("a", "bob")];
        render_guide_and_attribution(
            &mut items,
            &durations,
            datetime!(2026-08-13 00:00 UTC),
            "diehard",
            None,
            true,
            &history,
        );
        assert_eq!(
            items[0].program.as_ref().unwrap().description.as_deref(),
            Some("Watched recently by bob")
        );
    }

    /// A description template that does NOT reference `{watched_by}` still
    /// gets the #113 line appended.
    #[test]
    fn attribution_appends_after_a_description_template_that_omits_it() {
        let guide = GuideConfig {
            description: Some("Now playing".into()),
            title: None,
            sub_title: None,
            categories: None,
        };
        let mut items = vec![item_with_guide("a", "Die Hard", Some(guide))];
        let durations = vec![Duration::from_secs(60)];
        let history = vec![watch("a", "bob")];
        render_guide_and_attribution(
            &mut items,
            &durations,
            datetime!(2026-08-13 00:00 UTC),
            "diehard",
            None,
            true,
            &history,
        );
        assert_eq!(
            items[0].program.as_ref().unwrap().description.as_deref(),
            Some("Now playing\n\nWatched recently by bob")
        );
    }

    /// A description template that DOES reference `{watched_by}` already
    /// carries the line — the shorthand must not duplicate it (#158
    /// decision #4).
    #[test]
    fn an_explicit_watched_by_reference_is_not_duplicated() {
        let guide = GuideConfig {
            description: Some("Now playing. {watched_by}".into()),
            title: None,
            sub_title: None,
            categories: None,
        };
        let mut items = vec![item_with_guide("a", "Die Hard", Some(guide))];
        let durations = vec![Duration::from_secs(60)];
        let history = vec![watch("a", "bob")];
        render_guide_and_attribution(
            &mut items,
            &durations,
            datetime!(2026-08-13 00:00 UTC),
            "diehard",
            None,
            true,
            &history,
        );
        let desc = items[0]
            .program
            .as_ref()
            .unwrap()
            .description
            .clone()
            .unwrap();
        assert_eq!(desc, "Now playing. Watched recently by bob");
        assert_eq!(
            desc.matches("Watched recently by bob").count(),
            1,
            "the line must appear exactly once, got {desc:?}"
        );
    }
}

/// `OverlaySpec` (an ETV-next type) is not `Clone`, and each generation in a
/// catch-up builds its own rule.
fn clone_overlay_spec(spec: &PlayoutOverlaySpec) -> PlayoutOverlaySpec {
    PlayoutOverlaySpec {
        fifo_path: spec.fifo_path.clone(),
        pixel_format: spec.pixel_format.clone(),
        width: spec.width,
        height: spec.height,
        framerate: spec.framerate,
        x: spec.x,
        y: spec.y,
    }
}

pub(crate) fn window_duration(window_days: u32) -> time::Duration {
    time::Duration::seconds(window_days as i64 * 24 * 3600)
}

fn log_emission(
    channel: &str,
    phase: &'static str,
    written: &[PathBuf],
    from: OffsetDateTime,
    to: OffsetDateTime,
) {
    tracing::info!(
        event = "chunk.write",
        channel = %channel,
        phase = phase,
        files = written.len(),
        from = %from,
        to = %to,
        "emitted playout files",
    );
}

#[cfg(test)]
mod ingest_plan_tests {
    use super::*;

    const REFRESH: u64 = 900;
    const SWEEP: u64 = 86_400;

    #[test]
    fn a_catalog_never_ingested_is_read_in_full() {
        assert_eq!(
            plex_ingest_plan(None, 1_000_000, REFRESH, SWEEP),
            PlexIngestPlan::Full
        );
    }

    #[test]
    fn a_restart_inside_the_refresh_window_does_not_contact_plex() {
        let last = 1_000_000;
        assert_eq!(
            plex_ingest_plan(Some(last), last + 899, REFRESH, SWEEP),
            PlexIngestPlan::Skip { age_secs: 899 }
        );
    }

    #[test]
    fn past_the_refresh_window_asks_plex_only_for_changes() {
        let last = 1_000_000;
        assert_eq!(
            plex_ingest_plan(Some(last), last + 900, REFRESH, SWEEP),
            PlexIngestPlan::Delta { since: last }
        );
    }

    #[test]
    fn past_the_sweep_interval_forces_a_full_pass() {
        let last = 1_000_000;
        assert_eq!(
            plex_ingest_plan(Some(last), last + SWEEP as i64, REFRESH, SWEEP),
            PlexIngestPlan::Full
        );
    }

    /// The sweep outranks the refresh window: a catalog older than the sweep
    /// interval must not be skipped as "recent", or deletions would never be
    /// noticed on a station that is restarted constantly.
    #[test]
    fn the_sweep_wins_over_the_refresh_window() {
        assert_eq!(
            plex_ingest_plan(Some(0), 10, /* refresh */ 100_000, /* sweep */ 5),
            PlexIngestPlan::Full
        );
    }

    #[test]
    fn a_zero_sweep_interval_disables_delta_entirely() {
        let last = 1_000_000;
        assert_eq!(
            plex_ingest_plan(Some(last), last + 1, REFRESH, 0),
            PlexIngestPlan::Full
        );
    }

    #[test]
    fn a_zero_refresh_window_never_skips() {
        let last = 1_000_000;
        assert_eq!(
            plex_ingest_plan(Some(last), last, 0, SWEEP),
            PlexIngestPlan::Delta { since: last }
        );
    }

    /// A clock that jumped backwards (NTP correction, restored snapshot) leaves
    /// a "last ingest" in the future. There is no sound delta cursor in that
    /// case, so fall back to reading everything.
    #[test]
    fn a_backwards_clock_falls_back_to_a_full_pass() {
        assert_eq!(
            plex_ingest_plan(Some(2_000_000), 1_000_000, REFRESH, SWEEP),
            PlexIngestPlan::Full
        );
    }
}

/// The station-wide watch history (#126): one fetch per tick, shared by every
/// channel, refetched once the refresh window has passed — and joined to the
/// catalog only when that fetch actually returned rows (#141).
#[cfg(test)]
mod shared_history_tests {
    use std::sync::Mutex as StdMutex;

    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

    use super::*;

    /// Counts events carrying a given `event = "…"` name.
    struct CountEvents {
        name: &'static str,
        seen: Arc<StdMutex<usize>>,
    }

    #[derive(Default)]
    struct EventName(Option<String>);

    impl Visit for EventName {
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "event" {
                self.0 = Some(value.to_string());
            }
        }
        fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
    }

    impl<S: tracing::Subscriber> Layer<S> for CountEvents {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let mut name = EventName::default();
            event.record(&mut name);
            if name.0.as_deref() == Some(self.name) {
                *self.seen.lock().unwrap() += 1;
            }
        }
    }

    /// Install a counter for one event name on this thread for as long as the
    /// returned guard lives. `#[tokio::test]` runs a current-thread runtime, so
    /// every `.await` in the test body is polled on this same thread and stays
    /// under it.
    fn count_events(
        name: &'static str,
    ) -> (Arc<StdMutex<usize>>, tracing::subscriber::DefaultGuard) {
        let seen = Arc::new(StdMutex::new(0));
        let subscriber = tracing_subscriber::registry().with(CountEvents {
            name,
            seen: Arc::clone(&seen),
        });
        let guard = tracing::subscriber::set_default(subscriber);
        (seen, guard)
    }

    /// A catalog to join against. Every `SharedHistory` in this module is built
    /// with `None` for the Tautulli connection, which is what keeps these tests
    /// hermetic: the dev shell exports `TAUTULLI_URL`/`TAUTULLI_API_KEY`, and a
    /// fetch that read them would make a real network call. Passing the
    /// connection in means the test simply withholds it — nothing touches the
    /// process environment, so nothing races another test thread reading it
    /// (#132). No connection means no rows, which is exactly the path an
    /// unreachable or idle Tautulli takes: the cache still turns over on its
    /// refresh window, but the join is skipped (#141).
    ///
    /// Returns the `TempDir` alongside the info: `SharedHistory` reopens the
    /// file per join, so it has to survive the test rather than being an
    /// in-memory catalog that exists only behind one handle.
    fn catalog_without_tautulli() -> (Arc<CatalogInfo>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.db");
        // Opened and dropped: this creates the schema the readers expect.
        crate::catalog::Catalog::open(&path).unwrap();
        (
            Arc::new(CatalogInfo {
                path,
                path_index: HashMap::new(),
            }),
            dir,
        )
    }

    /// When one scope's history was last fetched, or `None` if no channel has
    /// asked for that scope yet.
    ///
    /// The cache is keyed by audience since #112, so a test that used to read
    /// "the" stamp now has to say whose.
    async fn stamp(history: &SharedHistory, scope: &HistoryScope) -> Option<tokio::time::Instant> {
        history
            .state
            .lock()
            .await
            .get(scope)
            .and_then(|c| c.fetched_at)
    }

    /// The acceptance criterion: N channels ticking inside one refresh window
    /// produce **one** fetch between them, not N.
    ///
    /// Two independent proofs, because with no rows to join there is no
    /// `tautulli.join` event left to count (#141). The stamp is the load-bearing
    /// one: every pass writes `fetched_at`, so if a later channel had run its
    /// own pass the stamp would have moved to that channel's clock reading.
    /// Allocation identity backs it up — a fresh pass stores a newly built
    /// `Arc` and `first` holds the original alive for the whole test, so the
    /// allocator cannot hand the same address back.
    #[tokio::test(start_paused = true)]
    async fn every_channel_in_one_tick_shares_a_single_fetch() {
        let (info, _catalog_dir) = catalog_without_tautulli();
        let history = SharedHistory::new(Some(info), None, None, Duration::from_secs(3600));

        // Four channels, each asking at the top of its own tick. The clock moves
        // a second between them — well inside the hour-long window, but enough
        // that a second pass would restamp `fetched_at` to a different instant.
        let first = history.current(&HistoryScope::AllUsers).await;
        let stamped_at = stamp(&history, &HistoryScope::AllUsers).await.unwrap();
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(1)).await;
            let again = history.current(&HistoryScope::AllUsers).await;
            assert_eq!(
                stamp(&history, &HistoryScope::AllUsers).await.unwrap(),
                stamped_at,
                "a channel inside the window must be served from the cache, \
                 leaving the stamp of the one pass that filled it",
            );
            assert!(
                Arc::ptr_eq(&first, &again),
                "every channel must get the same allocation, not a copy",
            );
        }
    }

    /// The cache is a refresh window, not a one-shot: a channel ticking after
    /// the window has passed gets freshly-fetched history. Without this the
    /// station would rank forever against whatever was watched at startup.
    ///
    /// The empty result of a skipped join has to obey the window exactly as a
    /// real join's would — otherwise skipping the join trades one wasted
    /// catalog open for a Tautulli request on every single channel tick.
    #[tokio::test(start_paused = true)]
    async fn the_next_tick_refetches_once_the_window_has_passed() {
        let (info, _catalog_dir) = catalog_without_tautulli();
        let history = SharedHistory::new(Some(info), None, None, Duration::from_secs(60));

        let first = history.current(&HistoryScope::AllUsers).await;
        let first_at = stamp(&history, &HistoryScope::AllUsers).await.unwrap();

        tokio::time::advance(Duration::from_secs(60)).await;
        let second = history.current(&HistoryScope::AllUsers).await;
        let second_at = stamp(&history, &HistoryScope::AllUsers).await.unwrap();

        assert_eq!(
            second_at.duration_since(first_at),
            Duration::from_secs(60),
            "the first tick past the window must refetch and restamp the cache",
        );
        assert!(
            !Arc::ptr_eq(&first, &second),
            "a refetch must store its own result, not hand back the stale one",
        );

        let third = history.current(&HistoryScope::AllUsers).await;
        assert!(
            Arc::ptr_eq(&second, &third),
            "the refetched history is shared by the rest of its window too",
        );
    }

    /// #112's acceptance criterion, and the thing #126's single cache could not
    /// do: two channels ranking against different people must not be served each
    /// other's history.
    ///
    /// Before the cache was keyed, the second scope to ask inside a refresh
    /// window got whatever the first scope had fetched — so a personal For You
    /// channel would have ranked against the whole server's viewing, silently
    /// and with no log line saying so.
    #[tokio::test(start_paused = true)]
    async fn two_scopes_do_not_serve_each_other_the_wrong_history() {
        let (info, _catalog_dir) = catalog_without_tautulli();
        let history = SharedHistory::new(Some(info), None, None, Duration::from_secs(3600));

        let pierce = HistoryScope::User("Pierce".to_string());
        let madi = HistoryScope::User("Madi".to_string());

        history.current(&HistoryScope::AllUsers).await;
        tokio::time::advance(Duration::from_secs(1)).await;
        history.current(&pierce).await;
        tokio::time::advance(Duration::from_secs(1)).await;
        history.current(&madi).await;

        let all = stamp(&history, &HistoryScope::AllUsers).await.unwrap();
        let p = stamp(&history, &pierce).await.unwrap();
        let m = stamp(&history, &madi).await.unwrap();

        assert_eq!(
            p.duration_since(all),
            Duration::from_secs(1),
            "a new audience must run its own fetch, not read the pooled one",
        );
        assert_eq!(
            m.duration_since(p),
            Duration::from_secs(1),
            "and so must the next one",
        );
        assert_eq!(
            history.state.lock().await.len(),
            3,
            "three audiences asked, so three cache entries",
        );
    }

    /// The sharing #126 bought is not lost by keying the cache: two channels
    /// pointed at the *same* person still make one request between them.
    ///
    /// This is what makes "one fetch per audience" different from "one fetch per
    /// channel" — the shape a naive per-channel fix would have produced.
    #[tokio::test(start_paused = true)]
    async fn channels_sharing_one_audience_share_one_fetch() {
        let (info, _catalog_dir) = catalog_without_tautulli();
        let history = SharedHistory::new(Some(info), None, None, Duration::from_secs(3600));
        let pierce = HistoryScope::User("Pierce".to_string());

        let first = history.current(&pierce).await;
        let stamped_at = stamp(&history, &pierce).await.unwrap();

        tokio::time::advance(Duration::from_secs(1)).await;
        let second = history.current(&pierce).await;

        assert_eq!(
            stamp(&history, &pierce).await.unwrap(),
            stamped_at,
            "the second channel on this audience must be served from the cache",
        );
        assert!(
            Arc::ptr_eq(&first, &second),
            "both channels must get the same allocation, not a copy",
        );
        assert_eq!(
            history.state.lock().await.len(),
            1,
            "one audience, one cache entry, however many channels want it",
        );
    }

    /// Each audience ages on its own clock. A personal channel refreshing does
    /// not drag the server-wide pool along with it, and vice versa — otherwise
    /// adding one personal channel would multiply the station's Tautulli traffic
    /// by re-fetching every scope whenever any one of them expired.
    #[tokio::test(start_paused = true)]
    async fn one_scope_expiring_does_not_refetch_the_others() {
        let (info, _catalog_dir) = catalog_without_tautulli();
        let history = SharedHistory::new(Some(info), None, None, Duration::from_secs(60));
        let pierce = HistoryScope::User("Pierce".to_string());

        history.current(&HistoryScope::AllUsers).await;
        let all_at = stamp(&history, &HistoryScope::AllUsers).await.unwrap();

        // Half a window later the personal scope asks for the first time, so its
        // own window starts here — 30s out of step with the pooled one.
        tokio::time::advance(Duration::from_secs(30)).await;
        history.current(&pierce).await;
        let pierce_at = stamp(&history, &pierce).await.unwrap();

        // Now cross the pooled scope's expiry but not the personal one's.
        tokio::time::advance(Duration::from_secs(31)).await;
        history.current(&HistoryScope::AllUsers).await;
        history.current(&pierce).await;

        assert!(
            stamp(&history, &HistoryScope::AllUsers).await.unwrap() > all_at,
            "the pooled scope was past its window and must have refetched",
        );
        assert_eq!(
            stamp(&history, &pierce).await.unwrap(),
            pierce_at,
            "the personal scope was still inside its own window and must not have",
        );
    }

    // ---- SharedHistory::account_id (#278) ------------------------------------

    /// A pooled channel has no one account to resolve, on a station with a
    /// perfectly ordinary catalog and cache behind it.
    #[tokio::test(start_paused = true)]
    async fn account_id_for_all_users_is_always_none() {
        let (info, _catalog_dir) = catalog_without_tautulli();
        let history = SharedHistory::new(Some(info), None, None, Duration::from_secs(3600));
        assert_eq!(history.account_id(&HistoryScope::AllUsers).await, Ok(None));
    }

    /// A digit-authored `single_user` scope's Tautulli id resolves with no
    /// catalog and no Tautulli connection at all — `resolve_account_id`
    /// needs no row for that half. But #281 requires translating it into
    /// Plex's own id space before it can become `ctx.account_id`, and with no
    /// `PLEX_URL`/`PLEX_TOKEN` configured there is nothing to translate
    /// against — the digit must not be trusted as already being the Plex id,
    /// which is exactly the silent bug #281 exists to close.
    #[tokio::test(start_paused = true)]
    async fn account_id_for_a_digit_scope_fails_with_no_plex_configured() {
        let history = SharedHistory::new(None, None, None, Duration::ZERO);
        let scope = HistoryScope::User("501".to_string());
        let err = history.account_id(&scope).await.unwrap_err();
        assert!(
            err.contains("PLEX_URL") || err.contains("PLEX_TOKEN"),
            "must say why nothing could be translated: {err}",
        );
    }

    /// A name-authored `single_user` scope with nothing to resolve it from —
    /// no catalog at all, so no fetch ever ran — fails naming the user rather
    /// than defaulting to "no account", which a scorer plugin would read as
    /// "use the pooled vector".
    #[tokio::test(start_paused = true)]
    async fn account_id_for_a_name_scope_with_no_catalog_fails_naming_the_user() {
        let history = SharedHistory::new(None, None, None, Duration::ZERO);
        let scope = HistoryScope::User("Pierce".to_string());
        let err = history.account_id(&scope).await.unwrap_err();
        assert!(err.contains("Pierce"), "must name the user: {err}");
    }

    /// The same failure, this time from a real fetch that came back empty —
    /// what an unreachable Tautulli or a genuine typo in `user:` both look
    /// like from here. #278's acceptance criterion is explicit that neither
    /// may quietly resolve to "no account".
    #[tokio::test(start_paused = true)]
    async fn account_id_for_a_name_scope_with_an_empty_fetch_fails_naming_the_user() {
        let (info, _catalog_dir) = catalog_without_tautulli();
        let history = SharedHistory::new(Some(info), None, None, Duration::from_secs(3600));
        let scope = HistoryScope::User("Madi".to_string());
        let err = history.account_id(&scope).await.unwrap_err();
        assert!(err.contains("Madi"), "must name the user: {err}");
    }

    /// `account_id` reads back the same fetch `current` already ran, rather
    /// than paying for a second one — the cache stamp must not move between
    /// the two calls inside one refresh window.
    #[tokio::test(start_paused = true)]
    async fn account_id_reads_the_same_fetch_current_already_ran() {
        let (info, _catalog_dir) = catalog_without_tautulli();
        let history = SharedHistory::new(Some(info), None, None, Duration::from_secs(3600));
        let scope = HistoryScope::User("Pierce".to_string());

        history.current(&scope).await;
        let stamped_at = stamp(&history, &scope).await.unwrap();

        tokio::time::advance(Duration::from_secs(1)).await;
        let _ = history.account_id(&scope).await;

        assert_eq!(
            stamp(&history, &scope).await.unwrap(),
            stamped_at,
            "reading the account id inside the same window must not trigger a refetch",
        );
    }

    /// A catalog-free station has nothing to join rows against, so it never
    /// contacts Tautulli at all — unchanged from before #126. `current`
    /// returns before it even takes the lock, so nothing ever stamps
    /// `fetched_at`.
    #[tokio::test(start_paused = true)]
    async fn a_station_with_no_catalog_never_fetches() {
        let history = SharedHistory::new(None, None, None, Duration::ZERO);

        assert!(history.current(&HistoryScope::AllUsers).await.is_empty());
        assert!(history.current(&HistoryScope::AllUsers).await.is_empty());
        assert!(
            history.state.lock().await.is_empty(),
            "a station with no catalog must never engage the cache at all",
        );
    }

    /// `tautulli.join` must mean a join happened (#141). A station with no
    /// Tautulli configured fetches no rows, and announcing "joined watch
    /// history to the catalog" over `rows=0, keys=0, matched=0` once a minute
    /// forever reads as a wired-up server nobody has watched — a completely
    /// different situation from one that was never configured.
    #[tokio::test(start_paused = true)]
    async fn no_rows_logs_no_join_event() {
        let (joins, _guard) = count_events("tautulli.join");
        let (info, _catalog_dir) = catalog_without_tautulli();
        let history = SharedHistory::new(Some(info), None, None, Duration::from_secs(3600));

        assert!(history.current(&HistoryScope::AllUsers).await.is_empty());
        assert_eq!(
            *joins.lock().unwrap(),
            0,
            "nothing was joined, so nothing may claim a join happened",
        );
    }

    /// The other half of #141: with no rows there is nothing to open the
    /// catalog *for*. Pointing the station at a catalog file that does not
    /// exist makes the open observable — a read-only open of a missing file
    /// fails, and the failure is logged as `tautulli.catalog_unavailable`.
    /// Silence is the proof no open was attempted.
    #[tokio::test(start_paused = true)]
    async fn no_rows_never_opens_the_catalog() {
        let (failures, _guard) = count_events("tautulli.catalog_unavailable");
        let dir = tempfile::tempdir().unwrap();
        let info = Arc::new(CatalogInfo {
            path: dir.path().join("never-created.db"),
            path_index: HashMap::new(),
        });
        // The premise, checked rather than assumed: if this path were somehow
        // openable the test could pass without proving anything at all.
        assert!(
            Catalog::open_readonly(&info.path).is_err(),
            "the observation only works if opening this catalog really does fail",
        );
        let history = SharedHistory::new(Some(info), None, None, Duration::from_secs(3600));

        assert!(history.current(&HistoryScope::AllUsers).await.is_empty());
        assert_eq!(
            *failures.lock().unwrap(),
            0,
            "a fetch that produced no rows must not open a catalog reader",
        );
    }

    // ---- translate_tautulli_id_to_plex (#281) --------------------------------

    fn plex_account(id: i64, name: &str) -> PlexAccount {
        PlexAccount {
            id,
            name: name.to_string(),
        }
    }

    /// A row carrying `user_id` and `user` — built through `HistoryRow`'s own
    /// `Deserialize` impl since its fields are private to `tautulli.rs`; every
    /// field is `#[serde(default)]`, so a row naming just these two is valid.
    fn history_row(user_id: i64, user: &str) -> HistoryRow {
        serde_json::from_str(&format!(r#"{{"user_id": {user_id}, "user": {user:?}}}"#)).unwrap()
    }

    /// The common case, and the one #281 must not regress: for every account
    /// except the owner, the Tautulli id already IS the Plex id, so a direct
    /// match resolves with no row consulted at all.
    #[test]
    fn direct_match_needs_no_rows() {
        let accounts = [plex_account(1, "pierce"), plex_account(12345, "carol")];
        assert_eq!(
            translate_tautulli_id_to_plex(
                12345,
                &HistoryScope::User("12345".into()),
                &[],
                &accounts,
            ),
            Ok(12345),
        );
    }

    /// The owner's exact mismatch #281 exists to fix: Tautulli's much larger
    /// plex.tv id matches no Plex account directly, so the fallback reads the
    /// row's Tautulli username and matches it against Plex's account name
    /// instead — landing on Plex's own small server-local id.
    #[test]
    fn fallback_translates_through_the_tautulli_username() {
        let accounts = [plex_account(1, "pierce"), plex_account(12345, "carol")];
        let scope = HistoryScope::User("22831969".into());
        let rows = [history_row(22831969, "pierce")];
        assert_eq!(
            translate_tautulli_id_to_plex(22831969, &scope, &rows, &accounts),
            Ok(1)
        );
    }

    /// The same fallback for a name-authored scope: any row answers it, since
    /// Tautulli's own `user=` filter already narrowed every row to this one
    /// account.
    #[test]
    fn fallback_works_for_a_name_authored_scope_too() {
        let accounts = [plex_account(1, "pierce")];
        let scope = HistoryScope::User("pierce".into());
        let rows = [history_row(22831969, "pierce")];
        assert_eq!(
            translate_tautulli_id_to_plex(22831969, &scope, &rows, &accounts),
            Ok(1)
        );
    }

    /// Neither a direct id match nor a row to translate from — the exact
    /// combination #281's own "digit scope with no recent plays" limitation
    /// produces. Must fail naming the Tautulli id, not silently degrade.
    #[test]
    fn no_direct_match_and_no_row_fails_naming_the_tautulli_id() {
        let accounts = [plex_account(1, "pierce")];
        let scope = HistoryScope::User("22831969".into());
        let err = translate_tautulli_id_to_plex(22831969, &scope, &[], &accounts).unwrap_err();
        assert!(
            err.contains("22831969"),
            "must name the unresolved Tautulli id: {err}"
        );
    }

    /// A row names a username, but nothing in Plex's own `/accounts` carries
    /// that name — a real config error (a typo, a renamed account) must fail
    /// naming what was actually looked up, not the id that already failed.
    #[test]
    fn fallback_with_no_matching_plex_account_fails_naming_the_username() {
        let accounts = [plex_account(1, "someone_else")];
        let scope = HistoryScope::User("22831969".into());
        let rows = [history_row(22831969, "pierce")];
        let err = translate_tautulli_id_to_plex(22831969, &scope, &rows, &accounts).unwrap_err();
        assert!(err.contains("pierce"), "must name the username: {err}");
    }

    /// Id 0 must never be reachable as a direct match, even if some future
    /// data shape resolved a Tautulli id of 0 — [`valid_accounts`] excludes it
    /// upstream, but this pins the behaviour here too: an accounts list with
    /// no id-0 entry (the normal, filtered shape) simply cannot match it.
    #[test]
    fn id_zero_is_never_a_direct_match() {
        let accounts = [plex_account(1, "pierce")];
        let scope = HistoryScope::User("0".into());
        let err = translate_tautulli_id_to_plex(0, &scope, &[], &accounts).unwrap_err();
        assert!(err.contains('0'));
    }
}

/// Whether a generation asks for a watch history at all (#131) — the decision
/// [`history_catalog`] makes once, before any channel ticks.
#[cfg(test)]
mod history_catalog_tests {
    use super::*;

    /// A catalog `history_catalog` may hand on. Nothing in this module opens
    /// it: the decision is made from the channel list alone, so the path only
    /// has to be distinguishable from `None`.
    fn some_catalog() -> Arc<CatalogInfo> {
        Arc::new(CatalogInfo {
            path: PathBuf::from("/catalog.db"),
            path_index: HashMap::new(),
        })
    }

    /// A one-block channel whose single pool draws from `source` — either
    /// `plugin: <path>` or `expr: <cel>`.
    fn channel(name: &str, source: &str) -> LoadedChannel {
        let yaml = format!(
            "rule:\n  blocks:\n    - pools:\n        - name: p\n          {source}\n      pattern:\n        - pool: p\n          take: 1\n"
        );
        LoadedChannel {
            name: name.to_string(),
            config_path: PathBuf::from(format!("{name}.yaml")),
            output_folder: PathBuf::from(name),
            config: serde_norway::from_str(&yaml).unwrap(),
        }
    }

    /// The acceptance criterion: every channel drawing from a CEL expression
    /// means nobody would read a watch history, so the generation is handed no
    /// catalog and makes no Tautulli request — even though the station has one
    /// configured.
    #[test]
    fn a_station_with_no_plugin_pool_is_given_no_catalog_to_join_against() {
        let channels = vec![
            channel("movies", "expr: 'item.type == \"movie\"'"),
            channel("shows", "expr: 'item.type == \"episode\"'"),
        ];

        assert!(history_catalog(&channels, Some(&some_catalog())).is_none());
    }

    /// One plugin pool on one channel is a reader, and restores the fetch for
    /// the whole station.
    #[test]
    fn one_plugin_pool_anywhere_restores_the_fetch() {
        let channels = vec![
            channel("movies", "expr: 'item.type == \"movie\"'"),
            channel("foryou", "plugin: taste-engine.rhai"),
        ];

        assert!(history_catalog(&channels, Some(&some_catalog())).is_some());
    }

    /// A station with no `catalog_path` is unchanged — there is nothing to join
    /// rows against, plugin pool or not.
    #[test]
    fn a_station_with_no_catalog_stays_catalog_free() {
        let channels = vec![channel("foryou", "plugin: taste-engine.rhai")];

        assert!(history_catalog(&channels, None).is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHANNEL_BODY: &str = r#"
window_days = 1
chunk_hours = 6
roll_interval = "60s"
retention_days = 1

[[rule.blocks]]
mode = "all"
order = "manual"

[[rule.blocks.entries]]
kind = "item"
in_point = "0s"
out_point = "30s"

[rule.blocks.entries.source]
kind = "lavfi"
params = "testsrc=size=1280x720:rate=30 [out0]"
"#;

    /// Write a station.toml (with the given tz) plus a lavfi channel into a
    /// fresh tempdir and return the dir handle and the station path. The
    /// channel's output_folder points inside the tempdir so `prepare_generation`
    /// mkdir's there rather than polluting the crate directory.
    fn write_station(tz: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let out_base = dir.path().join("out");
        let station = format!(
            "tz = {:?}\noutput_base = {:?}\nchannels = [\"channel.toml\"]\n",
            tz,
            out_base.to_string_lossy(),
        );
        // No output_folder — the channel's identity is its file stem
        // ("channel"), so it writes to {out_base}/channel inside the tempdir.
        std::fs::write(dir.path().join("station.toml"), station).unwrap();
        std::fs::write(dir.path().join("channel.toml"), CHANNEL_BODY).unwrap();
        let path = dir.path().join("station.toml");
        (dir, path)
    }

    #[tokio::test]
    async fn prepare_generation_accepts_valid_config() {
        let (_dir, path) = write_station("America/Chicago");
        let station = crate::config::load(&path).expect("valid config should load");
        prepare_generation(&station)
            .await
            .expect("valid config should prepare");
    }

    #[tokio::test]
    async fn catalog_disabled_when_no_path() {
        // A station without `catalog_path` stays catalog-free — today's behavior.
        let (_dir, path) = write_station("UTC");
        let station = crate::config::load(&path).unwrap();
        assert!(
            open_and_ingest_catalog(&station, None)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn catalog_opens_and_ingests_when_path_set() {
        let dir = tempfile::tempdir().unwrap();
        let out_base = dir.path().join("out");
        let db = dir.path().join("catalog.db");
        let station_toml = format!(
            "tz = \"UTC\"\noutput_base = {:?}\ncatalog_path = {:?}\nchannels = [\"channel.toml\"]\n",
            out_base.to_string_lossy(),
            db.to_string_lossy(),
        );
        std::fs::write(dir.path().join("station.toml"), station_toml).unwrap();
        std::fs::write(dir.path().join("channel.toml"), CHANNEL_BODY).unwrap();
        let station = crate::config::load(&dir.path().join("station.toml")).unwrap();

        // No source_roots, no identity_roots, no Plex connection → a clean,
        // empty ingest that still opens the db and returns a shareable
        // handle. Passing `None` is what
        // keeps this hermetic: the dev shell exports `PLEX_URL`/`PLEX_TOKEN`, and
        // an ingest that read them itself would hit a live server. Nothing here
        // touches the process environment, so nothing races another test thread
        // reading it (#132).
        let catalog = open_and_ingest_catalog(&station, None).await.unwrap();
        assert!(catalog.is_some());
        assert!(db.exists());
    }

    #[tokio::test]
    async fn prepare_generation_rejects_invalid_timezone() {
        // A non-empty-but-bogus tz passes `config::load`'s `validate_station`
        // (which only checks non-empty) and is caught by the timezone parse in
        // `prepare_generation` — the gate that, on reload, reverts to the
        // previous config instead of running a broken one. tz is parsed before
        // the mkdir, so this never touches the filesystem.
        let (_dir, path) = write_station("Totally/Bogus/Zone");
        let station = crate::config::load(&path).expect("bogus tz still parses as config");
        assert!(prepare_generation(&station).await.is_err());
    }
}

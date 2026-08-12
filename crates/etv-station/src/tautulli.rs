//! Tautulli watch history (#74) — the station's one source of "what has been
//! watched on this server lately".
//!
//! Read at most once per station tick *per audience* and shared by every channel
//! that wants that audience (#126, #112), then handed to a scorer plugin as a
//! signal. An audience is a [`HistoryScope`]: every user pooled together with no
//! user dimension, which is the default and what #74 shipped, or one named
//! account. It is never a query filter field: a channel cannot say "movies
//! nobody has watched" in CEL, because watch activity belongs to the algorithm's
//! judgment, not to the catalog.
//!
//! Connection details come from the environment — `TAUTULLI_URL` and
//! `TAUTULLI_API_KEY` — and never from tracked config, so a deployment supplies
//! them as container environment variables or Docker secrets. The environment is
//! read in exactly one place, [`credentials_from_env`], and everything below it
//! takes the connection as an argument (#132).
//!
//! # Failure is not fatal
//!
//! [`fetch_rows`] returns an empty history rather than an error when Tautulli is
//! unreachable, and a station with no credentials never calls it at all. A
//! plugin still has release dates, `last_seen`, tags, and the channel's own
//! recently-aired tail to rank on, so an outage degrades the ranking instead of
//! stopping a channel that is otherwise fine. The reason is logged on each fetch
//! so the degradation is visible.

use std::time::Duration;

use crate::catalog::{Catalog, Source};
use crate::score::WatchEvent;

const URL_VAR: &str = "TAUTULLI_URL";
const KEY_VAR: &str = "TAUTULLI_API_KEY";

/// How long to wait on Tautulli before giving up and generating without it.
const TIMEOUT: Duration = Duration::from_secs(10);

/// How many history rows to ask for. Tautulli returns newest-first, so this is
/// a recency window expressed as a row count — the API has no "since" filter on
/// `get_history`.
const HISTORY_ROWS: usize = 1000;

/// The `(url, api key)` pair to fetch against, or `None` when the deployment has
/// not configured Tautulli — which is normal, not an error.
///
/// This is the only place in the crate that reads `TAUTULLI_URL` /
/// `TAUTULLI_API_KEY`, and it is called once per generation, at startup. Keeping
/// the read here and passing the result down is what makes the fetch testable:
/// a test hands its `SharedHistory` a `None` instead of deleting the variables
/// out of the running process. `std::env::remove_var` is unsound in Rust 2024
/// the moment any other thread reads the environment, and `cargo test` runs a
/// module's tests concurrently, so the old approach was one added env-reading
/// test away from a real data race (#132).
pub fn credentials_from_env() -> Option<(String, String)> {
    match (std::env::var(URL_VAR), std::env::var(KEY_VAR)) {
        (Ok(u), Ok(k)) if !u.is_empty() && !k.is_empty() => Some((u, k)),
        _ => {
            tracing::debug!(
                event = "tautulli.skip",
                "{URL_VAR}/{KEY_VAR} unset; generating with no watch history",
            );
            None
        }
    }
}

/// Whose history to ask for (#112).
///
/// Two jobs in one type: it is the `user`/`user_id` parameter on the request,
/// and it is the key the daemon caches the result under. Those have to be the
/// same value or the cache would serve one person's ranking to another, so
/// there is deliberately no way to build a request without stating the scope.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HistoryScope {
    /// Every user, pooled — the unfiltered server-wide call (#74).
    AllUsers,
    /// One account, named by Tautulli username or numeric user id.
    User(String),
}

impl HistoryScope {
    /// The query parameter this scope adds to `get_history`, if any.
    ///
    /// Tautulli takes either `user_id` (numeric) or `user` (the username), and
    /// a config says which by what it wrote: `"1234567"` is an id, `"bob"` is
    /// a name. Inferring beats a second config field that could disagree with
    /// the first.
    ///
    /// The test is "every character is an ASCII digit", deliberately narrower
    /// than `parse::<i64>()`. A signed parse also accepts `"+5"` and `"-3"`,
    /// neither of which is a Tautulli id, and routing them to `user_id` would
    /// send a request that matches nobody and returns an empty history — which
    /// looks exactly like a person who has not watched anything.
    ///
    /// A username made only of digits is still read as an id; there is no way to
    /// tell those apart from one field, and nobody on this server has one.
    ///
    /// The value is *not* escaped here — [`request_rows`] passes it through
    /// `ureq`'s `.query()`, which percent-encodes it. That matters for real
    /// accounts on this server: a username like `Sam + Alex` written straight
    /// into a URL would arrive as `Jacob   Liv`, because `+` decodes to a space
    /// in a query string, and would match no user.
    fn query_param(&self) -> Option<(&'static str, &str)> {
        match self {
            HistoryScope::AllUsers => None,
            HistoryScope::User(u) if is_numeric_id(u) => Some(("user_id", u)),
            HistoryScope::User(u) => Some(("user", u)),
        }
    }

    /// How this scope reads in a log line.
    fn label(&self) -> &str {
        match self {
            HistoryScope::AllUsers => "all_users",
            HistoryScope::User(u) => u,
        }
    }
}

/// Every character an ASCII digit, and at least one of them — the one test
/// [`HistoryScope::query_param`] and [`resolve_account_id`] both use to tell a
/// config-authored numeric id from a username, kept in one place so the two
/// cannot drift apart on what counts as "digits" (see `query_param`'s doc for
/// why a signed-integer parse is deliberately not used instead).
fn is_numeric_id(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

/// The numeric account id `scope` resolves to, going by `rows` — the same
/// rows [`fetch_rows`] just returned for this exact scope (#278).
///
/// `Ok(None)` for [`HistoryScope::AllUsers`]: there is no one account to
/// resolve, and every pooled channel must stay untouched by this. A `User`
/// scope written as plain digits already *is* the id
/// ([`HistoryScope::query_param`]'s own inference) and is parsed straight
/// back with no row consulted — an account with zero recent plays must not
/// read as "no such account" just because this fetch came back empty. A
/// `User` scope written as a name resolves from the first row's `user_id`,
/// since Tautulli's own `user=` filter already narrowed every returned row to
/// that one account; `Err`, naming the user, when nothing came back to
/// resolve from — whether the name is wrong or Tautulli was unreachable this
/// fetch, either way there is no account to rank against, and a scorer
/// plugin must never quietly fall back to the pooled vector instead (#278's
/// whole point).
///
/// Either branch hands back a Tautulli-space id, not a plex-db-ex-space one —
/// see [`HistoryRow::user_id`]'s doc for the one account those two id spaces
/// disagree on. A digit-authored `user:` is just as exposed to that gap as a
/// name is: it is never checked against the store, so a channel scoped to the
/// Plex server owner's own numeric id resolves successfully here and then
/// finds nothing in `plays.plex_account_id` downstream.
pub fn resolve_account_id(
    scope: &HistoryScope,
    rows: &[HistoryRow],
) -> Result<Option<i64>, String> {
    match scope {
        HistoryScope::AllUsers => Ok(None),
        HistoryScope::User(u) if is_numeric_id(u) => u
            .parse::<i64>()
            .map(Some)
            .map_err(|e| format!("user {u:?} does not fit a 64-bit account id: {e}")),
        HistoryScope::User(u) => rows
            .iter()
            .find_map(|r| r.user_id)
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "no account found for user {u:?} (wrong name, or Tautulli was \
                     unreachable this fetch)"
                )
            }),
    }
}

/// Raw history rows straight off the API, before any catalog join.
///
/// Split from [`join`] because the two halves want different threads: this one
/// blocks on the network (`ureq`) and must run under `spawn_blocking`, while
/// the join needs the catalog mutex and no network at all. Never returns an
/// error — see the module docs.
pub fn fetch_rows(url: &str, key: &str, scope: &HistoryScope) -> Vec<HistoryRow> {
    match request_rows(url, key, scope) {
        Ok(rows) => {
            tracing::info!(
                event = "tautulli.history",
                rows = rows.len(),
                scope = scope.label(),
                "fetched watch history",
            );
            rows
        }
        Err(e) => {
            tracing::warn!(
                event = "tautulli.unavailable",
                error = %e,
                scope = scope.label(),
                "watch history unavailable; generating without it",
            );
            Vec::new()
        }
    }
}

/// Join raw rows to catalog entries.
///
/// Rows that match nothing in the catalog are dropped: Tautulli remembers plays
/// of media that has since been removed, and an `entry_id` a plugin cannot look
/// up is noise.
pub fn join(catalog: &Catalog, rows: Vec<HistoryRow>) -> Vec<WatchEvent> {
    resolve(catalog, rows)
}

/// One `get_history` row, narrowed to the fields that matter.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct HistoryRow {
    /// The Plex `ratingKey`, which is also the `source_id` of the entry's
    /// `plex` provenance row — the join back to the catalog.
    #[serde(default)]
    rating_key: Option<serde_json::Value>,
    /// Unix seconds when playback stopped. Rows for an in-flight stream have
    /// no stop time yet.
    #[serde(default)]
    stopped: Option<i64>,
    /// The account's display name, which is what a person would recognise
    /// (#113). Tautulli sets it to the username when no friendly name is
    /// configured, and both are absent on an anonymised row.
    #[serde(default)]
    friendly_name: Option<String>,
    /// The account name, used when `friendly_name` is missing or blank.
    #[serde(default)]
    user: Option<String>,
    /// The account's numeric id, in Tautulli's own id space. Present on every
    /// row Tautulli sends — `#[serde(default)]` only guards a row shape this
    /// crate has not seen — and it is what [`resolve_account_id`] hands a
    /// `single_user` channel's scorer plugin as `ctx.account_id` (#278).
    ///
    /// Not guaranteed to equal `plays.plex_account_id` in the plex-db-ex
    /// store: the two id spaces agree for every account except the Plex
    /// server's owner, whom Plex and Tautulli report under two different ids
    /// (plex-db-ex ADR-0010; `enrich-tautulli-plays` resolves that one case
    /// via a runtime name join before writing `plays`). Nothing on this side
    /// performs that translation — a `single_user` channel scoped to the
    /// owner's own Tautulli username resolves to Tautulli's id here, not the
    /// store's, and `taste_vector_for` would then match no plays at all.
    #[serde(default)]
    user_id: Option<i64>,
}

impl HistoryRow {
    /// Who to credit this row to, or `None` when it names nobody.
    ///
    /// `friendly_name` first: on this server it is what distinguishes `Bob Example`
    /// from the account spelled `bob`. Blank strings are treated as absent —
    /// Tautulli sends `""` rather than omitting the key.
    fn watcher(&self) -> Option<String> {
        [self.friendly_name.as_deref(), self.user.as_deref()]
            .into_iter()
            .flatten()
            .map(str::trim)
            .find(|s| !s.is_empty())
            .map(str::to_string)
    }
}

/// Strip the API key out of anything on its way to a log.
///
/// Tautulli authenticates by query parameter, so the key is part of the request
/// URL — and `ureq` puts the URL it was given into its error's `Display`. That
/// error is what [`fetch_rows`] logs on every failed poll, so without this the
/// key lands in the daemon log in plaintext, once per tick, for as long as the
/// outage lasts.
fn redact_key(e: impl std::fmt::Display, key: &str) -> String {
    let text = e.to_string();
    if key.is_empty() {
        // `str::replace` with an empty pattern splices the replacement between
        // every character; never let an empty key reach it.
        return text;
    }
    text.replace(key, "<redacted>")
}

fn request_rows(url: &str, key: &str, scope: &HistoryScope) -> Result<Vec<HistoryRow>, String> {
    let endpoint = format!(
        "{}/api/v2?apikey={}&cmd=get_history&length={HISTORY_ROWS}",
        url.trim_end_matches('/'),
        key
    );

    let mut request = ureq::get(&endpoint).timeout(TIMEOUT);
    // Percent-encoded by `ureq`, which is why the scope is appended here rather
    // than formatted into the string above — see `HistoryScope::query_param`.
    if let Some((param, value)) = scope.query_param() {
        request = request.query(param, value);
    }

    let body: serde_json::Value = request
        .call()
        .map_err(|e| format!("request failed: {}", redact_key(e, key)))?
        .into_json()
        .map_err(|e| format!("decode response: {}", redact_key(e, key)))?;

    let data = body
        .get("response")
        .and_then(|r| r.get("data"))
        .and_then(|d| d.get("data"))
        .ok_or_else(|| "response has no response.data.data array".to_string())?;

    serde_json::from_value(data.clone()).map_err(|e| format!("decode history rows: {e}"))
}

/// Join history rows to catalog entries by Plex `ratingKey`, emitting one
/// [`WatchEvent`] per row that resolves.
///
/// A rewatch is deliberately two events — a plugin ranking on play counts needs
/// to see both — which is why the health numbers logged below are counted per
/// distinct `ratingKey` instead of per row.
fn resolve(catalog: &Catalog, rows: Vec<HistoryRow>) -> Vec<WatchEvent> {
    let row_count = rows.len();
    // Counted per distinct `ratingKey`, not per row, so `matched` lines up with
    // `SELECT count(*) FROM entry_sources WHERE source_id IN (…)` — which
    // dedupes the key list. Per-row counts would exceed that query by however
    // many rewatches the batch happens to contain, making an honest join
    // indistinguishable from a broken one.
    let mut looked_up: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut matched: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<WatchEvent> = Vec::new();
    for row in rows {
        // Tautulli types rating_key inconsistently across versions — a JSON
        // number in some, a string in others — so accept either rather than
        // silently matching nothing.
        let rating_key = match row.rating_key.as_ref() {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            _ => continue,
        };
        let Some(watched_at) = row.stopped else {
            continue;
        };
        looked_up.insert(rating_key.clone());
        match catalog.entry_id_for_source(Source::Plex, &rating_key) {
            Ok(Some(entry_id)) => {
                matched.insert(rating_key);
                out.push(WatchEvent {
                    entry_id,
                    watched_at,
                    watcher: row.watcher(),
                });
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    event = "tautulli.lookup_failed",
                    rating_key = %rating_key,
                    error = %e,
                    "skipping a history row",
                );
            }
        }
    }

    // The join's health on one line. A per-row warn only fires on a lookup
    // error, so without this a total join failure — every ratingKey missing
    // from `entry_sources` — reads exactly like a healthy server nobody has
    // watched lately: rows in, nothing out, no warning.
    //
    // `keys - matched` is exactly the set of misses to explain.
    tracing::info!(
        event = "tautulli.join",
        rows = row_count,
        keys = looked_up.len(),
        matched = matched.len(),
        "joined watch history to the catalog",
    );

    out
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

    use super::*;
    use crate::catalog::{Entry, EntrySource};

    /// The counts on a `tautulli.join` event, pulled off a `tracing` event
    /// without a formatter in the way.
    #[derive(Default)]
    struct JoinFields {
        event: Option<String>,
        rows: Option<u64>,
        keys: Option<u64>,
        matched: Option<u64>,
    }

    impl Visit for JoinFields {
        fn record_u64(&mut self, field: &Field, value: u64) {
            match field.name() {
                "rows" => self.rows = Some(value),
                "keys" => self.keys = Some(value),
                "matched" => self.matched = Some(value),
                _ => {}
            }
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "event" {
                self.event = Some(value.to_string());
            }
        }

        fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
    }

    /// Collects every `tautulli.join` event's `(rows, keys, matched)` triple.
    struct CaptureJoins(Arc<Mutex<Vec<(u64, u64, u64)>>>);

    impl<S: tracing::Subscriber> Layer<S> for CaptureJoins {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let mut fields = JoinFields::default();
            event.record(&mut fields);
            if fields.event.as_deref() != Some("tautulli.join") {
                return;
            }
            let (Some(rows), Some(keys), Some(matched)) =
                (fields.rows, fields.keys, fields.matched)
            else {
                panic!("a tautulli.join event must carry rows, keys and matched");
            };
            self.0.lock().unwrap().push((rows, keys, matched));
        }
    }

    /// Run [`resolve`] under a subscriber that captures `tautulli.join`.
    ///
    /// **Every** test in this module goes through here, including the ones that
    /// only assert on the returned events. `tracing` caches callsite interest
    /// process-globally: the first time `tautulli.join` is hit with no
    /// subscriber installed, the callsite is registered against `NoSubscriber`
    /// and cached as `Interest::never()` — which then silently blinds a test
    /// running concurrently on another thread that *did* install one. Keeping
    /// every call under a subscriber means the callsite is never registered
    /// against the no-op, so the capture cannot race.
    fn resolve_capturing(
        catalog: &Catalog,
        rows: Vec<HistoryRow>,
    ) -> (Vec<WatchEvent>, Vec<(u64, u64, u64)>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(CaptureJoins(Arc::clone(&seen)));
        let events = tracing::subscriber::with_default(subscriber, || resolve(catalog, rows));
        let logged = seen.lock().unwrap().clone();
        (events, logged)
    }

    /// Just the watch events, for tests that do not assert on the log.
    fn resolved(catalog: &Catalog, rows: Vec<HistoryRow>) -> Vec<WatchEvent> {
        resolve_capturing(catalog, rows).0
    }

    fn seeded() -> Catalog {
        let c = Catalog::open_in_memory().unwrap();
        c.upsert_entry(&Entry::new("m1", "movie", "Alpha", Source::Plex))
            .unwrap();
        c.add_source(&EntrySource {
            source: Source::Plex,
            source_id: "plex-1".into(),
            entry_id: "m1".into(),
            playback_path: "/media/alpha.mkv".into(),
            last_seen: None,
        })
        .unwrap();
        c
    }

    fn row(key: serde_json::Value, stopped: Option<i64>) -> HistoryRow {
        HistoryRow {
            rating_key: Some(key),
            stopped,
            friendly_name: None,
            user: None,
            user_id: None,
        }
    }

    /// The same row, credited to someone (#113).
    fn row_by(
        key: serde_json::Value,
        stopped: Option<i64>,
        friendly_name: Option<&str>,
        user: Option<&str>,
    ) -> HistoryRow {
        HistoryRow {
            rating_key: Some(key),
            stopped,
            friendly_name: friendly_name.map(str::to_string),
            user: user.map(str::to_string),
            user_id: None,
        }
    }

    /// A row carrying only `user_id` (#278) — what [`resolve_account_id`]
    /// reads, independent of the display-name fields [`row_by`] exercises.
    fn row_with_account(user_id: i64) -> HistoryRow {
        HistoryRow {
            rating_key: Some("plex-1".into()),
            stopped: Some(100),
            friendly_name: None,
            user: None,
            user_id: Some(user_id),
        }
    }

    /// `friendly_name` wins because it is the name a person recognises — on
    /// this server the account spelled `bob` displays as `Bob Example`.
    #[test]
    fn the_friendly_name_is_preferred_over_the_account_name() {
        let got = resolved(
            &seeded(),
            vec![row_by(
                "plex-1".into(),
                Some(100),
                Some("Bob Example"),
                Some("bob"),
            )],
        );
        assert_eq!(got[0].watcher.as_deref(), Some("Bob Example"));
    }

    /// Tautulli sends `""` rather than omitting the key, so a blank friendly
    /// name has to fall through to the username instead of crediting nobody.
    #[test]
    fn a_blank_friendly_name_falls_through_to_the_username() {
        let got = resolved(
            &seeded(),
            vec![row_by("plex-1".into(), Some(100), Some("  "), Some("bob"))],
        );
        assert_eq!(got[0].watcher.as_deref(), Some("bob"));
    }

    #[test]
    fn a_row_naming_nobody_credits_nobody() {
        let got = resolved(&seeded(), vec![row("plex-1".into(), Some(100))]);
        assert_eq!(got[0].watcher, None);
    }

    #[test]
    fn joins_rows_to_entries_by_rating_key() {
        let got = resolved(&seeded(), vec![row("plex-1".into(), Some(100))]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].entry_id, "m1");
        assert_eq!(got[0].watched_at, 100);
    }

    #[test]
    fn accepts_a_numeric_rating_key() {
        let c = Catalog::open_in_memory().unwrap();
        c.upsert_entry(&Entry::new("m2", "movie", "Beta", Source::Plex))
            .unwrap();
        c.add_source(&EntrySource {
            source: Source::Plex,
            source_id: "4242".into(),
            entry_id: "m2".into(),
            playback_path: "/media/beta.mkv".into(),
            last_seen: None,
        })
        .unwrap();
        let got = resolved(&c, vec![row(serde_json::json!(4242), Some(7))]);
        assert_eq!(got.len(), 1, "a numeric ratingKey must still match");
        assert_eq!(got[0].entry_id, "m2");
    }

    #[test]
    fn drops_rows_that_match_nothing_in_the_catalog() {
        // Tautulli remembers plays of media that has since been removed.
        assert!(resolved(&seeded(), vec![row("gone".into(), Some(1))]).is_empty());
    }

    #[test]
    fn drops_rows_still_playing() {
        assert!(resolved(&seeded(), vec![row("plex-1".into(), None)]).is_empty());
    }

    /// The server-wide scope must add nothing at all — that is what keeps #74's
    /// unfiltered call byte-for-byte what it was before #112.
    #[test]
    fn the_pooled_scope_adds_no_filter() {
        assert_eq!(HistoryScope::AllUsers.query_param(), None);
    }

    /// Which parameter a scope becomes is inferred from what was written, since
    /// Tautulli takes the numeric id and the username under different names.
    #[test]
    fn a_numeric_user_is_an_id_and_a_name_is_a_name() {
        assert_eq!(
            HistoryScope::User("7654321".into()).query_param(),
            Some(("user_id", "7654321")),
            "a value that parses as an integer is a Tautulli user_id",
        );
        assert_eq!(
            HistoryScope::User("carol".into()).query_param(),
            Some(("user", "carol")),
            "anything else is a username",
        );
        // Real account on this server, and the reason the inference is not
        // "starts with a digit".
        assert_eq!(
            HistoryScope::User("Sam + Alex".into()).query_param(),
            Some(("user", "Sam + Alex")),
        );
    }

    /// A signed-integer parse would accept these and send them as `user_id`,
    /// where they match no account and return an empty history — the same
    /// silent nothing a real person who stopped watching would produce. Only
    /// runs of plain digits are ids.
    #[test]
    fn a_signed_number_is_a_name_not_an_id() {
        for name in ["+5", "-3", " 12", "12 ", "1_2", ""] {
            assert_eq!(
                HistoryScope::User(name.into()).query_param(),
                Some(("user", name)),
                "{name:?} is not a Tautulli user_id",
            );
        }
    }

    /// A username with a `+` or a space has to survive the trip as itself.
    ///
    /// Written straight into a query string, `Sam + Alex` arrives at Tautulli as
    /// `Jacob   Liv` — `+` is the query-string encoding of a space — and matches
    /// no user, so the channel would rank against an empty history and look
    /// merely unlucky. This pins the encoding `request_rows` relies on.
    #[test]
    fn a_username_with_punctuation_survives_encoding() {
        let scope = HistoryScope::User("Sam + Alex".into());
        let (param, value) = scope.query_param().unwrap();
        let url = ureq::get("http://tautulli.invalid/api/v2")
            .query(param, value)
            .request_url()
            .unwrap()
            .as_url()
            .to_string();
        assert!(
            url.contains("user=Sam+%2B+Alex"),
            "the literal + must be percent-encoded, not left to mean a space: {url}"
        );
    }

    // ---- resolve_account_id (#278) ------------------------------------------

    /// A pooled channel has no one account to resolve, regardless of what
    /// rows happen to be lying around — `resolve_account_id` never even looks
    /// at them for `AllUsers`.
    #[test]
    fn the_pooled_scope_resolves_to_no_account() {
        assert_eq!(
            resolve_account_id(&HistoryScope::AllUsers, &[row_with_account(501)]),
            Ok(None),
        );
    }

    /// A config written as digits already is the id (`HistoryScope::param`'s
    /// own inference) — no row is consulted, so an account with zero recent
    /// plays still resolves rather than reading as "no such account".
    #[test]
    fn a_digit_scope_resolves_from_the_config_alone() {
        assert_eq!(
            resolve_account_id(&HistoryScope::User("7654321".into()), &[]),
            Ok(Some(7654321)),
        );
    }

    /// A name scope resolves from the first row's `user_id` — Tautulli's own
    /// `user=` filter already narrowed every returned row to that one
    /// account, so the first row's id is every row's id.
    #[test]
    fn a_name_scope_resolves_from_the_first_rows_account_id() {
        assert_eq!(
            resolve_account_id(
                &HistoryScope::User("carol".into()),
                &[row_with_account(501), row_with_account(501)],
            ),
            Ok(Some(501)),
        );
    }

    /// The acceptance criterion this ticket exists to satisfy: a name that
    /// resolved to no rows at all fails naming the user, rather than handing
    /// back `None` for a scorer plugin to quietly treat as "use the pooled
    /// vector instead".
    #[test]
    fn a_name_scope_with_no_matching_rows_fails_naming_the_user() {
        let err = resolve_account_id(&HistoryScope::User("nobody".into()), &[]).unwrap_err();
        assert!(
            err.contains("nobody"),
            "the failure must name the user: {err}"
        );
    }

    /// A row that matched the filter but carries no `user_id` (an anonymised
    /// row, or a Tautulli version that omits it) must not silently resolve to
    /// nothing being wrong — it is exactly as unresolved as no rows at all.
    #[test]
    fn a_name_scope_with_rows_naming_nobody_still_fails() {
        let unattributed = row("plex-1".into(), Some(100));
        let err =
            resolve_account_id(&HistoryScope::User("carol".into()), &[unattributed]).unwrap_err();
        assert!(err.contains("carol"));
    }

    #[test]
    fn keeps_the_api_key_out_of_a_failure_message() {
        // Shaped like a real `ureq` transport error: the whole request URL,
        // key and all, followed by the transport's own complaint.
        let key = "s3cr3t-tautulli-key";
        let raw = format!(
            "http://tautulli.invalid:8181/api/v2?apikey={key}&cmd=get_history&length=1000: \
             Connection Failed: Connect error: Connection refused (os error 61)"
        );
        let got = redact_key(raw, key);
        assert!(
            !got.contains(key),
            "the api key must not survive into a log"
        );
        assert!(
            got.contains("apikey=<redacted>"),
            "the rest of the message must survive so the error stays diagnosable: {got}"
        );
    }

    #[test]
    fn an_empty_key_is_left_alone() {
        // `str::replace` with an empty pattern would splice the replacement
        // between every character, turning the message into garbage.
        assert_eq!(redact_key("request failed", ""), "request failed");
    }

    #[test]
    fn logs_one_join_event_carrying_rows_keys_and_matched() {
        let (events, logged) = resolve_capturing(
            &seeded(),
            vec![
                row("plex-1".into(), Some(100)),
                // A rewatch: a second row for a ratingKey already counted.
                row("plex-1".into(), Some(200)),
                // Removed from the library since it was played.
                row("gone".into(), Some(1)),
                // Still playing, so it never reaches the catalog lookup and
                // never counts towards `keys`.
                row("plex-1".into(), None),
            ],
        );

        // 4 rows in; 2 distinct ratingKeys reached the lookup (plex-1, gone);
        // 1 of those resolved. `keys - matched == 1` is the single miss to
        // explain — the rewatch does not inflate any of the three.
        assert_eq!(logged, vec![(4, 2, 1)]);

        // The plugin-facing events are untouched by the per-key counting: two
        // plays of plex-1 are still two events, so a scorer can count plays.
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e.entry_id == "m1"));
    }

    #[test]
    fn a_rewatch_never_pushes_matched_past_the_distinct_key_count() {
        // The reconciliation in #114 compares `matched` against
        // `SELECT count(*) FROM entry_sources WHERE source_id IN (…)`, which
        // dedupes the key list. Ten plays of one film must still read as one.
        let rows = (0..10)
            .map(|i| row("plex-1".into(), Some(i)))
            .collect::<Vec<_>>();
        let (events, logged) = resolve_capturing(&seeded(), rows);
        assert_eq!(logged, vec![(10, 1, 1)]);
        assert_eq!(events.len(), 10, "every play is still its own event");
    }

    #[test]
    fn logs_the_join_event_even_when_nothing_matched() {
        // The case the event exists for: a broken join and an idle server are
        // indistinguishable without a line that says zero of many matched.
        let (_, logged) = resolve_capturing(&seeded(), vec![row("gone".into(), Some(1))]);
        assert_eq!(logged, vec![(1, 1, 0)]);
    }
}

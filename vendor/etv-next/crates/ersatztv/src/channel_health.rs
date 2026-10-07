//! Per-channel failure accounting behind the "this channel is not coming back"
//! signal.
//!
//! A stalled channel already recovers on its own: the worker tears itself down
//! after 60s of producing nothing while a viewer watches, and the session route
//! respawns it on the next poll. That path is deliberately silent, because it
//! heals in about a minute and saying anything would just make a transient blip
//! look like a fault.
//!
//! What this tracks is the case that does NOT heal — a channel whose worker
//! keeps dying under a viewer, because its media root is gone, ffmpeg cannot
//! start, or the same wedge recurs every cycle. Left alone that becomes an
//! invisible respawn treadmill: the worker dies, the next poll respawns it, and
//! a player sees an endless stutter with nothing to explain it.
//!
//! Two rules do the work:
//!
//! - **A failure is an exit with a viewer still attached.** Freshness of the
//!   heartbeat is what separates "died on someone" from an ordinary idle exit
//!   after the last viewer left, which must never count — otherwise every
//!   channel would march toward `failed` just by being watched occasionally.
//! - **Retries never stop, they only slow down.** The mark is for signalling,
//!   not for giving up. A channel broken by something transient (a stale mount,
//!   an unreachable Plex) heals with no intervention once the cause clears;
//!   backoff only keeps a permanently broken one from respawning every poll.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use time::OffsetDateTime;

/// Consecutive viewer-visible failures before a channel is declared failed and
/// starts signalling. Two would fire on an unlucky pair (a stall that recurs
/// once); three means the channel has failed every attempt across the whole
/// backoff ramp below, which no working channel does.
pub const FAILURE_THRESHOLD: u32 = 3;

/// How long a worker must survive for its run to count as healthy rather than a
/// failure, even though a viewer was watching when it died.
///
/// Reaching `.ready` is NOT the test, which is the trap this replaced: a worker
/// that starts, serves, and dies a minute later reaches ready every cycle, so
/// resetting there meant a channel dying repeatedly sat at one failure forever
/// and was never declared failed.
///
/// This is the fallback, not the primary test. It only applies to exits the
/// worker could not classify; a stall it did classify arrives as `stalled` and
/// counts no matter how long the run lasted. That split matters because the
/// stall watchdog fires 60s after the LAST segment, not 60s after startup — a
/// session that hands out ten segments and then wedges is torn down four-odd
/// minutes in, so measuring it against any threshold short enough to be useful
/// for genuine crash loops would still have called it healthy.
const HEALTHY_UPTIME: Duration = Duration::from_secs(180);

/// Backoff schedule, indexed by how many failures have accumulated past the
/// threshold. Starts well above the ~4s poll interval that caused the treadmill
/// and caps at five minutes so a channel fixed at 3am is serving by 3:05 without
/// anyone touching it.
const BACKOFF: [Duration; 4] = [
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(300),
];

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChannelHealth {
    /// Consecutive exits that happened while a viewer was still watching.
    pub consecutive_failures: u32,
    /// When a respawn may next be attempted. `None` means "any time".
    pub retry_after: Option<Instant>,
    /// The most recent counted failure. Kept after the count clears, so the
    /// health endpoint can still say what last went wrong on a channel that
    /// has since recovered.
    pub last_failure: Option<FailureRecord>,
}

/// One counted failure: when it happened and the worker's own account of why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureRecord {
    pub at: OffsetDateTime,
    pub cause: String,
}

/// What one recorded exit did to a channel's health — what the caller logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitVerdict {
    /// Counted as a failure. `crossed` is true only on the exit that took the
    /// channel past [`FAILURE_THRESHOLD`]; `retry_in` is how long until a
    /// respawn is allowed again, `None` while the channel is not yet failed.
    Failure {
        consecutive: u32,
        crossed: bool,
        retry_in: Option<Duration>,
    },
    /// A failed channel exited healthily, clearing `cleared` failures.
    Recovered { cleared: u32 },
    /// A channel below the threshold exited healthily, clearing `cleared`
    /// failures.
    Cleared { cleared: u32 },
    /// Healthy exit with nothing to clear.
    Healthy,
}

impl ChannelHealth {
    /// Whether this channel should be signalled as failed to clients.
    pub fn is_failed(&self) -> bool {
        self.consecutive_failures >= FAILURE_THRESHOLD
    }

    /// Whether a respawn is allowed at `now`. A healthy channel is always
    /// allowed; a failed one waits out its backoff.
    pub fn may_spawn_at(&self, now: Instant) -> bool {
        match self.retry_after {
            Some(at) => now >= at,
            None => true,
        }
    }

    /// A worker exited while a viewer was still attached.
    fn record_failure(&mut self, now: Instant, cause: String) -> ExitVerdict {
        let was_failed = self.is_failed();
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.last_failure = Some(FailureRecord {
            at: OffsetDateTime::now_utc(),
            cause,
        });
        let mut retry_in = None;
        if self.is_failed() {
            // Index from the first failure PAST the threshold, so the ramp
            // starts at its shortest delay rather than jumping mid-schedule.
            let step = (self.consecutive_failures - FAILURE_THRESHOLD) as usize;
            let wait = BACKOFF[step.min(BACKOFF.len() - 1)];
            self.retry_after = Some(now + wait);
            retry_in = Some(wait);
        }
        ExitVerdict::Failure {
            consecutive: self.consecutive_failures,
            crossed: !was_failed && self.is_failed(),
            retry_in,
        }
    }

    /// The channel served successfully, or exited with nobody watching. Either
    /// way it is not broken, so the count and the backoff both clear.
    fn record_healthy(&mut self) -> ExitVerdict {
        let cleared = self.consecutive_failures;
        let was_failed = self.is_failed();
        self.consecutive_failures = 0;
        self.retry_after = None;
        match (was_failed, cleared) {
            (true, _) => ExitVerdict::Recovered { cleared },
            (false, 0) => ExitVerdict::Healthy,
            (false, _) => ExitVerdict::Cleared { cleared },
        }
    }
}

/// Health for every channel, keyed by channel number. Absent means healthy.
#[derive(Debug, Default)]
pub struct HealthMap {
    channels: HashMap<String, ChannelHealth>,
}

impl HealthMap {
    pub fn get(&self, channel_number: &str) -> ChannelHealth {
        self.channels
            .get(channel_number)
            .cloned()
            .unwrap_or_default()
    }

    /// Record a worker exit.
    ///
    /// `viewer_attached` is whether the heartbeat was still fresh at exit — it
    /// MUST be sampled before the worker's cleanup removes the file, or every
    /// exit reads as unwatched and nothing is ever counted.
    ///
    /// `stalled` is the worker's own verdict, carried over as
    /// [`ersatztv_core::STALL_EXIT_CODE`]: the stream stopped reaching the
    /// viewer. It settles the question outright, because uptime cannot. A
    /// channel that starts, hands out ten segments, wedges, and is torn down by
    /// the 60s stall watchdog has been alive for over four minutes — past
    /// `HEALTHY_UPTIME` — while delivering a frozen picture the whole time. Read
    /// on uptime alone that exit cleared the failure count, so the backoff never
    /// engaged and the channel respawned immediately and forever.
    ///
    /// `uptime` still decides every other kind of failure, where the server has
    /// nothing better to go on: something that genuinely served for minutes
    /// before dying is not a channel that cannot start.
    ///
    /// `cause` is kept as the channel's `last_failure` when the exit counts.
    pub fn record_exit(
        &mut self,
        channel_number: &str,
        viewer_attached: bool,
        stalled: bool,
        uptime: Duration,
        now: Instant,
        cause: String,
    ) -> ExitVerdict {
        let entry = self.channels.entry(channel_number.to_owned()).or_default();
        if viewer_attached && (stalled || uptime < HEALTHY_UPTIME) {
            entry.record_failure(now, cause)
        } else {
            entry.record_healthy()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    fn cause() -> String {
        String::from("exit status: 75: channel 1 terminated after producing no segments")
    }

    // The verdict is what the session logs, so each transition must be named
    // exactly once: a WARN per failure, the ERROR only on the crossing, and a
    // recovery line only when a failed channel comes back.
    #[test]
    fn verdicts_name_each_failure_the_crossing_and_the_recovery() {
        let mut map = HealthMap::default();
        let now = t0();
        let short = Duration::from_secs(3);
        assert_eq!(
            map.record_exit("1", true, true, short, now, cause()),
            ExitVerdict::Failure {
                consecutive: 1,
                crossed: false,
                retry_in: None
            }
        );
        map.record_exit("1", true, true, short, now, cause());
        assert_eq!(
            map.record_exit("1", true, true, short, now, cause()),
            ExitVerdict::Failure {
                consecutive: 3,
                crossed: true,
                retry_in: Some(Duration::from_secs(30)),
            }
        );
        assert_eq!(
            map.record_exit("1", true, true, short, now, cause()),
            ExitVerdict::Failure {
                consecutive: 4,
                crossed: false,
                retry_in: Some(Duration::from_secs(60)),
            }
        );
        assert_eq!(
            map.record_exit("1", false, false, short, now, cause()),
            ExitVerdict::Recovered { cleared: 4 }
        );
        assert_eq!(
            map.record_exit("1", false, false, short, now, cause()),
            ExitVerdict::Healthy
        );
    }

    #[test]
    fn a_failure_keeps_its_cause_after_the_count_clears() {
        let mut map = HealthMap::default();
        let now = t0();
        map.record_exit("1", true, true, Duration::from_secs(3), now, cause());
        assert_eq!(
            map.record_exit(
                "1",
                false,
                false,
                Duration::from_secs(3),
                now,
                String::new()
            ),
            ExitVerdict::Cleared { cleared: 1 }
        );
        let h = map.get("1");
        assert_eq!(h.consecutive_failures, 0);
        assert_eq!(h.last_failure.map(|f| f.cause), Some(cause()));
    }

    #[test]
    fn a_fresh_channel_is_healthy_and_may_spawn() {
        let map = HealthMap::default();
        let h = map.get("1");
        assert!(!h.is_failed());
        assert!(h.may_spawn_at(t0()));
    }

    // An idle exit is the normal end of every channel nobody is watching. If it
    // counted, a channel watched occasionally would reach the threshold purely
    // by being switched off three times.
    #[test]
    fn exits_with_no_viewer_never_accumulate() {
        let mut map = HealthMap::default();
        let now = t0();
        for _ in 0..10 {
            map.record_exit("1", false, false, Duration::from_secs(10), now, cause());
        }
        assert!(!map.get("1").is_failed());
        assert_eq!(map.get("1").consecutive_failures, 0);
    }

    #[test]
    fn three_exits_under_a_viewer_mark_the_channel_failed() {
        let mut map = HealthMap::default();
        let now = t0();
        map.record_exit("1", true, false, Duration::from_secs(10), now, cause());
        assert!(!map.get("1").is_failed(), "one failure is not a verdict");
        map.record_exit("1", true, false, Duration::from_secs(10), now, cause());
        assert!(!map.get("1").is_failed(), "two failures is not a verdict");
        map.record_exit("1", true, false, Duration::from_secs(10), now, cause());
        assert!(map.get("1").is_failed());
    }

    // The treadmill this exists to stop: without backoff the channel respawns on
    // every ~4s poll for as long as anyone keeps watching.
    #[test]
    fn a_failed_channel_stops_spawning_until_its_backoff_elapses() {
        let mut map = HealthMap::default();
        let now = t0();
        for _ in 0..FAILURE_THRESHOLD {
            map.record_exit("1", true, false, Duration::from_secs(10), now, cause());
        }
        let h = map.get("1");
        assert!(!h.may_spawn_at(now), "must not respawn immediately");
        assert!(!h.may_spawn_at(now + Duration::from_secs(29)));
        assert!(h.may_spawn_at(now + Duration::from_secs(30)));
    }

    #[test]
    fn repeated_failures_widen_the_backoff_and_then_cap() {
        let mut map = HealthMap::default();
        let now = t0();
        for _ in 0..FAILURE_THRESHOLD {
            map.record_exit("1", true, false, Duration::from_secs(10), now, cause());
        }
        assert!(map.get("1").may_spawn_at(now + Duration::from_secs(30)));
        map.record_exit("1", true, false, Duration::from_secs(10), now, cause());
        assert!(!map.get("1").may_spawn_at(now + Duration::from_secs(59)));
        map.record_exit("1", true, false, Duration::from_secs(10), now, cause());
        assert!(!map.get("1").may_spawn_at(now + Duration::from_secs(119)));
        // Far past the end of the schedule the wait must stay at the cap, not
        // run off the end of the array or grow without bound.
        for _ in 0..20 {
            map.record_exit("1", true, false, Duration::from_secs(10), now, cause());
        }
        assert!(!map.get("1").may_spawn_at(now + Duration::from_secs(299)));
        assert!(map.get("1").may_spawn_at(now + Duration::from_secs(300)));
    }

    // The whole point of retrying rather than giving up: a stale mount that
    // comes back should clear the mark with no human involved. A run that lasts
    // is the evidence — the channel served for minutes, so whatever was broken
    // has cleared.
    #[test]
    fn a_long_healthy_run_clears_the_failure_and_the_backoff() {
        let mut map = HealthMap::default();
        let now = t0();
        for _ in 0..FAILURE_THRESHOLD {
            map.record_exit("1", true, false, Duration::from_secs(10), now, cause());
        }
        assert!(map.get("1").is_failed());

        map.record_exit("1", true, false, HEALTHY_UPTIME, now, cause());
        let h = map.get("1");
        assert!(!h.is_failed());
        assert!(h.may_spawn_at(now));
    }

    // Regression test for the bug the live run caught. Every cycle of a
    // repeatedly-dying channel is spawn -> reaches ready -> dies, so resetting
    // the count on ready pinned it at one failure forever and the channel was
    // never declared failed no matter how many times it died. Only surviving
    // resets it, so a short run under a viewer must always accumulate even
    // though the worker started fine each time.
    #[test]
    fn a_channel_that_starts_then_dies_each_cycle_still_accumulates() {
        let mut map = HealthMap::default();
        let now = t0();
        // Each iteration models a full cycle: the worker came up, served, and
        // died well inside HEALTHY_UPTIME with a viewer still attached.
        for expected in 1..=FAILURE_THRESHOLD {
            map.record_exit("1", true, false, Duration::from_secs(70), now, cause());
            assert_eq!(
                map.get("1").consecutive_failures,
                expected,
                "a short run must count even though the worker reached ready"
            );
        }
        assert!(map.get("1").is_failed());
    }

    // Regression test for the live run on 2026-08-11. Channel 4 was torn down by
    // the stall watchdog four times in a row while someone watched, and every
    // one of those exits logged "consecutive failures now 0" — because the
    // watchdog only fires 60s after the LAST segment, so each run had been alive
    // 256-284s, past HEALTHY_UPTIME. Nothing ever backed off, the channel
    // respawned on a loop, and each respawn wiped the HLS folder and restarted
    // segment numbering under a viewer still asking for the old numbers.
    #[test]
    fn a_stall_counts_even_when_the_run_outlived_the_healthy_threshold() {
        let mut map = HealthMap::default();
        let now = t0();
        let long_enough_to_look_healthy = HEALTHY_UPTIME + Duration::from_secs(80);

        for expected in 1..=FAILURE_THRESHOLD {
            map.record_exit("1", true, true, long_enough_to_look_healthy, now, cause());
            assert_eq!(
                map.get("1").consecutive_failures,
                expected,
                "a stall must count no matter how long the worker stayed alive"
            );
        }
        assert!(map.get("1").is_failed());
        assert!(
            !map.get("1").may_spawn_at(now),
            "must back off, not respawn"
        );
    }

    // The counterpart: a stall nobody was watching is not a viewer-visible
    // failure, so it must still clear rather than accumulate against a channel
    // that is simply idle.
    #[test]
    fn a_stall_with_no_viewer_still_does_not_count() {
        let mut map = HealthMap::default();
        let now = t0();
        for _ in 0..10 {
            map.record_exit("1", false, true, Duration::from_secs(10), now, cause());
        }
        assert_eq!(map.get("1").consecutive_failures, 0);
    }

    #[test]
    fn channels_are_tracked_independently() {
        let mut map = HealthMap::default();
        let now = t0();
        for _ in 0..FAILURE_THRESHOLD {
            map.record_exit("1", true, false, Duration::from_secs(10), now, cause());
        }
        assert!(map.get("1").is_failed());
        assert!(!map.get("2").is_failed());
    }
}

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
/// and was never declared failed. Staying up is the only thing that
/// distinguishes working from broken.
///
/// Comfortably above both timers that end a short run — the stall watchdog fires
/// at 60s, and an idle exit lands at the 90s heartbeat timeout — so a channel
/// caught in either loop keeps accumulating, while one that genuinely served for
/// three minutes starts from zero next time.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChannelHealth {
    /// Consecutive exits that happened while a viewer was still watching.
    pub consecutive_failures: u32,
    /// When a respawn may next be attempted. `None` means "any time".
    pub retry_after: Option<Instant>,
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
    fn record_failure(&mut self, now: Instant) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.is_failed() {
            // Index from the first failure PAST the threshold, so the ramp
            // starts at its shortest delay rather than jumping mid-schedule.
            let step = (self.consecutive_failures - FAILURE_THRESHOLD) as usize;
            let wait = BACKOFF[step.min(BACKOFF.len() - 1)];
            self.retry_after = Some(now + wait);
        }
    }

    /// The channel served successfully, or exited with nobody watching. Either
    /// way it is not broken, so the count and the backoff both clear.
    fn record_healthy(&mut self) {
        self.consecutive_failures = 0;
        self.retry_after = None;
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
            .copied()
            .unwrap_or_default()
    }

    /// Record a worker exit.
    ///
    /// `viewer_attached` is whether the heartbeat was still fresh at exit — it
    /// MUST be sampled before the worker's cleanup removes the file, or every
    /// exit reads as unwatched and nothing is ever counted. `uptime` is how long
    /// the worker ran; a long run counts as healthy even though it ended under a
    /// viewer, because something that served for minutes is not a channel that
    /// cannot start.
    pub fn record_exit(
        &mut self,
        channel_number: &str,
        viewer_attached: bool,
        uptime: Duration,
        now: Instant,
    ) {
        let entry = self.channels.entry(channel_number.to_owned()).or_default();
        if viewer_attached && uptime < HEALTHY_UPTIME {
            entry.record_failure(now);
        } else {
            entry.record_healthy();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
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
            map.record_exit("1", false, Duration::from_secs(10), now);
        }
        assert!(!map.get("1").is_failed());
        assert_eq!(map.get("1").consecutive_failures, 0);
    }

    #[test]
    fn three_exits_under_a_viewer_mark_the_channel_failed() {
        let mut map = HealthMap::default();
        let now = t0();
        map.record_exit("1", true, Duration::from_secs(10), now);
        assert!(!map.get("1").is_failed(), "one failure is not a verdict");
        map.record_exit("1", true, Duration::from_secs(10), now);
        assert!(!map.get("1").is_failed(), "two failures is not a verdict");
        map.record_exit("1", true, Duration::from_secs(10), now);
        assert!(map.get("1").is_failed());
    }

    // The treadmill this exists to stop: without backoff the channel respawns on
    // every ~4s poll for as long as anyone keeps watching.
    #[test]
    fn a_failed_channel_stops_spawning_until_its_backoff_elapses() {
        let mut map = HealthMap::default();
        let now = t0();
        for _ in 0..FAILURE_THRESHOLD {
            map.record_exit("1", true, Duration::from_secs(10), now);
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
            map.record_exit("1", true, Duration::from_secs(10), now);
        }
        assert!(map.get("1").may_spawn_at(now + Duration::from_secs(30)));
        map.record_exit("1", true, Duration::from_secs(10), now);
        assert!(!map.get("1").may_spawn_at(now + Duration::from_secs(59)));
        map.record_exit("1", true, Duration::from_secs(10), now);
        assert!(!map.get("1").may_spawn_at(now + Duration::from_secs(119)));
        // Far past the end of the schedule the wait must stay at the cap, not
        // run off the end of the array or grow without bound.
        for _ in 0..20 {
            map.record_exit("1", true, Duration::from_secs(10), now);
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
            map.record_exit("1", true, Duration::from_secs(10), now);
        }
        assert!(map.get("1").is_failed());

        map.record_exit("1", true, HEALTHY_UPTIME, now);
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
            map.record_exit("1", true, Duration::from_secs(70), now);
            assert_eq!(
                map.get("1").consecutive_failures,
                expected,
                "a short run must count even though the worker reached ready"
            );
        }
        assert!(map.get("1").is_failed());
    }

    #[test]
    fn channels_are_tracked_independently() {
        let mut map = HealthMap::default();
        let now = t0();
        for _ in 0..FAILURE_THRESHOLD {
            map.record_exit("1", true, Duration::from_secs(10), now);
        }
        assert!(map.get("1").is_failed());
        assert!(!map.get("2").is_failed());
    }
}

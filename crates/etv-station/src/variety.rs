//! Which seed and which temperature a pool draws with at a given instant.
//!
//! A channel's `seed:` and `temperature:` ([`crate::config::variety`]) are each
//! fixed or rotating. A rotating value is cut into **periods** of `rotate`
//! length, counted from 1970-01-01 00:00 in the station's local time, and each
//! period gets its own value. Everything here is a pure function of the config
//! and the instant asked about, so a pinned config and date reproduce the same
//! schedule on every run.
//!
//! The lowest level that sets a field wins outright: a pool's own `seed:` or
//! `temperature:` replaces the channel's, which (at load) already replaced the
//! station's. Nothing set anywhere means [`default_seed`] and temperature 0.
//!
//! Who reads what:
//!
//! - `seed` — every random choice: a plugin's `ctx.seed`, `order: random`,
//!   `select: random`, `from: random`, `chance:`.
//! - `temperature` — only scripts (`ctx.temperature`). Nothing in the core
//!   ranks, so there is nothing in the core for it to loosen.
//! - the period itself — [`Dice::list_key`], which tells the pattern walk when
//!   a plugin pool's ranked list is stale and must be ranked again.

use std::time::Duration;

use time::OffsetDateTime;
use time_tz::{OffsetDateTimeExt, Tz};

use crate::config::variety::{SeedRotate, SeedSpec, TemperatureSpec};
use crate::config::{ChannelConfig, Pool};
use crate::pattern::mix_seed;

/// The base a `seed: { rotate: … }` with no `cycle` draws its per-period seeds
/// from. Fixed so the draw is reproducible; salted with the channel (and pool)
/// name so two channels rotating the same way do not draw the same seeds.
const DERIVED_SEED_BASE: u64 = 0x5EED_0F57_4710_A5C3;

/// The base a `temperature: { between: … }` draws its per-period value from —
/// a different constant from [`DERIVED_SEED_BASE`] so the temperature draw is
/// never the seed draw read a second way.
const TEMPERATURE_BASE: u64 = 0x7E3B_E7A7_0DE5_1A2B;

/// What a channel or pool gets when nothing at any level sets `seed:` — a new
/// seed each local day, drawn from a fixed base. Reproducible (a rerun of the
/// same date draws the same seed), and it still varies a channel nobody tuned.
pub fn default_seed() -> SeedSpec {
    SeedSpec::Rotate(SeedRotate {
        rotate: Duration::from_secs(86_400),
        cycle: None,
    })
}

/// The values one pool draws with at one instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dice {
    pub seed: u64,
    pub temperature: f64,
    /// The seed's period index, `None` when the seed is fixed.
    pub seed_period: Option<i64>,
    /// The temperature's period index, `None` when it is fixed.
    pub temperature_period: Option<i64>,
}

impl Dice {
    /// Fixed dice: one seed, temperature 0, nothing rotating. What a caller
    /// with no channel config behind it (a test, a stateless tool) draws with.
    pub fn fixed(seed: u64) -> Self {
        Self {
            seed,
            temperature: 0.0,
            seed_period: None,
            temperature_period: None,
        }
    }

    /// Whether either value rotates.
    pub fn rotates(&self) -> bool {
        self.seed_period.is_some() || self.temperature_period.is_some()
    }

    /// The identity of the ranked list a plugin pool produces under these
    /// dice, or `None` when nothing rotates (the list is then ranked afresh
    /// every generation, as it always was).
    ///
    /// Two instants share a key exactly when they share both periods, both
    /// values and the pool's own config — so a mid-period edit to the pool
    /// re-ranks it rather than walking a list ranked under the old rules.
    pub fn list_key(&self, pool: &Pool) -> Option<String> {
        if !self.rotates() {
            return None;
        }
        let config = serde_json::to_string(pool).unwrap_or_default();
        Some(format!(
            "s{:016x}.{}/t{}.{}/c{:016x}",
            self.seed,
            period_label(self.seed_period),
            self.temperature,
            period_label(self.temperature_period),
            crate::catalog::identity::fnv1a_64(&config),
        ))
    }
}

fn period_label(p: Option<i64>) -> String {
    p.map_or_else(|| "-".to_string(), |p| p.to_string())
}

/// A channel's resolved `seed:` and `temperature:`, ready to answer for any
/// instant and any of its pools.
#[derive(Debug, Clone)]
pub struct Variety {
    seed: Option<SeedSpec>,
    temperature: Option<TemperatureSpec>,
    /// The channel's identity (#414) — the salt on a derived seed.
    channel: String,
    tz: &'static Tz,
}

impl Variety {
    /// `config.seed` / `config.temperature` are already the channel's
    /// effective values: `config::load` copies the station's in when the
    /// channel set none.
    pub fn new(config: &ChannelConfig, tz: &'static Tz) -> Self {
        Self {
            seed: config.seed.clone(),
            temperature: config.temperature.clone(),
            channel: config.name.trim().to_string(),
            tz,
        }
    }

    /// One fixed seed at temperature 0 for every pool that sets neither field
    /// itself — what a caller with no channel behind it (a unit test of the
    /// pattern walk) draws with.
    pub fn fixed(seed: u64) -> Self {
        Self {
            seed: Some(SeedSpec::Fixed(seed)),
            temperature: None,
            channel: String::new(),
            tz: time_tz::timezones::db::UTC,
        }
    }

    /// A channel named `channel` in UTC with these values set — what a unit
    /// test of the walk builds instead of a whole `ChannelConfig`.
    #[cfg(test)]
    pub(crate) fn for_test(seed: Option<SeedSpec>, temperature: Option<TemperatureSpec>) -> Self {
        Self {
            seed,
            temperature,
            channel: "test".to_string(),
            tz: time_tz::timezones::db::UTC,
        }
    }

    /// The dice `pool` draws with at `at` — the channel's, when `pool` is
    /// `None` or sets neither field itself.
    pub fn dice(&self, at: OffsetDateTime, pool: Option<&Pool>) -> Dice {
        let local = local_secs(at, self.tz);
        let pool_seed = pool.and_then(|p| p.seed.as_ref());
        let default = default_seed();
        let (seed_spec, seed_salt) = match (pool_seed, self.seed.as_ref()) {
            (Some(s), _) => (s, self.pool_salt(pool)),
            (None, Some(s)) => (s, self.channel_salt()),
            (None, None) => (&default, self.channel_salt()),
        };
        let (seed, seed_period) = match seed_spec {
            SeedSpec::Fixed(n) => (*n, None),
            SeedSpec::Rotate(r) => {
                let p = period(local, r.rotate);
                let seed = match &r.cycle {
                    Some(list) => list[p.rem_euclid(list.len() as i64) as usize],
                    None => mix_seed(mix_seed(DERIVED_SEED_BASE, seed_salt), p as u64),
                };
                (seed, Some(p))
            }
        };

        let (temp_spec, temp_salt) = match (
            pool.and_then(|p| p.temperature.as_ref()),
            self.temperature.as_ref(),
        ) {
            (Some(t), _) => (Some(t), self.pool_salt(pool)),
            (None, t) => (t, self.channel_salt()),
        };
        let (temperature, temperature_period) = match temp_spec {
            None => (0.0, None),
            Some(TemperatureSpec::Fixed(t)) => (*t, None),
            Some(TemperatureSpec::Rotate(r)) => {
                let p = period(local, r.rotate);
                let t = match (&r.cycle, &r.between) {
                    (Some(list), _) if !list.is_empty() => {
                        list[p.rem_euclid(list.len() as i64) as usize]
                    }
                    (_, Some([lo, hi])) => {
                        let draw = mix_seed(mix_seed(TEMPERATURE_BASE, temp_salt), p as u64);
                        lo + (hi - lo) * unit(draw)
                    }
                    // Validation refuses a rotation with neither; this keeps a
                    // config that bypassed it at the stable default.
                    _ => 0.0,
                };
                (t, Some(p))
            }
        };

        Dice {
            seed,
            temperature,
            seed_period,
            temperature_period,
        }
    }

    /// The first instant after `after` at which any of `pools`' dice — or the
    /// channel's own — change, or `None` when none of them rotates.
    ///
    /// A generation that would run past this is cut there, so the next one
    /// starts in the new period: that is what lands a rollover on the first
    /// item after the boundary for blocks that lay a list in one pass (an
    /// `entries` block, a sequencer), which cannot re-rank partway through.
    pub fn next_rollover<'p>(
        &self,
        after: OffsetDateTime,
        pools: impl IntoIterator<Item = &'p Pool>,
    ) -> Option<OffsetDateTime> {
        let default = default_seed();
        let mut cadences: Vec<Duration> = Vec::new();
        let mut push_seed = |s: Option<&SeedSpec>| {
            if let Some(SeedSpec::Rotate(r)) = s {
                cadences.push(r.rotate);
            }
        };
        push_seed(Some(self.seed.as_ref().unwrap_or(&default)));
        let pools: Vec<&Pool> = pools.into_iter().collect();
        for p in &pools {
            push_seed(p.seed.as_ref());
        }
        let mut push_temp = |t: Option<&TemperatureSpec>| {
            if let Some(TemperatureSpec::Rotate(r)) = t {
                cadences.push(r.rotate);
            }
        };
        push_temp(self.temperature.as_ref());
        for p in &pools {
            push_temp(p.temperature.as_ref());
        }

        let local = local_secs(after, self.tz);
        cadences
            .into_iter()
            .map(|c| {
                let secs = c.as_secs().max(1) as i64;
                let current = local.div_euclid(secs);
                let mut at = local_to_utc((current + 1) * secs, after, self.tz);
                // A boundary inside a DST gap names a wall-clock time that
                // never happens, and converting it can land before `after`.
                // The period really changes at the first instant past the
                // gap: step forward an hour at a time (every gap is whole
                // hours) until the local clock has left `current`.
                for _ in 0..48 {
                    if at > after && period(local_secs(at, self.tz), c) != current {
                        break;
                    }
                    at += time::Duration::hours(1);
                }
                at
            })
            .min()
    }

    /// The period of length `span` that holds `at`, as `(start, end)` instants.
    ///
    /// A span of whole weeks counts its periods from a local Monday midnight,
    /// so `1w` rolls over at the start of each week; any other span counts
    /// from 1970-01-01 00:00 local, the clock `seed: { rotate }` uses.
    pub fn span_period(
        &self,
        at: OffsetDateTime,
        span: Duration,
    ) -> (OffsetDateTime, OffsetDateTime) {
        let secs = span.as_secs().max(1) as i64;
        let anchor = if secs % WEEK_SECS == 0 {
            FIRST_MONDAY_SECS
        } else {
            0
        };
        let start = anchor + (local_secs(at, self.tz) - anchor).div_euclid(secs) * secs;
        (
            local_to_utc(start, at, self.tz),
            local_to_utc(start + secs, at, self.tz),
        )
    }

    fn channel_salt(&self) -> u64 {
        crate::catalog::identity::fnv1a_64(&self.channel)
    }

    fn pool_salt(&self, pool: Option<&Pool>) -> u64 {
        let name = pool.map_or("", |p| p.name.as_str());
        crate::catalog::identity::fnv1a_64(&format!("{}/{name}", self.channel))
    }
}

const WEEK_SECS: i64 = 7 * 86_400;

/// 1970-01-05 00:00, the first Monday after the epoch (1970-01-01 was a
/// Thursday), in seconds since the epoch.
const FIRST_MONDAY_SECS: i64 = 4 * 86_400;

/// Seconds since 1970-01-01 00:00 on the local wall clock at `at`.
fn local_secs(at: OffsetDateTime, tz: &'static Tz) -> i64 {
    at.unix_timestamp() + i64::from(at.to_timezone(tz).offset().whole_seconds())
}

fn period(local_secs: i64, rotate: Duration) -> i64 {
    local_secs.div_euclid(rotate.as_secs().max(1) as i64)
}

/// The UTC instant whose local wall clock reads `local` seconds since the
/// epoch. Starts from the offset at `near` and corrects once, which settles
/// any boundary that is not itself inside a DST gap.
fn local_to_utc(local: i64, near: OffsetDateTime, tz: &'static Tz) -> OffsetDateTime {
    let offset = |t: OffsetDateTime| i64::from(t.to_timezone(tz).offset().whole_seconds());
    let guess = OffsetDateTime::from_unix_timestamp(local - offset(near))
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    OffsetDateTime::from_unix_timestamp(local - offset(guess)).unwrap_or(guess)
}

/// A draw in `[0, 1)` from a mixed `u64`.
fn unit(x: u64) -> f64 {
    (x >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::variety::TemperatureRotate;
    use time::macros::datetime;

    fn chicago() -> &'static Tz {
        crate::tz::parse("America/Chicago").unwrap()
    }

    fn rotate_seed(days: u64, cycle: Option<Vec<u64>>) -> SeedSpec {
        SeedSpec::Rotate(SeedRotate {
            rotate: Duration::from_secs(days * 86_400),
            cycle,
        })
    }

    fn variety(seed: Option<&SeedSpec>, temperature: Option<&TemperatureSpec>) -> Variety {
        Variety {
            seed: seed.cloned(),
            temperature: temperature.cloned(),
            channel: "034-syfy".to_string(),
            tz: chicago(),
        }
    }

    /// A 2-day seed holds for two local days and changes at local midnight —
    /// 05:00 UTC in Chicago summer time, not 00:00 UTC.
    #[test]
    fn a_rotating_seed_changes_at_local_midnight_and_nowhere_else() {
        let spec = rotate_seed(2, None);
        let v = variety(Some(&spec), None);
        let first = v.dice(datetime!(2026-10-14 06:00 UTC), None);
        let boundary = v
            .next_rollover(datetime!(2026-10-14 06:00 UTC), [])
            .unwrap();
        assert_eq!(boundary.to_timezone(chicago()).hour(), 0, "local midnight");
        let before = v.dice(boundary - time::Duration::seconds(1), None);
        let after = v.dice(boundary, None);
        assert_eq!(before, first, "the same period up to the boundary");
        assert_ne!(after.seed, first.seed, "a new seed from the boundary on");
        assert_eq!(after.seed_period, first.seed_period.map(|p| p + 1));
        assert_eq!(
            v.next_rollover(boundary, []).unwrap() - boundary,
            time::Duration::days(2)
        );
    }

    /// A `cycle` walks its list one entry per period and wraps.
    #[test]
    fn a_seed_cycle_walks_its_list_and_wraps() {
        let spec = rotate_seed(1, Some(vec![34, 99, 7]));
        let v = variety(Some(&spec), None);
        let day0 = datetime!(2026-10-14 12:00 UTC);
        let seeds: Vec<u64> = (0..6)
            .map(|d| v.dice(day0 + time::Duration::days(d), None).seed)
            .collect();
        let start = seeds.iter().position(|s| *s == 34).unwrap();
        let walked: Vec<u64> = seeds[start..].iter().take(3).copied().collect();
        assert_eq!(walked, vec![34, 99, 7]);
        assert_eq!(seeds[start + 3], 34, "wraps back to the first seed");
    }

    /// The dice are a pure function of config and instant: asking twice gives
    /// the same answer, which is the whole of the web UI's before/after.
    #[test]
    fn the_same_instant_draws_the_same_dice() {
        let spec = rotate_seed(2, None);
        let temp = TemperatureSpec::Rotate(TemperatureRotate {
            rotate: Duration::from_secs(86_400),
            cycle: None,
            between: Some([0.25, 0.7]),
        });
        let at = datetime!(2026-10-20 03:00 UTC);
        let a = variety(Some(&spec), Some(&temp)).dice(at, None);
        let b = variety(Some(&spec), Some(&temp)).dice(at, None);
        assert_eq!(a, b);
        assert!((0.25..0.7).contains(&a.temperature), "{}", a.temperature);
    }

    /// A 2-hour boundary at 02:00 on the night Chicago springs forward names
    /// a time that never happens; the rollover lands at 03:00 local, after
    /// the instant asked from, never before it.
    #[test]
    fn a_boundary_inside_a_dst_gap_lands_after_the_gap() {
        let spec = SeedSpec::Rotate(SeedRotate {
            rotate: Duration::from_secs(2 * 3600),
            cycle: None,
        });
        let v = variety(Some(&spec), None);
        let after = datetime!(2026-03-08 07:10 UTC); // 01:10 CST
        let at = v.next_rollover(after, []).unwrap();
        assert!(at > after, "{at} must be after {after}");
        assert_eq!(at, datetime!(2026-03-08 08:00 UTC), "03:00 CDT");
        assert_ne!(v.dice(at, None).seed, v.dice(after, None).seed);
    }

    /// Nothing set anywhere is the documented default: a new seed each local
    /// day, temperature 0.
    #[test]
    fn an_unconfigured_channel_rotates_daily_at_temperature_zero() {
        let v = variety(None, None);
        let at = datetime!(2026-10-14 12:00 UTC);
        let today = v.dice(at, None);
        let tomorrow = v.dice(at + time::Duration::days(1), None);
        assert_eq!(today.temperature, 0.0);
        assert!(today.seed_period.is_some());
        assert_ne!(today.seed, tomorrow.seed);
        assert_eq!(tomorrow.seed_period, today.seed_period.map(|p| p + 1));
    }

    /// A fixed seed and a fixed temperature never roll over, so a channel
    /// pinned the way production is keeps re-ranking every generation.
    #[test]
    fn fixed_values_never_roll_over() {
        let spec = SeedSpec::Fixed(1988);
        let temp = TemperatureSpec::Fixed(0.4);
        let v = variety(Some(&spec), Some(&temp));
        let d = v.dice(datetime!(2026-10-14 12:00 UTC), None);
        assert_eq!(
            d,
            Dice {
                temperature: 0.4,
                ..Dice::fixed(1988)
            }
        );
        assert!(!d.rotates());
        assert_eq!(v.next_rollover(datetime!(2026-10-14 12:00 UTC), []), None);
    }
}

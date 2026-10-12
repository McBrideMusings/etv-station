//! The numbers behind taste-lab's channel Review view: how deep a channel's
//! schedule runs before it repeats, how its airtime splits between films and
//! series, and what a viewer tuning in at random would land on.
//!
//! Everything here reads a flat list of [`ReviewAiring`]s in air order — the
//! caller builds it from a [`crate::simulate::Simulation`] plus catalog
//! lookups and clips it to the span under review. None of it is a pass/fail
//! judgement: each figure is data for a person reading the channel against
//! `docs/programming.md`.

use std::collections::{BTreeMap, HashSet};

use time::{Duration, OffsetDateTime};

/// One airing inside the span under review.
#[derive(Debug, Clone)]
pub struct ReviewAiring {
    pub id: String,
    pub title: String,
    /// The catalog's `kind` (`movie`, `episode`, …).
    pub kind: String,
    pub show: Option<String>,
    pub show_id: Option<String>,
    /// The pool it was drawn from, or the block's name when the block has no
    /// pools.
    pub pool: String,
    pub start: OffsetDateTime,
    pub duration: Duration,
}

/// The first airing of an id that already aired earlier in the list.
#[derive(Debug, Clone, PartialEq)]
pub struct FirstRepeat {
    /// Index into the airing list.
    pub index: usize,
    /// Days from the span's start to the repeat's start.
    pub days: f64,
}

/// The first repeat among `airings` restricted to those `keep` accepts.
pub fn first_repeat(
    airings: &[ReviewAiring],
    start: OffsetDateTime,
    keep: impl Fn(&ReviewAiring) -> bool,
) -> Option<FirstRepeat> {
    let mut seen = HashSet::new();
    for (index, a) in airings.iter().enumerate() {
        if !keep(a) {
            continue;
        }
        if !seen.insert(a.id.as_str()) {
            return Some(FirstRepeat {
                index,
                days: days_between(start, a.start),
            });
        }
    }
    None
}

fn days_between(from: OffsetDateTime, to: OffsetDateTime) -> f64 {
    (to - from).as_seconds_f64() / 86_400.0
}

/// One row of the depth table: a pool, or one show inside the schedule.
#[derive(Debug, Clone, PartialEq)]
pub struct DepthRow {
    /// `"pool"` or `"show"`.
    pub kind: &'static str,
    pub name: String,
    /// The pool a show row was drawn from (the first pool it aired from).
    pub pool: String,
    pub aired: usize,
    pub distinct: usize,
    pub first_repeat: Option<FirstRepeat>,
}

/// A row per pool, in first-airing order, each followed by a row per show
/// that pool aired, in first-airing order.
pub fn depth_rows(airings: &[ReviewAiring], start: OffsetDateTime) -> Vec<DepthRow> {
    let mut pools: Vec<&str> = Vec::new();
    let mut shows: Vec<(&str, &str, &str)> = Vec::new(); // (pool, show_id, show name)
    for a in airings {
        if !pools.contains(&a.pool.as_str()) {
            pools.push(&a.pool);
        }
        if let Some(show_id) = a.show_id.as_deref()
            && !shows.iter().any(|(_, id, _)| *id == show_id)
        {
            shows.push((&a.pool, show_id, a.show.as_deref().unwrap_or(show_id)));
        }
    }

    let row = |kind, name: &str, pool: &str, keep: &dyn Fn(&ReviewAiring) -> bool| {
        let matching: Vec<&ReviewAiring> = airings.iter().filter(|a| keep(a)).collect();
        let distinct = matching
            .iter()
            .map(|a| a.id.as_str())
            .collect::<HashSet<_>>()
            .len();
        DepthRow {
            kind,
            name: name.to_string(),
            pool: pool.to_string(),
            aired: matching.len(),
            distinct,
            first_repeat: first_repeat(airings, start, keep),
        }
    };

    let mut out = Vec::new();
    for pool in pools {
        out.push(row("pool", pool, pool, &|a: &ReviewAiring| a.pool == pool));
        for (_, show_id, name) in shows.iter().filter(|(p, _, _)| *p == pool) {
            out.push(row("show", name, pool, &|a: &ReviewAiring| {
                a.show_id.as_deref() == Some(*show_id)
            }));
        }
    }
    out
}

/// Seconds of airtime per catalog kind (`movie`, `episode`, …).
pub fn airtime_by_kind(airings: &[ReviewAiring]) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    for a in airings {
        *out.entry(a.kind.clone()).or_insert(0.0) += a.duration.as_seconds_f64();
    }
    out
}

/// What a viewer tuning in at one moment sees.
#[derive(Debug, Clone, PartialEq)]
pub struct TuneIn {
    pub at: OffsetDateTime,
    /// Index of the airing on screen at `at`.
    pub index: usize,
    /// How far into that airing `at` falls.
    pub into: Duration,
}

/// `count` seeded moments inside `[start, end)`, in time order, each with the
/// airing on screen then. A moment that falls in a gap no airing covers is
/// dropped, so fewer than `count` can come back.
pub fn tune_in_samples(
    airings: &[ReviewAiring],
    start: OffsetDateTime,
    end: OffsetDateTime,
    seed: u64,
    count: usize,
) -> Vec<TuneIn> {
    let span = (end - start).whole_seconds();
    if span <= 0 {
        return Vec::new();
    }
    let mut rng = SplitMix64(seed);
    let mut offsets: Vec<i64> = (0..count)
        .map(|_| (rng.next() % span as u64) as i64)
        .collect();
    offsets.sort_unstable();
    offsets
        .into_iter()
        .filter_map(|offset| {
            let at = start + Duration::seconds(offset);
            let index = airings
                .iter()
                .position(|a| a.start <= at && at < a.start + a.duration)?;
            Some(TuneIn {
                at,
                index,
                into: at - airings[index].start,
            })
        })
        .collect()
}

/// A small seeded generator, so the same channel seed lands its tune-in
/// samples on the same moments every run.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const T0: OffsetDateTime = datetime!(2026-10-12 05:00 UTC);

    fn airing(id: &str, pool: &str, show_id: Option<&str>, start_h: i64) -> ReviewAiring {
        ReviewAiring {
            id: id.to_string(),
            title: id.to_string(),
            kind: if show_id.is_some() {
                "episode"
            } else {
                "movie"
            }
            .to_string(),
            show: show_id.map(str::to_string),
            show_id: show_id.map(str::to_string),
            pool: pool.to_string(),
            start: T0 + Duration::hours(start_h),
            duration: Duration::hours(1),
        }
    }

    fn schedule() -> Vec<ReviewAiring> {
        vec![
            airing("m1", "films", None, 0),
            airing("e1", "series", Some("s"), 1),
            airing("e2", "series", Some("s"), 2),
            airing("m2", "films", None, 3),
            airing("e1", "series", Some("s"), 4),
            airing("m1", "films", None, 48),
        ]
    }

    #[test]
    fn first_repeat_is_the_earliest_reairing() {
        let r = first_repeat(&schedule(), T0, |_| true).unwrap();
        assert_eq!(r.index, 4);
        assert!((r.days - 4.0 / 24.0).abs() < 1e-9);
    }

    #[test]
    fn depth_rows_measure_each_pool_and_show_separately() {
        let rows = depth_rows(&schedule(), T0);
        let names: Vec<_> = rows.iter().map(|r| (r.kind, r.name.as_str())).collect();
        assert_eq!(
            names,
            [("pool", "films"), ("pool", "series"), ("show", "s")]
        );
        assert_eq!(rows[0].first_repeat.as_ref().unwrap().days, 2.0);
        assert_eq!(rows[0].aired, 3);
        assert_eq!(rows[0].distinct, 2);
        assert_eq!(rows[1].first_repeat.as_ref().unwrap().index, 4);
    }

    #[test]
    fn a_pool_that_never_repeats_has_no_first_repeat() {
        let rows = depth_rows(&schedule()[..4], T0);
        assert!(rows.iter().all(|r| r.first_repeat.is_none()));
    }

    #[test]
    fn airtime_splits_by_kind() {
        let by_kind = airtime_by_kind(&schedule());
        assert_eq!(by_kind["movie"], 3.0 * 3600.0);
        assert_eq!(by_kind["episode"], 3.0 * 3600.0);
    }

    #[test]
    fn tune_in_samples_are_seeded_and_land_inside_an_airing() {
        let s = schedule();
        let end = T0 + Duration::hours(5);
        let a = tune_in_samples(&s, T0, end, 34, 5);
        assert_eq!(a, tune_in_samples(&s, T0, end, 34, 5));
        assert_eq!(a.len(), 5);
        for t in &a {
            let air = &s[t.index];
            assert_eq!(t.at - air.start, t.into);
            assert!(t.into < air.duration);
        }
        assert!(a.windows(2).all(|w| w[0].at <= w[1].at));
        assert_ne!(a, tune_in_samples(&s, T0, end, 35, 5));
    }
}

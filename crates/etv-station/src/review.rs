//! The numbers behind taste-lab's channel Review view: how deep a channel's
//! schedule runs before it repeats, how its airtime splits between films and
//! series, and what a viewer tuning in at random would land on.
//!
//! Everything here reads a flat list of [`ReviewAiring`]s in air order — the
//! caller builds it from a [`crate::simulate::Simulation`] plus catalog
//! lookups and clips it to the span under review. Each figure is data for a
//! person reading the channel against `docs/programming.md`; [`grade_principles`]
//! adds a 0–3 grade to the principles a schedule measures, by the thresholds
//! that page states under each principle's **Grade.**

use std::collections::{BTreeMap, HashMap, HashSet};

use time::{Duration, OffsetDateTime, Weekday};
use time_tz::{OffsetDateTimeExt, Tz};

/// One airing inside the span under review.
#[derive(Debug, Clone)]
pub struct ReviewAiring {
    pub id: String,
    pub title: String,
    /// The catalog's `kind` (`movie`, `episode`, …).
    pub kind: String,
    pub show: Option<String>,
    pub show_id: Option<String>,
    pub season: Option<i64>,
    pub episode: Option<i64>,
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

/// An airing shorter than this is an interstitial (a bumper, a promo), not a
/// program.
pub const INTERSTITIAL_MAX: Duration = Duration::minutes(5);

/// A show needs this many episodes in the catalog before stripping applies to
/// it: three seasons, about 75 episodes, gives 15 weeks of weekdays
/// ([Wikipedia, Strip programming](https://en.wikipedia.org/wiki/Strip_programming)).
pub const STRIP_MIN_EPISODES: usize = 75;

// Grade thresholds. `docs/programming.md` states each in its principle's
// **Grade.** paragraph; `grade_thresholds_match_the_doc` fails when the two
// differ.

/// Dayparting: swing in points of airtime that earns grade 3, 2, 1.
const DAYPARTING_SWING: [f64; 3] = [30.0, 20.0, 10.0];
/// Block programming: episodes per visit that earn grade 3 (inclusive range).
const BLOCK_IDEAL: (f64, f64) = (2.0, 4.0);
/// Block programming: the upper bound of grade 2.
const BLOCK_GRADE2_MAX: f64 = 6.0;
/// Block programming: the lower bound of grade 1 below the ideal range.
const BLOCK_GRADE1_MIN: f64 = 1.5;
/// Stripping: percent of weekdays that earns grade 3, 2, 1.
const STRIPPING_SHARE: [f64; 3] = [80.0, 60.0, 40.0];
/// Stripping: minutes either side of a show's usual start that still count as
/// holding its time slot.
const STRIPPING_WINDOW_MINUTES: i64 = 30;
/// Episode order: percent out of order that still earns grade 3, 2, 1.
const EPISODE_ORDER_OUT: [f64; 3] = [0.0, 5.0, 15.0];
/// Interstitials: percent of program changes filled that earns grade 3, 2.
const INTERSTITIAL_SHARE: [f64; 2] = [90.0, 50.0];
/// Rest periods: days to the first repeat that earn grade 3, 2, 1.
const REST_DAYS: [f64; 3] = [14.0, 7.0, 3.0];

/// One principle from `docs/programming.md`, measured against a schedule.
#[derive(Debug, Clone, PartialEq)]
pub struct PrincipleGrade {
    pub name: &'static str,
    /// 0 to 3. `None` when the principle does not apply to this channel, or
    /// when the station has no number to measure it by — `not_scored` says
    /// which.
    pub grade: Option<u8>,
    /// The number the grade comes from, in the unit `measure` names.
    pub value: Option<f64>,
    /// The measurement in words, shown beside the grade.
    pub measure: String,
    pub not_scored: Option<String>,
}

/// What [`grade_principles`] needs beyond the airings themselves.
pub struct GradeInputs<'a> {
    pub airings: &'a [ReviewAiring],
    pub start: OffsetDateTime,
    pub end: OffsetDateTime,
    pub tz: &'static Tz,
    /// Pools whose `order` is `random`: their episodes are shuffled on
    /// purpose, so episode order is not measured on them.
    pub random_order_pools: &'a HashSet<String>,
    /// Catalog episode count per `show_id`, for the shows that aired.
    pub episode_counts: &'a HashMap<String, usize>,
}

/// Every principle in `docs/programming.md`, in the page's order. A principle
/// is graded only when the schedule yields a number for it; the rest carry a
/// `not_scored` reason.
pub fn grade_principles(g: &GradeInputs) -> Vec<PrincipleGrade> {
    vec![
        dayparting(g),
        block_programming(g),
        stripping(g),
        episode_order(g),
        not_scored(
            "Movie slot length",
            "the station cannot pad a film to a clock-aligned slot yet (etv-station-85)",
        ),
        not_scored(
            "Themed and seasonal periods",
            "a seasonal run is a choice a channel makes, not a standard it meets",
        ),
        marathons(g),
        interstitials(g),
        rest_periods(g),
    ]
}

fn not_scored(name: &'static str, why: &str) -> PrincipleGrade {
    PrincipleGrade {
        name,
        grade: None,
        value: None,
        measure: String::new(),
        not_scored: Some(why.to_string()),
    }
}

fn graded(name: &'static str, grade: u8, value: f64, measure: String) -> PrincipleGrade {
    PrincipleGrade {
        name,
        grade: Some(grade),
        value: Some(value),
        measure,
        not_scored: None,
    }
}

fn not_applicable(name: &'static str, why: &str) -> PrincipleGrade {
    not_scored(name, &format!("does not apply: {why}"))
}

/// The show's name as the catalog spells it, for a `show_id`.
fn show_name<'a>(airings: &'a [ReviewAiring], show_id: &'a str) -> &'a str {
    airings
        .iter()
        .find(|a| a.show_id.as_deref() == Some(show_id))
        .and_then(|a| a.show.as_deref())
        .unwrap_or(show_id)
}

/// The largest swing in any one pool's share of airtime between the day's
/// four 6-hour quarters, in percentage points.
fn dayparting(g: &GradeInputs) -> PrincipleGrade {
    const NAME: &str = "Dayparting";
    let pools: HashSet<&str> = g.airings.iter().map(|a| a.pool.as_str()).collect();
    if pools.len() < 2 {
        return not_applicable(NAME, "one pool fills the whole day");
    }
    let mut secs = [
        HashMap::<&str, f64>::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
    ];
    for a in g.airings {
        // Split the airing at each local 6-hour boundary it crosses.
        let end = a.start + a.duration;
        let mut t = a.start;
        while t < end {
            let local = t.to_timezone(g.tz);
            let q = local.hour() as usize / 6;
            let into_quarter = Duration::seconds(
                i64::from(local.hour() % 6) * 3600
                    + i64::from(local.minute()) * 60
                    + i64::from(local.second()),
            ) + Duration::nanoseconds(i64::from(local.nanosecond()));
            let next = (t + (Duration::hours(6) - into_quarter)).min(end);
            *secs[q].entry(a.pool.as_str()).or_insert(0.0) += (next - t).as_seconds_f64();
            t = next;
        }
    }
    let shares: Vec<HashMap<&str, f64>> = secs
        .iter()
        .map(|q| {
            let total: f64 = q.values().sum();
            q.iter()
                .map(|(p, s)| (*p, if total > 0.0 { s / total * 100.0 } else { 0.0 }))
                .collect()
        })
        .collect();
    let (pool, swing) = pools
        .iter()
        .map(|p| {
            let v: Vec<f64> = shares
                .iter()
                .map(|q| q.get(p).copied().unwrap_or(0.0))
                .collect();
            let max = v.iter().cloned().fold(f64::MIN, f64::max);
            let min = v.iter().cloned().fold(f64::MAX, f64::min);
            (*p, max - min)
        })
        .max_by(|a, b| a.1.total_cmp(&b.1).then_with(|| b.0.cmp(a.0)))
        .expect("two or more pools");
    let grade = match swing {
        s if s >= DAYPARTING_SWING[0] => 3,
        s if s >= DAYPARTING_SWING[1] => 2,
        s if s >= DAYPARTING_SWING[2] => 1,
        _ => 0,
    };
    graded(
        NAME,
        grade,
        swing,
        format!("largest swing between 6-hour quarters: {pool}, {swing:.0} points of airtime"),
    )
}

/// Runs of consecutive episodes of one show, ignoring interstitials between
/// them, in air order.
fn show_runs(airings: &[ReviewAiring]) -> Vec<(&str, usize)> {
    let mut runs: Vec<(&str, usize)> = Vec::new();
    let mut last: Option<&str> = None;
    for a in airings.iter().filter(|a| a.duration >= INTERSTITIAL_MAX) {
        let show = a.show_id.as_deref().filter(|_| a.kind == "episode");
        match (show, last) {
            (Some(s), Some(l)) if s == l => runs.last_mut().expect("run open").1 += 1,
            (Some(s), _) => runs.push((s, 1)),
            (None, _) => {}
        }
        last = show;
    }
    runs
}

/// Mean episodes of one show per visit.
fn block_programming(g: &GradeInputs) -> PrincipleGrade {
    const NAME: &str = "Block programming";
    let runs = show_runs(g.airings);
    if runs.is_empty() {
        return not_applicable(NAME, "no series aired");
    }
    let mean = runs.iter().map(|r| r.1).sum::<usize>() as f64 / runs.len() as f64;
    let grade = match mean {
        m if (BLOCK_IDEAL.0..=BLOCK_IDEAL.1).contains(&m) => 3,
        m if m > BLOCK_IDEAL.1 && m <= BLOCK_GRADE2_MAX => 2,
        m if m > BLOCK_GRADE2_MAX || m >= BLOCK_GRADE1_MIN => 1,
        _ => 0,
    };
    graded(
        NAME,
        grade,
        mean,
        format!("{mean:.2} episodes per visit over {} visits", runs.len()),
    )
}

/// For each show with a strippable library, the share of weekdays in the span
/// on which it starts an episode within [`STRIPPING_WINDOW_MINUTES`] of its
/// usual weekday start time.
fn stripping(g: &GradeInputs) -> PrincipleGrade {
    const NAME: &str = "Stripping";
    let weekdays: HashSet<time::Date> = {
        let mut out = HashSet::new();
        let mut t = g.start;
        while t < g.end {
            let local = t.to_timezone(g.tz);
            if !matches!(local.weekday(), Weekday::Saturday | Weekday::Sunday) {
                out.insert(local.date());
            }
            t += Duration::hours(1);
        }
        out
    };
    // Sorted, so two shows tied on share always name the same one.
    let deep: std::collections::BTreeSet<&str> = g
        .airings
        .iter()
        .filter_map(|a| a.show_id.as_deref())
        .filter(|s| g.episode_counts.get(*s).copied().unwrap_or(0) >= STRIP_MIN_EPISODES)
        .collect();
    if deep.is_empty() || weekdays.is_empty() {
        return not_applicable(
            NAME,
            &format!("no show that aired has {STRIP_MIN_EPISODES}+ episodes"),
        );
    }
    let mut best: Option<(&str, f64)> = None;
    for show in deep {
        let starts: Vec<(time::Date, i64)> = g
            .airings
            .iter()
            .filter(|a| a.show_id.as_deref() == Some(show))
            .map(|a| a.start.to_timezone(g.tz))
            .filter(|l| !matches!(l.weekday(), Weekday::Saturday | Weekday::Sunday))
            .map(|l| (l.date(), i64::from(l.hour()) * 60 + i64::from(l.minute())))
            .collect();
        // The usual start is whichever actual start has the most weekdays
        // starting within the window of it (clock distance wraps at midnight).
        let days = starts
            .iter()
            .map(|(_, anchor)| {
                starts
                    .iter()
                    .filter(|(_, m)| {
                        let d = (m - anchor).abs();
                        d.min(24 * 60 - d) <= STRIPPING_WINDOW_MINUTES
                    })
                    .map(|(date, _)| *date)
                    .collect::<HashSet<_>>()
                    .len()
            })
            .max()
            .unwrap_or(0);
        let share = days as f64 / weekdays.len() as f64 * 100.0;
        if best.is_none_or(|(_, b)| share > b) {
            best = Some((show, share));
        }
    }
    let (show, share) = best.expect("at least one deep show");
    let grade = match share {
        s if s >= STRIPPING_SHARE[0] => 3,
        s if s >= STRIPPING_SHARE[1] => 2,
        s if s >= STRIPPING_SHARE[2] => 1,
        _ => 0,
    };
    graded(
        NAME,
        grade,
        share,
        format!(
            "{} holds its start time on {share:.0}% of weekdays",
            show_name(g.airings, show)
        ),
    )
}

/// The share of steps between consecutive airings of one show that do not go
/// to the next episode, to episode 1 of the next season, or back to the
/// show's first aired episode after its last.
fn episode_order(g: &GradeInputs) -> PrincipleGrade {
    const NAME: &str = "Episode order";
    let mut last: HashMap<&str, (i64, i64)> = HashMap::new();
    // A loop that reaches a show's last aired episode starts over at its first.
    let mut span: HashMap<&str, [(i64, i64); 2]> = HashMap::new();
    for a in g.airings {
        if a.kind != "episode" || g.random_order_pools.contains(&a.pool) {
            continue;
        }
        if let (Some(show), Some(season), Some(episode)) =
            (a.show_id.as_deref(), a.season, a.episode)
        {
            let key = (season, episode);
            let e = span.entry(show).or_insert([key, key]);
            e[0] = e[0].min(key);
            e[1] = e[1].max(key);
        }
    }
    let (mut steps, mut out) = (0usize, 0usize);
    for a in g.airings {
        if a.kind != "episode" || g.random_order_pools.contains(&a.pool) {
            continue;
        }
        let (Some(show), Some(season), Some(episode)) = (a.show_id.as_deref(), a.season, a.episode)
        else {
            continue;
        };
        if let Some((ps, pe)) = last.insert(show, (season, episode)) {
            steps += 1;
            let [first, final_ep] = span[show];
            let wrap = first != final_ep
                && first.1 == 1
                && (ps, pe) == final_ep
                && (season, episode) == first;
            let next =
                (season == ps && episode == pe + 1) || (season == ps + 1 && episode == 1) || wrap;
            if !next {
                out += 1;
            }
        }
    }
    if steps == 0 {
        return not_applicable(NAME, "no show aired twice from an ordered pool");
    }
    let pct = out as f64 / steps as f64 * 100.0;
    let grade = match pct {
        p if p <= EPISODE_ORDER_OUT[0] => 3,
        p if p <= EPISODE_ORDER_OUT[1] => 2,
        p if p <= EPISODE_ORDER_OUT[2] => 1,
        _ => 0,
    };
    graded(
        NAME,
        grade,
        pct,
        format!("{out} of {steps} steps out of order ({pct:.1}%)"),
    )
}

/// Marathons are a choice, not a standard: the longest run is shown, never
/// graded.
fn marathons(g: &GradeInputs) -> PrincipleGrade {
    let longest = show_runs(g.airings).into_iter().max_by_key(|r| r.1);
    PrincipleGrade {
        name: "Marathons",
        grade: None,
        value: longest.map(|r| r.1 as f64),
        measure: longest
            .map(|(s, n)| format!("longest run: {n} episodes of {}", show_name(g.airings, s)))
            .unwrap_or_default(),
        not_scored: Some("a marathon is a choice a channel makes, not a standard it meets".into()),
    }
}

/// The share of changes from one program to the next with an interstitial
/// between them.
fn interstitials(g: &GradeInputs) -> PrincipleGrade {
    const NAME: &str = "Interstitials and bumpers";
    let (mut changes, mut filled) = (0usize, 0usize);
    let mut since_program: Option<bool> = None; // Some(saw an interstitial) once a program has aired
    for a in g.airings {
        if a.duration < INTERSTITIAL_MAX {
            if let Some(saw) = since_program.as_mut() {
                *saw = true;
            }
            continue;
        }
        if let Some(saw) = since_program {
            changes += 1;
            if saw {
                filled += 1;
            }
        }
        since_program = Some(false);
    }
    if changes == 0 {
        return not_applicable(NAME, "fewer than two programs aired");
    }
    let pct = filled as f64 / changes as f64 * 100.0;
    let grade = match pct {
        p if p >= INTERSTITIAL_SHARE[0] => 3,
        p if p >= INTERSTITIAL_SHARE[1] => 2,
        p if p > 0.0 => 1,
        _ => 0,
    };
    graded(
        NAME,
        grade,
        pct,
        format!("{filled} of {changes} program changes have an interstitial ({pct:.1}%)"),
    )
}

/// Days until any program airs a second time; the whole span when none does.
/// Interstitials are left out: a bumper pool repeats within hours by design.
fn rest_periods(g: &GradeInputs) -> PrincipleGrade {
    const NAME: &str = "Rest periods";
    let is_program = |a: &ReviewAiring| a.duration >= INTERSTITIAL_MAX;
    if !g.airings.iter().any(is_program) {
        return not_applicable(NAME, "no program aired");
    }
    let span = days_between(g.start, g.end);
    let (days, measure) = match first_repeat(g.airings, g.start, is_program) {
        Some(r) => (
            r.days,
            format!(
                "first repeat after {:.1} days: {}",
                r.days, g.airings[r.index].title
            ),
        ),
        None => (span, format!("nothing repeats in {span:.0} days")),
    };
    let grade = match days {
        d if d >= REST_DAYS[0] => 3,
        d if d >= REST_DAYS[1] => 2,
        d if d >= REST_DAYS[2] => 1,
        _ => 0,
    };
    graded(NAME, grade, days, measure)
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
            season: None,
            episode: None,
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

    fn ep(
        id: &str,
        show: &str,
        pool: &str,
        season: i64,
        episode: i64,
        start_h: i64,
    ) -> ReviewAiring {
        ReviewAiring {
            season: Some(season),
            episode: Some(episode),
            ..airing(id, pool, Some(show), start_h)
        }
    }

    fn inputs<'a>(
        airings: &'a [ReviewAiring],
        random: &'a HashSet<String>,
        counts: &'a HashMap<String, usize>,
        days: i64,
    ) -> GradeInputs<'a> {
        GradeInputs {
            airings,
            start: T0,
            end: T0 + Duration::days(days),
            tz: crate::tz::parse("UTC").unwrap(),
            random_order_pools: random,
            episode_counts: counts,
        }
    }

    fn grade_of(grades: &[PrincipleGrade], name: &str) -> PrincipleGrade {
        grades.iter().find(|g| g.name == name).unwrap().clone()
    }

    #[test]
    fn dayparting_grades_the_largest_swing_in_a_pools_share_across_quarters() {
        // T0 is 05:00 UTC: films fill 05:00-06:00 (quarter 0) and series 06:00-12:00 (quarter 1).
        let mut s = vec![airing("m1", "films", None, 0)];
        s.extend((1..7).map(|h| airing(&format!("e{h}"), "series", Some("s"), h)));
        let g = grade_principles(&inputs(&s, &HashSet::new(), &HashMap::new(), 1));
        let d = grade_of(&g, "Dayparting");
        assert_eq!(d.value, Some(100.0));
        assert_eq!(d.grade, Some(3));
        let one_pool = &s[1..];
        let g = grade_principles(&inputs(one_pool, &HashSet::new(), &HashMap::new(), 1));
        assert_eq!(grade_of(&g, "Dayparting").grade, None);
    }

    #[test]
    fn block_programming_grades_episodes_per_visit() {
        // Visits of 3, then 1 with a film between: mean 2.0.
        let s = vec![
            ep("a1", "a", "series", 1, 1, 0),
            ep("a2", "a", "series", 1, 2, 1),
            ep("a3", "a", "series", 1, 3, 2),
            airing("m1", "films", None, 3),
            ep("b1", "b", "series", 1, 1, 4),
        ];
        let g = grade_principles(&inputs(&s, &HashSet::new(), &HashMap::new(), 1));
        let b = grade_of(&g, "Block programming");
        assert_eq!(b.value, Some(2.0));
        assert_eq!(b.grade, Some(3));
        assert_eq!(grade_of(&g, "Marathons").value, Some(3.0));
    }

    #[test]
    fn episode_order_counts_skips_but_not_on_a_random_order_pool() {
        let s = vec![
            ep("a1", "a", "series", 1, 1, 0),
            ep("a2", "a", "series", 1, 2, 1),
            ep("a5", "a", "series", 1, 5, 2),
            ep("b1", "b", "series", 2, 1, 3),
            ep("b2", "b", "series", 1, 9, 4),
        ];
        let none = HashSet::new();
        let g = grade_principles(&inputs(&s, &none, &HashMap::new(), 1));
        let o = grade_of(&g, "Episode order");
        assert!((o.value.unwrap() - 200.0 / 3.0).abs() < 1e-9);
        assert_eq!(o.grade, Some(0));
        let random: HashSet<String> = ["series".to_string()].into();
        let g = grade_principles(&inputs(&s, &random, &HashMap::new(), 1));
        assert_eq!(grade_of(&g, "Episode order").grade, None);
    }

    #[test]
    fn stripping_measures_weekdays_a_deep_show_holds_one_hour() {
        // 2026-10-12 is a Monday; one episode at 05:00 UTC on each of four weekdays out of five.
        let s: Vec<ReviewAiring> = (0..4)
            .map(|d| ep(&format!("x{d}"), "x", "strip", 1, d + 1, d * 24))
            .collect();
        let deep: HashMap<String, usize> = [("x".to_string(), 100)].into();
        let g = grade_principles(&inputs(&s, &HashSet::new(), &deep, 5));
        let st = grade_of(&g, "Stripping");
        assert_eq!(st.value, Some(80.0));
        assert_eq!(st.grade, Some(3));
        let shallow: HashMap<String, usize> = [("x".to_string(), 40)].into();
        let g = grade_principles(&inputs(&s, &HashSet::new(), &shallow, 5));
        assert_eq!(grade_of(&g, "Stripping").grade, None);
    }

    #[test]
    fn stripping_counts_a_start_time_that_drifts_within_the_window() {
        // Weekdays Mon-Fri; starts at 05:00, 05:25, 04:45, 05:10, 05:20 UTC (all within 30 min).
        let offs = [0, 24 * 60 + 25, 48 * 60 - 15, 72 * 60 + 10, 96 * 60 + 20];
        let s: Vec<ReviewAiring> = offs
            .iter()
            .enumerate()
            .map(|(d, m)| ReviewAiring {
                start: T0 + Duration::minutes(*m),
                ..ep(&format!("x{d}"), "x", "strip", 1, d as i64 + 1, 0)
            })
            .collect();
        let deep: HashMap<String, usize> = [("x".to_string(), 100)].into();
        let g = grade_principles(&inputs(&s, &HashSet::new(), &deep, 5));
        assert_eq!(grade_of(&g, "Stripping").value, Some(100.0));
    }

    #[test]
    fn dayparting_splits_an_airing_across_the_quarters_it_covers() {
        // T0 is 05:00 UTC. The 3-hour film is 1h in quarter 0 and 2h in quarter 1.
        let film = ReviewAiring {
            duration: Duration::hours(3),
            ..airing("m1", "films", None, 0)
        };
        let series = |id: &str, h: i64, hours: i64| ReviewAiring {
            duration: Duration::hours(hours),
            ..airing(id, "series", Some("s"), h)
        };
        let s = vec![
            series("e0", -5, 1),
            film,
            series("e1", 3, 4),
            airing("m2", "films", None, 7),
            series("e2", 8, 1),
            airing("m3", "films", None, 13),
            series("e3", 14, 1),
        ];
        let g = grade_principles(&inputs(&s, &HashSet::new(), &HashMap::new(), 1));
        let d = grade_of(&g, "Dayparting");
        // films: 50%, 33.3%, 50%, 50% of each quarter; series is the mirror.
        assert!((d.value.unwrap() - 50.0 / 3.0).abs() < 1e-9);
        assert_eq!(d.grade, Some(1));
    }

    #[test]
    fn episode_order_counts_a_loop_wrap_to_the_first_episode_as_in_order() {
        let s = vec![
            ep("a1", "a", "series", 1, 1, 0),
            ep("a2", "a", "series", 1, 2, 1),
            ep("a3", "a", "series", 1, 3, 2),
            ep("a1b", "a", "series", 1, 1, 3),
        ];
        let g = grade_principles(&inputs(&s, &HashSet::new(), &HashMap::new(), 1));
        let o = grade_of(&g, "Episode order");
        assert_eq!(o.value, Some(0.0));
        assert_eq!(o.grade, Some(3));
    }

    #[test]
    fn a_single_aired_episode_repeating_is_not_a_loop_wrap() {
        let s = vec![
            ep("a1", "a", "series", 1, 1, 0),
            ep("a1b", "a", "series", 1, 1, 1),
        ];
        let g = grade_principles(&inputs(&s, &HashSet::new(), &HashMap::new(), 1));
        assert_eq!(grade_of(&g, "Episode order").value, Some(100.0));
    }

    #[test]
    fn interstitials_grade_the_share_of_program_changes_with_one_between() {
        let bumper = |id: &str, h: i64| ReviewAiring {
            duration: Duration::minutes(1),
            ..airing(id, "bumpers", None, h)
        };
        let s = vec![
            airing("m1", "films", None, 0),
            bumper("b1", 1),
            airing("m2", "films", None, 2),
            airing("m3", "films", None, 3),
        ];
        let g = grade_principles(&inputs(&s, &HashSet::new(), &HashMap::new(), 1));
        let i = grade_of(&g, "Interstitials and bumpers");
        assert_eq!(i.value, Some(50.0));
        assert_eq!(i.grade, Some(2));
    }

    #[test]
    fn rest_periods_grade_days_to_the_first_repeat() {
        let g = grade_principles(&inputs(&schedule(), &HashSet::new(), &HashMap::new(), 14));
        let r = grade_of(&g, "Rest periods");
        assert!((r.value.unwrap() - 4.0 / 24.0).abs() < 1e-9);
        assert_eq!(r.grade, Some(0));
        let g = grade_principles(&inputs(
            &schedule()[..4],
            &HashSet::new(),
            &HashMap::new(),
            14,
        ));
        assert_eq!(grade_of(&g, "Rest periods").grade, Some(3));
    }

    #[test]
    fn rest_periods_ignore_repeating_interstitials_and_skip_an_empty_schedule() {
        let bumper = |h: i64| ReviewAiring {
            duration: Duration::minutes(1),
            ..airing("b1", "bumpers", None, h)
        };
        let s = vec![
            airing("m1", "films", None, 0),
            bumper(1),
            airing("m2", "films", None, 2),
            bumper(3),
        ];
        let g = grade_principles(&inputs(&s, &HashSet::new(), &HashMap::new(), 14));
        assert_eq!(grade_of(&g, "Rest periods").grade, Some(3));
        let g = grade_principles(&inputs(&[], &HashSet::new(), &HashMap::new(), 14));
        assert_eq!(grade_of(&g, "Rest periods").grade, None);
    }

    /// The text of the **Grade.** paragraph under the `## <heading>` section
    /// of `docs/programming.md`, whitespace collapsed to single spaces.
    fn doc_grade(heading: &str) -> String {
        let doc = include_str!("../../../docs/programming.md");
        let section = doc
            .split("\n## ")
            .find(|s| s.starts_with(heading))
            .unwrap_or_else(|| panic!("no `## {heading}` section in docs/programming.md"));
        let grade = section
            .split("**Grade.**")
            .nth(1)
            .unwrap_or_else(|| panic!("`## {heading}` has no **Grade.** paragraph"));
        let para = grade.split("\n\n").next().expect("a paragraph");
        para.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn assert_doc_says(heading: &str, expected: &[String]) {
        let para = doc_grade(heading);
        for want in expected {
            assert!(
                para.contains(want.as_str()),
                "docs/programming.md `## {heading}` Grade paragraph does not say {want:?}:\n{para}"
            );
        }
    }

    #[test]
    fn grade_thresholds_match_the_doc() {
        let [d3, d2, d1] = DAYPARTING_SWING;
        assert_doc_says(
            "Dayparting",
            &[format!(
                "{d3} points or more is 3, {d2} is 2, {d1} is 1, under {d1} is 0"
            )],
        );
        let (lo, hi) = BLOCK_IDEAL;
        assert_doc_says(
            "Block programming",
            &[format!(
                "{lo} to {hi} is 3 (the unsourced range above), over {hi} up to {BLOCK_GRADE2_MAX} is 2, over {BLOCK_GRADE2_MAX} or {BLOCK_GRADE1_MIN} to {lo} is 1, under {BLOCK_GRADE1_MIN} is 0"
            )],
        );
        let [s3, s2, s1] = STRIPPING_SHARE;
        assert_doc_says(
            "Stripping",
            &[
                format!("{STRIP_MIN_EPISODES} or more episodes"),
                format!("{s3}% or more is 3, {s2}% is 2, {s1}% is 1, under {s1}% is 0"),
            ],
        );
        let [e3, e2, e1] = EPISODE_ORDER_OUT;
        assert_doc_says(
            "Episode order",
            &[format!(
                "{e3}% is 3, up to {e2}% is 2, up to {e1}% is 1, more is 0"
            )],
        );
        let [i3, i2] = INTERSTITIAL_SHARE;
        assert_doc_says(
            "Interstitials and bumpers",
            &[
                format!("under {} minutes", INTERSTITIAL_MAX.whole_minutes()),
                format!("{i3}% or more is 3, {i2}% is 2, anything above 0 is 1, none is 0"),
            ],
        );
        let [r3, r2, r1] = REST_DAYS;
        assert_doc_says(
            "Rest periods",
            &[
                format!("{r3} days or more is 3, {r2} is 2"),
                format!("{r1} is 1, under {r1} is 0"),
                format!(
                    "an airing of {} minutes or more",
                    INTERSTITIAL_MAX.whole_minutes()
                ),
            ],
        );
    }

    #[test]
    fn principles_without_a_measure_are_listed_but_never_graded() {
        let g = grade_principles(&inputs(&schedule(), &HashSet::new(), &HashMap::new(), 14));
        assert_eq!(g.len(), 9);
        for name in [
            "Movie slot length",
            "Themed and seasonal periods",
            "Marathons",
        ] {
            assert_eq!(grade_of(&g, name).grade, None);
        }
    }
}

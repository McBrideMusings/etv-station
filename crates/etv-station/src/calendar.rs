//! Which block is on air at a given instant, under a channel's
//! `rule.schedules` (ADR 0004).
//!
//! Schedules are walked highest first — a dated schedule, then a weekday one,
//! then one with neither, written order breaking ties — and the first whose
//! day matches and whose clock range covers the instant names the slot that
//! airs. When none does, the channel's any-time `rule.blocks` air.
//!
//! A generation airs one answer: [`Calendar::next_change`] is where the
//! resolver cuts it, and the item playing at that instant finishes before the
//! next generation asks again.

use time::{Duration, OffsetDateTime};
use time_tz::{OffsetDateTimeExt, Tz};

use crate::config::{RuleConfig, Schedule};

/// What airs at one instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnAir<'a> {
    /// No schedule claims the instant: the any-time `rule.blocks`.
    AnyTime,
    /// `schedule` places slot `slot`, which is block index `index`.
    Slot {
        slot: &'a str,
        index: usize,
        schedule: &'a str,
        /// Set when `slot` is a rotation: the unix time its placement began.
        /// Two placements of one rotation are two changes, so a generation
        /// stops where one ends and the next draws its own member.
        placement: Option<i64>,
        /// Set once the rotation has drawn ([`Calendar::on_air_drawn`]): the
        /// rotation the schedule placed, with `slot` and `index` naming the
        /// member that airs in its place.
        rotation: Option<&'a str>,
    },
}

impl OnAir<'_> {
    /// Two answers air the same block — a change of schedule that keeps the
    /// same slot on air is not a change.
    fn same_block(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::AnyTime, Self::AnyTime) => true,
            (
                Self::Slot {
                    index: a,
                    placement: pa,
                    ..
                },
                Self::Slot {
                    index: b,
                    placement: pb,
                    ..
                },
            ) => a == b && pa == pb,
            _ => false,
        }
    }
}

pub struct Calendar<'a> {
    rule: &'a RuleConfig,
    /// The schedules, highest first.
    order: Vec<&'a Schedule>,
    tz: &'static Tz,
}

impl<'a> Calendar<'a> {
    /// `None` for a channel with no schedules: its any-time blocks are all it
    /// ever airs, and nothing about its generation changes.
    pub fn new(rule: &'a RuleConfig, tz: &'static Tz) -> Option<Self> {
        if rule.schedules.is_empty() {
            return None;
        }
        let mut order: Vec<&Schedule> = rule.schedules.iter().collect();
        // Stable, so written order breaks ties within a rank.
        order.sort_by_key(|s| std::cmp::Reverse(s.rank()));
        Some(Self { rule, order, tz })
    }

    pub fn on_air(&self, at: OffsetDateTime) -> OnAir<'a> {
        let local = at.to_timezone(self.tz);
        let minute = u16::from(local.hour()) * 60 + u16::from(local.minute());
        for schedule in &self.order {
            if let Some((slot, range, start_day)) = schedule.hit_at(local.date(), minute) {
                // Validation guarantees every placed slot exists.
                if let Some(index) = self.rule.slot_index(slot) {
                    let rotation = self.rule.slots.get(slot).is_some_and(|b| b.is_rotation());
                    let placement = rotation.then(|| {
                        let begins = time::Time::from_hms(
                            (range.start / 60) as u8,
                            (range.start % 60) as u8,
                            0,
                        )
                        .unwrap_or(time::Time::MIDNIGHT);
                        crate::tz::to_utc_assume_local(start_day, begins, self.tz).unix_timestamp()
                    });
                    return OnAir::Slot {
                        slot,
                        index,
                        schedule: &schedule.name,
                        placement,
                        rotation: None,
                    };
                }
            }
        }
        OnAir::AnyTime
    }

    /// [`Self::on_air`] with any rotation drawn down to the member that airs:
    /// the answer names the member in `slot` and `index`, and the rotation the
    /// schedule placed in `rotation`. The draws land in `rotations`.
    pub fn on_air_drawn(
        &self,
        at: OffsetDateTime,
        seed_at: &dyn Fn(OffsetDateTime) -> u64,
        rotations: &mut std::collections::BTreeMap<String, crate::resume::RotationResume>,
    ) -> (OnAir<'a>, Option<crate::rotation::Drawn>) {
        let rule: &'a RuleConfig = self.rule;
        match self.on_air(at) {
            OnAir::Slot {
                slot,
                schedule,
                placement: Some(start),
                ..
            } => {
                let drawn = crate::rotation::resolve(rule, slot, start, seed_at, rotations);
                let leaf = drawn.leaf();
                // Validation guarantees every member exists.
                let (leaf, index) = rule
                    .slots
                    .get_key_value(leaf)
                    .and_then(|(name, _)| Some((name.as_str(), rule.slot_index(name)?)))
                    .expect("a drawn member is a declared slot");
                (
                    OnAir::Slot {
                        slot: leaf,
                        index,
                        schedule,
                        placement: Some(start),
                        rotation: Some(slot),
                    },
                    Some(drawn),
                )
            }
            other => (other, None),
        }
    }

    /// Every instant in `(from, until)` at which what [`Self::on_air`] answers
    /// can change, in time order: local midnight and each schedule range's
    /// start and end, on every local day. Between two of them the answer is
    /// constant, so a caller that wants each placement samples these instead of
    /// every minute.
    pub fn boundaries(&self, from: OffsetDateTime, until: OffsetDateTime) -> Vec<OffsetDateTime> {
        let mut minutes: Vec<u16> = vec![0];
        for schedule in &self.order {
            for range in schedule.hours.keys() {
                minutes.push(range.start);
                minutes.push(range.end);
            }
        }
        minutes.retain(|m| *m < 1440);
        minutes.sort_unstable();
        minutes.dedup();

        let mut out = Vec::new();
        let mut date = from.to_timezone(self.tz).date().previous_day();
        while let Some(day) = date {
            let mut any_in_range = false;
            for m in &minutes {
                let time = time::Time::from_hms((m / 60) as u8, (m % 60) as u8, 0)
                    .unwrap_or(time::Time::MIDNIGHT);
                let at = crate::tz::to_utc_assume_local(day, time, self.tz);
                if at >= until {
                    continue;
                }
                any_in_range = true;
                if at > from {
                    out.push(at);
                }
                // A local time the clock skips (spring forward) converts to an
                // instant before the gap; the first real minute after it is
                // where the zone's offset changes, within the next hour.
                let local = at.to_timezone(self.tz);
                if u16::from(local.hour()) * 60 + u16::from(local.minute()) != *m {
                    let offset = local.offset();
                    let (mut lo, mut hi) = (0i64, 60i64);
                    while hi - lo > 1 {
                        let mid = (lo + hi) / 2;
                        let probe = (at + Duration::minutes(mid)).to_timezone(self.tz);
                        if probe.offset() == offset {
                            lo = mid;
                        } else {
                            hi = mid;
                        }
                    }
                    let after = at + Duration::minutes(hi);
                    if after > from && after < until {
                        out.push(after);
                    }
                }
            }
            if !any_in_range {
                break;
            }
            date = day.next_day();
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The first whole minute after `from`, and before `until`, at which a
    /// different block is on air than at `from`.
    ///
    /// Walked a minute at a time in UTC and read back in the station's zone,
    /// so a daylight-saving change needs no special case: the local clock
    /// simply skips or repeats an hour under the walk.
    pub fn next_change(
        &self,
        from: OffsetDateTime,
        until: OffsetDateTime,
    ) -> Option<OffsetDateTime> {
        let now = self.on_air(from);
        let floor = from.replace_nanosecond(0).ok()?;
        let floor = floor - Duration::seconds(i64::from(floor.second()));
        let mut t = floor + Duration::MINUTE;
        while t < until {
            if !self.on_air(t).same_block(&now) {
                return Some(t);
            }
            t += Duration::MINUTE;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn rule(yaml: &str) -> RuleConfig {
        serde_norway::from_str(yaml).unwrap()
    }

    const SEASONAL: &str = r#"
blocks: [{ entries: [] }]
slots:
  halloween: { entries: [] }
  autumn: { entries: [] }
  prime: { entries: [] }
  cartoons: { entries: [] }
schedules:
  - name: every-day
    hours: { "20..23": prime }
  - name: autumn
    dates: ["09-22..12-20"]
    hours: { "00..24": autumn }
  - name: weekend
    weekdays: [sat, sun]
    hours: { "06..12": cartoons }
  - name: halloween
    dates: ["10-25..10-31"]
    hours: { "18..24": halloween }
"#;

    fn slot_at<'a>(cal: &Calendar<'a>, at: OffsetDateTime) -> &'a str {
        match cal.on_air(at) {
            OnAir::AnyTime => "any-time",
            OnAir::Slot { slot, .. } => slot,
        }
    }

    #[test]
    fn a_dated_schedule_outranks_a_weekday_one_which_outranks_every_day() {
        let r = rule(SEASONAL);
        let cal = Calendar::new(&r, time_tz::timezones::db::UTC).unwrap();
        // Oct 28: both dated schedules match; they tie on rank, so written
        // order puts `autumn` (written first) above `halloween`.
        assert_eq!(slot_at(&cal, datetime!(2026-10-28 19:00 UTC)), "autumn");
        // Feb 3, a Tuesday: nothing dated; `every-day` claims 20-23 only.
        assert_eq!(slot_at(&cal, datetime!(2027-02-02 21:00 UTC)), "prime");
        assert_eq!(slot_at(&cal, datetime!(2027-02-02 09:00 UTC)), "any-time");
        // Feb 6, a Saturday morning: the weekend outranks every day.
        assert_eq!(slot_at(&cal, datetime!(2027-02-06 09:00 UTC)), "cartoons");
    }

    #[test]
    fn an_hour_the_top_schedule_leaves_open_falls_to_the_next_one_down() {
        let r = rule(
            r#"
blocks: [{ entries: [] }]
slots:
  marathon: { entries: [] }
  cartoons: { entries: [] }
schedules:
  - name: weekend
    weekdays: [sat]
    hours: { "06..12": cartoons }
  - name: holiday
    dates: ["12-26"]
    hours: { "20..23": marathon }
"#,
        );
        let cal = Calendar::new(&r, time_tz::timezones::db::UTC).unwrap();
        // 2026-12-26 is a Saturday: the holiday takes 20-23 and leaves the
        // weekend's morning alone.
        assert_eq!(
            cal.on_air(datetime!(2026-12-26 21:00 UTC)),
            OnAir::Slot {
                slot: "marathon",
                index: 2,
                schedule: "holiday",
                placement: None,
                rotation: None,
            }
        );
        assert_eq!(
            cal.on_air(datetime!(2026-12-26 07:00 UTC)),
            OnAir::Slot {
                slot: "cartoons",
                index: 1,
                schedule: "weekend",
                placement: None,
                rotation: None,
            }
        );
        assert_eq!(cal.on_air(datetime!(2026-12-26 15:00 UTC)), OnAir::AnyTime);
    }

    #[test]
    fn the_next_change_is_the_first_minute_another_block_airs_in_local_time() {
        let r = rule(
            r#"
blocks: [{ entries: [] }]
slots:
  newyear: { entries: [] }
schedules:
  - name: newyear
    dates: ["01-01", "11-01"]
    hours: { "00..24": newyear }
"#,
        );
        let chicago = crate::tz::parse("America/Chicago").unwrap();
        let cal = Calendar::new(&r, chicago).unwrap();
        // 2026-12-31 14:00 CST. Local midnight is 06:00 UTC on Jan 1.
        let from = datetime!(2026-12-31 20:00:30 UTC);
        assert_eq!(
            cal.next_change(from, from + Duration::DAY),
            Some(datetime!(2027-01-01 06:00 UTC))
        );
        let during = datetime!(2027-01-01 12:00 UTC);
        assert_eq!(
            cal.next_change(during, during + Duration::DAY),
            Some(datetime!(2027-01-02 06:00 UTC))
        );
        // Nothing changes inside a horizon that ends before midnight.
        assert_eq!(cal.next_change(from, from + Duration::HOUR), None);
        // 2026-11-01 is the fall-back day: it starts at 05:00 UTC (CDT) and
        // runs 25 hours, ending at 06:00 UTC (CST).
        let fall_back = datetime!(2026-11-01 04:00 UTC);
        assert_eq!(
            cal.next_change(fall_back, fall_back + Duration::DAY),
            Some(datetime!(2026-11-01 05:00 UTC))
        );
        let during = datetime!(2026-11-01 05:00 UTC);
        assert_eq!(
            cal.next_change(during, during + Duration::DAY * 2),
            Some(datetime!(2026-11-02 06:00 UTC))
        );
    }
}

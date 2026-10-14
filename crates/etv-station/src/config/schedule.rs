//! A pattern block's `schedule:` (ADR 0021): named patterns over the block's
//! pools, each claiming local hours (and optionally weekdays) in the station
//! `tz`. The walker in [`crate::pattern`] runs whichever entry is active at its
//! running clock; this module owns only the question "which entry claims this
//! local hour", so validation and the walk can never disagree about it.

use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, Weekday};
use time_tz::{OffsetDateTimeExt, Tz};

use super::pool::PatternStep;

/// One `schedule:` entry.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleEntry {
    /// Keys this entry's place in its pattern in the `.resume` sidecar, and
    /// names it in the audit trail.
    pub name: String,
    /// The local hours this entry claims. Unset with `weekdays` unset makes it
    /// the default, which fills every hour no other entry claims; unset with
    /// `weekdays` set claims those whole days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hours: Option<HourRange>,
    /// The days this entry claims, naming the day its hour range *starts* on —
    /// a `sat` 21–6 entry is still Saturday's at Sunday 02:00. Unset means
    /// every day.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekdays: Option<Vec<Day>>,
    /// The repeating template walked while this entry is active.
    pub pattern: Vec<PatternStep>,
}

/// `[start, end)` in local whole hours. `end < start` wraps past midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HourRange {
    pub start: u8,
    pub end: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Day {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl Day {
    pub const ALL: [Day; 7] = [
        Day::Mon,
        Day::Tue,
        Day::Wed,
        Day::Thu,
        Day::Fri,
        Day::Sat,
        Day::Sun,
    ];

    fn previous(self) -> Day {
        let i = Day::ALL.iter().position(|d| *d == self).unwrap_or(0);
        Day::ALL[(i + 6) % 7]
    }

    fn from_weekday(w: Weekday) -> Day {
        match w {
            Weekday::Monday => Day::Mon,
            Weekday::Tuesday => Day::Tue,
            Weekday::Wednesday => Day::Wed,
            Weekday::Thursday => Day::Thu,
            Weekday::Friday => Day::Fri,
            Weekday::Saturday => Day::Sat,
            Weekday::Sunday => Day::Sun,
        }
    }
}

impl ScheduleEntry {
    /// The default entry claims nothing itself; it fills what nothing claims.
    pub fn is_default(&self) -> bool {
        self.hours.is_none() && self.weekdays.is_none()
    }

    fn on(&self, day: Day) -> bool {
        self.weekdays
            .as_ref()
            .is_none_or(|days| days.contains(&day))
    }

    /// Whether this entry claims local `hour` (0–23) on `day`. The one place
    /// the wrap-past-midnight rule is decided.
    pub fn claims(&self, day: Day, hour: u8) -> bool {
        if self.is_default() {
            return false;
        }
        let Some(HourRange { start, end }) = self.hours else {
            return self.on(day);
        };
        if start <= end {
            self.on(day) && hour >= start && hour < end
        } else if hour < end {
            self.on(day.previous())
        } else {
            self.on(day) && hour >= start
        }
    }
}

/// The entry active at local `(day, hour)`: the one claiming it, else the
/// default. `None` only for a schedule validation would have refused — an
/// unclaimed hour with no default.
pub fn active_entry(schedule: &[ScheduleEntry], day: Day, hour: u8) -> Option<usize> {
    schedule
        .iter()
        .position(|e| e.claims(day, hour))
        .or_else(|| schedule.iter().position(ScheduleEntry::is_default))
}

/// The local `(day, hour)` of a unix-second instant in `tz`.
pub fn local_slot(unix_secs: i64, tz: &'static Tz) -> (Day, u8) {
    let utc = OffsetDateTime::from_unix_timestamp(unix_secs).unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let local = utc.to_timezone(tz);
    (Day::from_weekday(local.weekday()), local.hour())
}

/// What is wrong with a schedule's claims, if anything: an hour range out of
/// bounds, two entries claiming one hour, two defaults, or an hour nothing
/// covers. Structural checks only — the patterns are checked by the caller.
pub fn claims_error(schedule: &[ScheduleEntry]) -> Option<String> {
    let defaults: Vec<&str> = schedule
        .iter()
        .filter(|e| e.is_default())
        .map(|e| e.name.as_str())
        .collect();
    if defaults.len() > 1 {
        return Some(format!(
            "schedule entries {:?} and {:?} both declare no `hours` or `weekdays` — at most one \
             entry may be the default that fills unclaimed hours",
            defaults[0], defaults[1]
        ));
    }
    for e in schedule {
        if let Some(HourRange { start, end }) = e.hours
            && (start > 23 || end > 24 || start == end)
        {
            return Some(format!(
                "schedule entry {:?} has hours {start}–{end}; start must be 0–23, end 1–24, \
                 and the two must differ",
                e.name
            ));
        }
        if e.weekdays.as_ref().is_some_and(Vec::is_empty) {
            return Some(format!(
                "schedule entry {:?} has an empty `weekdays` list, so it never airs",
                e.name
            ));
        }
    }
    for day in Day::ALL {
        for hour in 0..24u8 {
            let claiming: Vec<&str> = schedule
                .iter()
                .filter(|e| e.claims(day, hour))
                .map(|e| e.name.as_str())
                .collect();
            if claiming.len() > 1 {
                return Some(format!(
                    "schedule entries {:?} and {:?} both claim {day:?} {hour:02}:00 local — \
                     narrow one of their hours or weekdays",
                    claiming[0], claiming[1]
                ));
            }
            if claiming.is_empty() && defaults.is_empty() {
                return Some(format!(
                    "no schedule entry claims {day:?} {hour:02}:00 local — cover every hour, \
                     or add one entry with no `hours` or `weekdays` as the default",
                ));
            }
        }
    }
    None
}

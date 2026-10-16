//! `rule.schedules` and their date and clock ranges (ADR 0004).
//!
//! A schedule places named `rule.slots` blocks into ranges of the local day,
//! optionally only on some `dates:` or `weekdays:`. Which block is on air at a
//! given instant is [`crate::calendar`]'s question; this module owns the
//! written forms and what each one matches.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use time::{Date, Month};

/// One `rule.schedules` entry.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Schedule {
    pub name: String,

    /// Calendar days this schedule is limited to. Empty means every day.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dates: Vec<DateSpan>,

    /// Days of the week this schedule is limited to. Empty means every day.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub weekdays: Vec<DayName>,

    /// Clock range → the `rule.slots` name that airs over it.
    pub hours: BTreeMap<ClockRange, String>,
}

impl Schedule {
    /// How high this schedule sits: a dated schedule outranks a weekday one,
    /// which outranks one with neither. Written order breaks ties.
    pub fn rank(&self) -> u8 {
        if !self.dates.is_empty() {
            2
        } else if !self.weekdays.is_empty() {
            1
        } else {
            0
        }
    }

    /// Whether this schedule applies on local day `date`.
    pub fn matches_day(&self, date: Date) -> bool {
        let date_ok = self.dates.is_empty() || self.dates.iter().any(|d| d.contains(date));
        let weekday = DayName::of(date);
        let weekday_ok = self.weekdays.is_empty() || self.weekdays.contains(&weekday);
        date_ok && weekday_ok
    }

    /// The slot this schedule airs at local minute `minute` of day `date`.
    /// A range that wraps past midnight belongs to the day it starts on, so
    /// `"21..02"` on a Friday-only schedule covers Saturday 01:00.
    pub fn slot_at(&self, date: Date, minute: u16) -> Option<&str> {
        let today = self.matches_day(date);
        let yesterday = date.previous_day().is_some_and(|d| self.matches_day(d));
        self.hours.iter().find_map(|(range, slot)| {
            let hit = if range.wraps() {
                (today && minute >= range.start) || (yesterday && minute < range.end)
            } else {
                today && minute >= range.start && minute < range.end
            };
            hit.then_some(slot.as_str())
        })
    }
}

/// A day of the week, written `mon`..`sun`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DayName {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl DayName {
    pub fn of(date: Date) -> Self {
        match date.weekday() {
            time::Weekday::Monday => Self::Mon,
            time::Weekday::Tuesday => Self::Tue,
            time::Weekday::Wednesday => Self::Wed,
            time::Weekday::Thursday => Self::Thu,
            time::Weekday::Friday => Self::Fri,
            time::Weekday::Saturday => Self::Sat,
            time::Weekday::Sunday => Self::Sun,
        }
    }
}

/// A range of the local day, written `"HH..HH"` or `"HH:MM..HH:MM"`. The end
/// is exclusive and may be `24`; an end before the start wraps past midnight.
/// Held as minutes since local midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct ClockRange {
    pub start: u16,
    pub end: u16,
}

impl ClockRange {
    pub fn wraps(&self) -> bool {
        self.end < self.start
    }

    /// The minutes this range covers, as one or two `[start, end)` spans of a
    /// single day — a wrapping range splits at midnight.
    pub fn spans(&self) -> Vec<(u16, u16)> {
        if self.wraps() {
            vec![(self.start, 1440), (0, self.end)]
        } else {
            vec![(self.start, self.end)]
        }
    }
}

fn parse_clock(s: &str) -> Result<u16, String> {
    let (h, m) = match s.split_once(':') {
        Some((h, m)) => (h, m),
        None => (s, "00"),
    };
    let bad = || format!("{s:?} is not a time; write HH or HH:MM");
    if h.len() != 2 || m.len() != 2 {
        return Err(bad());
    }
    let h: u16 = h.parse().map_err(|_| bad())?;
    let m: u16 = m.parse().map_err(|_| bad())?;
    if m > 59 || h > 24 || (h == 24 && m != 0) {
        return Err(format!("{s:?} is not a time of day (00:00 to 24:00)"));
    }
    Ok(h * 60 + m)
}

fn fmt_clock(minute: u16) -> String {
    format!("{:02}:{:02}", minute / 60, minute % 60)
}

impl TryFrom<String> for ClockRange {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        let (a, b) = s
            .split_once("..")
            .ok_or_else(|| format!("{s:?} is not a range; write \"HH:MM..HH:MM\""))?;
        let start = parse_clock(a.trim())?;
        let end = parse_clock(b.trim())?;
        if start == 1440 {
            return Err(format!("{s:?} starts at 24:00; write 00:00"));
        }
        if start == end {
            return Err(format!(
                "{s:?} is empty; write \"00..24\" for the whole day"
            ));
        }
        Ok(Self { start, end })
    }
}

impl From<ClockRange> for String {
    fn from(r: ClockRange) -> String {
        format!("{}..{}", fmt_clock(r.start), fmt_clock(r.end))
    }
}

impl fmt::Display for ClockRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from(*self))
    }
}

/// A span of calendar days, both ends inclusive.
///
/// `"MM-DD"` or `"MM-DD..MM-DD"` repeats every year, and an end before the
/// start crosses December 31st. `"YYYY-MM-DD"` or `"YYYY-MM-DD..YYYY-MM-DD"`
/// is a one-off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub enum DateSpan {
    Annual { from: (u8, u8), to: (u8, u8) },
    Fixed { from: Date, to: Date },
}

impl DateSpan {
    pub fn contains(&self, date: Date) -> bool {
        match *self {
            Self::Annual { from, to } => {
                let md = (date.month() as u8, date.day());
                if from <= to {
                    from <= md && md <= to
                } else {
                    md >= from || md <= to
                }
            }
            Self::Fixed { from, to } => from <= date && date <= to,
        }
    }
}

enum DatePart {
    Annual(u8, u8),
    Fixed(Date),
}

fn parse_month_day(s: &str, whole: &str) -> Result<(u8, u8), String> {
    let (m, d) = s
        .split_once('-')
        .ok_or_else(|| format!("{whole:?}: {s:?} is not a date"))?;
    let bad = || format!("{whole:?}: {s:?} is not a date; write MM-DD or YYYY-MM-DD");
    if m.len() != 2 || d.len() != 2 {
        return Err(bad());
    }
    let m: u8 = m.parse().map_err(|_| bad())?;
    let d: u8 = d.parse().map_err(|_| bad())?;
    let month = Month::try_from(m).map_err(|_| bad())?;
    // 2024 is a leap year, so 02-29 is a day some years have.
    if d == 0 || d > month.length(2024) {
        return Err(format!("{whole:?}: {s:?} is not a day of the year"));
    }
    Ok((m, d))
}

fn parse_date_part(s: &str, whole: &str) -> Result<DatePart, String> {
    let s = s.trim();
    match s.len() {
        5 => {
            let (m, d) = parse_month_day(s, whole)?;
            Ok(DatePart::Annual(m, d))
        }
        10 if s.as_bytes()[4] == b'-' => {
            let year: i32 = s[..4]
                .parse()
                .map_err(|_| format!("{whole:?}: {s:?} is not a date"))?;
            let (m, d) = parse_month_day(&s[5..], whole)?;
            let month = Month::try_from(m).map_err(|_| format!("{whole:?}: bad month"))?;
            let date = Date::from_calendar_date(year, month, d)
                .map_err(|_| format!("{whole:?}: {s:?} is not a day of {year}"))?;
            Ok(DatePart::Fixed(date))
        }
        _ => Err(format!(
            "{whole:?}: {s:?} is not a date; write MM-DD or YYYY-MM-DD"
        )),
    }
}

impl TryFrom<String> for DateSpan {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        let (a, b) = match s.split_once("..") {
            Some((a, b)) => (a, b),
            None => (s.as_str(), s.as_str()),
        };
        match (parse_date_part(a, &s)?, parse_date_part(b, &s)?) {
            (DatePart::Annual(m1, d1), DatePart::Annual(m2, d2)) => Ok(Self::Annual {
                from: (m1, d1),
                to: (m2, d2),
            }),
            (DatePart::Fixed(from), DatePart::Fixed(to)) => {
                if to < from {
                    return Err(format!("{s:?} ends before it starts"));
                }
                Ok(Self::Fixed { from, to })
            }
            _ => Err(format!(
                "{s:?} mixes a yearly date with a dated one; give both ends a year or neither"
            )),
        }
    }
}

impl From<DateSpan> for String {
    fn from(d: DateSpan) -> String {
        match d {
            DateSpan::Annual { from, to } => {
                format!("{:02}-{:02}..{:02}-{:02}", from.0, from.1, to.0, to.1)
            }
            DateSpan::Fixed { from, to } => format!("{}..{}", fixed_date(from), fixed_date(to)),
        }
    }
}

fn fixed_date(d: Date) -> String {
    format!("{:04}-{:02}-{:02}", d.year(), d.month() as u8, d.day())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    fn span(s: &str) -> DateSpan {
        DateSpan::try_from(s.to_string()).unwrap()
    }

    fn clock(s: &str) -> ClockRange {
        ClockRange::try_from(s.to_string()).unwrap()
    }

    #[test]
    fn date_span_forms_match_the_days_they_name() {
        let halloween = span("10-25..10-31");
        assert!(halloween.contains(date!(2026 - 10 - 28)));
        assert!(halloween.contains(date!(2031 - 10 - 31)));
        assert!(!halloween.contains(date!(2026 - 11 - 01)));

        let winter = span("12-20..01-05");
        assert!(winter.contains(date!(2026 - 12 - 31)));
        assert!(winter.contains(date!(2027 - 01 - 05)));
        assert!(!winter.contains(date!(2027 - 01 - 06)));
        assert!(!winter.contains(date!(2026 - 12 - 19)));

        let new_year = span("01-01");
        assert!(new_year.contains(date!(2027 - 01 - 01)));
        assert!(!new_year.contains(date!(2027 - 01 - 02)));

        let thanksgiving = span("2026-11-26..2026-11-29");
        assert!(thanksgiving.contains(date!(2026 - 11 - 29)));
        assert!(!thanksgiving.contains(date!(2027 - 11 - 26)));
    }

    #[test]
    fn a_wrapping_range_belongs_to_the_day_it_starts_on() {
        let friday_late = Schedule {
            name: "late".into(),
            dates: vec![],
            weekdays: vec![DayName::Fri],
            hours: [(clock("21..02"), "late".to_string())].into(),
        };
        // 2026-10-16 is a Friday.
        assert_eq!(
            friday_late.slot_at(date!(2026 - 10 - 16), 22 * 60),
            Some("late")
        );
        assert_eq!(friday_late.slot_at(date!(2026 - 10 - 17), 60), Some("late"));
        assert_eq!(friday_late.slot_at(date!(2026 - 10 - 17), 22 * 60), None);
        assert_eq!(friday_late.slot_at(date!(2026 - 10 - 16), 60), None);
    }
}

use time::{Date, Duration, OffsetDateTime, Time};
use time_tz::{Offset, OffsetDateTimeExt, TimeZone, Tz, timezones};

use crate::errors::StationError;

pub fn parse(name: &str) -> Result<&'static Tz, StationError> {
    timezones::get_by_name(name).ok_or_else(|| StationError::Tz {
        tz: name.to_string(),
        reason: "unknown IANA zone".into(),
    })
}

/// Local midnight in `tz` at-or-before `now_utc`. Result is a UTC instant.
pub fn local_midnight_at_or_before(now_utc: OffsetDateTime, tz: &'static Tz) -> OffsetDateTime {
    let local_date = now_utc.to_timezone(tz).date();
    let candidate = to_utc_assume_local(local_date, Time::MIDNIGHT, tz);
    if candidate > now_utc {
        // DST edge case: walked into the future; step back a day.
        let prev_date = local_date.previous_day().expect("not min date");
        to_utc_assume_local(prev_date, Time::MIDNIGHT, tz)
    } else {
        candidate
    }
}

/// Return the next chunk boundary in `tz`, strictly after `start_utc`, snapped
/// to the local-time grid at multiples of `chunk_hours` (00:00, then chunk_hours,
/// 2 * chunk_hours, …). DST is honored via `to_utc_assume_local`, so a 24h chunk
/// can span 23h or 25h of UTC across spring-forward / fall-back, and non-24h
/// chunks land on local-clock boundaries even when DST changes inside them.
///
/// `chunk_hours` is expected to divide 24 (1/2/3/4/6/8/12/24); other values
/// still produce monotonic boundaries but may not be periodic across days.
pub fn add_chunk(start_utc: OffsetDateTime, chunk_hours: u32, tz: &'static Tz) -> OffsetDateTime {
    let local = start_utc.to_timezone(tz);
    let h = chunk_hours.max(1) as u8;
    let next_hour = ((local.hour() as u32 / h as u32) + 1) * h as u32;
    if next_hour >= 24 {
        let next_date = local.date().next_day().expect("not max date");
        to_utc_assume_local(next_date, Time::MIDNIGHT, tz)
    } else {
        let target = Time::from_hms(next_hour as u8, 0, 0).expect("valid hour");
        to_utc_assume_local(local.date(), target, tz)
    }
}

fn to_utc_assume_local(date: Date, time: Time, tz: &'static Tz) -> OffsetDateTime {
    let naive = date.with_time(time).assume_utc();
    let offset = tz.get_offset_primary().to_utc();
    let approx = naive - Duration::seconds(offset.whole_seconds() as i64);
    let actual_offset = approx.to_timezone(tz).offset();
    naive - Duration::seconds(actual_offset.whole_seconds() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn rejects_unknown_zone() {
        let err = parse("Atlantis/Lemuria").unwrap_err();
        assert!(matches!(err, StationError::Tz { .. }));
    }

    #[test]
    fn parses_chicago() {
        let tz = parse("America/Chicago").unwrap();
        assert_eq!(tz.name(), "America/Chicago");
    }

    #[test]
    fn add_24h_chunk_in_utc_zone() {
        let utc = parse("UTC").unwrap();
        let start = datetime!(2026-04-13 00:00:00 UTC);
        let next = add_chunk(start, 24, utc);
        assert_eq!(next, datetime!(2026-04-14 00:00:00 UTC));
    }

    #[test]
    fn add_24h_chunk_handles_dst_spring_forward() {
        // US/Central spring-forward: 2026-03-08 02:00 CST -> 03:00 CDT.
        // Local midnight 2026-03-08 → midnight 2026-03-09 spans 23h of UTC.
        let chicago = parse("America/Chicago").unwrap();
        let start = datetime!(2026-03-08 06:00:00 UTC); // = local midnight CST
        let next = add_chunk(start, 24, chicago);
        let span = next - start;
        assert_eq!(span.whole_hours(), 23, "spring forward span = {span}");
    }

    #[test]
    fn add_24h_chunk_handles_dst_fall_back() {
        // US/Central fall-back: 2026-11-01 02:00 CDT -> 01:00 CST.
        let chicago = parse("America/Chicago").unwrap();
        let start = datetime!(2026-11-01 05:00:00 UTC); // local midnight CDT
        let next = add_chunk(start, 24, chicago);
        let span = next - start;
        assert_eq!(span.whole_hours(), 25, "fall back span = {span}");
    }

    #[test]
    fn local_midnight_at_or_before_aligns_to_local_day() {
        let chicago = parse("America/Chicago").unwrap();
        // 2026-04-13 12:00 UTC = 07:00 CDT same date
        let now = datetime!(2026-04-13 12:00:00 UTC);
        let midnight = local_midnight_at_or_before(now, chicago);
        // local midnight 2026-04-13 CDT = 05:00 UTC
        assert_eq!(midnight, datetime!(2026-04-13 05:00:00 UTC));
    }
}

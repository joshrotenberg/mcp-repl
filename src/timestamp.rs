//! RFC 3339 timestamps on the proleptic Gregorian calendar.

use std::time::{SystemTime, UNIX_EPOCH};

const UNIX_EPOCH_OFFSET_DAYS: i64 = 719_468;

/// Parse an RFC 3339 timestamp such as `2026-07-28T00:00:01.5Z`.
///
/// Conservative on purpose: a leap second, a fraction longer than nine
/// digits, an invalid date, or an out-of-range offset returns `None`.
pub(crate) fn parse_rfc3339(text: &str) -> Option<i128> {
    let digits = |part: &str| -> Option<i64> {
        (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())).then(|| part.parse().ok())?
    };
    let field = |range: std::ops::Range<usize>| text.get(range).and_then(digits);
    let bytes = text.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let (year, month, day) = (field(0..4)?, field(5..7)?, field(8..10)?);
    let (hour, minute, second) = (field(11..13)?, field(14..16)?, field(17..19)?);
    let leap_year = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year => 29,
        2 => 28,
        _ => return None,
    };
    if !(1..=month_days).contains(&day) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let mut rest = &text[19..];
    let mut nanos: i128 = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let len = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if !(1..=9).contains(&len) {
            return None;
        }
        nanos = i128::from(digits(&fraction[..len])?) * 10i128.pow(9 - len as u32);
        rest = &fraction[len..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        zone if zone.len() == 6 && zone.is_ascii() && &zone[3..4] == ":" => {
            let sign = match &zone[..1] {
                "+" => 1,
                "-" => -1,
                _ => return None,
            };
            let hours = digits(&zone[1..3])?;
            let minutes = digits(&zone[4..6])?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            sign * (hours * 3600 + minutes * 60)
        }
        _ => return None,
    };
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - UNIX_EPOCH_OFFSET_DAYS;
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset;
    Some(i128::from(seconds) * 1_000_000_000 + nanos)
}

/// Format UTC to second precision, within RFC 3339's four-digit year range.
pub(crate) fn format_rfc3339_utc(time: SystemTime) -> Option<String> {
    let seconds = match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).ok()?,
        Err(error) => {
            let duration = error.duration();
            -i64::try_from(duration.as_secs()).ok()? - i64::from(duration.subsec_nanos() != 0)
        }
    };
    let (days, second_of_day) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));

    // Civil date from the day count, the inverse of the parser's conversion.
    let shifted = days + UNIX_EPOCH_OFFSET_DAYS;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let march_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * march_month + 2) / 5 + 1;
    let month = (march_month + 2) % 12 + 1;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    if !(0..=9999).contains(&year) {
        return None;
    }

    let hour = second_of_day / 3600;
    let minute = second_of_day / 60 % 60;
    let second = second_of_day % 60;
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn stamps_order_rfc3339_times_and_reject_other_text() {
        let base = parse_rfc3339("2026-07-28T00:00:01Z").unwrap();
        assert!(parse_rfc3339("2026-07-28T00:00:01.5Z").unwrap() > base);
        assert_eq!(parse_rfc3339("2026-07-28T02:00:01+02:00"), Some(base));
        assert_eq!(parse_rfc3339("2026-07-27T23:00:01-01:00"), Some(base));
        assert!(parse_rfc3339("2026-07-28T00:00:00.999999999Z").unwrap() < base);
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0), "epoch");
        assert!(parse_rfc3339("2028-02-29T00:00:00Z").is_some(), "leap day");
        assert!(
            parse_rfc3339("2000-02-29T00:00:00Z").is_some(),
            "400-year leap day"
        );
        assert!(parse_rfc3339("2026-07-28T00:00:01.123456789Z").is_some());
        assert!(parse_rfc3339("2026-07-28T23:59:59-23:59").is_some());
        for text in [
            "",
            "yesterday",
            "2026-07-28",
            "2026-07-28T00:00:01",
            "2026-13-28T00:00:01Z",
            "2026-07-28T00:00:01.Z",
            "2026-07-28T00:00:01+0200",
            // A leap second carries no ordering evidence.
            "2016-12-31T23:59:60Z",
            "2016-12-31T23:59:60.900Z",
            // More precision than nanoseconds.
            "2026-07-28T00:00:01.1234567891Z",
            // Days the month does not have.
            "2026-04-31T00:00:00Z",
            "2027-02-29T00:00:00Z",
            "1900-02-29T00:00:00Z",
            "2026-07-00T00:00:00Z",
            // Offsets outside HH 00-23 and MM 00-59, or not two digits each.
            "2026-07-28T00:00:01+00:99",
            "2026-07-28T00:00:01+24:00",
            "2026-07-28T00:00:01+5:000",
            "2026-07-28T00:00:01+0a:00",
            "2026-07-28T00:00:01+\u{e9}:00",
        ] {
            assert_eq!(parse_rfc3339(text), None, "{text}");
        }
    }

    #[test]
    fn utc_formatting_round_trips_dates_and_floors_to_seconds() {
        for date in [
            "0000-01-01T00:00:00Z",
            "1900-03-01T00:00:00Z",
            "1969-12-31T23:59:59Z",
            "1970-01-01T00:00:00Z",
            "2000-02-29T12:34:56Z",
            "2026-07-28T23:59:59Z",
            "9999-12-31T23:59:59Z",
        ] {
            let seconds = parse_rfc3339(date).unwrap() / 1_000_000_000;
            let offset = Duration::from_secs(u64::try_from(seconds.abs()).unwrap());
            let time = if seconds < 0 {
                UNIX_EPOCH.checked_sub(offset)
            } else {
                UNIX_EPOCH.checked_add(offset)
            };
            // SystemTime has platform-specific bounds, including Windows' epoch.
            if let Some(time) = time {
                let time = time + Duration::from_millis(500);
                assert_eq!(format_rfc3339_utc(time).as_deref(), Some(date));
            }
        }
    }
}

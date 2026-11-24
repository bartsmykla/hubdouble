use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A point in time with second precision, as GitHub renders it: RFC 3339 in UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp(pub i64);

impl Timestamp {
    /// The fixed time of seeded commits and objects (`2026-01-01T00:00:00Z`).
    pub const SEED: Timestamp = Timestamp(1_767_225_600);

    #[must_use]
    pub fn unix(self) -> i64 {
        self.0
    }

    #[must_use]
    pub fn plus_seconds(self, seconds: i64) -> Timestamp {
        Timestamp(self.0 + seconds)
    }

    /// Parses `YYYY-MM-DDTHH:MM:SSZ`, optionally with fractional seconds, or a `+HH:MM`/`-HH:MM` offset.
    ///
    /// # Errors
    ///
    /// Returns [`TimestampError`] when the text is not such a time.
    pub fn parse(text: &str) -> Result<Timestamp, TimestampError> {
        let b = text.as_bytes();
        if b.len() < 20
            || b[4] != b'-'
            || b[7] != b'-'
            || !matches!(b[10], b'T' | b't')
            || b[13] != b':'
            || b[16] != b':'
        {
            return Err(TimestampError);
        }
        let num = |from: usize, to: usize| -> Result<i64, TimestampError> {
            text.get(from..to)
                .and_then(|s| s.parse::<i64>().ok())
                .ok_or(TimestampError)
        };
        let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
        let (hour, minute, second) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
        if !(1..=12).contains(&month)
            || !(1..=31).contains(&day)
            || hour > 23
            || minute > 59
            || second > 60
        {
            return Err(TimestampError);
        }
        let mut rest = &text[19..];
        if let Some(frac) = rest.strip_prefix('.') {
            let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 {
                return Err(TimestampError);
            }
            rest = &frac[digits..];
        }
        let offset = match rest {
            "Z" | "z" => 0,
            _ if rest.len() == 6
                && (rest.starts_with('+') || rest.starts_with('-'))
                && &rest[3..4] == ":" =>
            {
                let h: i64 = rest[1..3].parse().map_err(|_| TimestampError)?;
                let m: i64 = rest[4..6].parse().map_err(|_| TimestampError)?;
                let sign = if rest.starts_with('-') { -1 } else { 1 };
                sign * (h * 3600 + m * 60)
            }
            _ => return Err(TimestampError),
        };
        let days = days_from_civil(year, month, day);
        Ok(Timestamp(
            days * 86_400 + hour * 3600 + minute * 60 + second - offset,
        ))
    }

    /// Reads a query time, such as `since`, as GitHub does: Ruby's `Time.parse` with
    /// two-digit years completed, in the `America/Los_Angeles` zone. It takes the first
    /// date and time that `Date._parse` finds in free text and ignores the rest. A missing
    /// leading field comes from `now`, a missing trailing field is its lowest value, a day
    /// past the month's end rolls over, and a zone that Ruby does not name is local time.
    /// Local time follows the zone's full history, from local mean time before 1883, and
    /// resolves a gap, a repeated hour or a `:60` second as Ruby's `Time.local` does on Linux.
    ///
    /// # Errors
    ///
    /// Returns [`TimestampError`] where Ruby raises: no date or time, a field out of
    /// range, or more than 128 characters.
    pub fn parse_query(text: &str, now: Timestamp) -> Result<QueryTime, TimestampError> {
        crate::ruby_time::time_parse(text, now.0)
            .map(|(seconds, micros)| QueryTime {
                at: Timestamp(seconds),
                micros,
            })
            .ok_or(TimestampError)
    }

    /// Reads a commit list `since` or `until` as GitHub does. Ruby's `DateTime.parse` in a UTC
    /// process reads the text: the first date and time that `Date._parse` finds, a missing
    /// field completed from `now` as Ruby completes it, a zone that Ruby does not name read
    /// as UTC, on the Julian calendar before 1582-10-15. GitHub writes that time back as
    /// `%Y-%m-%dT%H:%M:%S` with `Z` or the `%:z` offset, and Git's `approxidate` reads it in
    /// UTC at `now`. Git reads a year from 1970 to 2099 exactly. For another year it keeps
    /// the month and day, takes the year from `now` (the year before when the month is
    /// later), and reads the hours and minutes of an offset as the time of day.
    ///
    /// # Errors
    ///
    /// Returns [`TimestampError`] where Ruby raises `Date::Error`: no date, an invalid
    /// date or time, or more than 128 characters.
    pub fn parse_commit_date(text: &str, now: Timestamp) -> Result<Timestamp, TimestampError> {
        let time = crate::ruby_datetime::date_time_parse(text, now.0).ok_or(TimestampError)?;
        Ok(Timestamp(crate::git_date::approxidate(
            &time.xmlschema(),
            now.0,
        )))
    }

    /// Reads an HTTP date in any of the three forms of RFC 9110: IMF-fixdate, RFC 850 and
    /// asctime, as GitHub reads `If-Modified-Since`. The weekday is not checked.
    ///
    /// # Errors
    ///
    /// Returns [`TimestampError`] for any other text.
    pub fn parse_http_date(text: &str) -> Result<Timestamp, TimestampError> {
        let parts: Vec<&str> = text.split_ascii_whitespace().collect();
        let (day, month, year, time) = match parts.as_slice() {
            [_, day, month, year, time, "GMT"] | [_, month, day, time, year] => {
                (*day, *month, (*year).to_owned(), *time)
            }
            [_, date, time, "GMT"] => {
                let mut d = date.split('-');
                let (Some(day), Some(month), Some(year), None) =
                    (d.next(), d.next(), d.next(), d.next())
                else {
                    return Err(TimestampError);
                };
                let year = match year.parse::<u8>() {
                    Ok(short) if year.len() == 2 && short < 50 => format!("20{year}"),
                    Ok(_) if year.len() == 2 => format!("19{year}"),
                    _ => year.to_owned(),
                };
                (day, month, year, *time)
            }
            _ => return Err(TimestampError),
        };
        let month = MONTHS
            .iter()
            .position(|m| *m == month)
            .ok_or(TimestampError)?;
        if year.len() != 4 || time.len() != 8 || day.is_empty() || day.len() > 2 {
            return Err(TimestampError);
        }
        Timestamp::parse(&format!("{year}-{:02}-{day:0>2}T{time}Z", month + 1))
    }

    /// Whether a SQL `DATETIME` holds the time: `1000-01-01T00:00:00Z` to `9999-12-31T23:59:59Z`.
    #[must_use]
    pub fn is_datetime(self) -> bool {
        (-30_610_224_000..=253_402_300_799).contains(&self.0)
    }
}

/// A query time that keeps the microseconds that a second-precision [`Timestamp`] drops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryTime {
    pub at: Timestamp,
    pub micros: u32,
}

impl QueryTime {
    /// Whether a second-precision time is at or after this one, as a `since` filter keeps it.
    #[must_use]
    pub fn admits(self, t: Timestamp) -> bool {
        t > self.at || (t == self.at && self.micros == 0)
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not an RFC 3339 timestamp")]
pub struct TimestampError;

pub(crate) fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The proleptic Gregorian `(year, month, day)` of a count of days since 1970-01-01.
#[must_use]
pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

impl Timestamp {
    /// The time as an HTTP date (RFC 9110 `IMF-fixdate`): `Tue, 24 Nov 2026 07:28:47 GMT`.
    #[must_use]
    pub fn http_date(self) -> String {
        const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        let days = self.0.div_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        let secs = self.0.rem_euclid(86_400);
        let weekday = DAYS[usize::try_from((days + 4).rem_euclid(7)).unwrap_or(0)];
        let month = MONTHS[usize::try_from(month - 1).unwrap_or(0)];
        format!(
            "{weekday}, {day:02} {month} {year:04} {:02}:{:02}:{:02} GMT",
            secs / 3600,
            secs / 60 % 60,
            secs % 60
        )
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (year, month, day) = civil_from_days(self.0.div_euclid(86_400));
        let secs = self.0.rem_euclid(86_400);
        write!(
            f,
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
            secs / 3600,
            secs / 60 % 60,
            secs % 60
        )
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        Timestamp::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// The source of wall time. Every time-dependent rule reads it, so tests use [`ManualClock`].
pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Timestamp(i64::try_from(secs).unwrap_or(i64::MAX))
    }
}

/// A clock that moves only when a test sets or advances it.
#[derive(Debug)]
pub struct ManualClock(AtomicI64);

impl ManualClock {
    #[must_use]
    pub fn new(start: Timestamp) -> ManualClock {
        ManualClock(AtomicI64::new(start.0))
    }

    pub fn set(&self, t: Timestamp) {
        self.0.store(t.0, Ordering::SeqCst);
    }

    pub fn advance(&self, seconds: i64) {
        self.0.fetch_add(seconds, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Timestamp {
        Timestamp(self.0.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_http_dates_as_the_storage_answer_of_an_expired_job_log_link() {
        let current = Timestamp::parse("2026-11-24T07:28:47Z").unwrap();
        assert_eq!(current.http_date(), "Tue, 24 Nov 2026 07:28:47 GMT");
        assert_eq!(Timestamp::SEED.http_date(), "Thu, 01 Jan 2026 00:00:00 GMT");
    }

    #[test]
    fn formats_and_parses_rfc3339_utc() {
        assert_eq!(Timestamp::SEED.to_string(), "2026-01-01T00:00:00Z");
        assert_eq!(Timestamp(0).to_string(), "1970-01-01T00:00:00Z");
        assert_eq!(Timestamp(951_782_400).to_string(), "2000-02-29T00:00:00Z");
        for t in [0, 951_782_400, 1_767_225_600, 4_102_444_799] {
            assert_eq!(
                Timestamp::parse(&Timestamp(t).to_string()),
                Ok(Timestamp(t))
            );
        }
        assert_eq!(
            Timestamp::parse("2026-01-01T01:00:00+01:00"),
            Ok(Timestamp::SEED)
        );
        assert_eq!(
            Timestamp::parse("2026-01-01T00:00:00.250Z"),
            Ok(Timestamp::SEED)
        );
        assert!(Timestamp::parse("2026-13-01T00:00:00Z").is_err());
        assert!(Timestamp::parse("yesterday").is_err());
    }

    #[test]
    fn query_times_read_free_text_as_live_github_does() {
        // Given the since values that live GET /repos/microsoft/vscode/issues read, 2026-11-23,
        // and a clock at 2026-11-23T12:00:00Z
        let now = Timestamp(1_795_435_200);
        let accepted = [
            ("2025-11-20T10:00:00Z", "2025-11-20T10:00:00Z"),
            ("Nov 20 2025", "2025-11-20T08:00:00Z"),
            ("20 Nov 2025", "2025-11-20T08:00:00Z"),
            ("November 20, 2025", "2025-11-20T08:00:00Z"),
            ("2025 Nov 20", "2025-11-20T08:00:00Z"),
            ("Mon Nov 20 2025", "2025-11-20T08:00:00Z"),
            ("20th Nov 2025", "2025-11-20T08:00:00Z"),
            ("Nov 20th, 2025", "2025-11-20T08:00:00Z"),
            ("20-Nov-2025", "2025-11-20T08:00:00Z"),
            ("20/11/2025", "2025-11-20T08:00:00Z"),
            ("2025/11/20", "2025-11-20T08:00:00Z"),
            ("20.11.2025", "2025-11-20T08:00:00Z"),
            ("20251120", "2025-11-20T08:00:00Z"),
            ("2025-324", "2025-11-20T08:00:00Z"),
            ("H27.11.20", "2015-11-20T08:00:00Z"),
            ("Nov 20 25", "2025-11-20T08:00:00Z"),
            ("'25-11-20", "2025-11-20T08:00:00Z"),
            ("20 Nov 99", "1999-11-20T08:00:00Z"),
            ("20 Nov 68", "2068-11-20T08:00:00Z"),
            ("x2025-11-20", "2025-11-20T08:00:00Z"),
            ("2025-11-20 foo", "2025-11-20T08:00:00Z"),
            ("abc 2025-11-20 def 10:00", "2025-11-20T18:00:00Z"),
            ("2025-11-20T10:00:00Zjunk", "2025-11-20T18:00:00Z"),
            ("2025-11-20 10:00", "2025-11-20T18:00:00Z"),
            ("2025-07-01 10:00", "2025-07-01T17:00:00Z"),
            ("2025-03-09 02:30", "2025-03-09T10:30:00Z"),
            ("2025-11-02 01:30", "2025-11-02T09:30:00Z"),
            ("2025-11-02 01:30:60", "2025-11-02T08:31:00Z"),
            ("1950-09-24 01:30:60", "1950-09-24T08:31:00Z"),
            ("1950-07-01 10:00", "1950-07-01T17:00:00Z"),
            ("1943-01-15 10:00", "1943-01-15T17:00:00Z"),
            ("1883-01-01 10:00", "1883-01-01T17:52:58Z"),
            ("2040-03-11 02:30", "2040-03-11T10:30:00Z"),
            ("2040-11-04 01:30", "2040-11-04T09:30:00Z"),
            ("Nov 20 2025 10am", "2025-11-20T18:00:00Z"),
            ("Nov 20 2025 10:00 pm", "2025-11-21T06:00:00Z"),
            ("Nov 20 2025 24:00", "2025-11-21T08:00:00Z"),
            ("Nov 20 2025 23:59:60", "2025-11-21T08:00:00Z"),
            ("2025-11-20 10:00 PST", "2025-11-20T18:00:00Z"),
            ("2025-11-20 10:00 EDT", "2025-11-20T14:00:00Z"),
            ("2025-11-20 10:00 UTC", "2025-11-20T10:00:00Z"),
            ("2025-11-20 10:00 a", "2025-11-20T09:00:00Z"),
            ("2025-11-20 10:00 +01:30", "2025-11-20T08:30:00Z"),
            ("2025-11-20 10:00 -0130", "2025-11-20T11:30:00Z"),
            ("2025-11-20 10:00 CET", "2025-11-20T18:00:00Z"),
            ("2025-11-20 10:00 GMT+1", "2025-11-20T18:00:00Z"),
            ("Sat, 20 Nov 2025 10:00:00 GMT", "2025-11-20T10:00:00Z"),
            ("2025-02-30", "2025-03-02T08:00:00Z"),
            ("Nov 31 2025", "2025-12-01T08:00:00Z"),
            ("10:00", "2026-11-23T18:00:00Z"),
            ("Nov", "2026-11-01T07:00:00Z"),
            ("20 November", "2026-11-20T08:00:00Z"),
        ];
        for (text, expected) in accepted {
            // When / Then
            let read = Timestamp::parse_query(text, now).map(|t| t.at.to_string());
            assert_eq!(read.as_deref(), Ok(expected), "{text}");
        }
        let too_long = format!("2025-11-20 {}", "x".repeat(118));
        for text in [
            "",
            "bogus",
            "yesterday",
            "Thursday",
            "1700000000",
            "2025",
            "2025-11",
            "2025-W47-4",
            "11/20/2025",
            "2025-02-32",
            "2025-11-20 25:00",
            "2025-11-20 10:61",
            "2025-13-01",
            &too_long,
        ] {
            assert_eq!(
                Timestamp::parse_query(text, now),
                Err(TimestampError),
                "{text}"
            );
        }
        for (text, held) in [
            ("1000-01-01T00:00:00Z", true),
            ("9999-12-31T23:59:59Z", true),
            ("Dec 31 0999", false),
            ("0999-12-31T16:07:02", true),
            ("0999-12-31T16:07:01", false),
            ("Jan 1 10000", false),
            ("2025-11-20 BC", false),
            ("1000-01-01T00:00:00+01:00", false),
            ("9999-12-31T23:00:00-02:00", false),
        ] {
            let read = Timestamp::parse_query(text, now).map(|t| t.at.is_datetime());
            assert_eq!(read, Ok(held), "{text}");
        }
    }

    #[test]
    fn commit_dates_read_free_text_and_years_past_git_as_the_live_commit_list_does() {
        // Given the since and until values that live GET /repos/cli/cli/commits read, 2026-11-24,
        // a year outside 1970 to 2099 read as Git reads it (docs.github.com/rest/commits/commits),
        // and a clock at Tuesday 2026-11-24T12:00:00Z
        let now = Timestamp(1_795_521_600);
        for (text, expected) in [
            ("2100-11-20T12:00:00Z", "2026-11-20T12:00:00Z"),
            ("10000-05-05T12:00:00Z", "2026-05-05T12:00:00Z"),
            ("1969-12-01T12:00:00Z", "2025-12-01T12:00:00Z"),
            ("1969-12-10T12:00:00Z", "2026-10-12T12:00:00Z"),
            ("2100-12-20T12:00:00Z", "2012-12-20T12:00:00Z"),
            ("9999-12-31T12:00:00Z", "2012-12-31T12:00:00Z"),
            ("2100-11-23T18:24:25+05:00", "2026-11-23T05:00:00Z"),
            ("2100-11-19T10:00:00+18:46", "2026-11-19T18:46:00Z"),
            ("1500-02-29T12:00:00Z", "2026-03-01T12:00:00Z"),
            ("0080-05-05T12:00:00Z", "1980-05-05T12:00:00Z"),
            ("0005-03-04T12:00:00Z", "2004-05-03T12:00:00Z"),
            ("1999-12-31 BC", "1998-12-31T00:00:00Z"),
            ("1970-01-01T02:00:00+05:00", "1970-01-01T05:00:00Z"),
            ("2099-12-31T23:00:00-05:00", "2099-12-31T05:00:00Z"),
            ("2026-11-18T17:16:33Z", "2026-11-18T17:16:33Z"),
            ("2026-11-18T17:16:33.5Z", "2026-11-18T17:16:33Z"),
            ("2026-11-18 17:16:33", "2026-11-18T17:16:33Z"),
            ("18 Nov 2026 17:16:33 PST", "2026-11-19T01:16:33Z"),
            ("2026-11-18 17:16:33 +0530", "2026-11-18T11:46:33Z"),
            ("2026-11-18 23:59:60", "2026-11-18T23:59:59Z"),
            ("2026-11-18 24:00", "2026-11-19T00:00:00Z"),
            ("2026-W47-3", "2026-11-18T00:00:00Z"),
            ("2026-322", "2026-11-18T00:00:00Z"),
            ("Wed", "2026-11-25T00:00:00Z"),
            ("10:00", "2026-11-24T10:00:00Z"),
            ("Nov", "2026-11-01T00:00:00Z"),
            ("2028-02-29", "2028-02-29T00:00:00Z"),
        ] {
            // When / Then
            let read = Timestamp::parse_commit_date(text, now).map(|t| t.to_string());
            assert_eq!(read.as_deref(), Ok(expected), "{text}");
        }
        for text in [
            "",
            "garbage",
            "1763486193",
            "2026",
            "2026-11-31",
            "2026-02-29",
            "25:00",
            "2026-11-18 24:00:01",
            "2026-13-01",
        ] {
            assert_eq!(
                Timestamp::parse_commit_date(text, now),
                Err(TimestampError),
                "{text}"
            );
        }
    }

    #[test]
    fn http_dates_read_as_live_github_reads_if_modified_since() {
        // Given the Last-Modified of live GET /users/octocat, 2026-11-24, and the forms of
        // If-Modified-Since that it answered with 304
        let at = Timestamp::parse("2026-11-22T12:47:20Z");
        // When / Then
        for text in [
            "Sun, 22 Nov 2026 12:47:20 GMT",
            "Sunday, 22-Nov-26 12:47:20 GMT",
            "Sun Nov 22 12:47:20 2026",
            "Sun Nov  2 12:47:20 2026",
        ] {
            let expected = if text.contains(" 2 ") {
                Timestamp::parse("2026-11-02T12:47:20Z")
            } else {
                at
            };
            assert_eq!(Timestamp::parse_http_date(text), expected, "{text}");
        }
        for text in [
            "garbage",
            "2026-11-22T12:47:20Z",
            "Sun, 22 Foo 2026 12:47:20 GMT",
            "",
        ] {
            assert_eq!(
                Timestamp::parse_http_date(text),
                Err(TimestampError),
                "{text}"
            );
        }
    }

    #[test]
    fn manual_clock_moves_only_when_told() {
        let clock = ManualClock::new(Timestamp::SEED);
        assert_eq!(clock.now(), Timestamp::SEED);
        clock.advance(61);
        assert_eq!(clock.now(), Timestamp::SEED.plus_seconds(61));
    }
}

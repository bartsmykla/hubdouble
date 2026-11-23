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

    /// Parses a time in a query, such as `since`, as GitHub reads one: an ISO 8601 date
    /// (`2020-01-01`, `2020-1-1` or `20200101`), optionally followed by `T` or a space and
    /// `HH:MM`, `HH:MM:SS` or `HH:MM:SS.fff`, and by `Z` or a `+HH:MM`, `+HHMM` or `+HH`
    /// offset. A time without an offset is UTC, and a date without a time is its midnight.
    ///
    /// # Errors
    ///
    /// Returns [`TimestampError`] when the text is not such a time.
    pub fn parse_query(text: &str) -> Result<Timestamp, TimestampError> {
        let text = text.trim();
        let split = text.find(['T', 't', ' ']).unwrap_or(text.len());
        let (date, rest) = text.split_at(split);
        let (year, month, day) = query_date(date)?;
        let (seconds, offset) = match rest.get(1..) {
            None => (0, 0),
            Some(time) => query_time(time)?,
        };
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return Err(TimestampError);
        }
        Ok(Timestamp(
            days_from_civil(year, month, day) * 86_400 + seconds - offset,
        ))
    }
}

fn digits(text: &str, lengths: std::ops::RangeInclusive<usize>) -> Result<i64, TimestampError> {
    if !lengths.contains(&text.len()) || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(TimestampError);
    }
    text.parse().map_err(|_| TimestampError)
}

fn query_date(date: &str) -> Result<(i64, i64, i64), TimestampError> {
    let mut parts = date.split('-');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(y), Some(m), Some(d), None) => {
            Ok((digits(y, 4..=4)?, digits(m, 1..=2)?, digits(d, 1..=2)?))
        }
        (Some(compact), None, None, None) if compact.len() == 8 => Ok((
            digits(&compact[..4], 4..=4)?,
            digits(&compact[4..6], 2..=2)?,
            digits(&compact[6..], 2..=2)?,
        )),
        _ => Err(TimestampError),
    }
}

fn query_time(time: &str) -> Result<(i64, i64), TimestampError> {
    let zone_at = time.find(['Z', 'z', '+', '-']).unwrap_or(time.len());
    let (clock, zone) = time.split_at(zone_at);
    let clock = clock.split_once('.').map_or(clock, |(whole, frac)| {
        if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
            ""
        } else {
            whole
        }
    });
    let mut parts = clock.split(':');
    let (hour, minute, second) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(m), s, None) => (
            digits(h, 2..=2)?,
            digits(m, 2..=2)?,
            s.map_or(Ok(0), |s| digits(s, 2..=2))?,
        ),
        _ => return Err(TimestampError),
    };
    if hour > 23 || minute > 59 || second > 60 {
        return Err(TimestampError);
    }
    let offset = match zone.as_bytes().first() {
        None => 0,
        Some(b'Z' | b'z') if zone.len() == 1 => 0,
        Some(&sign @ (b'+' | b'-')) => {
            let body = &zone[1..];
            let (h, m) = match body.len() {
                2 => (body, "00"),
                4 => body.split_at(2),
                5 if &body[2..3] == ":" => (&body[..2], &body[3..]),
                _ => return Err(TimestampError),
            };
            let seconds = digits(h, 2..=2)? * 3600 + digits(m, 2..=2)? * 60;
            if sign == b'-' { -seconds } else { seconds }
        }
        Some(_) => return Err(TimestampError),
    };
    Ok((hour * 3600 + minute * 60 + second, offset))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not an RFC 3339 timestamp")]
pub struct TimestampError;

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
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
    fn query_times_read_the_iso_8601_forms_that_live_github_accepts() {
        // Given the since values live GET /repos/cli/cli/issues accepted and refused, 2026-11-23
        let midnight = Timestamp(1_577_836_800);
        let accepted = [
            ("2020-01-01", midnight),
            ("2020-1-1", midnight),
            ("20200101", midnight),
            (" 2020-01-01", midnight),
            ("2020-01-01T00:00", midnight),
            ("2020-01-01 00:00:00", midnight),
            ("2020-01-01T00:00:00", midnight),
            ("2020-01-01T00:00:00.123Z", midnight),
            ("2020-01-01T02:00:00+02:00", midnight),
            ("2020-01-01T02:00:00+0200", midnight),
            ("2019-12-31T22:00:00-02", midnight),
        ];
        for (text, expected) in accepted {
            // When / Then
            assert_eq!(Timestamp::parse_query(text), Ok(expected), "{text}");
        }
        for text in [
            "",
            "bogus",
            "2020",
            "2020-01",
            "2020-01-01T25:00:00Z",
            "2020-13-01",
        ] {
            assert_eq!(Timestamp::parse_query(text), Err(TimestampError), "{text}");
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

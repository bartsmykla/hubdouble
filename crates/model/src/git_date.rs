use crate::time::{civil_from_days, days_from_civil};

const DAY: i64 = 86_400;
const MDAYS: [i64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
const TIMESTAMP_MAX: i64 = ((2100 - 1970) * 365 + 32) * DAY - 1;
const FUTURE_SLACK: i64 = 10 * DAY;

#[derive(Clone, Copy)]
struct Tm {
    year: i64,
    mon: i64,
    mday: i64,
    hour: i64,
    min: i64,
    sec: i64,
}

impl Tm {
    const UNSET: Tm = Tm {
        year: -1,
        mon: -1,
        mday: -1,
        hour: -1,
        min: -1,
        sec: -1,
    };

    fn utc(t: i64) -> Tm {
        let (year, mon, mday) = civil_from_days(t.div_euclid(DAY));
        let s = t.rem_euclid(DAY);
        Tm {
            year: year - 1900,
            mon: mon - 1,
            mday,
            hour: s / 3600,
            min: s / 60 % 60,
            sec: s % 60,
        }
    }

    fn nodate(&self) -> bool {
        [
            self.year, self.mon, self.mday, self.hour, self.min, self.sec,
        ]
        .iter()
        .all(|f| *f < 0)
    }

    fn date_known(&self) -> bool {
        self.year != -1 && self.mon != -1 && self.mday != -1
    }

    fn mktime(&self) -> i64 {
        (days_from_civil(self.year + 1900, self.mon + 1, 1) + self.mday - 1) * DAY
            + self.hour * 3600
            + self.min * 60
            + self.sec
    }
}

fn tm_to_time_t(tm: &Tm) -> Option<i64> {
    let year = tm.year - 70;
    if !(0..=129).contains(&year) || !(0..=11).contains(&tm.mon) {
        return None;
    }
    let mut day = tm.mday;
    if tm.mon < 2 || (year + 2) % 4 != 0 {
        day -= 1;
    }
    if tm.hour < 0 || tm.min < 0 || tm.sec < 0 {
        return None;
    }
    let mon = usize::try_from(tm.mon).ok()?;
    Some(
        (year * 365 + (year + 1) / 4 + MDAYS[mon] + day) * DAY
            + tm.hour * 3600
            + tm.min * 60
            + tm.sec,
    )
}

fn number(s: &[u8], from: usize) -> (i64, usize) {
    let n = s[from..].iter().take_while(|b| b.is_ascii_digit()).count();
    let value = s[from..from + n].iter().fold(0_i64, |v, d| {
        v.saturating_mul(10).saturating_add(i64::from(d - b'0'))
    });
    (value, from + n)
}

fn digit_at(s: &[u8], i: usize) -> bool {
    s.get(i).is_some_and(u8::is_ascii_digit)
}

fn set_date(
    year: i64,
    month: i64,
    day: i64,
    refuse_future: Option<(&Tm, i64)>,
    tm: &mut Tm,
) -> bool {
    if !(1..13).contains(&month) || !(1..32).contains(&day) {
        return false;
    }
    let mut check = *tm;
    let r = if refuse_future.is_some() {
        &mut check
    } else {
        &mut *tm
    };
    r.mon = month - 1;
    r.mday = day;
    if year == -1 {
        match refuse_future {
            Some((now_tm, _)) => r.year = now_tm.year,
            None => return false,
        }
    } else if (1970..2100).contains(&year) {
        r.year = year - 1900;
    } else if year > 70 && year < 100 {
        r.year = year;
    } else if year < 38 {
        r.year = year + 100;
    } else {
        return false;
    }
    let Some((_, now)) = refuse_future else {
        return true;
    };
    if tm_to_time_t(&check).is_some_and(|specified| now + FUTURE_SLACK < specified) {
        return false;
    }
    tm.mon = check.mon;
    tm.mday = check.mday;
    if year != -1 {
        tm.year = check.year;
    }
    true
}

fn set_time(hour: i64, min: i64, sec: i64, tm: &mut Tm) -> bool {
    let valid = (0..=24).contains(&hour) && (0..60).contains(&min) && (0..=60).contains(&sec);
    if valid {
        (tm.hour, tm.min, tm.sec) = (hour, min, sec);
    }
    valid
}

fn match_multi_number(
    num: i64,
    s: &[u8],
    start: usize,
    end: usize,
    tm: &mut Tm,
    now: i64,
) -> usize {
    let c = s[end];
    let (num2, mut end) = number(s, end + 1);
    let mut num3 = -1;
    if s.get(end) == Some(&c) && digit_at(s, end + 1) {
        (num3, end) = number(s, end + 1);
    }
    if c == b':' {
        if !set_time(num, num2, num3.max(0), tm) {
            return 0;
        }
        if s.get(end) == Some(&b'.') && digit_at(s, end + 1) && tm.date_known() {
            end = number(s, end + 1).1;
        }
        return end - start;
    }
    let now_tm = Tm::utc(now);
    let refuse = Some((&now_tm, now));
    let found = (num > 70
        && (set_date(num, num2, num3, None, tm) || set_date(num, num3, num2, None, tm)))
        || (c != b'.' && set_date(num3, num, num2, refuse, tm))
        || set_date(num3, num2, num, refuse, tm)
        || (c == b'.' && set_date(num3, num, num2, refuse, tm));
    if found { end - start } else { 0 }
}

fn match_digit(s: &[u8], i: usize, tm: &mut Tm, offset: &mut Option<i64>, now: i64) -> usize {
    let (num, end) = number(s, i);
    if matches!(s.get(end), Some(b':' | b'.' | b'/' | b'-')) && digit_at(s, end + 1) {
        let matched = match_multi_number(num, s, i, end, tm, now);
        if matched > 0 {
            return matched;
        }
    }
    let n = end - i;
    if n == 8 || n == 6 {
        let (high, mid, low) = (num / 10_000, num % 10_000 / 100, num % 100);
        if n == 8 {
            set_date(high, mid, low, None, tm);
        } else if set_time(high, mid, low, tm) && s.get(end) == Some(&b'.') && digit_at(s, end + 1)
        {
            return number(s, end + 1).1 - i;
        }
        return n;
    }
    if tm.hour == -1 && tm.min == 0 && tm.sec == 0 {
        let (hour, min) = if n == 4 {
            (num / 100, num % 100)
        } else {
            (num, 0)
        };
        if (n == 4 || n == 2) && !tm.nodate() && set_time(hour, min, 0, tm) {
            return n;
        }
        (tm.min, tm.sec) = (-1, -1);
    }
    if n == 4 {
        if num <= 1400 && offset.is_none() {
            *offset = Some(num / 100 * 60 + num % 100);
        } else if num > 1900 && num < 2100 {
            tm.year = num - 1900;
        }
        return n;
    }
    if n > 2 {
        return n;
    }
    if num > 0 && num < 32 && tm.mday < 0 {
        tm.mday = num;
        return n;
    }
    if n == 2 && tm.year < 0 {
        if num < 10 && tm.mday >= 0 {
            tm.year = num + 100;
            return n;
        }
        if num >= 70 {
            tm.year = num;
            return n;
        }
    }
    if num > 0 && num < 13 && tm.mon < 0 {
        tm.mon = num - 1;
    }
    n
}

fn match_tz(s: &[u8], i: usize, offset: &mut Option<i64>) -> usize {
    let (mut hour, mut end) = number(s, i + 1);
    let n = end - (i + 1);
    let mut min = 0;
    if n == 4 {
        min = hour % 100;
        hour /= 100;
    } else if n != 2 {
        min = 99;
    } else if s.get(end) == Some(&b':') {
        (min, end) = number(s, end + 1);
        if end - (i + 1) != 5 {
            min = 99;
        }
    }
    if min < 60 && hour < 24 {
        let minutes = hour * 60 + min;
        *offset = Some(if s[i] == b'-' { -minutes } else { minutes });
    }
    end - i
}

fn parse_date_basic(s: &[u8], now: i64) -> Option<i64> {
    let mut tm = Tm::UNSET;
    let mut offset = None;
    let mut i = 0;
    while i < s.len() && s[i] != b'\n' {
        let c = s[i];
        let matched = if c.is_ascii_alphabetic() {
            if c.eq_ignore_ascii_case(&b'z') && !s.get(i + 1).is_some_and(u8::is_ascii_alphanumeric)
            {
                offset.get_or_insert(0);
                1
            } else if c == b'T' && digit_at(s, i + 1) && tm.hour == -1 {
                (tm.min, tm.sec) = (0, 0);
                1
            } else {
                s[i..]
                    .iter()
                    .take_while(|b| b.is_ascii_alphabetic())
                    .count()
            }
        } else if c.is_ascii_digit() {
            match_digit(s, i, &mut tm, &mut offset, now)
        } else if (c == b'-' || c == b'+') && digit_at(s, i + 1) {
            match_tz(s, i, &mut offset)
        } else {
            0
        };
        i += matched.max(1);
    }
    let timestamp = tm_to_time_t(&tm)?;
    let offset = offset.unwrap_or(0);
    if offset > 0 && offset * 60 > timestamp {
        return None;
    }
    if offset < 0 && -offset * 60 > TIMESTAMP_MAX - timestamp {
        return None;
    }
    Some(timestamp - offset * 60)
}

fn pending_number(tm: &mut Tm, num: &mut i64) {
    let number = std::mem::take(num);
    if number == 0 {
        return;
    }
    if tm.mday < 0 && number < 32 {
        tm.mday = number;
    } else if tm.mon < 0 && number < 13 {
        tm.mon = number - 1;
    } else if tm.year < 0 {
        if number > 1969 && number < 2100 {
            tm.year = number - 1900;
        } else if number > 69 && number < 100 {
            tm.year = number;
        } else if number < 38 {
            tm.year = 100 + number;
        }
    }
}

fn approxidate_str(s: &[u8], now: i64) -> i64 {
    let today = Tm::utc(now);
    let mut tm = Tm {
        year: -1,
        mon: -1,
        mday: -1,
        ..today
    };
    let mut num = 0;
    let mut i = 0;
    while i < s.len() {
        if s[i].is_ascii_digit() {
            pending_number(&mut tm, &mut num);
            let (value, end) = number(s, i);
            if matches!(s.get(end), Some(b':' | b'.' | b'/' | b'-')) && digit_at(s, end + 1) {
                let matched = match_multi_number(value, s, i, end, &mut tm, now);
                if matched > 0 {
                    i += matched;
                    continue;
                }
            }
            if s[i] != b'0' || end - i <= 2 {
                num = value;
            }
            i = end;
        } else {
            i += 1;
        }
    }
    pending_number(&mut tm, &mut num);
    if tm.mday < 0 {
        tm.mday = today.mday;
    }
    if tm.mon < 0 {
        tm.mon = today.mon;
    }
    if tm.year < 0 {
        tm.year = today.year - i64::from(tm.mon > today.mon);
    }
    tm.mktime()
}

/// Git's `approxidate_careful(text)` in a UTC process at `now`, for a text of digits and of
/// `-`, `+`, `:`, `T` and `Z`, as `DateTime::xmlschema` writes it.
pub(crate) fn approxidate(text: &str, now: i64) -> i64 {
    let s = text.as_bytes();
    parse_date_basic(s, now).unwrap_or_else(|| approxidate_str(s, now))
}

use crate::ruby_time::{Parts, parts};
use crate::time::{civil_from_days, days_from_civil};

const DAY: i64 = 86_400;
const UNIX_JD: i64 = 2_440_588;
const YEAR_LIMIT: i64 = 1_000_000;
const MAX_ZONE_WORD: usize = 17;

const ZONES: [(&str, i64); 316] = [
    ("a", 3600),
    ("acdt", 37800),
    ("acst", 34200),
    ("act", -18000),
    ("acwst", 31500),
    ("adt", -10800),
    ("aedt", 39600),
    ("aest", 36000),
    ("afghanistan", 16200),
    ("aft", 16200),
    ("ahst", -36000),
    ("akdt", -28800),
    ("akst", -32400),
    ("alaskan", -32400),
    ("almt", 21600),
    ("anast", 43200),
    ("anat", 43200),
    ("aoe", -43200),
    ("aqtt", 18000),
    ("arab", 10800),
    ("arabian", 14400),
    ("arabic", 10800),
    ("art", -10800),
    ("ast", -14400),
    ("at", -7200),
    ("atlantic", -14400),
    ("aus central", 34200),
    ("aus eastern", 36000),
    ("awdt", 32400),
    ("awst", 28800),
    ("azores", -3600),
    ("azost", 0),
    ("azot", -3600),
    ("azst", 18000),
    ("azt", 14400),
    ("b", 7200),
    ("bnt", 28800),
    ("bot", -14400),
    ("brst", -7200),
    ("brt", -10800),
    ("bst", 3600),
    ("bt", 10800),
    ("btt", 21600),
    ("c", 10800),
    ("canada central", -21600),
    ("cape verde", -3600),
    ("cast", 28800),
    ("cat", 7200),
    ("caucasus", 14400),
    ("cct", 23400),
    ("cdt", -18000),
    ("cen. australia", 34200),
    ("central", -21600),
    ("central america", -21600),
    ("central asia", 21600),
    ("central europe", 3600),
    ("central european", 3600),
    ("central pacific", 39600),
    ("cest", 7200),
    ("cet", 3600),
    ("chadt", 49500),
    ("chast", 45900),
    ("china", 28800),
    ("chost", 32400),
    ("chot", 28800),
    ("chst", 36000),
    ("chut", 36000),
    ("cidst", -14400),
    ("cist", -18000),
    ("ckt", -36000),
    ("clst", -10800),
    ("clt", -14400),
    ("cot", -18000),
    ("cst", -21600),
    ("cvt", -3600),
    ("cxt", 25200),
    ("d", 14400),
    ("dateline", -43200),
    ("davt", 25200),
    ("ddut", 36000),
    ("e", 18000),
    ("e. africa", 10800),
    ("e. australia", 36000),
    ("e. europe", 7200),
    ("e. south america", -10800),
    ("eadt", 39600),
    ("easst", -18000),
    ("east", -21600),
    ("eastern", -18000),
    ("eat", 10800),
    ("ect", -18000),
    ("edt", -14400),
    ("eest", 10800),
    ("eet", 7200),
    ("egst", 0),
    ("egt", -3600),
    ("egypt", 7200),
    ("ekaterinburg", 18000),
    ("est", -18000),
    ("f", 21600),
    ("fet", 10800),
    ("fiji", 43200),
    ("fjst", 46800),
    ("fjt", 43200),
    ("fkst", -10800),
    ("fkt", -14400),
    ("fle", 7200),
    ("fnt", -7200),
    ("fst", 7200),
    ("fwt", 3600),
    ("g", 25200),
    ("galt", -21600),
    ("gamt", -32400),
    ("get", 14400),
    ("gft", -10800),
    ("gilt", 43200),
    ("gmt", 0),
    ("greenland", -10800),
    ("greenwich", 0),
    ("gst", 36000),
    ("gtb", 7200),
    ("gyt", -14400),
    ("h", 28800),
    ("hadt", -32400),
    ("hast", -36000),
    ("hawaiian", -36000),
    ("hdt", -32400),
    ("hkt", 28800),
    ("hovst", 28800),
    ("hovt", 25200),
    ("hst", -36000),
    ("i", 32400),
    ("ict", 25200),
    ("idle", 43200),
    ("idlw", -43200),
    ("idt", 10800),
    ("india", 19800),
    ("iot", 21600),
    ("iran", 12600),
    ("irdt", 16200),
    ("irkst", 32400),
    ("irkt", 28800),
    ("irst", 12600),
    ("ist", 19800),
    ("jerusalem", 7200),
    ("jst", 32400),
    ("k", 36000),
    ("kgt", 21600),
    ("korea", 32400),
    ("kost", 39600),
    ("krast", 28800),
    ("krat", 25200),
    ("kst", 32400),
    ("kuyt", 14400),
    ("l", 39600),
    ("lhdt", 39600),
    ("lhst", 37800),
    ("lint", 50400),
    ("m", 43200),
    ("magst", 43200),
    ("magt", 39600),
    ("malay peninsula", 28800),
    ("mart", -30600),
    ("mawt", 18000),
    ("mdt", -21600),
    ("mest", 7200),
    ("mesz", 7200),
    ("met", 3600),
    ("mewt", 3600),
    ("mexico", -21600),
    ("mez", 3600),
    ("mht", 43200),
    ("mid-atlantic", -7200),
    ("mmt", 23400),
    ("mountain", -25200),
    ("msd", 14400),
    ("msk", 10800),
    ("mst", -25200),
    ("mut", 14400),
    ("mvt", 18000),
    ("myanmar", 23400),
    ("myt", 28800),
    ("n", -3600),
    ("n. central asia", 21600),
    ("nct", 39600),
    ("ndt", -5400),
    ("nepal", 20700),
    ("new zealand", 43200),
    ("newfoundland", -12600),
    ("nfdt", 43200),
    ("nft", 39600),
    ("north asia", 25200),
    ("north asia east", 28800),
    ("novst", 25200),
    ("novt", 25200),
    ("npt", 20700),
    ("nrt", 43200),
    ("nst", -9000),
    ("nt", -39600),
    ("nut", -39600),
    ("nzdt", 46800),
    ("nzst", 43200),
    ("nzt", 43200),
    ("o", -7200),
    ("omsst", 25200),
    ("omst", 21600),
    ("orat", 18000),
    ("p", -10800),
    ("pacific", -28800),
    ("pacific sa", -14400),
    ("pdt", -25200),
    ("pet", -18000),
    ("petst", 43200),
    ("pett", 43200),
    ("pgt", 36000),
    ("phot", 46800),
    ("pht", 28800),
    ("pkt", 18000),
    ("pmdt", -7200),
    ("pmst", -10800),
    ("pont", 39600),
    ("pst", -28800),
    ("pwt", 32400),
    ("pyst", -10800),
    ("q", -14400),
    ("qyzt", 21600),
    ("r", -18000),
    ("ret", 14400),
    ("romance", 3600),
    ("rott", -10800),
    ("russian", 10800),
    ("s", -21600),
    ("sa eastern", -10800),
    ("sa pacific", -18000),
    ("sa western", -14400),
    ("sakt", 39600),
    ("samoa", -39600),
    ("samt", 14400),
    ("sast", 7200),
    ("sbt", 39600),
    ("sct", 14400),
    ("se asia", 25200),
    ("sgt", 28800),
    ("south africa", 7200),
    ("sret", 39600),
    ("sri lanka", 21600),
    ("srt", -10800),
    ("sst", -39600),
    ("swt", 3600),
    ("syot", 10800),
    ("t", -25200),
    ("taht", -36000),
    ("taipei", 28800),
    ("tasmania", 36000),
    ("tft", 18000),
    ("tjt", 18000),
    ("tkt", 46800),
    ("tlt", 32400),
    ("tmt", 18000),
    ("tokyo", 32400),
    ("tonga", 46800),
    ("tost", 50400),
    ("tot", 46800),
    ("trt", 10800),
    ("tvt", 43200),
    ("u", -28800),
    ("ulast", 32400),
    ("ulat", 28800),
    ("us eastern", -18000),
    ("us mountain", -25200),
    ("ut", 0),
    ("utc", 0),
    ("uyst", -7200),
    ("uyt", -10800),
    ("uzt", 18000),
    ("v", -32400),
    ("vet", -14400),
    ("vladivostok", 36000),
    ("vlast", 39600),
    ("vlat", 36000),
    ("vost", 21600),
    ("vut", 39600),
    ("w", -36000),
    ("w. australia", 28800),
    ("w. central africa", 3600),
    ("w. europe", 3600),
    ("wadt", 28800),
    ("wakt", 43200),
    ("warst", -10800),
    ("wast", 7200),
    ("wat", 3600),
    ("west", 3600),
    ("west asia", 18000),
    ("west pacific", 36000),
    ("wet", 0),
    ("wft", 43200),
    ("wgst", -3600),
    ("wgt", -7200),
    ("wib", 25200),
    ("wit", 32400),
    ("wita", 28800),
    ("wt", 0),
    ("x", -39600),
    ("y", -43200),
    ("yakst", 36000),
    ("yakt", 32400),
    ("yakutsk", 32400),
    ("yapt", 36000),
    ("ydt", -28800),
    ("yekst", 21600),
    ("yekt", 18000),
    ("yst", -32400),
    ("z", 0),
    ("zp4", 14400),
    ("zp5", 18000),
    ("zp6", 21600),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum F {
    Year,
    Mon,
    Mday,
    Yday,
    Hour,
    Min,
    Sec,
    Wday,
    Cwyear,
    Cweek,
    Cwday,
    Wnum0,
    Wnum1,
    Jd,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Time,
    Ordinal,
    Civil,
    Commercial,
    Wday,
    Wnum0,
    Wnum1,
    Other,
}

const TABLE: [(Kind, &[F]); 11] = [
    (Kind::Time, &[F::Hour, F::Min, F::Sec]),
    (Kind::Other, &[F::Jd]),
    (Kind::Ordinal, &[F::Year, F::Yday, F::Hour, F::Min, F::Sec]),
    (
        Kind::Civil,
        &[F::Year, F::Mon, F::Mday, F::Hour, F::Min, F::Sec],
    ),
    (
        Kind::Commercial,
        &[F::Cwyear, F::Cweek, F::Cwday, F::Hour, F::Min, F::Sec],
    ),
    (Kind::Wday, &[F::Wday, F::Hour, F::Min, F::Sec]),
    (
        Kind::Wnum0,
        &[F::Year, F::Wnum0, F::Wday, F::Hour, F::Min, F::Sec],
    ),
    (
        Kind::Wnum1,
        &[F::Year, F::Wnum1, F::Wday, F::Hour, F::Min, F::Sec],
    ),
    (
        Kind::Other,
        &[F::Cwyear, F::Cweek, F::Wday, F::Hour, F::Min, F::Sec],
    ),
    (
        Kind::Other,
        &[F::Year, F::Wnum0, F::Cwday, F::Hour, F::Min, F::Sec],
    ),
    (
        Kind::Other,
        &[F::Year, F::Wnum1, F::Cwday, F::Hour, F::Min, F::Sec],
    ),
];

#[derive(Default)]
struct Frags([Option<i64>; 14]);

impl Frags {
    fn get(&self, f: F) -> Option<i64> {
        self.0[f as usize]
    }

    fn set(&mut self, f: F, value: i64) {
        self.0[f as usize] = Some(value);
    }

    fn or(&mut self, f: F, value: i64) {
        if self.get(f).is_none() {
            self.set(f, value);
        }
    }
}

struct Day {
    jd: i64,
}

impl Day {
    fn civil(&self) -> (i64, i64, i64) {
        civil_from_days(self.jd - UNIX_JD)
    }

    fn wday(&self) -> i64 {
        (self.jd + 1).rem_euclid(7)
    }

    fn commercial(&self) -> (i64, i64, i64) {
        let cwday = self.jd.rem_euclid(7) + 1;
        let thursday = self.jd - cwday + 4;
        let (cwyear, _, _) = civil_from_days(thursday - UNIX_JD);
        let cweek = (thursday - jd_of(cwyear, 1, 1)).div_euclid(7) + 1;
        (cwyear, cweek, cwday)
    }

    fn field(&self, f: F) -> Option<i64> {
        let (year, mon, mday) = self.civil();
        let (cwyear, cweek, cwday) = self.commercial();
        match f {
            F::Year => Some(year),
            F::Mon => Some(mon),
            F::Mday => Some(mday),
            F::Cwyear => Some(cwyear),
            F::Cweek => Some(cweek),
            F::Cwday => Some(cwday),
            _ => None,
        }
    }
}

fn jd_of(year: i64, mon: i64, mday: i64) -> i64 {
    days_from_civil(year, mon, mday) + UNIX_JD
}

fn days_in_month(year: i64, mon: i64) -> i64 {
    let (next_year, next) = if mon == 12 {
        (year + 1, 1)
    } else {
        (year, mon + 1)
    };
    jd_of(next_year, next, 1) - jd_of(year, mon, 1)
}

fn valid_civil(year: i64, mon: i64, mday: i64) -> Option<i64> {
    let mon = if mon < 0 { mon + 13 } else { mon };
    if !(1..=12).contains(&mon) {
        return None;
    }
    let days = days_in_month(year, mon);
    let mday = if mday < 0 { days + mday + 1 } else { mday };
    (1..=days).contains(&mday).then(|| jd_of(year, mon, mday))
}

fn valid_ordinal(year: i64, yday: i64) -> Option<i64> {
    let days = jd_of(year + 1, 1, 1) - jd_of(year, 1, 1);
    let yday = if yday < 0 { days + yday + 1 } else { yday };
    (1..=days)
        .contains(&yday)
        .then(|| jd_of(year, 1, 1) + yday - 1)
}

fn commercial_to_jd(cwyear: i64, cweek: i64, cwday: i64) -> i64 {
    let jan4 = jd_of(cwyear, 1, 4);
    jan4 - jan4.rem_euclid(7) + 7 * (cweek - 1) + (cwday - 1)
}

fn valid_commercial(cwyear: i64, cweek: i64, cwday: i64) -> Option<i64> {
    let cwday = if cwday < 0 { cwday + 8 } else { cwday };
    let cweek = if cweek < 0 {
        let (y, w, _) = Day {
            jd: commercial_to_jd(cwyear + 1, 1, 1) + cweek * 7,
        }
        .commercial();
        if y != cwyear {
            return None;
        }
        w
    } else {
        cweek
    };
    let jd = commercial_to_jd(cwyear, cweek, cwday);
    (Day { jd }.commercial() == (cwyear, cweek, cwday)).then_some(jd)
}

fn weeknum_to_jd(year: i64, week: i64, wday: i64, first: i64) -> i64 {
    let start = jd_of(year, 1, 1) + 6;
    (start - (start - first + 1).rem_euclid(7) - 7) + 7 * week + wday
}

fn jd_to_weeknum(jd: i64, first: i64) -> (i64, i64, i64) {
    let (year, _, _) = Day { jd }.civil();
    let start = jd_of(year, 1, 1) + 6;
    let j = jd - (start - (start - first + 1).rem_euclid(7)) + 7;
    (year, j.div_euclid(7), j.rem_euclid(7))
}

fn valid_weeknum(year: i64, week: i64, wday: i64, first: i64) -> Option<i64> {
    let wday = if wday < 0 { wday + 7 } else { wday };
    let week = if week < 0 {
        let (y, w, _) = jd_to_weeknum(weeknum_to_jd(year + 1, 1, first, first) + week * 7, first);
        if y != year {
            return None;
        }
        w
    } else {
        week
    };
    let jd = weeknum_to_jd(year, week, wday, first);
    (jd_to_weeknum(jd, first) == (year, week, wday)).then_some(jd)
}

fn complete(frags: &mut Frags, today: &Day) {
    let mut best: Option<(Kind, &[F], usize)> = None;
    for (kind, fields) in TABLE {
        let n = fields.iter().filter(|f| frags.get(**f).is_some()).count();
        if n > best.map_or(0, |(_, _, b)| b) {
            best = Some((kind, fields, n));
        }
    }
    if let Some((kind, fields, n)) = best
        && n < fields.len()
    {
        match kind {
            Kind::Ordinal => {
                frags.or(F::Year, today.civil().0);
                frags.or(F::Yday, 1);
            }
            Kind::Civil | Kind::Commercial => {
                for f in fields {
                    if frags.get(*f).is_some() {
                        break;
                    }
                    if let Some(value) = today.field(*f) {
                        frags.set(*f, value);
                    }
                }
                let (week, day) = if kind == Kind::Civil {
                    (F::Mon, F::Mday)
                } else {
                    (F::Cweek, F::Cwday)
                };
                frags.or(week, 1);
                frags.or(day, 1);
            }
            Kind::Wday => {
                if let Some(wday) = frags.get(F::Wday) {
                    frags.set(F::Jd, today.jd - today.wday() + wday);
                }
            }
            Kind::Wnum0 | Kind::Wnum1 => {
                frags.or(F::Year, today.civil().0);
                frags.or(
                    if kind == Kind::Wnum0 {
                        F::Wnum0
                    } else {
                        F::Wnum1
                    },
                    0,
                );
                frags.or(F::Wday, i64::from(kind == Kind::Wnum1));
            }
            Kind::Time | Kind::Other => {}
        }
    }
    if best.is_some_and(|(kind, _, _)| kind == Kind::Time) {
        frags.or(F::Jd, today.jd);
    }
    frags.or(F::Hour, 0);
    frags.or(F::Min, 0);
    match frags.get(F::Sec) {
        None => frags.set(F::Sec, 0),
        Some(s) if s > 59 => frags.set(F::Sec, 59),
        Some(_) => {}
    }
}

fn valid_date(frags: &Frags) -> Option<i64> {
    let get = |f| frags.get(f);
    if let Some(jd) = get(F::Jd) {
        return Some(jd);
    }
    if let (Some(year), Some(yday)) = (get(F::Year), get(F::Yday))
        && let Some(jd) = valid_ordinal(year, yday)
    {
        return Some(jd);
    }
    if let (Some(year), Some(mon), Some(mday)) = (get(F::Year), get(F::Mon), get(F::Mday))
        && let Some(jd) = valid_civil(year, mon, mday)
    {
        return Some(jd);
    }
    let cwday = get(F::Cwday).or_else(|| get(F::Wday).map(|d| if d == 0 { 7 } else { d }));
    if let (Some(cwyear), Some(cweek), Some(cwday)) = (get(F::Cwyear), get(F::Cweek), cwday)
        && let Some(jd) = valid_commercial(cwyear, cweek, cwday)
    {
        return Some(jd);
    }
    let wday0 = get(F::Wday).or_else(|| get(F::Cwday).map(|d| if d == 7 { 0 } else { d }));
    if let (Some(year), Some(week), Some(wday)) = (get(F::Year), get(F::Wnum0), wday0)
        && let Some(jd) = valid_weeknum(year, week, wday, 0)
    {
        return Some(jd);
    }
    let wday1 = get(F::Wday)
        .or_else(|| get(F::Cwday))
        .map(|d| (d - 1).rem_euclid(7));
    if let (Some(year), Some(week), Some(wday)) = (get(F::Year), get(F::Wnum1), wday1) {
        return valid_weeknum(year, week, wday, 1);
    }
    None
}

fn end_word<'s>(s: &'s str, word: &str) -> Option<&'s str> {
    let cut = s.len().checked_sub(word.len())?;
    let (head, tail) = (s.get(..cut)?, s.get(cut..)?);
    (head.ends_with(|c: char| c.is_ascii_whitespace()) && tail.eq_ignore_ascii_case(word))
        .then(|| head.trim_end_matches(|c: char| c.is_ascii_whitespace()))
}

fn digits(s: &str, most: usize) -> (i64, usize) {
    let n = s.bytes().take(most).take_while(u8::is_ascii_digit).count();
    let value = s[..n].bytes().fold(0_i64, |v, d| {
        v.saturating_mul(10).saturating_add(i64::from(d - b'0'))
    });
    (value, n)
}

fn shrink(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if c.is_ascii_whitespace() {
            space = true;
        } else {
            if space {
                out.push(' ');
            }
            space = false;
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}

/// Ruby's `Date.zone_to_diff`, the offset in seconds that `Date._parse` gives a zone.
fn zone_to_diff(zone: &str) -> Option<i64> {
    let mut s = zone;
    let mut dst = false;
    if let Some(rest) = end_word(s, "time") {
        if let Some(r) = end_word(rest, "standard") {
            s = r;
        } else if let Some(r) = end_word(rest, "daylight") {
            s = r;
            dst = true;
        }
    } else if let Some(r) = end_word(s, "dst") {
        s = r;
        dst = true;
    }
    let name = shrink(s);
    if !name.is_empty()
        && name.len() <= MAX_ZONE_WORD
        && let Ok(i) = ZONES.binary_search_by(|(n, _)| (*n).cmp(name.as_str()))
    {
        return Some(ZONES[i].1 + if dst { 3600 } else { 0 });
    }
    if s.len() > 3
        && s.get(..3)
            .is_some_and(|p| p.eq_ignore_ascii_case("gmt") || p.eq_ignore_ascii_case("utc"))
    {
        s = &s[3..];
    }
    let sign = match s.as_bytes().first() {
        Some(b'+') => 1,
        Some(b'-') => -1,
        _ => return None,
    };
    let s = &s[1..];
    let (hour, n) = digits(s, usize::MAX);
    let rest = &s[n..];
    let seconds = if let Some(rest) = rest.strip_prefix(':') {
        let (min, n) = digits(rest, usize::MAX);
        let mut sec = 0;
        if let Some(after) = rest[n..].strip_prefix(':') {
            sec = digits(after, usize::MAX).0;
            if sec > 59 {
                return None;
            }
        }
        if hour > 23 || min > 59 {
            return None;
        }
        hour * 3600 + min * 60 + sec
    } else if let Some(fraction) = rest.strip_prefix(['.', ',']) {
        if hour > 23 {
            return None;
        }
        let (mut part, n) = digits(fraction, 7);
        let next = fraction.as_bytes().get(n).copied().unwrap_or(0);
        if next.is_ascii_digit() && next >= b'5' + u8::from(part & 1 == 0) {
            part += 1;
        }
        let part = part * 36;
        let part = match n {
            0 | 2 => part,
            1 => part * 10,
            _ => part / 10_i64.pow(u32::try_from(n - 2).unwrap_or(0)),
        };
        hour * 3600 + part
    } else if s.len() > 2 {
        let odd = s.len() % 2;
        let field = |from: usize, width: usize| s.get(from..).map_or(0, |t| digits(t, width).0);
        let mut seconds = field(0, 2 - odd) * 3600;
        if s.len() >= 3 {
            seconds += field(2 - odd, 2) * 60;
        }
        if s.len() >= 5 {
            seconds += field(4 - odd, 2);
        }
        seconds
    } else {
        hour * 3600
    };
    Some(sign * seconds)
}

/// Ruby's `DateTime.parse(text)` in a UTC process, as Unix seconds without the fraction.
/// `None` is the `Date::Error` that Ruby raises.
pub(crate) fn date_time_parse(text: &str, now: i64) -> Option<i64> {
    let p: Parts = parts(text)?;
    let mut frags = Frags::default();
    for (f, value) in [
        (F::Year, p.year),
        (F::Mon, p.mon),
        (F::Mday, p.mday),
        (F::Yday, p.yday),
        (F::Hour, p.hour),
        (F::Min, p.min),
        (F::Sec, p.sec),
        (F::Wday, p.wday),
        (F::Cwyear, p.cwyear),
        (F::Cweek, p.cweek),
        (F::Cwday, p.cwday),
    ] {
        if let Some(v) = value {
            let v = if matches!(f, F::Year | F::Cwyear) {
                v.clamp(-YEAR_LIMIT, YEAR_LIMIT)
            } else {
                v
            };
            frags.set(f, v);
        }
    }
    let jd = if let (None, Some(year), Some(mon), Some(mday)) =
        (p.yday, frags.get(F::Year), p.mon, p.mday)
    {
        frags.or(F::Hour, 0);
        frags.or(F::Min, 0);
        match frags.get(F::Sec) {
            None => frags.set(F::Sec, 0),
            Some(60) => frags.set(F::Sec, 59),
            Some(_) => {}
        }
        valid_civil(year, mon, mday)?
    } else {
        let today = Day {
            jd: now.div_euclid(DAY) + UNIX_JD,
        };
        complete(&mut frags, &today);
        valid_date(&frags)?
    };
    let (hour, min, sec) = (frags.get(F::Hour)?, frags.get(F::Min)?, frags.get(F::Sec)?);
    let hour = if hour < 0 { hour + 24 } else { hour };
    let min = if min < 0 { min + 60 } else { min };
    let sec = if sec < 0 { sec + 60 } else { sec };
    if !(0..=24).contains(&hour)
        || !(0..=59).contains(&min)
        || !(0..=59).contains(&sec)
        || (hour == 24 && (min > 0 || sec > 0))
    {
        return None;
    }
    let offset = p
        .offset_text
        .as_deref()
        .or(p.zone.as_deref())
        .and_then(zone_to_diff)
        .filter(|o| o.abs() <= DAY)
        .unwrap_or(0);
    Some((jd - UNIX_JD) * DAY + hour * 3600 + min * 60 + sec - offset)
}

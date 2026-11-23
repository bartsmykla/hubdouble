use std::sync::LazyLock;

use fancy_regex::Regex;

use crate::time::{civil_from_days, days_from_civil};

const MAX_CHARS: usize = 128;
const FIELD_LIMIT: i64 = 1_000_000_000_000_000;
const YEAR_LIMIT: i64 = 1_000_000;
const STANDARD: i64 = -8 * 3600;
const DAYLIGHT: i64 = -7 * 3600;

const ALPHA: u8 = 1;
const DIGIT: u8 = 2;
const DASH: u8 = 4;
const DOT: u8 = 8;
const SLASH: u8 = 16;

macro_rules! months {
    () => {
        "jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec"
    };
}

macro_rules! era {
    () => {
        r"c(?:e|\.e\.)|b(?:ce|\.c\.e\.)|a(?:d|\.d\.)|b(?:c|\.c\.)"
    };
}

struct Patterns {
    space: Regex,
    day: Regex,
    time: Regex,
    clock: Regex,
    eu: Regex,
    us: Regex,
    iso: Regex,
    iso21: Regex,
    iso22: Regex,
    iso23: Regex,
    iso24: Regex,
    iso25_guard: Regex,
    iso25: Regex,
    iso26_guard: Regex,
    iso26: Regex,
    jis: Regex,
    vms11: Regex,
    vms12: Regex,
    sla: Regex,
    dot: Regex,
    year: Regex,
    mon: Regex,
    mday: Regex,
    ddd: Regex,
    bc: Regex,
    frag: Regex,
}

impl Patterns {
    fn compile() -> Option<Patterns> {
        let re = |pattern: &str| Regex::new(pattern).ok();
        Some(Patterns {
            space: re(r"[^-+',./:@\p{L}\p{M}\p{Nd}\[\]]+")?,
            day: re(r"(?i)\b(sun|mon|tue|wed|thu|fri|sat)[^-/0-9\s]*")?,
            time: re(concat!(
                r"(?i)((?<![0-9])[0-9]+\s*",
                r"(?:(?::\s*[0-9]+(?:\s*:\s*[0-9]+(?:[,.][0-9]*)?)?|h(?:\s*[0-9]+m?(?:\s*[0-9]+s?)?)?)",
                r"(?:\s*[ap](?:m\b|\.m\.))?|[ap](?:m\b|\.m\.)))",
                r"(?:\s*((?:gmt|utc?)?[-+][0-9]+(?:[,.:][0-9]+(?::[0-9]+)?)?",
                r"|(?-i:[\p{Alphabetic}.\s]+)(?:standard|daylight)\stime\b",
                r"|(?-i:\p{Alphabetic}+)(?:\sdst)?\b))?",
            ))?,
            clock: re(concat!(
                r"(?i)\A([0-9]+)h?(?:\s*:?\s*([0-9]+)m?",
                r"(?:\s*:?\s*([0-9]+)(?:[,.]([0-9]+))?s?)?)?",
                r"(?:\s*([ap])(?:m\b|\.m\.))?",
            ))?,
            eu: re(concat!(
                r"(?i)('?(?<![0-9])[0-9]+)[^-0-9\s]*\s*(",
                months!(),
                r")[^-0-9\s']*(?:\s*(?:\b(",
                era!(),
                r")(?!(?<!\.)[a-z]))?\s*('?-?[0-9]+(?:(?:st|nd|rd|th)\b)?))?",
            ))?,
            us: re(concat!(
                r"(?i)\b(",
                months!(),
                r")[^-0-9\s']*\s*('?[0-9]+)[^-0-9\s']*(?:(?>\s*),?(?>\s*)(",
                era!(),
                r")?\s*('?-?[0-9]+))?",
            ))?,
            iso: re(r"('?[-+]?(?<![0-9])[0-9]+)-([0-9]+)-('?-?[0-9]+)")?,
            iso21: re(r"(?i)\b([0-9]{2}|[0-9]{4})?-?w([0-9]{2})(?:-?([0-9]))?\b")?,
            iso22: re(r"(?i)-w-([0-9])\b")?,
            iso23: re(r"--([0-9]{2})?-([0-9]{2})\b")?,
            iso24: re(r"--([0-9]{2})([0-9]{2})?\b")?,
            iso25_guard: re(r"[,.]([0-9]{2}|[0-9]{4})-[0-9]{3}\b")?,
            iso25: re(r"\b([0-9]{2}|[0-9]{4})-([0-9]{3})\b")?,
            iso26_guard: re(r"[0-9]-[0-9]{3}\b")?,
            iso26: re(r"\b-([0-9]{3})\b")?,
            jis: re(r"(?i)\b([mtshr])([0-9]+)\.([0-9]+)\.([0-9]+)")?,
            vms11: re(concat!(
                r"(?i)('?-?(?<![0-9])[0-9]+)-(",
                months!(),
                r")[^-/.]*-('?-?[0-9]+)",
            ))?,
            vms12: re(concat!(
                r"(?i)\b(",
                months!(),
                r")[^-/.]*-('?-?[0-9]+)(?:-('?-?[0-9]+))?",
            ))?,
            sla: re(r"(?i)('?-?(?<![0-9])[0-9]+)/\s*('?[0-9]+)(?:[^0-9]\s*('?-?[0-9]+))?")?,
            dot: re(r"(?i)('?-?(?<![0-9])[0-9]+)\.\s*('?[0-9]+)\.\s*('?-?[0-9]+)")?,
            year: re(r"'([0-9]+)\b")?,
            mon: re(concat!(r"(?i)\b(", months!(), r")\S*"))?,
            mday: re(r"(?i)((?<![0-9])[0-9]+)(st|nd|rd|th)\b")?,
            ddd: re(concat!(
                r"(?i)([-+]?)((?<![0-9])[0-9]{2,14})",
                r"(?:\s*t?\s*([0-9]{2,6})?(?:[,.]([0-9]*))?)?",
                r"(?:\s*(z\b|[-+][0-9]{1,4}\b|\[[-+]?[0-9][^\]]*\]))?",
            ))?,
            bc: re(r"(?i)\b(bc\b|bce\b|b\.c\.|b\.c\.e\.)")?,
            frag: re(r"(?i)\A\s*([0-9]{1,2})\s*\z")?,
        })
    }
}

static PATTERNS: LazyLock<Option<Patterns>> = LazyLock::new(Patterns::compile);

#[derive(Default)]
struct Parts {
    year: Option<i64>,
    yday: Option<i64>,
    mon: Option<i64>,
    mday: Option<i64>,
    hour: Option<i64>,
    min: Option<i64>,
    sec: Option<i64>,
    fraction: bool,
    zone: Option<String>,
    bc: bool,
    no_comp: bool,
}

type Groups = Vec<Option<String>>;

fn subx(text: &mut String, pattern: &Regex) -> Option<Groups> {
    let caps = pattern.captures(text).ok().flatten()?;
    let groups: Groups = caps
        .iter()
        .map(|m| m.map(|m| m.as_str().to_owned()))
        .collect();
    let range = caps.get(0)?.range();
    text.replace_range(range, " ");
    Some(groups)
}

fn group(groups: &Groups, index: usize) -> Option<&str> {
    groups.get(index).and_then(Option::as_deref)
}

fn owned(groups: &Groups, index: usize) -> Option<String> {
    group(groups, index).map(str::to_owned)
}

fn num(text: &str) -> i64 {
    let text = text.trim_start();
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let value = digits
        .bytes()
        .take_while(u8::is_ascii_digit)
        .fold(0, |v: i64, b| {
            (v * 10 + i64::from(b - b'0')).min(FIELD_LIMIT)
        });
    if negative { -value } else { value }
}

fn n2i(text: &str, from: usize, width: usize) -> i64 {
    text.get(from..from + width).map_or(0, num)
}

fn class(text: &str) -> u8 {
    text.bytes().fold(0, |flags, b| {
        flags
            | if b.is_ascii_alphabetic() { ALPHA } else { 0 }
            | if b.is_ascii_digit() { DIGIT } else { 0 }
            | match b {
                b'-' => DASH,
                b'.' => DOT,
                b'/' => SLASH,
                _ => 0,
            }
    })
}

fn has(text: &str, flags: u8) -> bool {
    class(text) & flags == flags
}

fn mon_num(name: &str) -> String {
    let abbr = name.get(..3).unwrap_or(name);
    let index = months!()
        .split('|')
        .position(|m| m.eq_ignore_ascii_case(abbr))
        .unwrap_or(12);
    (index + 1).to_string()
}

fn sign_or_digit(c: char) -> bool {
    c == '-' || c == '+' || c.is_ascii_digit()
}

fn digit_span(text: &str) -> usize {
    text.bytes().take_while(u8::is_ascii_digit).count()
}

fn leading_number(text: &str) -> Option<i64> {
    let start = text.find(|c: char| c.is_ascii_digit())?;
    Some(num(&text[start..]))
}

fn s3e(
    parts: &mut Parts,
    mut y: Option<String>,
    mut m: Option<String>,
    mut d: Option<String>,
    bc: bool,
) {
    let mut no_comp = false;
    if y.is_some() && m.is_some() && d.is_none() {
        (y, m, d) = (None, y, m);
    }
    if y.is_none() {
        if d.as_ref().is_some_and(|d| d.len() > 2) {
            y = d.take();
        }
        if d.as_ref().is_some_and(|d| d.starts_with('\'')) {
            y = d.take();
        }
    }
    if let Some(text) = &y
        && let Some(start) = text.find(sign_or_digit)
    {
        let digits = start + usize::from(!text[start..].starts_with(|c: char| c.is_ascii_digit()));
        let end = digits + digit_span(&text[digits..]);
        if end < text.len() {
            let number = text[start..end].to_owned();
            y = d.take();
            d = Some(number);
        }
    }
    if m.as_ref()
        .is_some_and(|m| m.starts_with('\'') || m.len() > 2)
    {
        (y, m, d) = (m, d, y);
    }
    if d.as_ref()
        .is_some_and(|d| d.starts_with('\'') || d.len() > 2)
    {
        std::mem::swap(&mut y, &mut d);
    }
    if let Some(text) = &y
        && let Some(start) = text.find(sign_or_digit)
    {
        let signed = !text[start..].starts_with(|c: char| c.is_ascii_digit());
        let digits = start + usize::from(signed);
        let len = digit_span(&text[digits..]);
        no_comp = signed || len > 2;
        parts.year = Some(num(&text[start..digits + len]));
    }
    if bc {
        parts.bc = true;
    }
    if let Some(mon) = m.as_deref().and_then(leading_number) {
        parts.mon = Some(mon);
    }
    if let Some(mday) = d.as_deref().and_then(leading_number) {
        parts.mday = Some(mday);
    }
    if no_comp {
        parts.no_comp = true;
    }
}

fn is_bc(era: Option<&str>) -> bool {
    era.is_some_and(|e| e.starts_with(['b', 'B']))
}

fn time(text: &mut String, p: &Patterns, parts: &mut Parts) {
    let Some(g) = subx(text, &p.time) else {
        return;
    };
    if let Some(zone) = owned(&g, 2) {
        parts.zone = Some(zone);
    }
    let Some(Ok(Some(c))) = group(&g, 1).map(|clock| p.clock.captures(clock)) else {
        return;
    };
    let value = |i: usize| c.get(i).map(|m| num(m.as_str()));
    let hour = value(1).unwrap_or(0);
    parts.hour = Some(match c.get(5) {
        Some(merid) if merid.as_str().eq_ignore_ascii_case("p") => hour % 12 + 12,
        Some(_) => hour % 12,
        None => hour,
    });
    if let Some(min) = value(2) {
        parts.min = Some(min);
    }
    if let Some(sec) = value(3) {
        parts.sec = Some(sec);
    }
    if c.get(4).is_some() {
        parts.fraction = true;
    }
}

fn eu(text: &mut String, p: &Patterns, parts: &mut Parts) -> bool {
    subx(text, &p.eu).is_some_and(|g| {
        let mon = group(&g, 2).map(mon_num);
        s3e(parts, owned(&g, 4), mon, owned(&g, 1), is_bc(group(&g, 3)));
        true
    })
}

fn us(text: &mut String, p: &Patterns, parts: &mut Parts) -> bool {
    subx(text, &p.us).is_some_and(|g| {
        let mon = group(&g, 1).map(mon_num);
        s3e(parts, owned(&g, 4), mon, owned(&g, 2), is_bc(group(&g, 3)));
        true
    })
}

fn ymd(text: &mut String, pattern: &Regex, parts: &mut Parts) -> bool {
    subx(text, pattern).is_some_and(|g| {
        s3e(parts, owned(&g, 1), owned(&g, 2), owned(&g, 3), false);
        true
    })
}

fn iso2(text: &mut String, p: &Patterns, parts: &mut Parts) -> bool {
    if subx(text, &p.iso21).is_some() || subx(text, &p.iso22).is_some() {
        return true;
    }
    if let Some(g) = subx(text, &p.iso23) {
        parts.mon = group(&g, 1).map(num).or(parts.mon);
        parts.mday = group(&g, 2).map(num);
        return true;
    }
    if let Some(g) = subx(text, &p.iso24) {
        parts.mon = group(&g, 1).map(num);
        parts.mday = group(&g, 2).map(num).or(parts.mday);
        return true;
    }
    if !p.iso25_guard.is_match(text).unwrap_or(true)
        && let Some(g) = subx(text, &p.iso25)
    {
        parts.year = group(&g, 1).map(num);
        parts.yday = group(&g, 2).map(num);
        return true;
    }
    if !p.iso26_guard.is_match(text).unwrap_or(true)
        && let Some(g) = subx(text, &p.iso26)
    {
        parts.yday = group(&g, 1).map(num);
        return true;
    }
    false
}

fn jis(text: &mut String, p: &Patterns, parts: &mut Parts) -> bool {
    subx(text, &p.jis).is_some_and(|g| {
        let era = match group(&g, 1).and_then(|e| e.chars().next()) {
            Some('M' | 'm') => 1867,
            Some('T' | 't') => 1911,
            Some('S' | 's') => 1925,
            Some('H' | 'h') => 1988,
            Some('R' | 'r') => 2018,
            _ => 0,
        };
        parts.year = group(&g, 2).map(|y| num(y) + era);
        parts.mon = group(&g, 3).map(num);
        parts.mday = group(&g, 4).map(num);
        true
    })
}

fn vms(text: &mut String, p: &Patterns, parts: &mut Parts) -> bool {
    if let Some(g) = subx(text, &p.vms11) {
        let mon = group(&g, 2).map(mon_num);
        s3e(parts, owned(&g, 3), mon, owned(&g, 1), false);
        return true;
    }
    subx(text, &p.vms12).is_some_and(|g| {
        let mon = group(&g, 1).map(mon_num);
        s3e(parts, owned(&g, 3), mon, owned(&g, 2), false);
        true
    })
}

fn single(text: &mut String, pattern: &Regex) -> Option<String> {
    subx(text, pattern).and_then(|g| owned(&g, 1))
}

fn ddd(text: &mut String, p: &Patterns, parts: &mut Parts) -> bool {
    let Some(g) = subx(text, &p.ddd) else {
        return false;
    };
    let (s3, s4) = (group(&g, 3), group(&g, 4));
    ddd_digits(
        parts,
        group(&g, 2).unwrap_or_default(),
        group(&g, 1) == Some("-"),
        s3.is_none() && s4.is_some(),
    );
    if let Some(s3) = s3
        && matches!(s3.len(), 2 | 4 | 6)
    {
        let l3 = s3.len();
        let n = |from: usize| Some(n2i(s3, from, 2));
        if s4.is_some() {
            parts.sec = n(l3 - 2);
            if l3 >= 4 {
                parts.min = n(l3 - 4);
            }
            if l3 >= 6 {
                parts.hour = n(l3 - 6);
            }
        } else {
            parts.hour = n(0);
            if l3 >= 4 {
                parts.min = n(2);
            }
            if l3 >= 6 {
                parts.sec = n(4);
            }
        }
    }
    if s4.is_some() {
        parts.fraction = true;
    }
    if let Some(zone) = group(&g, 5) {
        parts.zone = Some(match zone.strip_prefix('[') {
            Some(inner) => {
                let inner = inner.strip_suffix(']').unwrap_or(inner);
                inner
                    .split_once(':')
                    .map_or(inner, |(_, name)| name)
                    .to_owned()
            }
            None => zone.to_owned(),
        });
    }
    true
}

fn ddd_digits(parts: &mut Parts, s2: &str, negative: bool, time_only: bool) {
    let l2 = s2.len();
    let n = |from: usize, width: usize| n2i(s2, from, width);
    let signed = |y: i64| if negative { -y } else { y };
    match l2 {
        2 if time_only => parts.sec = Some(n(0, 2)),
        2 => parts.mday = Some(n(0, 2)),
        4 if time_only => {
            parts.sec = Some(n(2, 2));
            parts.min = Some(n(0, 2));
        }
        4 => {
            parts.mon = Some(n(0, 2));
            parts.mday = Some(n(2, 2));
        }
        6 if time_only => {
            parts.sec = Some(n(4, 2));
            parts.min = Some(n(2, 2));
            parts.hour = Some(n(0, 2));
        }
        6 => {
            parts.year = Some(signed(n(0, 2)));
            parts.mon = Some(n(2, 2));
            parts.mday = Some(n(4, 2));
        }
        8 | 10 | 12 | 14 if time_only => {
            parts.sec = Some(n(l2 - 2, 2));
            parts.min = Some(n(l2 - 4, 2));
            parts.hour = Some(n(l2 - 6, 2));
            parts.mday = Some(n(l2 - 8, 2));
            if l2 >= 10 {
                parts.mon = Some(n(l2 - 10, 2));
            }
            if l2 == 12 {
                parts.year = Some(signed(n(0, 2)));
            }
            if l2 == 14 {
                parts.year = Some(signed(n(0, 4)));
                parts.no_comp = true;
            }
        }
        8 | 10 | 12 | 14 => {
            parts.year = Some(signed(n(0, 4)));
            parts.mon = Some(n(4, 2));
            parts.mday = Some(n(6, 2));
            if l2 >= 10 {
                parts.hour = Some(n(8, 2));
            }
            if l2 >= 12 {
                parts.min = Some(n(10, 2));
            }
            if l2 >= 14 {
                parts.sec = Some(n(12, 2));
            }
            parts.no_comp = true;
        }
        3 if time_only => {
            parts.sec = Some(n(1, 2));
            parts.min = Some(n(0, 1));
        }
        3 => parts.yday = Some(n(0, 3)),
        5 if time_only => {
            parts.sec = Some(n(3, 2));
            parts.min = Some(n(1, 2));
            parts.hour = Some(n(0, 1));
        }
        5 => {
            parts.year = Some(signed(n(0, 2)));
            parts.yday = Some(n(2, 3));
        }
        7 if time_only => {
            parts.sec = Some(n(5, 2));
            parts.min = Some(n(3, 2));
            parts.hour = Some(n(1, 2));
            parts.mday = Some(n(0, 1));
        }
        7 => {
            parts.year = Some(signed(n(0, 4)));
            parts.yday = Some(n(4, 3));
        }
        _ => {}
    }
}

fn date(text: &mut String, p: &Patterns, parts: &mut Parts) -> bool {
    if has(text, ALPHA | DIGIT) && (eu(text, p, parts) || us(text, p, parts)) {
        return true;
    }
    if has(text, DIGIT | DASH) && ymd(text, &p.iso, parts) {
        return true;
    }
    if has(text, DIGIT | DOT) && jis(text, p, parts) {
        return true;
    }
    if has(text, ALPHA | DIGIT | DASH) && vms(text, p, parts) {
        return true;
    }
    if has(text, DIGIT | SLASH) && ymd(text, &p.sla, parts) {
        return true;
    }
    if has(text, DIGIT | DOT) && ymd(text, &p.dot, parts) {
        return true;
    }
    if has(text, DIGIT) && iso2(text, p, parts) {
        return true;
    }
    if has(text, DIGIT)
        && let Some(year) = single(text, &p.year)
    {
        parts.year = Some(num(&year));
        return true;
    }
    if has(text, ALPHA)
        && let Some(mon) = single(text, &p.mon)
    {
        parts.mon = Some(num(&mon_num(&mon)));
        return true;
    }
    if has(text, DIGIT)
        && let Some(mday) = single(text, &p.mday)
    {
        parts.mday = Some(num(&mday));
        return true;
    }
    has(text, DIGIT) && ddd(text, p, parts)
}

fn date_parse(text: &str, p: &Patterns) -> Option<Parts> {
    let mut text = p.space.try_replacen(text, 0, " ").ok()?.into_owned();
    let mut parts = Parts::default();
    if has(&text, ALPHA) {
        subx(&mut text, &p.day);
    }
    if has(&text, DIGIT) {
        time(&mut text, p, &mut parts);
    }
    date(&mut text, p, &mut parts);
    if has(&text, ALPHA) && subx(&mut text, &p.bc).is_some() {
        parts.bc = true;
    }
    if has(&text, DIGIT)
        && let Some(n) = single(&mut text, &p.frag).map(|n| num(&n))
    {
        if parts.hour.is_some() && parts.mday.is_none() && (1..=31).contains(&n) {
            parts.mday = Some(n);
        }
        if parts.mday.is_some() && parts.hour.is_none() && (0..=24).contains(&n) {
            parts.hour = Some(n);
        }
    }
    if parts.bc {
        parts.year = parts.year.map(|y| 1 - y);
    }
    if !parts.no_comp
        && let Some(y @ 0..=99) = parts.year
    {
        parts.year = Some(if y >= 69 { y + 1900 } else { y + 2000 });
    }
    Some(parts)
}

fn zone_offset(zone: &str) -> Option<i64> {
    let zone = zone.to_uppercase();
    let b = zone.as_bytes();
    if let Some(&sign @ (b'+' | b'-')) = b.first() {
        let sign = if sign == b'-' { -1 } else { 1 };
        let two = |i: usize| {
            b.get(i..i + 2)
                .filter(|d| d.iter().all(u8::is_ascii_digit))
                .map(|d| i64::from(d[0] - b'0') * 10 + i64::from(d[1] - b'0'))
        };
        let seconds = match b.len() {
            3 => two(1)? * 3600,
            5 => two(1)? * 3600 + two(3)? * 60,
            6 if b[3] == b':' => two(1)? * 3600 + two(4)? * 60,
            7 => two(1)? * 3600 + two(3)? * 60 + two(5)?,
            9 if b[3] == b':' && b[6] == b':' => two(1)? * 3600 + two(4)? * 60 + two(7)?,
            _ => return None,
        };
        return Some(sign * seconds);
    }
    let hours = match zone.as_str() {
        "UTC" | "Z" | "UT" | "GMT" => 0,
        "EST" | "CDT" => -5,
        "EDT" => -4,
        "CST" | "MDT" => -6,
        "MST" | "PDT" => -7,
        "PST" => -8,
        _ => match b {
            [letter @ b'A'..=b'I'] => i64::from(letter - b'A') + 1,
            [letter @ b'K'..=b'M'] => i64::from(letter - b'K') + 10,
            [letter @ b'N'..=b'Y'] => -(i64::from(letter - b'N') + 1),
            _ => return None,
        },
    };
    Some(hours * 3600)
}

fn sunday(year: i64, month: i64, nth: i64) -> i64 {
    let first = days_from_civil(year, month, 1);
    first + (3 - first).rem_euclid(7) + 7 * (nth - 1)
}

fn last_sunday(year: i64, month: i64) -> i64 {
    let last = days_from_civil(year, month + 1, 1) - 1;
    last - (last - 3).rem_euclid(7)
}

fn daylight_days(year: i64) -> Option<(i64, i64)> {
    let start = match year {
        2007.. => sunday(year, 3, 2),
        1987..=2006 => sunday(year, 4, 1),
        1976..=1986 | 1967..=1973 => last_sunday(year, 4),
        1975 => days_from_civil(1975, 2, 23),
        1974 => days_from_civil(1974, 1, 6),
        _ => return None,
    };
    let end = if year >= 2007 {
        sunday(year, 11, 1)
    } else {
        last_sunday(year, 10)
    };
    Some((start, end))
}

fn year_of(seconds: i64) -> i64 {
    civil_from_days(seconds.div_euclid(86_400)).0
}

fn local_offset(utc: i64) -> i64 {
    let daylight = daylight_days(year_of(utc + STANDARD)).is_some_and(|(start, end)| {
        (start * 86_400 + 2 * 3600 - STANDARD..end * 86_400 + 2 * 3600 - DAYLIGHT).contains(&utc)
    });
    if daylight { DAYLIGHT } else { STANDARD }
}

fn local_to_utc(wall: i64) -> i64 {
    let daylight = daylight_days(year_of(wall)).is_some_and(|(start, end)| {
        (start * 86_400 + 3 * 3600..end * 86_400 + 3600).contains(&wall)
    });
    wall - if daylight { DAYLIGHT } else { STANDARD }
}

fn month_days(year: i64, mon: i64) -> Option<i64> {
    let index = if mon < 1 { mon + 11 } else { mon - 1 };
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    usize::try_from(index)
        .ok()
        .and_then(|i| days.get(i).copied())
}

type Fields = (i64, i64, i64, i64, i64, i64);

fn carry(value: &mut i64, off: &mut i64, step: i64, unit: i64) {
    let rest = *off % unit;
    *off /= unit;
    if rest != 0 {
        *value += step * rest;
        *off += step * value.div_euclid(unit);
        *value = value.rem_euclid(unit);
    }
}

fn apply_offset(fields: Fields, offset: i64) -> Fields {
    let (mut year, mut mon, mut day, mut hour, mut min, mut sec) = fields;
    let forward = offset < 0;
    let step = if forward { 1 } else { -1 };
    let mut off = offset.abs();
    carry(&mut sec, &mut off, step, 60);
    carry(&mut min, &mut off, step, 60);
    carry(&mut hour, &mut off, step, 24);
    if off != 0 && forward {
        day += off;
        if month_days(year, mon).is_some_and(|days| days < day) {
            mon += 1;
            if mon > 12 {
                mon = 1;
                year += 1;
            }
            day = 1;
        }
    } else if off != 0 {
        day -= off;
        if day < 1 {
            mon -= 1;
            if mon < 1 {
                year -= 1;
                mon = 12;
            }
            day = month_days(year, mon).unwrap_or(1);
        }
    }
    (year, mon, day, hour, min, sec)
}

fn timegm((year, mon, day, hour, min, sec): Fields) -> Option<i64> {
    let valid = (1..=12).contains(&mon)
        && (1..=31).contains(&day)
        && (0..=24).contains(&hour)
        && (0..=59).contains(&min)
        && (0..=60).contains(&sec)
        && (hour < 24 || (min == 0 && sec == 0));
    let year = year.clamp(-YEAR_LIMIT, YEAR_LIMIT);
    valid.then(|| (days_from_civil(year, mon, 1) + day - 1) * 86_400 + hour * 3600 + min * 60 + sec)
}

fn now_fields(seconds: i64) -> [i64; 6] {
    let (year, mon, day) = civil_from_days(seconds.div_euclid(86_400));
    let clock = seconds.rem_euclid(86_400);
    [year, mon, day, clock / 3600, clock / 60 % 60, clock % 60]
}

/// Ruby's `Time.parse(text, now)` in the `America/Los_Angeles` zone, as Unix seconds.
/// `None` is the `ArgumentError` that Ruby raises.
pub(crate) fn time_parse(text: &str, now: i64) -> Option<i64> {
    if text.chars().count() > MAX_CHARS {
        return None;
    }
    let p = PATTERNS.as_ref()?;
    let parts = date_parse(text, p)?;
    let wanted = Wanted {
        year: parts.year,
        yday: parts.yday,
        mon: parts.mon,
        mday: parts.mday,
        hour: parts.hour,
        min: parts.min,
        sec: parts.sec,
    };
    let given = [
        wanted.year,
        wanted.yday,
        wanted.mon,
        wanted.mday,
        wanted.hour,
        wanted.min,
        wanted.sec,
    ];
    if given.iter().all(Option::is_none) && !parts.fraction {
        return None;
    }
    let offset = parts.zone.as_deref().and_then(zone_offset);
    if offset.is_some_and(|o| o.abs() >= 86_400) {
        return None;
    }
    make_time(wanted, offset, now)
}

#[derive(Clone, Copy)]
struct Wanted {
    year: Option<i64>,
    yday: Option<i64>,
    mon: Option<i64>,
    mday: Option<i64>,
    hour: Option<i64>,
    min: Option<i64>,
    sec: Option<i64>,
}

fn wall(utc: i64, offset: Option<i64>) -> i64 {
    utc + offset.unwrap_or_else(|| local_offset(utc))
}

fn make_time(wanted: Wanted, offset: Option<i64>, now: i64) -> Option<i64> {
    if let Some(yday) = wanted.yday {
        if !(1..=366).contains(&yday) {
            return None;
        }
        let off_year = wanted.year.unwrap_or_else(|| year_of(wall(now, None)));
        let at = |mon: i64, day: i64| {
            let on_day = Wanted {
                yday: None,
                mon: Some(mon),
                mday: Some(day),
                ..wanted
            };
            make_time(on_day, offset, now)
        };
        let (mut mon, mut day) = ((yday - 1) / 31 + 1, (yday - 1) % 31 + 1);
        let t = at(mon, day)?;
        let local = wall(t, offset);
        let t_yday = local.div_euclid(86_400) - days_from_civil(year_of(local), 1, 1) + 1;
        if yday == t_yday {
            return Some(t);
        }
        day += yday - t_yday;
        if let Some(days) = month_days(off_year, mon)
            && day > 28
            && day > days
        {
            mon += 1;
            if mon > 12 {
                return None;
            }
            day -= days;
        }
        return at(mon, day);
    }
    let mut fields = [
        wanted.year,
        wanted.mon,
        wanted.mday,
        wanted.hour,
        wanted.min,
        wanted.sec,
    ];
    for (field, current) in fields.iter_mut().zip(now_fields(wall(now, offset))) {
        if field.is_some() {
            break;
        }
        *field = Some(current);
    }
    let [year, mon, day, hour, min, sec] = fields;
    let fields = (
        year.unwrap_or(1970),
        mon.unwrap_or(1),
        day.unwrap_or(1),
        hour.unwrap_or(0),
        min.unwrap_or(0),
        sec.unwrap_or(0),
    );
    match offset {
        Some(offset) => timegm(apply_offset(fields, offset)),
        None => timegm(fields).map(local_to_utc),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pattern_compiles() {
        assert!(Patterns::compile().is_some());
    }
}

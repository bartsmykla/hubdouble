use std::borrow::Cow;

use percent_encoding::percent_decode_str;

/// Decoded path parameters, in path order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Params(Vec<(Cow<'static, str>, String)>);

impl Params {
    #[must_use]
    pub fn new(pairs: Vec<(Cow<'static, str>, String)>) -> Params {
        Params(pairs)
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(n, v)| (n.as_ref(), v.as_str()))
    }
}

/// Ruby's `Integer(text)`, which GitHub applies to `page` and `per_page`: ASCII white space
/// around the number, one sign, a `0x`, `0b`, `0o` or `0d` prefix or a leading `0` for octal,
/// and single underscores between digits. `None` is the `ArgumentError` that Ruby raises, and a
/// value past `i128` saturates.
#[must_use]
pub fn ruby_integer(text: &str) -> Option<i128> {
    let text = text.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\u{b}' | '\u{c}' | '\r'));
    let (negative, unsigned) = match text.as_bytes().first()? {
        b'-' => (true, &text[1..]),
        b'+' => (false, &text[1..]),
        _ => (false, text),
    };
    let (radix, digits) = match unsigned.as_bytes() {
        [b'0', b'x' | b'X', ..] => (16, &unsigned[2..]),
        [b'0', b'b' | b'B', ..] => (2, &unsigned[2..]),
        [b'0', b'o' | b'O', ..] => (8, &unsigned[2..]),
        [b'0', b'd' | b'D', ..] => (10, &unsigned[2..]),
        [b'0'] => return Some(0),
        [b'0', ..] => (8, unsigned[1..].strip_prefix('_').unwrap_or(&unsigned[1..])),
        _ => (10, unsigned),
    };
    let mut value: i128 = 0;
    let mut after_digit = false;
    for c in digits.chars() {
        if c == '_' {
            if !after_digit {
                return None;
            }
            after_digit = false;
            continue;
        }
        let digit = c.to_digit(radix)?;
        value = value
            .saturating_mul(radix.into())
            .saturating_add(digit.into());
        after_digit = true;
    }
    after_digit.then_some(if negative { -value } else { value })
}

/// Decoded query parameters, in request order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query(Vec<(String, String)>);

impl Query {
    /// Parses `a=1&b=x+y`; `+` is a space, and a pair that is not UTF-8 after decoding is dropped.
    #[must_use]
    pub fn parse(query: &str) -> Query {
        let decode = |s: &str| {
            let spaced = s.replace('+', " ");
            percent_decode_str(&spaced)
                .decode_utf8()
                .ok()
                .map(Cow::into_owned)
        };
        Query(
            query
                .split('&')
                .filter(|p| !p.is_empty())
                .filter_map(|pair| {
                    let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                    Some((decode(k)?, decode(v)?))
                })
                .collect(),
        )
    }

    /// The last value of `name`, as GitHub reads a repeated parameter.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .rfind(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// Every value of `name`, for repeated parameters such as `labels[]`.
    pub fn all<'q>(&'q self, name: &'q str) -> impl Iterator<Item = &'q str> {
        self.0
            .iter()
            .filter(move |(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }

    /// Each name once, at its first place, with its last value, as GitHub writes a query back
    /// into a `Link` header.
    pub fn distinct(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().enumerate().filter_map(|(i, (n, _))| {
            let first = !self.0[..i].iter().any(|(m, _)| m == n);
            first.then(|| (n.as_str(), self.get(n).unwrap_or_default()))
        })
    }
}

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hubdouble_model::Timestamp;
use serde::Serialize as _;
use serde_json::value::RawValue;
use serde_json::{Map, Value};

pub(crate) fn matches(name: &str, value: &Value) -> bool {
    match name {
        "Boolean" => value.is_boolean(),
        "Int" => value.is_i64() || value.is_u64() || wide_digits(value).is_some(),
        "Float" => value.is_number() || wide_digits(value).is_some() || infinite(value).is_some(),
        "String" | "ID" => value.is_string(),
        "CustomPropertyValue" => {
            value.is_string()
                || value.is_boolean()
                || value
                    .as_array()
                    .is_some_and(|items| items.iter().all(Value::is_string))
        }
        _ => value.as_str().is_some_and(|s| matches_string(name, s)),
    }
}

fn matches_string(name: &str, s: &str) -> bool {
    match name {
        "Date" => date(s),
        "DateTime" => s.ends_with('Z') && timestamp(s),
        "GitTimestamp" => timestamp(s),
        "PreciseDateTime" => {
            s.len() == 24 && s.as_bytes()[19] == b'.' && s.ends_with('Z') && timestamp(s)
        }
        "URI" => uri(s),
        "HTML" => !s.contains('\0'),
        "GitObjectID" => s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()),
        "GitRefname" => s.starts_with("refs/") && s.len() > 5 && !s.contains(char::is_whitespace),
        "GitSSHRemote" => ssh_remote(s),
        "Base64String" => STANDARD.decode(s).is_ok(),
        "BigInt" => digits(s.strip_prefix('-').unwrap_or(s)),
        "X509Certificate" => certificate(s),
        _ => false,
    }
}

pub(crate) fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

const WIDE_INT: &str = "\0int";
const INFINITE: &str = "\0inf";

/// Decodes a JSON string token as GitHub's parser does. A high surrogate escape and the
/// `\u` escape after it make one code point, a high surrogate with no `\u` escape after it
/// is `?`, and a lone low surrogate is three U+FFFD, the bytes that GitHub keeps for it and
/// cannot write as UTF-8. The flag is true when the token holds a lone low surrogate.
/// `None` when `token` is not a JSON string.
#[must_use]
pub fn json_string(token: &str) -> Option<(String, bool)> {
    if let Ok(text) = serde_json::from_str::<String>(token) {
        return Some((text, false));
    }
    let mut rest = token.strip_prefix('"')?.strip_suffix('"')?;
    let mut text = String::with_capacity(rest.len());
    let mut lone_low = false;
    while let Some(at) = rest.find('\\') {
        text.push_str(rest.get(..at)?);
        let escape = rest.get(at + 1..)?;
        let kind = escape.chars().next()?;
        rest = escape.get(kind.len_utf8()..)?;
        match kind {
            '"' | '\\' | '/' => text.push(kind),
            'b' => text.push('\u{8}'),
            'f' => text.push('\u{c}'),
            'n' => text.push('\n'),
            'r' => text.push('\r'),
            't' => text.push('\t'),
            'u' => {
                let unit = hex4(rest)?;
                rest = rest.get(4..)?;
                if unit & 0xFC00 == 0xD800 {
                    match rest.strip_prefix("\\u").and_then(hex4) {
                        Some(next) => {
                            rest = rest.get(6..)?;
                            text.push(char::from_u32(
                                (0x10000 + ((unit & 0x3FF) << 10)) | (next & 0x3FF),
                            )?);
                        }
                        None => text.push('?'),
                    }
                } else if unit & 0xFC00 == 0xDC00 {
                    lone_low = true;
                    text.push_str("\u{FFFD}\u{FFFD}\u{FFFD}");
                } else {
                    text.push(char::from_u32(unit)?);
                }
            }
            _ => return None,
        }
    }
    text.push_str(rest);
    Some((text, lone_low))
}

fn hex4(text: &str) -> Option<u32> {
    let digits = text.get(..4)?;
    if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(digits, 16).ok()
}

/// The value that stands for a number past the range of a double, which GitHub reads as
/// `Infinity` or `-Infinity`.
pub(crate) fn infinite(value: &Value) -> Option<&str> {
    let entry = value.as_object().filter(|o| o.len() == 1)?;
    entry.get(INFINITE)?.as_str()
}

/// GraphQL request variables. An integer past 64 bits keeps its digits in a form that only
/// [`Variables::parse`] makes: a request that sends the same form gets its object back. The
/// second field names each variable whose value holds a lone low surrogate.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Variables(pub(crate) Map<String, Value>, pub(crate) Vec<String>);

impl Variables {
    /// Reads the `variables` member of a GraphQL request body, keeping the digits of each
    /// integer past 64 bits and a number past the range of a double as GitHub's `Infinity`.
    /// It takes the nesting that GitHub takes in that member: 255 levels, the object of the
    /// member included. Strings decode as [`json_string`] decodes them.
    ///
    /// # Errors
    ///
    /// The `serde_json` error when `text` is not a JSON object or nests deeper than that.
    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        let mut exact = Exact::new(text);
        let fields = exact.whole_object()?;
        Ok(Self(fields, exact.lone_low_members))
    }
}

impl From<Map<String, Value>> for Variables {
    fn from(mut fields: Map<String, Value>) -> Self {
        escape_keys(&mut fields);
        Self(fields, Vec::new())
    }
}

/// The value that stands for an integer past 64 bits, which a `serde_json::Number` cannot
/// hold: a one-entry object whose key [`Variables`] keeps out of every request.
pub(crate) fn wide_int(digits: &str) -> Value {
    let mut entry = Map::new();
    entry.insert(WIDE_INT.to_owned(), Value::String(digits.to_owned()));
    Value::Object(entry)
}

/// The decimal digits of an integer of any size, as an `ID` takes it.
pub(crate) fn int_digits(value: &Value) -> Option<String> {
    match value {
        Value::Number(n) if n.is_i64() || n.is_u64() => Some(n.to_string()),
        v => wide_digits(v).map(str::to_owned),
    }
}

pub(crate) fn wide_digits(value: &Value) -> Option<&str> {
    let entry = value.as_object().filter(|o| o.len() == 1)?;
    entry.get(WIDE_INT)?.as_str()
}

/// A request's object key as it sent it.
pub(crate) fn unescaped(key: &str) -> &str {
    key.strip_prefix('\0').unwrap_or(key)
}

/// Writes `value` as the request sent it: each integer past 64 bits with all its digits, and
/// each object key without the escape that [`Variables`] adds.
#[expect(
    clippy::ref_option,
    reason = "serde's serialize_with passes a reference to the field"
)]
pub(crate) fn serialize_as_sent<S: serde::Serializer>(
    value: &Option<Value>,
    s: S,
) -> Result<S::Ok, S::Error> {
    value.as_ref().map(Sent).serialize(s)
}

pub(crate) struct Sent<'a>(pub(crate) &'a Value);

impl serde::Serialize for Sent<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if let Some(digits) = wide_digits(self.0) {
            return RawValue::from_string(digits.to_owned())
                .map_err(serde::ser::Error::custom)?
                .serialize(s);
        }
        if infinite(self.0).is_some() {
            return s.serialize_unit();
        }
        match self.0 {
            Value::Array(items) => s.collect_seq(items.iter().map(Sent)),
            Value::Object(fields) => {
                s.collect_map(fields.iter().map(|(k, v)| (unescaped(k), Sent(v))))
            }
            v => v.serialize(s),
        }
    }
}

/// `value` with each integer past `i64` at the `i64` bound of its sign, for a typed read.
pub(crate) fn saturate_wide_ints(value: &mut Value) {
    if value.is_u64() && !value.is_i64() {
        *value = Value::from(i64::MAX);
        return;
    }
    if let Some(digits) = wide_digits(value) {
        *value = Value::from(if digits.starts_with('-') {
            i64::MIN
        } else {
            i64::MAX
        });
        return;
    }
    match value {
        Value::Array(items) => items.iter_mut().for_each(saturate_wide_ints),
        Value::Object(fields) => fields.values_mut().for_each(saturate_wide_ints),
        _ => {}
    }
}

fn escaped(key: String) -> String {
    if key.starts_with('\0') {
        format!("\0{key}")
    } else {
        key
    }
}

fn escape_keys(fields: &mut Map<String, Value>) {
    if fields.keys().any(|k| k.starts_with('\0')) {
        *fields = std::mem::take(fields)
            .into_iter()
            .map(|(k, v)| (escaped(k), v))
            .collect();
    }
    for value in fields.values_mut() {
        escape_value(value);
    }
}

fn escape_value(value: &mut Value) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(escape_value),
        Value::Object(fields) => escape_keys(fields),
        _ => {}
    }
}

const MAX_DEPTH: usize = 255;

fn json_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

fn invalid(what: &str) -> serde_json::Error {
    serde::de::Error::custom(what)
}

struct Exact<'t> {
    text: &'t str,
    at: usize,
    lone_low: bool,
    lone_low_members: Vec<String>,
    #[cfg(test)]
    parsed: usize,
}

impl<'t> Exact<'t> {
    fn new(text: &'t str) -> Self {
        Self {
            text,
            at: 0,
            lone_low: false,
            lone_low_members: Vec::new(),
            #[cfg(test)]
            parsed: 0,
        }
    }

    fn peek(&mut self) -> Option<u8> {
        while let Some(b) = self.text.as_bytes().get(self.at) {
            if !json_space(*b) {
                return Some(*b);
            }
            self.bump();
        }
        None
    }

    fn bump(&mut self) {
        self.at += 1;
    }

    fn eat(&mut self, byte: u8) -> Result<(), serde_json::Error> {
        if self.peek() != Some(byte) {
            return Err(invalid("unexpected character"));
        }
        self.bump();
        Ok(())
    }

    fn token(&mut self, end: usize) -> Result<&'t str, serde_json::Error> {
        let token = self
            .text
            .get(self.at..end)
            .ok_or_else(|| invalid("unexpected end"))?;
        #[cfg(test)]
        {
            self.parsed += token.len();
        }
        self.at = end;
        Ok(token)
    }

    fn whole_object(&mut self) -> Result<Map<String, Value>, serde_json::Error> {
        let fields = self.object(1)?;
        match self.peek() {
            None => Ok(fields),
            Some(_) => Err(invalid("trailing characters")),
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, serde_json::Error> {
        match self.peek() {
            Some(b'{') => self.object(depth).map(Value::Object),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string().map(Value::String),
            Some(_) => self.scalar(),
            None => Err(invalid("unexpected end")),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Map<String, Value>, serde_json::Error> {
        if depth > MAX_DEPTH {
            return Err(invalid("recursion limit exceeded"));
        }
        self.eat(b'{')?;
        let mut fields = Map::new();
        if self.peek() == Some(b'}') {
            self.bump();
            return Ok(fields);
        }
        loop {
            if self.peek() != Some(b'"') {
                return Err(invalid("key must be a string"));
            }
            let name = escaped(self.string()?);
            self.eat(b':')?;
            self.lone_low = false;
            let value = self.value(depth + 1)?;
            if depth == 1 && self.lone_low {
                self.lone_low_members.push(name.clone());
            }
            fields.insert(name, value);
            match self.peek() {
                Some(b',') => self.bump(),
                Some(b'}') => {
                    self.bump();
                    return Ok(fields);
                }
                _ => return Err(invalid("expected `,` or `}`")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, serde_json::Error> {
        if depth > MAX_DEPTH {
            return Err(invalid("recursion limit exceeded"));
        }
        self.eat(b'[')?;
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.bump();
            return Ok(Value::Array(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            match self.peek() {
                Some(b',') => self.bump(),
                Some(b']') => {
                    self.bump();
                    return Ok(Value::Array(items));
                }
                _ => return Err(invalid("expected `,` or `]`")),
            }
        }
    }

    fn string(&mut self) -> Result<String, serde_json::Error> {
        let bytes = self.text.as_bytes();
        let mut end = self.at + 1;
        while let Some(c) = bytes.get(end) {
            match c {
                b'\\' => end += 2,
                b'"' => break,
                _ => end += 1,
            }
        }
        let (text, lone_low) =
            json_string(self.token(end + 1)?).ok_or_else(|| invalid("invalid string"))?;
        self.lone_low |= lone_low;
        Ok(text)
    }

    fn scalar(&mut self) -> Result<Value, serde_json::Error> {
        let rest = self.text.as_bytes().get(self.at..).unwrap_or_default();
        let len = rest
            .iter()
            .position(|&b| matches!(b, b',' | b']' | b'}') || json_space(b))
            .unwrap_or(rest.len());
        let token = self.token(self.at + len)?;
        let unsigned = token.strip_prefix('-').unwrap_or(token);
        if digits(unsigned)
            && !unsigned.starts_with('0')
            && token.parse::<i64>().is_err()
            && token.parse::<u64>().is_err()
        {
            return Ok(wide_int(token));
        }
        serde_json::from_str(token).or_else(|e| match token.parse::<f64>() {
            Ok(x) if x.is_infinite() => {
                let shown = if x < 0.0 { "-Infinity" } else { "Infinity" };
                let mut entry = Map::new();
                entry.insert(INFINITE.to_owned(), Value::String(shown.to_owned()));
                Ok(Value::Object(entry))
            }
            _ => Err(e),
        })
    }
}

fn date(s: &str) -> bool {
    let [y, m, d] = s.split('-').collect::<Vec<_>>()[..] else {
        return false;
    };
    if y.len() != 4 || m.len() != 2 || d.len() != 2 || ![y, m, d].iter().all(|s| digits(s)) {
        return false;
    }
    let (Ok(y), Ok(m), Ok(d)) = (y.parse::<u16>(), m.parse::<u8>(), d.parse::<u8>()) else {
        return false;
    };
    let max = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => 28 + u8::from(y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)),
        _ => 0,
    };
    (1..=max).contains(&d)
}

fn timestamp(s: &str) -> bool {
    if !s.is_ascii()
        || !s.get(..10).is_some_and(date)
        || ![11..13, 14..16, 17..19]
            .iter()
            .all(|r| s.get(r.clone()).is_some_and(digits))
    {
        return false;
    }
    if !s.ends_with('Z') && !s.ends_with('z') {
        let Some([_, h1, h2, b':', m1, m2]) = s.as_bytes().last_chunk::<6>() else {
            return false;
        };
        if ![h1, h2, m1, m2].iter().all(|b| b.is_ascii_digit())
            || (*h1 - b'0') * 10 + (*h2 - b'0') > 23
            || (*m1 - b'0') * 10 + (*m2 - b'0') > 59
        {
            return false;
        }
    }
    Timestamp::parse(s).is_ok()
}

fn uri(s: &str) -> bool {
    let scheme = s.split_once(':').map(|(scheme, _)| scheme);
    let absolute = scheme.is_some_and(|scheme| {
        let mut bytes = scheme.bytes();
        bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
            && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    });
    let template = s.contains('{');
    let mut open = false;
    let mut rest = s.as_bytes().iter().copied();
    (absolute || template)
        && s.chars().all(|c| !c.is_whitespace() && !c.is_control())
        && s.matches('#').count() <= 1
        && std::iter::from_fn(|| {
            rest.next().map(|b| match b {
                b'%' => [rest.next(), rest.next()]
                    .into_iter()
                    .all(|b| b.is_some_and(|b| b.is_ascii_hexdigit())),
                b'{' if !open => {
                    open = true;
                    rest.clone().next().is_some_and(|b| b != b'}')
                }
                b'}' if open => {
                    open = false;
                    true
                }
                b'{' | b'}' | b'<' | b'>' | b'"' | b'\\' | b'^' | b'`' | b'|' => false,
                _ => true,
            })
        })
        .all(|valid| valid)
        && !open
}

fn ssh_remote(s: &str) -> bool {
    if let Some(remote) = s.strip_prefix("ssh://") {
        return remote
            .split_once('/')
            .is_some_and(|(host, path)| !host.is_empty() && !path.is_empty() && uri(s));
    }
    s.rsplit_once(':').is_some_and(|(host, path)| {
        !host.is_empty()
            && !host.contains('/')
            && !path.is_empty()
            && !s.contains(char::is_whitespace)
    })
}

fn certificate(s: &str) -> bool {
    let Some(body) = s
        .trim()
        .strip_prefix("-----BEGIN CERTIFICATE-----")
        .and_then(|s| s.strip_suffix("-----END CERTIFICATE-----"))
    else {
        return false;
    };
    let encoded: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let Ok(der) = STANDARD.decode(encoded) else {
        return false;
    };
    let Some((certificate, [])) = der_part(&der, 0x30) else {
        return false;
    };
    let Some((tbs, rest)) = der_part(certificate, 0x30) else {
        return false;
    };
    let tbs = if tbs.first() == Some(&0xa0) {
        der_part(tbs, 0xa0).map_or(&[][..], |(_, rest)| rest)
    } else {
        tbs
    };
    der_part(tbs, 0x02).is_some()
        && der_part(rest, 0x30).is_some_and(|(algorithm, rest)| {
            der_part(algorithm, 0x06).is_some()
                && der_part(rest, 0x03).is_some_and(|(signature, rest)| {
                    signature.len() > 1 && signature[0] < 8 && rest.is_empty()
                })
        })
}

fn der_part(bytes: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
    let [actual, size, tail @ ..] = bytes else {
        return None;
    };
    if *actual != tag {
        return None;
    }
    if *size < 128 {
        return tail.split_at_checked(usize::from(*size));
    }
    let (digits, tail) = tail.split_at_checked(usize::from(*size & 127))?;
    if digits.first().is_none_or(|first| *first == 0) || digits.len() > size_of::<usize>() {
        return None;
    }
    let len = digits
        .iter()
        .fold(0_usize, |n, b| n * 256 + usize::from(*b));
    (len >= 128).then(|| tail.split_at_checked(len)).flatten()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn variables_keep_the_digits_of_a_wide_integer_at_any_depth() {
        // Given variables with wide integers in a list and an input object, next to a string of
        // digits, a float, integers that fit 64 bits and one past the range of a double
        let past_doubles = "9".repeat(400);
        let raw = format!(
            r#"{{"a": [1, -99999999999999999999], "b": {{"c": 18446744073709551616}},
            "d": "99999999999999999999", "e": 1e20, "f": 18446744073709551615, "g": -5,
            "h": {past_doubles}}}"#
        );

        // When
        let Variables(vars, _) = Variables::parse(&raw).unwrap();

        // Then only the wide integers change form, and an Int and a Float take them
        assert_eq!(wide_digits(&vars["a"][1]), Some("-99999999999999999999"));
        assert_eq!(wide_digits(&vars["b"]["c"]), Some("18446744073709551616"));
        assert_eq!(wide_digits(&vars["h"]), Some(past_doubles.as_str()));
        assert_eq!(vars["a"][0], json!(1));
        assert_eq!(vars["d"], json!("99999999999999999999"));
        assert_eq!(vars["e"], json!(1e20));
        assert_eq!(vars["f"], json!(u64::MAX));
        assert_eq!(vars["g"], json!(-5));
        assert!(matches("Int", &vars["b"]["c"]) && matches("Float", &vars["b"]["c"]));
        assert!(!matches("Int", &vars["d"]) && !matches("Int", &vars["e"]));
    }

    fn wide_ints_nested(arrays: usize) -> String {
        let wide = "18446744073709551616";
        let inner = (0..arrays).fold("{\"x\": 1}".to_owned(), |inner, _| {
            format!("[{wide}, \"[{{\\\"\", {inner}]")
        });
        format!(r#"{{"a": {inner}}}"#)
    }

    #[test]
    fn variables_read_each_byte_once_up_to_255_levels() {
        // Given variables of 255 levels with a wide integer and a string at each level, and of
        // 256 levels
        let (fits, deeper) = (wide_ints_nested(253), wide_ints_nested(254));

        // When each is read
        let mut exact = Exact::new(&fits);
        let read = exact.object(1);

        // Then the 255 levels read, each byte reaches a parse at most once, and 256 levels are
        // too deep
        let mut value = &read.unwrap()["a"];
        for _ in 0..253 {
            assert_eq!(wide_digits(&value[0]), Some("18446744073709551616"));
            assert_eq!(value[1], json!("[{\""));
            value = &value[2];
        }
        assert_eq!(value, &json!({"x": 1}));
        assert!(
            exact.parsed <= fits.len(),
            "{} of {}",
            exact.parsed,
            fits.len()
        );
        assert!(Variables::parse(&deeper).is_err());
    }

    #[test]
    fn a_request_cannot_send_the_form_of_a_wide_integer() {
        // Given the form of a wide integer, sent at the top, in a list and inside an object
        let raw = r#"{"n": {"\u0000int": "5"}, "l": [{"\u0000int": "5"}],
            "o": {"x": {"\u0000int": "5"}}}"#;
        let read: Map<String, Value> = serde_json::from_str(raw).unwrap();

        // When the variables are read exactly, and from what serde_json read
        for Variables(vars, _) in [Variables::parse(raw).unwrap(), Variables::from(read)] {
            // Then no value reads as an integer, and each shows the object that was sent
            for value in [&vars["n"], &vars["l"][0], &vars["o"]["x"]] {
                assert_eq!(wide_digits(value), None);
                assert!(!matches("Int", value) && !matches("Float", value));
                let sent = serde_json::to_string(&Sent(value)).unwrap();
                assert_eq!(sent, r#"{"\u0000int":"5"}"#);
            }
        }
    }

    #[test]
    fn only_json_whitespace_and_numbers_without_leading_zeros_parse() {
        for (raw, parses) in [
            ("{\"n\":\t1\r\n}", true),
            ("{\"n\":18446744073709551616}", true),
            ("{\"n\":\u{c}1}", false),
            ("{\"n\":1\u{c}}", false),
            ("{\"n\":01}", false),
            ("{\"n\":018446744073709551616}", false),
            ("{\"n\":-018446744073709551616}", false),
        ] {
            // Given variables that RFC 8259 accepts or refuses
            // When they are read exactly
            let read = Variables::parse(raw);
            // Then only what JSON accepts parses
            assert_eq!(read.is_ok(), parses, "{raw:?}");
        }
    }
}

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
        "Float" => value.is_number() || wide_digits(value).is_some(),
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

/// GraphQL request variables. An integer past 64 bits keeps its digits in a form that only
/// [`Variables::parse`] makes: a request that sends the same form gets its object back.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Variables(pub(crate) Map<String, Value>);

impl Variables {
    /// Reads the `variables` member of a GraphQL request body, keeping the digits of each
    /// integer past 64 bits. It takes the nesting that `serde_json` takes in that member of a
    /// body: 126 levels, the object of the member included.
    ///
    /// # Errors
    ///
    /// The `serde_json` error when `text` is not a JSON object or nests deeper than that.
    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        exact_object(text, 1).map(Self)
    }

    /// Whether `fields`, as `serde_json` read them, may have lost the digits of an integer past
    /// 64 bits, so that [`Variables::parse`] must read them again.
    #[must_use]
    pub fn may_be_wide(fields: &Map<String, Value>) -> bool {
        fields.values().any(holds_a_large_double)
    }
}

impl From<Map<String, Value>> for Variables {
    fn from(mut fields: Map<String, Value>) -> Self {
        escape_keys(&mut fields);
        Self(fields)
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

fn holds_a_large_double(value: &Value) -> bool {
    match value {
        Value::Number(n) => n.is_f64() && n.as_f64().is_some_and(|f| f.abs() >= 2f64.powi(63)),
        Value::Array(items) => items.iter().any(holds_a_large_double),
        Value::Object(fields) => fields.values().any(holds_a_large_double),
        _ => false,
    }
}

const MAX_DEPTH: usize = 126;

fn exact_object(text: &str, depth: usize) -> Result<Map<String, Value>, serde_json::Error> {
    if depth > MAX_DEPTH {
        return Err(serde::de::Error::custom("recursion limit exceeded"));
    }
    let fields: std::collections::BTreeMap<String, &RawValue> = serde_json::from_str(text)?;
    fields
        .into_iter()
        .map(|(name, value)| Ok((escaped(name), exact(value.get(), depth + 1)?)))
        .collect()
}

fn exact(text: &str, depth: usize) -> Result<Value, serde_json::Error> {
    match text.as_bytes().first() {
        Some(b'{') => exact_object(text, depth).map(Value::Object),
        Some(b'[') if depth > MAX_DEPTH => {
            Err(serde::de::Error::custom("recursion limit exceeded"))
        }
        Some(b'[') => serde_json::from_str::<Vec<&RawValue>>(text)?
            .into_iter()
            .map(|item| exact(item.get(), depth + 1))
            .collect::<Result<_, _>>()
            .map(Value::Array),
        Some(b'-' | b'0'..=b'9')
            if digits(text.strip_prefix('-').unwrap_or(text))
                && text.parse::<i64>().is_err()
                && text.parse::<u64>().is_err() =>
        {
            Ok(wide_int(text))
        }
        _ => serde_json::from_str(text),
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
        let Variables(vars) = Variables::parse(&raw).unwrap();

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

    #[test]
    fn a_request_cannot_send_the_form_of_a_wide_integer() {
        // Given the form of a wide integer, sent at the top, in a list and inside an object
        let raw = r#"{"n": {"\u0000int": "5"}, "l": [{"\u0000int": "5"}],
            "o": {"x": {"\u0000int": "5"}}}"#;
        let read: Map<String, Value> = serde_json::from_str(raw).unwrap();

        // When the variables are read exactly, and from what serde_json read
        for Variables(vars) in [Variables::parse(raw).unwrap(), Variables::from(read)] {
            // Then no value reads as an integer, and each shows the object that was sent
            for value in [&vars["n"], &vars["l"][0], &vars["o"]["x"]] {
                assert_eq!(wide_digits(value), None);
                assert!(!matches("Int", value) && !matches("Float", value));
                let sent = serde_json::to_string(&Sent(value)).unwrap();
                assert_eq!(sent, r#"{"\u0000int":"5"}"#);
            }
        }
    }
}

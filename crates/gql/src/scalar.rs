use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hubdouble_model::Timestamp;
use serde_json::Value;

pub(crate) fn matches(name: &str, value: &Value) -> bool {
    match name {
        "Boolean" => value.is_boolean(),
        "Int" => value.as_i64().is_some_and(|n| i32::try_from(n).is_ok()),
        "Float" => value.is_number(),
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

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
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

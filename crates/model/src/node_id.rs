//! GitHub's node ID formats.
//!
//! The current format is a type prefix, `_`, and URL-safe base64 (no padding) of a `MessagePack`
//! array that starts with `0`. Entity IDs here use `[0, id]`, so [`entity`] and [`entity_id`]
//! round-trip. The legacy format is base64 of `0<len>:<Type><id>`.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};

pub const USER: &str = "U";
pub const ORGANIZATION: &str = "O";
pub const BOT: &str = "BOT";
pub const TEAM: &str = "T";
pub const REPOSITORY: &str = "R";
pub const APP: &str = "A";
pub const INSTALLATION: &str = "IN";
pub const ISSUE: &str = "I";
pub const PULL_REQUEST: &str = "PR";
pub const ISSUE_COMMENT: &str = "IC";
pub const REVIEW: &str = "PRR";
pub const REVIEW_COMMENT: &str = "PRRC";
pub const REVIEW_THREAD: &str = "PRRT";
pub const REACTION: &str = "REA";
pub const LABEL: &str = "LA";
pub const CHECK_SUITE: &str = "CS";
pub const CHECK_RUN: &str = "CR";
pub const STATUS: &str = "SC";
pub const RELEASE: &str = "RE";
pub const RELEASE_ASSET: &str = "RA";
pub const WORKFLOW_RUN: &str = "WFR";
pub const WORKFLOW_JOB: &str = "WFJ";
pub const RULESET: &str = "RRS";
pub const MEMBERSHIP: &str = "OM";

/// One element of a node ID payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    Int(i64),
    Str(String),
}

/// A decoded current-format node ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeId {
    pub prefix: String,
    pub parts: Vec<Part>,
}

/// The node ID of an entity: `prefix` with the payload `[0, id]`.
#[must_use]
pub fn entity(prefix: &str, id: i64) -> String {
    encode(prefix, &[Part::Int(0), Part::Int(id)])
}

/// The entity ID inside a node ID made by [`entity`] with this `prefix`.
#[must_use]
pub fn entity_id(prefix: &str, node_id: &str) -> Option<i64> {
    let n = decode(node_id)?;
    match n.parts.as_slice() {
        [Part::Int(0), Part::Int(id)] if n.prefix == prefix => Some(*id),
        _ => None,
    }
}

#[must_use]
pub fn encode(prefix: &str, parts: &[Part]) -> String {
    let mut buf = Vec::with_capacity(16);
    push_array_len(&mut buf, parts.len());
    for (i, part) in parts.iter().enumerate() {
        match part {
            Part::Int(v) => match u8::try_from(*v) {
                Ok(version) if i == 0 && version < 0x80 => buf.push(version),
                _ => push_int(&mut buf, *v),
            },
            Part::Str(s) => push_str(&mut buf, s),
        }
    }
    let mut out = String::with_capacity(prefix.len() + 1 + buf.len() * 4 / 3 + 2);
    out.push_str(prefix);
    out.push('_');
    URL_SAFE_NO_PAD.encode_string(&buf, &mut out);
    out
}

#[must_use]
pub fn decode(node_id: &str) -> Option<NodeId> {
    let (prefix, payload) = node_id.split_once('_')?;
    if prefix.is_empty() || !prefix.bytes().all(|b| b.is_ascii_uppercase()) {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let mut r = Reader {
        bytes: &bytes,
        at: 0,
    };
    let len = r.array_len()?;
    let mut parts = Vec::with_capacity(len);
    for _ in 0..len {
        parts.push(r.part()?);
    }
    (r.at == bytes.len()).then(|| NodeId {
        prefix: prefix.to_owned(),
        parts,
    })
}

/// The legacy ID (`MDQ6VXNlcjE=` for user 1) of a type name and ID.
#[must_use]
pub fn encode_legacy(type_name: &str, id: i64) -> String {
    STANDARD.encode(format!("0{}:{type_name}{id}", type_name.len()))
}

/// Splits a legacy ID into its type name and ID.
#[must_use]
pub fn decode_legacy(legacy: &str) -> Option<(String, i64)> {
    let text = String::from_utf8(STANDARD.decode(legacy).ok()?).ok()?;
    let (len, rest) = text.strip_prefix('0')?.split_once(':')?;
    let len: usize = len.parse().ok()?;
    let type_name = rest.get(..len)?;
    let id = rest.get(len..)?.parse().ok()?;
    Some((type_name.to_owned(), id))
}

fn push_array_len(buf: &mut Vec<u8>, len: usize) {
    match u8::try_from(len) {
        Ok(n) if n < 16 => buf.push(0x90 | n),
        _ => {
            buf.push(0xdc);
            buf.extend_from_slice(&u16::try_from(len).unwrap_or(u16::MAX).to_be_bytes());
        }
    }
}

fn push_int(buf: &mut Vec<u8>, v: i64) {
    match u32::try_from(v) {
        Ok(n) => {
            buf.push(0xce);
            buf.extend_from_slice(&n.to_be_bytes());
        }
        Err(_) => {
            if let Ok(n) = u64::try_from(v) {
                buf.push(0xcf);
                buf.extend_from_slice(&n.to_be_bytes());
            } else {
                buf.push(0xd3);
                buf.extend_from_slice(&v.to_be_bytes());
            }
        }
    }
}

fn push_str(buf: &mut Vec<u8>, s: &str) {
    match u8::try_from(s.len()) {
        Ok(n) if n < 32 => buf.push(0xa0 | n),
        Ok(n) => {
            buf.push(0xd9);
            buf.push(n);
        }
        Err(_) => {
            buf.push(0xda);
            buf.extend_from_slice(&u16::try_from(s.len()).unwrap_or(u16::MAX).to_be_bytes());
        }
    }
    buf.extend_from_slice(s.as_bytes());
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let s = self.bytes.get(self.at..self.at + n)?;
        self.at += n;
        Some(s)
    }

    fn byte(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn array_len(&mut self) -> Option<usize> {
        match self.byte()? {
            b @ 0x90..=0x9f => Some(usize::from(b & 0x0f)),
            0xdc => Some(usize::from(u16::from_be_bytes(
                self.take(2)?.try_into().ok()?,
            ))),
            _ => None,
        }
    }

    fn part(&mut self) -> Option<Part> {
        let int = |v: i64| Some(Part::Int(v));
        match self.byte()? {
            b @ 0x00..=0x7f => int(i64::from(b)),
            0xcc => int(i64::from(self.byte()?)),
            0xcd => int(i64::from(u16::from_be_bytes(
                self.take(2)?.try_into().ok()?,
            ))),
            0xce => int(i64::from(u32::from_be_bytes(
                self.take(4)?.try_into().ok()?,
            ))),
            0xcf => int(i64::try_from(u64::from_be_bytes(self.take(8)?.try_into().ok()?)).ok()?),
            0xd3 => int(i64::from_be_bytes(self.take(8)?.try_into().ok()?)),
            b @ 0xa0..=0xbf => self.string(usize::from(b & 0x1f)),
            0xd9 => {
                let n = self.byte()?;
                self.string(usize::from(n))
            }
            0xda => {
                let n = u16::from_be_bytes(self.take(2)?.try_into().ok()?);
                self.string(usize::from(n))
            }
            _ => None,
        }
    }

    fn string(&mut self, n: usize) -> Option<Part> {
        Some(Part::Str(
            std::str::from_utf8(self.take(n)?).ok()?.to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_github_current_ids() {
        // Given GitHub's node ID of user 1, `U_kgDOAAAAAQ`
        // When the user's ID is encoded
        // Then the result is the same text, and it decodes back
        assert_eq!(entity(USER, 1), "U_kgDOAAAAAQ");
        assert_eq!(entity_id(USER, "U_kgDOAAAAAQ"), Some(1));
        assert_eq!(entity_id(REPOSITORY, "U_kgDOAAAAAQ"), None);
    }

    #[test]
    fn round_trips_large_ids_and_strings() {
        let parts = vec![
            Part::Int(0),
            Part::Int(5_000_000_000),
            Part::Str("f".repeat(40)),
        ];
        let id = encode("C", &parts);
        assert_eq!(
            decode(&id),
            Some(NodeId {
                prefix: "C".into(),
                parts
            })
        );
        assert_eq!(decode("nope"), None);
    }

    #[test]
    fn reads_legacy_ids() {
        assert_eq!(decode_legacy("MDQ6VXNlcjE="), Some(("User".to_owned(), 1)));
        assert_eq!(encode_legacy("PullRequest", 1), "MDExOlB1bGxSZXF1ZXN0MQ==");
        assert_eq!(decode_legacy("!!"), None);
    }
}

use std::fmt::Write as _;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE;
use http::header::LINK;
use hubdouble_model::ApiError;
use hubdouble_render::query_escape;
use hubdouble_rest::{Ctx, Page, PageLimit, Reply};
use serde::Serialize;

use crate::shared::invalid_value;

const PREFIX: &[u8] = b"cursor:v2:";

const PAGES: PageLimit = PageLimit {
    results: 10_000,
    lookahead: 1,
    message: "Pagination with the page parameter is not supported for large datasets, please use cursor based pagination (after/before)",
    documentation_url: "https://docs.github.com/rest/issues/issues#list-repository-issues",
    before_overflow: false,
};

pub(crate) struct Keyed<T> {
    pub(crate) key: u64,
    pub(crate) id: u64,
    pub(crate) item: T,
}

pub(crate) fn encode(key: u64, id: u64) -> String {
    let mut bytes = PREFIX.to_vec();
    bytes.push(0x92);
    for n in [key, id] {
        push_uint(&mut bytes, n);
    }
    URL_SAFE.encode(bytes)
}

fn push_uint(out: &mut Vec<u8>, n: u64) {
    match n {
        0..=0x7f => out.push(n.to_be_bytes()[7]),
        0x80..=0xff => out.extend([0xcc, n.to_be_bytes()[7]]),
        0x100..=0xffff => {
            out.push(0xcd);
            out.extend(&n.to_be_bytes()[6..]);
        }
        0x1_0000..=0xffff_ffff => {
            out.push(0xce);
            out.extend(&n.to_be_bytes()[4..]);
        }
        _ => {
            out.push(0xcf);
            out.extend(n.to_be_bytes());
        }
    }
}

pub(crate) fn decode(cursor: &str) -> Option<(u64, u64)> {
    let bytes = URL_SAFE.decode(cursor).ok()?;
    let rest = bytes.strip_prefix(PREFIX)?.strip_prefix(&[0x92])?;
    let (key, rest) = read_uint(rest)?;
    let (id, rest) = read_uint(rest)?;
    rest.is_empty().then_some((key, id))
}

fn read_uint(bytes: &[u8]) -> Option<(u64, &[u8])> {
    let (&tag, rest) = bytes.split_first()?;
    let width = match tag {
        0..=0x7f => return Some((u64::from(tag), rest)),
        0xcc => 1,
        0xcd => 2,
        0xce => 4,
        0xcf => 8,
        _ => return None,
    };
    let (n, rest) = rest.split_at_checked(width)?;
    let mut buf = [0u8; 8];
    buf[8 - width..].copy_from_slice(n);
    Some((u64::from_be_bytes(buf), rest))
}

pub(crate) fn page<T, R: Serialize>(
    ctx: &Ctx<'_>,
    base: &str,
    descending: bool,
    items: &[Keyed<T>],
    render: impl FnMut(&T) -> R,
) -> Result<Reply, ApiError> {
    let q = ctx.query();
    let cursor = |field: &str| {
        q.get(field)
            .map(|c| {
                decode(c).ok_or_else(|| {
                    invalid_value("Issue", field, c).with_docs(PAGES.documentation_url)
                })
            })
            .transpose()
    };
    let (after, before) = (cursor("after")?, cursor("before")?);
    let Page {
        page: asked,
        per_page,
    } = if after.is_some() || before.is_some() {
        ctx.page_params()?
    } else {
        ctx.page_params_within(&PAGES)?
    };
    let passed = |e: &Keyed<T>, c: (u64, u64)| {
        let at = (e.key, e.id);
        if descending { at > c } else { at < c }
    };
    let (start, end) = if let Some(c) = after {
        let start = items.partition_point(|e| passed(e, c) || (e.key, e.id) == c);
        (start, start.saturating_add(per_page).min(items.len()))
    } else if let Some(c) = before {
        let end = items.partition_point(|e| passed(e, c));
        (end.saturating_sub(per_page), end)
    } else {
        let start = (asked - 1).saturating_mul(per_page).min(items.len());
        (start, start.saturating_add(per_page).min(items.len()))
    };
    let shown = &items[start..end];
    let rendered: Vec<R> = shown.iter().map(|e| &e.item).map(render).collect();
    let reply = Reply::json(&rendered)?;
    let (Some(first), Some(last)) = (shown.first(), shown.last()) else {
        return Ok(reply);
    };
    let page = q.get("page").map(|_| asked);
    let mut rels = Vec::with_capacity(2);
    if end < items.len() {
        let next_page = match page {
            Some(p) => Some(p.saturating_add(1)),
            None => (start == 0).then_some(2),
        };
        rels.push((
            link(
                ctx,
                base,
                "after",
                &encode(last.key, last.id),
                page,
                next_page,
            ),
            "next",
        ));
    }
    if start > 0 {
        let prev_page = page.map(|p| p.saturating_sub(1).max(1));
        rels.push((
            link(
                ctx,
                base,
                "before",
                &encode(first.key, first.id),
                page,
                prev_page,
            ),
            "prev",
        ));
    }
    if rels.is_empty() {
        return Ok(reply);
    }
    let header = rels
        .iter()
        .map(|(url, rel)| format!("<{url}>; rel=\"{rel}\""))
        .collect::<Vec<_>>()
        .join(", ");
    reply.header(LINK, &header)
}

fn link(
    ctx: &Ctx<'_>,
    base: &str,
    field: &str,
    cursor: &str,
    asked_page: Option<usize>,
    page: Option<usize>,
) -> String {
    let other = if field == "after" { "before" } else { "after" };
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut placed = false;
    for (name, value) in ctx.query().distinct().filter(|(n, _)| *n != other) {
        let value = match name {
            _ if name == field => {
                placed = true;
                cursor.to_owned()
            }
            "page" => page.map_or_else(|| value.to_owned(), |p| p.to_string()),
            _ => value.to_owned(),
        };
        pairs.push((name.to_owned(), value));
    }
    if !placed {
        pairs.push((field.to_owned(), cursor.to_owned()));
    }
    if asked_page.is_none()
        && let Some(p) = page
    {
        pairs.push(("page".to_owned(), p.to_string()));
    }
    let mut url = format!("{base}?");
    for (i, (name, value)) in pairs.iter().enumerate() {
        if i > 0 {
            url.push('&');
        }
        let _ = write!(url, "{}={}", query_escape(name), query_escape(value));
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_read_and_write_githubs_msgpack_form() {
        // Given cursors api.github.com wrote for cli/cli issues, decoded by hand
        let live = [
            (
                "Y3Vyc29yOnYyOpLPAAABofDqxIDPAAAAAYInK-o=",
                1_795_043_280_000,
                6_478_572_522,
            ),
            (
                "Y3Vyc29yOnYyOpLPAAABbxBZO2jOIBrTNA==",
                1_576_527_281_000,
                538_628_916,
            ),
            ("Y3Vyc29yOnYyOpJtziF2f9I=", 109, 561_414_098),
        ];
        for (cursor, key, id) in live {
            // When / Then
            assert_eq!(decode(cursor), Some((key, id)), "{cursor}");
            assert_eq!(encode(key, id), cursor);
        }
        assert_eq!(decode("bogus"), None);
        assert_eq!(decode(&URL_SAFE.encode(b"cursor:v2:\x92\x01")), None);
    }
}

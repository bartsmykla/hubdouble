use http::header::{self, HeaderValue};
use http::{HeaderMap, Method, StatusCode};
use hubdouble_model::ApiError;
use hubdouble_rest::Reply;
use hubdouble_store::{Blobs, Store};

use crate::pipeline::{HttpResponse, error_reply, to_response};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Range {
    Whole,
    Part { start: usize, end: usize },
    Unsatisfiable,
}

fn parse_range(value: &str, len: usize) -> Range {
    let Some(spec) = value.trim().strip_prefix("bytes=") else {
        return Range::Whole;
    };
    let Some((a, b)) = spec.split_once('-').filter(|_| !spec.contains(',')) else {
        return Range::Whole;
    };
    let (a, b) = (a.trim(), b.trim());
    let last = len.checked_sub(1);
    if a.is_empty() {
        return match (b.parse::<usize>(), last) {
            (Ok(0), _) | (Ok(_), None) => Range::Unsatisfiable,
            (Ok(n), Some(last)) => Range::Part {
                start: len.saturating_sub(n),
                end: last,
            },
            (Err(_), _) => Range::Whole,
        };
    }
    let Ok(start) = a.parse::<usize>() else {
        return Range::Whole;
    };
    let end = if b.is_empty() {
        None
    } else {
        match b.parse::<usize>() {
            Ok(e) if e >= start => Some(e),
            _ => return Range::Whole,
        }
    };
    match last {
        Some(last) if start <= last => Range::Part {
            start,
            end: end.map_or(last, |e| e.min(last)),
        },
        _ => Range::Unsatisfiable,
    }
}

pub(crate) fn serve(store: &Store, method: &Method, headers: &HeaderMap, id: &str) -> HttpResponse {
    let head = method == Method::HEAD;
    let blob = if head || method == Method::GET {
        store
            .view(|tx| Ok(tx.table::<Blobs>().get(id).cloned()))
            .ok()
            .flatten()
    } else {
        None
    };
    let Some(blob) = blob else {
        return to_response(error_reply(&ApiError::not_found()), head);
    };
    let len = blob.data.len();
    let content_type: &'static str = "application/octet-stream";
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map_or(Range::Whole, |v| parse_range(v, len));
    let mut reply = match range {
        Range::Whole => Reply::raw(content_type, blob.data.clone()),
        Range::Part { start, end } => {
            let mut r = Reply::raw(content_type, blob.data.slice(start..=end))
                .status(StatusCode::PARTIAL_CONTENT);
            insert(
                &mut r.headers,
                header::CONTENT_RANGE,
                &format!("bytes {start}-{end}/{len}"),
            );
            r
        }
        Range::Unsatisfiable => {
            let mut r = Reply::raw(content_type, bytes::Bytes::new())
                .status(StatusCode::RANGE_NOT_SATISFIABLE);
            insert(
                &mut r.headers,
                header::CONTENT_RANGE,
                &format!("bytes */{len}"),
            );
            r
        }
    };
    insert(&mut reply.headers, header::CONTENT_TYPE, &blob.content_type);
    reply
        .headers
        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    to_response(reply, head)
}

fn insert(headers: &mut HeaderMap, name: header::HeaderName, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        headers.insert(name, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_follow_rfc_9110() {
        for (value, want) in [
            ("bytes=0-3", Range::Part { start: 0, end: 3 }),
            ("bytes=4-", Range::Part { start: 4, end: 9 }),
            ("bytes=-3", Range::Part { start: 7, end: 9 }),
            ("bytes=-30", Range::Part { start: 0, end: 9 }),
            ("bytes=5-50", Range::Part { start: 5, end: 9 }),
            ("bytes=10-", Range::Unsatisfiable),
            ("bytes=-0", Range::Unsatisfiable),
            ("bytes=3-1", Range::Whole),
            ("bytes=0-1,4-5", Range::Whole),
            ("items=0-1", Range::Whole),
        ] {
            assert_eq!(parse_range(value, 10), want, "{value}");
        }
        assert_eq!(parse_range("bytes=0-", 0), Range::Unsatisfiable);
    }
}

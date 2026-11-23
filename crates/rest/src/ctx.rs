use std::fmt::Write as _;
use std::sync::Arc;

use bytes::Bytes;
use http::HeaderMap;
use http::header::LINK;
use hubdouble_auth::Actor;
use hubdouble_git::Git;
use hubdouble_model::{ApiError, Clock, Repo, Timestamp};
use hubdouble_render::Urls;
use hubdouble_store::{Tx, TxMut};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::params::{Params, Query};
use crate::reply::Reply;

const QUERY_VALUE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'&')
    .add(b'+')
    .add(b'<')
    .add(b'=')
    .add(b'>');

/// The store access of a request: `GET` and `HEAD` read, every other method writes.
pub enum Access<'a> {
    Read(&'a Tx),
    Write(TxMut<'a>),
}

/// Everything a [`Ctx`] is built from.
pub struct Parts<'a> {
    pub operation_id: &'static str,
    /// The escaped request path without the API prefix, for `Link` headers.
    pub path: &'a str,
    pub params: &'a Params,
    pub query: &'a Query,
    pub headers: &'a HeaderMap,
    pub body: &'a Bytes,
    pub actor: &'a Actor,
    pub repo: Option<&'a Arc<Repo>>,
    pub git: &'a Git,
    pub access: Access<'a>,
    pub clock: &'a dyn Clock,
    pub urls: &'a Urls,
}

/// A handler's view of one request. It lives inside the store transaction, so a handler never locks.
pub struct Ctx<'a> {
    p: Parts<'a>,
}

/// The `page` and `per_page` of a list request: `per_page` defaults to 30, at most 100.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    pub page: usize,
    pub per_page: usize,
}

impl<'a> Ctx<'a> {
    #[must_use]
    pub fn new(parts: Parts<'a>) -> Ctx<'a> {
        Ctx { p: parts }
    }

    #[must_use]
    pub fn operation_id(&self) -> &'static str {
        self.p.operation_id
    }

    /// A path parameter.
    ///
    /// # Errors
    ///
    /// `404` when the route has no such parameter.
    pub fn param(&self, name: &str) -> Result<&str, ApiError> {
        self.p.params.get(name).ok_or_else(ApiError::not_found)
    }

    /// A numeric path parameter; GitHub answers `404` for one that is not a number.
    ///
    /// # Errors
    ///
    /// `404` when the parameter is missing or not an integer.
    pub fn param_i64(&self, name: &str) -> Result<i64, ApiError> {
        self.param(name)?.parse().map_err(|_| ApiError::not_found())
    }

    #[must_use]
    pub fn params(&self) -> &Params {
        self.p.params
    }

    #[must_use]
    pub fn query(&self) -> &Query {
        self.p.query
    }

    #[must_use]
    pub fn headers(&self) -> &HeaderMap {
        self.p.headers
    }

    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.p.headers.get(name).and_then(|v| v.to_str().ok())
    }

    #[must_use]
    pub fn raw_body(&self) -> &Bytes {
        self.p.body
    }

    /// The JSON body; an empty body reads as `{}`, and a repeated object key takes its last
    /// value, as GitHub reads it.
    ///
    /// # Errors
    ///
    /// `400` for a body that is not JSON, `422` for JSON of the wrong shape.
    pub fn body<T: DeserializeOwned>(&self) -> Result<T, ApiError> {
        let body: &[u8] = if self.p.body.iter().all(u8::is_ascii_whitespace) {
            b"{}"
        } else {
            self.p.body
        };
        serde_json::from_slice(body)
            .or_else(|e| {
                if e.classify() != serde_json::error::Category::Data {
                    return Err(e);
                }
                serde_json::from_slice::<serde_json::Value>(body)
                    .and_then(T::deserialize)
                    .map_err(|_| e)
            })
            .map_err(|e| match e.classify() {
                serde_json::error::Category::Data => {
                    ApiError::unprocessable(format!("Invalid request.\n\n{e}"))
                }
                _ => ApiError::bad_request("Problems parsing JSON"),
            })
    }

    #[must_use]
    pub fn actor(&self) -> &Actor {
        self.p.actor
    }

    /// The repository named by `{owner}`/`{repo}`, resolved and checked before the handler runs.
    ///
    /// # Errors
    ///
    /// `404` for a route without a repository.
    pub fn repo(&self) -> Result<&Arc<Repo>, ApiError> {
        self.p.repo.ok_or_else(ApiError::not_found)
    }

    #[must_use]
    pub fn git(&self) -> &Git {
        self.p.git
    }

    #[must_use]
    pub fn tx(&self) -> &Tx {
        match &self.p.access {
            Access::Read(tx) => tx,
            Access::Write(tx) => tx,
        }
    }

    /// The write transaction of a non-`GET` request.
    ///
    /// # Errors
    ///
    /// `500` inside a `GET` or `HEAD` request, which only reads.
    pub fn tx_mut(&mut self) -> Result<&mut TxMut<'a>, ApiError> {
        match &mut self.p.access {
            Access::Write(tx) => Ok(tx),
            Access::Read(_) => Err(ApiError::internal("a read request cannot write")),
        }
    }

    /// The transaction and the git handle at once, for a write that also moves refs.
    ///
    /// # Errors
    ///
    /// `500` inside a `GET` or `HEAD` request.
    pub fn tx_mut_and_git(&mut self) -> Result<(&mut TxMut<'a>, &'a Git), ApiError> {
        let git = self.p.git;
        Ok((self.tx_mut()?, git))
    }

    #[must_use]
    pub fn clock(&self) -> &dyn Clock {
        self.p.clock
    }

    #[must_use]
    pub fn now(&self) -> Timestamp {
        self.p.clock.now()
    }

    #[must_use]
    pub fn urls(&self) -> &Urls {
        self.p.urls
    }

    #[must_use]
    pub fn page_params(&self) -> Page {
        let number = |name: &str| self.p.query.get(name).and_then(|v| v.parse::<usize>().ok());
        Page {
            page: number("page").filter(|p| *p > 0).unwrap_or(1),
            per_page: number("per_page")
                .filter(|p| *p > 0)
                .map_or(30, |p| p.min(100)),
        }
    }

    /// One page of `items` as a JSON array with GitHub's `Link` header; an empty page is `[]`.
    ///
    /// # Errors
    ///
    /// `500` when an item cannot be serialized.
    pub fn page<T, R: Serialize>(
        &self,
        items: &[T],
        render: impl FnMut(&T) -> R,
    ) -> Result<Reply, ApiError> {
        let Page { page, per_page } = self.page_params();
        let start = (page - 1).saturating_mul(per_page).min(items.len());
        let end = start.saturating_add(per_page).min(items.len());
        let rendered: Vec<R> = items[start..end].iter().map(render).collect();
        let reply = Reply::json(&rendered)?;
        let last = items.len().div_ceil(per_page).max(1);
        match self.link_header(page, last) {
            Some(link) => reply.header(LINK, &link),
            None => Ok(reply),
        }
    }

    /// GitHub's `Link` header for page `page` of `last`, or `None` for a single page.
    #[must_use]
    pub fn link_header(&self, page: usize, last: usize) -> Option<String> {
        if last <= 1 && page <= 1 {
            return None;
        }
        let mut rels: Vec<(usize, &str)> = Vec::with_capacity(4);
        if page > 1 {
            rels.push((page - 1, "prev"));
        }
        if page < last {
            rels.push((page + 1, "next"));
            rels.push((last, "last"));
        }
        if page > 1 {
            rels.push((1, "first"));
        }
        let mut out = String::new();
        for (i, (target, rel)) in rels.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "<{}{}?", self.p.urls.api, self.p.path);
            for (name, value) in self.p.query.iter().filter(|(n, _)| *n != "page") {
                let _ = write!(
                    out,
                    "{}={}&",
                    utf8_percent_encode(name, QUERY_VALUE),
                    utf8_percent_encode(value, QUERY_VALUE)
                );
            }
            let _ = write!(out, "page={target}>; rel=\"{rel}\"");
        }
        Some(out)
    }
}

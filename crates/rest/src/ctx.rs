use std::fmt::Write as _;
use std::sync::Arc;

use bytes::Bytes;
use http::HeaderMap;
use http::header::LINK;
use hubdouble_auth::Actor;
use hubdouble_git::{Git, ReadJobs};
use hubdouble_model::{ApiError, Clock, QueryTime, Repo, Timestamp};
use hubdouble_render::{Urls, query_escape};
use hubdouble_store::{Tx, TxMut};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::params::{Params, Query, ruby_integer};
use crate::reply::Reply;

const OVERFLOW_DOCS: &str = "https://docs.github.com/rest/guides/using-pagination-in-the-rest-api";

/// How deep `page` reaches into one list: GitHub answers `422` for a deeper page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageLimit {
    /// The most results GitHub serves: a page is refused when
    /// `page * (per_page + lookahead)` passes it, and [`Ctx::page_within`] counts at most this
    /// many results for the last page.
    pub results: usize,
    /// `1` for a list that reads one result past the page to see whether another page follows.
    pub lookahead: usize,
    pub message: &'static str,
    pub documentation_url: &'static str,
    /// Whether GitHub checks this limit before the limit of `page * per_page` to `i64::MAX`.
    pub before_overflow: bool,
}

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
    /// The request's own collector, which the pipeline runs after the handler.
    pub read_jobs: &'a ReadJobs,
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
    pub fn read_jobs(&self) -> &ReadJobs {
        self.p.read_jobs
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

    /// The `since` of a list request, read by [`Timestamp::parse_query`]; an empty value is none.
    ///
    /// # Errors
    ///
    /// `422` with `docs` when Ruby cannot read the time or SQL `DATETIME` cannot hold it.
    pub fn since(&self, docs: &str) -> Result<Option<QueryTime>, ApiError> {
        self.p
            .query
            .get("since")
            .filter(|s| !s.is_empty())
            .map(|s| {
                Timestamp::parse_query(s, self.now())
                    .ok()
                    .filter(|t| t.at.is_datetime())
                    .ok_or_else(|| {
                        ApiError::unprocessable(
                            "The since parameter needs to be in ISO 8601 format: YYYY-MM-DDTHH:MM:SSZ",
                        )
                        .with_docs(docs)
                    })
            })
            .transpose()
    }

    /// The `per_page` of a list request, without the checks of [`Ctx::page_params`], for a list
    /// that pages by cursor only.
    #[must_use]
    pub fn per_page(&self) -> usize {
        self.sized_per_page(30, 100)
    }

    fn sized_per_page(&self, default: usize, most: usize) -> usize {
        match self.p.query.get("per_page").and_then(ruby_integer) {
            Some(n) if n > 0 => usize::try_from(n).map_or(most, |n| n.min(most)),
            _ => default,
        }
    }

    /// The `page` and `per_page` of a list request.
    ///
    /// # Errors
    ///
    /// `422` when `page * per_page` passes `i64::MAX`, as GitHub refuses it on every list.
    pub fn page_params(&self) -> Result<Page, ApiError> {
        self.checked_page(None, 30, 100)
    }

    /// The `page` and `per_page` of a list request that GitHub serves only to `limit`.
    ///
    /// # Errors
    ///
    /// `422` with the list's own answer for a page past `limit`, and as [`Ctx::page_params`].
    pub fn page_params_within(&self, limit: &PageLimit) -> Result<Page, ApiError> {
        self.checked_page(Some(limit), 30, 100)
    }

    /// [`Ctx::page_params`] of a list whose `per_page` defaults to `default` and stops at `most`.
    ///
    /// # Errors
    ///
    /// As [`Ctx::page_params`].
    pub fn page_params_sized(&self, default: usize, most: usize) -> Result<Page, ApiError> {
        self.checked_page(None, default, most)
    }

    fn checked_page(
        &self,
        limit: Option<&PageLimit>,
        default: usize,
        most: usize,
    ) -> Result<Page, ApiError> {
        let page = self
            .p
            .query
            .get("page")
            .and_then(ruby_integer)
            .and_then(|n| u128::try_from(n).ok())
            .map_or(1, |n| n.max(1));
        let per_page = self.sized_per_page(default, most);
        let past = |l: &PageLimit| {
            let width = u128::try_from(per_page + l.lookahead).unwrap_or(u128::MAX);
            page.saturating_mul(width) > u128::try_from(l.results).unwrap_or(u128::MAX)
        };
        let refuse =
            |l: &PageLimit| ApiError::unprocessable(l.message).with_docs(l.documentation_url);
        if let Some(l) = limit.filter(|l| l.before_overflow && past(l)) {
            return Err(refuse(l));
        }
        let width = u128::try_from(per_page).unwrap_or(u128::MAX);
        if page.saturating_mul(width) > i64::MAX.unsigned_abs().into() {
            return Err(ApiError::unprocessable(format!(
                "Pagination exceeds maximum limit of {}",
                i64::MAX
            ))
            .with_docs(OVERFLOW_DOCS));
        }
        if let Some(l) = limit.filter(|l| past(l)) {
            return Err(refuse(l));
        }
        Ok(Page {
            page: usize::try_from(page).unwrap_or(usize::MAX),
            per_page,
        })
    }

    /// One page of `items` as a JSON array with GitHub's `Link` header; an empty page is `[]`.
    ///
    /// # Errors
    ///
    /// `500` when an item cannot be serialized, and as [`Ctx::page_params`].
    pub fn page<T, R: Serialize>(
        &self,
        items: &[T],
        render: impl FnMut(&T) -> R,
    ) -> Result<Reply, ApiError> {
        let page = self.page_params()?;
        self.page_of(page, items, render)
    }

    /// [`Ctx::page`] of a list that GitHub serves only to `limit`.
    ///
    /// # Errors
    ///
    /// As [`Ctx::page`] and [`Ctx::page_params_within`].
    pub fn page_within<T, R: Serialize>(
        &self,
        limit: &PageLimit,
        items: &[T],
        render: impl FnMut(&T) -> R,
    ) -> Result<Reply, ApiError> {
        let page = self.page_params_within(limit)?;
        self.page_of(page, &items[..items.len().min(limit.results)], render)
    }

    fn page_of<T, R: Serialize>(
        &self,
        Page { page, per_page }: Page,
        items: &[T],
        render: impl FnMut(&T) -> R,
    ) -> Result<Reply, ApiError> {
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

    /// GitHub's `Link` header for page `page` of `last`, or `None` for a single page. A page past
    /// the last has `prev` and `last` at the last page.
    #[must_use]
    pub fn link_header(&self, page: usize, last: usize) -> Option<String> {
        if last <= 1 && page <= 1 {
            return None;
        }
        let mut rels: Vec<(usize, &str)> = Vec::with_capacity(4);
        if page > 1 {
            rels.push(((page - 1).min(last), "prev"));
        }
        if page < last {
            rels.push((page + 1, "next"));
        }
        if page != last {
            rels.push((last, "last"));
        }
        if page > 1 {
            rels.push((1, "first"));
        }
        Some(self.links(&rels))
    }

    /// A `Link` header with one entry for each `(page, rel)`, in order.
    #[must_use]
    pub fn links(&self, rels: &[(usize, &str)]) -> String {
        let mut out = String::new();
        for (i, (target, rel)) in rels.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "<{}{}?", self.p.urls.api, self.p.path);
            out.push_str(&self.link_query("page", &target.to_string()));
            let _ = write!(out, ">; rel=\"{rel}\"");
        }
        out
    }

    /// The request's query as GitHub writes it into a `Link` header: each name once at its first
    /// place with its last value, `name` set to `value` in its place or added at the end.
    #[must_use]
    pub fn link_query(&self, name: &str, value: &str) -> String {
        let mut out = String::new();
        let mut placed = false;
        for (n, v) in self.p.query.distinct() {
            let v = if n == name {
                placed = true;
                value
            } else {
                v
            };
            let _ = write!(out, "{}={}&", query_escape(n), query_escape(v));
        }
        if !placed {
            let _ = write!(out, "{}={}&", query_escape(name), query_escape(value));
        }
        out.pop();
        out
    }
}

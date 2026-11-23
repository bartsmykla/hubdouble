use std::fmt::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use http::header::{self, HeaderName, HeaderValue};
use http::request::Parts as RequestParts;
use http::{HeaderMap, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hubdouble_admin::{Admin, Fault, FaultQuery, LogEntry, Services};
use hubdouble_auth::{Actor, Target};
use hubdouble_model::{Account, AccountKind, ApiError, FieldError, Repo};
use hubdouble_render::Urls;
use hubdouble_rest::{Access, Ctx, Entry, Params, Parts, Query, Reply};
use hubdouble_store::{Accounts, Repos, Tx};
use hyper::body::Incoming;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const VERSIONS: [&str; 2] = ["2022-11-28", "2026-03-10"];
const RATE_LIMIT: u32 = 5000;
const GRAPHQL_AUTH_DOCS: &str =
    "https://docs.github.com/graphql/guides/forming-calls-with-graphql#authenticating-with-graphql";

pub(crate) type HttpResponse = Response<Full<Bytes>>;

pub(crate) struct Inner {
    pub(crate) services: Arc<Services>,
    rest: hubdouble_rest::Registry,
    gql: hubdouble_gql::Registry,
    admin: Admin,
    root_urls: Urls,
    v3_urls: Urls,
    request_seq: AtomicU64,
}

#[derive(Default)]
struct Who {
    login: Option<String>,
    scopes: Option<String>,
}

struct Call<'r> {
    operation_id: &'static str,
    resource: &'static str,
    version: &'static str,
    result: Result<Reply, ApiError>,
    who: Who,
    parts: &'r RequestParts,
}

impl Inner {
    pub(crate) fn new(
        services: Arc<Services>,
        rest: hubdouble_rest::Registry,
        gql: hubdouble_gql::Registry,
        admin: Admin,
    ) -> Inner {
        Inner {
            root_urls: Urls::new(&services.config, ""),
            v3_urls: Urls::new(&services.config, "/api/v3"),
            services,
            rest,
            gql,
            admin,
            request_seq: AtomicU64::new(0),
        }
    }

    pub(crate) async fn handle(&self, req: Request<Incoming>) -> HttpResponse {
        let path = req.uri().path();
        if path == "/healthz" || path == "/readyz" {
            return plain(StatusCode::OK, "ok");
        }
        if path == "/_hubdouble" || path.starts_with("/_hubdouble/") {
            return self.admin.serve(req, self.services.clone()).await;
        }
        if let Some(id) = path.strip_prefix("/_blobs/") {
            let id = id.to_owned();
            return crate::blobs::serve(&self.services.store, req.method(), req.headers(), &id);
        }
        let (parts, body) = req.into_parts();
        let (v3, api_path) = split_prefix(parts.uri.path());
        let graphql = parts.method == Method::POST
            && (api_path == "/graphql" || parts.uri.path() == "/api/graphql");
        let urls = if v3 || graphql && parts.uri.path() != "/graphql" {
            &self.v3_urls
        } else {
            &self.root_urls
        };
        let resource = if graphql { "graphql" } else { "core" };
        let version = match api_version(&parts.headers) {
            Ok(v) => v,
            Err(e) => return self.finish(error_call(&parts, "", resource, "", e)),
        };
        let body = match Limited::new(body, self.services.config.max_body_bytes)
            .collect()
            .await
        {
            Ok(b) => b.to_bytes(),
            Err(e) if e.downcast_ref::<LengthLimitError>().is_some() => {
                let e = ApiError::new(413, "Payload Too Large");
                return self.finish(error_call(&parts, "", resource, version, e));
            }
            Err(_) => {
                let e = ApiError::bad_request("Problems reading the request body");
                return self.finish(error_call(&parts, "", resource, version, e));
            }
        };
        if graphql {
            return self.graphql(&parts, &body, urls, version).await;
        }
        self.rest(&parts, api_path, &body, urls, version).await
    }

    async fn rest(
        &self,
        parts: &RequestParts,
        api_path: &str,
        body: &Bytes,
        urls: &Urls,
        version: &'static str,
    ) -> HttpResponse {
        let canonical = match self.canonical_path(api_path) {
            Ok(canonical) => canonical,
            Err(e) => return self.finish(error_call(parts, "", "core", version, e)),
        };
        let api_path = canonical.as_deref().unwrap_or(api_path);
        let Some(route) = self.rest.route(&parts.method, api_path) else {
            return self.finish(error_call(
                parts,
                "",
                "core",
                version,
                ApiError::not_found(),
            ));
        };
        let operation_id = route.operation_id;
        if let Some(fault) = self.fault(parts, operation_id).await {
            return self.faulted(parts, operation_id, "core", version, fault);
        }
        let Some(entry) = route.entry else {
            let e = ApiError::not_implemented(operation_id);
            return self.finish(error_call(parts, operation_id, "core", version, e));
        };
        let query = Query::parse(parts.uri.query().unwrap_or(""));
        let request = RestRequest {
            entry,
            api_path,
            params: &route.params,
            query: &query,
            headers: &parts.headers,
            body,
            urls,
        };
        let mut who = Who::default();
        let read = (parts.method == Method::GET || parts.method == Method::HEAD) && !entry.writes;
        let store = &self.services.store;
        let result = catch_unwind(AssertUnwindSafe(|| {
            if read {
                store.view(|tx| self.run(&request, Access::Read(tx), &mut who))
            } else {
                store.update(|tx| self.run(&request, Access::Write(tx.reborrow()), &mut who))
            }
        }))
        .unwrap_or_else(|_| Err(ApiError::internal("Internal Server Error")));
        self.finish(Call {
            operation_id,
            resource: "core",
            version,
            result,
            who,
            parts,
        })
    }

    fn canonical_path(&self, api_path: &str) -> Result<Option<String>, ApiError> {
        let (alias, rest) = if let Some(rest) = api_path.strip_prefix("/repositories/") {
            (Alias::Repo, rest)
        } else if let Some(rest) = api_path.strip_prefix("/organizations/") {
            (Alias::Org, rest)
        } else {
            return Ok(None);
        };
        let Some((id, tail)) = rest.split_once('/') else {
            return Ok(None);
        };
        let id: i64 = id.parse().map_err(|_| ApiError::not_found())?;
        self.services.store.view(|tx| {
            let accounts = tx.table::<Accounts>();
            let path = match alias {
                Alias::Repo => {
                    let repo = tx
                        .table::<Repos>()
                        .get(id)
                        .ok_or_else(ApiError::not_found)?;
                    let owner = accounts
                        .get(repo.owner_id)
                        .ok_or_else(ApiError::not_found)?;
                    format!("/repos/{}/{}/{tail}", owner.login, repo.name)
                }
                Alias::Org => {
                    let org = accounts
                        .get(id)
                        .filter(|a| a.kind == AccountKind::Organization)
                        .ok_or_else(ApiError::not_found)?;
                    format!("/orgs/{}/{tail}", org.login)
                }
            };
            Ok(Some(path))
        })
    }

    fn run(
        &self,
        r: &RestRequest<'_>,
        access: Access<'_>,
        who: &mut Who,
    ) -> Result<Reply, ApiError> {
        let tx: &Tx = match &access {
            Access::Read(tx) => tx,
            Access::Write(tx) => tx,
        };
        let services = &self.services;
        let actor = if r.entry.authenticates {
            hubdouble_auth::Actor::Anonymous
        } else {
            hubdouble_auth::authenticate(tx, services.clock.as_ref(), r.headers)?
        };
        who.login = actor.login().map(str::to_owned);
        who.scopes = actor.oauth_scopes().map(|s| s.join(", "));
        let resolved = resolve_target(tx, r.params)?;
        let (target, repo) = match &resolved {
            Resolved::Repo(repo) => (Target::Repo(repo), Some(repo)),
            Resolved::Org(org) => (Target::Org(org), None),
            Resolved::None => (Target::None, None),
        };
        hubdouble_auth::authorize(tx, &actor, &r.entry.requirement, target)?;
        let link_path = link_path(r.api_path, &resolved);
        let mut ctx = Ctx::new(Parts {
            operation_id: r.entry.operation_id,
            path: link_path.as_deref().unwrap_or(r.api_path),
            params: r.params,
            query: r.query,
            headers: r.headers,
            body: r.body,
            actor: &actor,
            repo,
            git: &services.git,
            access,
            clock: services.clock.as_ref(),
            urls: r.urls,
        });
        (r.entry.handler)(&mut ctx)
    }

    async fn graphql(
        &self,
        parts: &RequestParts,
        body: &Bytes,
        urls: &Urls,
        version: &'static str,
    ) -> HttpResponse {
        let Ok(request) = serde_json::from_slice::<GraphqlBody>(body) else {
            let e = ApiError::bad_request("Problems parsing JSON");
            return self.finish(error_call(parts, "graphql", "graphql", version, e));
        };
        let operation = request.operation_name.as_deref().unwrap_or("graphql");
        if let Some(fault) = self.fault(parts, operation).await {
            return self.faulted(parts, "graphql", "graphql", version, fault);
        }
        let services = &self.services;
        let mut who = Who::default();
        let variables = request.variables.unwrap_or_default();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let actor = services.store.view(|tx| {
                hubdouble_auth::authenticate(tx, services.clock.as_ref(), &parts.headers)
            })?;
            who.login = actor.login().map(str::to_owned);
            who.scopes = actor.oauth_scopes().map(|s| s.join(", "));
            if matches!(actor, Actor::Anonymous | Actor::App { .. }) {
                return Err(
                    ApiError::new(401, "This endpoint requires you to be authenticated.")
                        .with_docs(GRAPHQL_AUTH_DOCS),
                );
            }
            let response = hubdouble_gql::execute(
                &self.gql,
                &services.store,
                &services.git,
                &actor,
                hubdouble_gql::Request {
                    query: &request.query,
                    operation_name: request.operation_name.as_deref(),
                    variables: &variables,
                    clock: services.clock.as_ref(),
                    urls,
                },
            )?;
            Reply::json(&response)
        }))
        .unwrap_or_else(|_| Err(ApiError::internal("Internal Server Error")));
        self.finish(Call {
            operation_id: "graphql",
            resource: "graphql",
            version,
            result,
            who,
            parts,
        })
    }

    async fn fault(&self, parts: &RequestParts, operation: &str) -> Option<Fault> {
        let fault = self.admin.fault(&FaultQuery {
            method: parts.method.as_str(),
            path: parts.uri.path(),
            operation,
        })?;
        if !fault.delay.is_zero() {
            tokio::time::sleep(fault.delay).await;
        }
        Some(fault)
    }

    fn faulted(
        &self,
        parts: &RequestParts,
        operation_id: &str,
        resource: &'static str,
        version: &'static str,
        fault: Fault,
    ) -> HttpResponse {
        let status =
            StatusCode::from_u16(fault.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut reply = Reply::raw(hubdouble_rest::JSON, fault.body).status(status);
        self.common_headers(&mut reply.headers, &parts.headers, resource, Some(version));
        for (name, value) in &fault.headers {
            if let (Ok(n), Ok(v)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                reply.headers.insert(n, v);
            }
        }
        self.record(parts, operation_id, None, status);
        to_response(reply, parts.method == Method::HEAD)
    }

    fn finish(&self, call: Call<'_>) -> HttpResponse {
        let Call {
            operation_id,
            resource,
            version,
            result,
            who,
            parts,
        } = call;
        let mut reply = result.unwrap_or_else(|e| error_reply(&e));
        let selected = (!version.is_empty()).then_some(version);
        let media = self.common_headers(&mut reply.headers, &parts.headers, resource, selected);
        if let Some(scopes) = &who.scopes {
            set(&mut reply.headers, "x-oauth-scopes", scopes);
        }
        let conditional = parts.method == Method::GET || parts.method == Method::HEAD;
        if conditional && reply.status == StatusCode::OK {
            let tag = if let Some(t) = reply.headers.get(header::ETAG) {
                t.clone()
            } else {
                let tag = weak_etag(&media, &reply.body);
                let value =
                    HeaderValue::from_str(&tag).unwrap_or(HeaderValue::from_static("W/\"\""));
                reply.headers.insert(header::ETAG, value.clone());
                value
            };
            if if_none_match(&parts.headers, &tag) {
                reply.status = StatusCode::NOT_MODIFIED;
                reply.body = Bytes::new();
                reply.headers.remove(header::CONTENT_TYPE);
            }
        }
        self.record(parts, operation_id, who.login.as_deref(), reply.status);
        to_response(reply, parts.method == Method::HEAD)
    }

    fn record(
        &self,
        parts: &RequestParts,
        operation_id: &str,
        actor: Option<&str>,
        status: StatusCode,
    ) {
        self.admin.record(&LogEntry {
            method: parts.method.as_str(),
            path: parts.uri.path(),
            operation_id,
            actor,
            status: status.as_u16(),
        });
    }

    fn common_headers(
        &self,
        out: &mut HeaderMap,
        request: &HeaderMap,
        resource: &str,
        selected: Option<&str>,
    ) -> String {
        let seq = self.request_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let now = self.services.clock.now();
        let media = media_type(request.get(header::ACCEPT).and_then(|v| v.to_str().ok()));
        set(
            out,
            "x-github-request-id",
            &format!("0000:{seq:04X}:{seq:06X}:{seq:06X}:{:08X}", now.unix()),
        );
        set(out, "x-github-media-type", &media);
        set(out, "x-ratelimit-limit", &RATE_LIMIT.to_string());
        set(out, "x-ratelimit-remaining", &(RATE_LIMIT - 1).to_string());
        set(out, "x-ratelimit-reset", &(now.unix() + 3600).to_string());
        set(out, "x-ratelimit-used", "1");
        set(out, "x-ratelimit-resource", resource);
        if let Some(v) = selected {
            set(out, "x-github-api-version-selected", v);
        }
        media
    }
}

struct RestRequest<'r> {
    entry: &'r Entry,
    api_path: &'r str,
    params: &'r Params,
    query: &'r Query,
    headers: &'r HeaderMap,
    body: &'r Bytes,
    urls: &'r Urls,
}

#[derive(Deserialize)]
struct GraphqlBody {
    query: String,
    #[serde(rename = "operationName")]
    operation_name: Option<String>,
    variables: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    message: &'a str,
    #[serde(skip_serializing_if = "Errors::is_empty")]
    errors: Errors<'a>,
    documentation_url: &'a str,
    status: String,
}

struct Errors<'a>(&'a [FieldError]);

impl Errors<'_> {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Serialize for Errors<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq as _;
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for e in self.0 {
            match &e.message {
                Some(text) if e.resource.is_empty() && e.field.is_empty() && e.code.is_empty() => {
                    seq.serialize_element(text)?;
                }
                _ => seq.serialize_element(e)?,
            }
        }
        seq.end()
    }
}

fn error_call<'r>(
    parts: &'r RequestParts,
    operation_id: &'static str,
    resource: &'static str,
    version: &'static str,
    e: ApiError,
) -> Call<'r> {
    Call {
        operation_id,
        resource,
        version,
        result: Err(e),
        who: Who::default(),
        parts,
    }
}

pub(crate) fn error_reply(e: &ApiError) -> Reply {
    let body = ErrorBody {
        message: &e.message,
        errors: Errors(&e.errors),
        documentation_url: &e.documentation_url,
        status: e.status.to_string(),
    };
    let status = StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut reply = Reply::raw(
        hubdouble_rest::JSON,
        serde_json::to_vec(&body).unwrap_or_default(),
    )
    .status(status);
    for (name, value) in &e.headers {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            reply.headers.append(n, v);
        }
    }
    reply
}

enum Resolved {
    None,
    Repo(Arc<Repo>),
    Org(Arc<Account>),
}

fn resolve_target(tx: &Tx, params: &Params) -> Result<Resolved, ApiError> {
    let accounts = tx.table::<Accounts>();
    if let (Some(owner), Some(name)) = (params.get("owner"), params.get("repo")) {
        let repo = accounts
            .by_login(owner)
            .and_then(|o| tx.table::<Repos>().by_name(o.id, name))
            .ok_or_else(ApiError::not_found)?;
        return Ok(Resolved::Repo(repo.clone()));
    }
    if let Some(id) = params.get("repository_id") {
        let repo = id
            .parse::<i64>()
            .ok()
            .and_then(|id| tx.table::<Repos>().get(id))
            .ok_or_else(ApiError::not_found)?;
        return Ok(Resolved::Repo(repo.clone()));
    }
    if let Some(org) = params.get("org") {
        let org = accounts.by_login(org).ok_or_else(ApiError::not_found)?;
        return Ok(Resolved::Org(org.clone()));
    }
    Ok(Resolved::None)
}

enum Alias {
    Repo,
    Org,
}

fn link_path(api_path: &str, resolved: &Resolved) -> Option<String> {
    let (prefix, skip, alias, id) = match resolved {
        Resolved::Repo(repo) => ("/repos/", 2, "/repositories/", repo.id),
        Resolved::Org(org) => ("/orgs/", 1, "/organizations/", org.id),
        Resolved::None => return None,
    };
    let rest = api_path.strip_prefix(prefix)?;
    Some(match rest.splitn(skip + 1, '/').nth(skip) {
        Some(tail) => format!("{alias}{id}/{tail}"),
        None => format!("{alias}{id}"),
    })
}

fn split_prefix(path: &str) -> (bool, &str) {
    match path.strip_prefix("/api/v3") {
        Some("") => (true, "/"),
        Some(rest) if rest.starts_with('/') => (true, rest),
        _ => (false, path),
    }
}

fn api_version(headers: &HeaderMap) -> Result<&'static str, ApiError> {
    let Some(v) = headers.get("x-github-api-version") else {
        return Ok(VERSIONS[0]);
    };
    VERSIONS
        .iter()
        .find(|known| v.as_bytes() == known.as_bytes())
        .copied()
        .ok_or_else(|| {
            ApiError::bad_request(format!(
                "Unsupported 'X-GitHub-Api-Version' header: {}. Supported versions are: {}",
                String::from_utf8_lossy(v.as_bytes()),
                VERSIONS.join(", ")
            ))
        })
}

pub(crate) fn media_type(accept: Option<&str>) -> String {
    let range = accept
        .unwrap_or("")
        .split(',')
        .filter_map(|r| r.split(';').next())
        .map(str::trim)
        .find_map(|r| r.strip_prefix("application/vnd.github"));
    let Some(range) = range else {
        return "github.v3; format=json".to_owned();
    };
    let (names, format) = match range.split_once('+') {
        Some((n, f)) => (n, Some(f)),
        None => (range, None),
    };
    let mut names = names.split('.').filter(|s| !s.is_empty()).peekable();
    let version = names
        .next_if(|n| {
            n.strip_prefix('v')
                .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        })
        .unwrap_or("v3");
    let param = names.next();
    let mut out = format!("github.{version}");
    if let Some(p) = param {
        let _ = write!(out, "; param={p}");
    }
    match (param, format) {
        (_, Some(f)) => {
            let _ = write!(out, "; format={f}");
        }
        (None, None) => out.push_str("; format=json"),
        (Some(_), None) => {}
    }
    out
}

pub(crate) fn weak_etag(media: &str, body: &[u8]) -> String {
    let digest = Sha256::new()
        .chain_update(media.as_bytes())
        .chain_update(body)
        .finalize();
    let mut tag = String::with_capacity(70);
    tag.push_str("W/\"");
    for b in digest {
        let _ = write!(tag, "{b:02x}");
    }
    tag.push('"');
    tag
}

fn if_none_match(headers: &HeaderMap, tag: &HeaderValue) -> bool {
    let strip = |t: &str| t.trim().trim_start_matches("W/").to_owned();
    let Ok(tag) = tag.to_str() else {
        return false;
    };
    let tag = strip(tag);
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|t| t.trim() == "*" || strip(t) == tag)
}

fn set(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        headers.insert(HeaderName::from_static(name), v);
    }
}

pub(crate) fn plain(status: StatusCode, body: &'static str) -> HttpResponse {
    let mut response = Response::new(Full::new(Bytes::from_static(body.as_bytes())));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

pub(crate) fn to_response(reply: Reply, head: bool) -> HttpResponse {
    let Reply {
        status,
        mut headers,
        body,
    } = reply;
    let body = if head {
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
        Bytes::new()
    } else {
        body
    };
    let mut response = Response::new(Full::new(body));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn media_type_follows_the_accept_header() {
        for (accept, want) in [
            (None, "github.v3; format=json"),
            (Some("application/json"), "github.v3; format=json"),
            (
                Some("application/vnd.github+json"),
                "github.v3; format=json",
            ),
            (
                Some("application/vnd.github.v3.sha"),
                "github.v3; param=sha",
            ),
            (
                Some("application/vnd.github.raw+json"),
                "github.v3; param=raw; format=json",
            ),
            (
                Some("text/html, application/vnd.github.v3.diff"),
                "github.v3; param=diff",
            ),
        ] {
            assert_eq!(media_type(accept), want, "{accept:?}");
        }
    }

    #[test]
    fn the_etag_changes_with_the_media_type_and_the_body() {
        let a = weak_etag("github.v3; format=json", b"{}");
        assert!(a.starts_with("W/\""));
        assert_eq!(a, weak_etag("github.v3; format=json", b"{}"));
        assert_ne!(a, weak_etag("github.v3; param=raw", b"{}"));
        assert_ne!(a, weak_etag("github.v3; format=json", b"[]"));
    }

    #[test]
    fn the_api_prefix_is_split_off() {
        assert_eq!(split_prefix("/api/v3"), (true, "/"));
        assert_eq!(split_prefix("/api/v3/repos/o/r"), (true, "/repos/o/r"));
        assert_eq!(split_prefix("/api/v3x"), (false, "/api/v3x"));
        assert_eq!(split_prefix("/repos/o/r"), (false, "/repos/o/r"));
    }
}

use std::fmt::Write as _;
use std::hash::{BuildHasher as _, Hash as _, Hasher as _};
use std::io::Write as _;
use std::net::IpAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use flate2::Compression;
use flate2::write::GzEncoder;
use foldhash::fast::{FixedState, FoldHasher};
use http::header::{self, HeaderName, HeaderValue};
use http::request::Parts as RequestParts;
use http::{HeaderMap, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hubdouble_admin::{Admin, Fault, FaultQuery, LogEntry, Services};
use hubdouble_auth::{Actor, Target};
use hubdouble_git::ReadJobs;
use hubdouble_model::{Account, AccountKind, ApiError, FieldError, Repo, Scope, Timestamp};
use hubdouble_render::Urls;
use hubdouble_rest::{Access, Ctx, Entry, Params, Parts, Query, Reply};
use hubdouble_store::{Accounts, RepoRedirects, Repos, Tx};
use hyper::body::Incoming;
use serde::{Deserialize, Serialize};

const VERSIONS: [&str; 2] = ["2022-11-28", "2026-03-10"];
const ARCHIVED_WRITES: [&str; 7] = [
    "repos/update",
    "repos/delete",
    "repos/create-fork",
    "activity/set-repo-subscription",
    "activity/delete-repo-subscription",
    "activity/star-repo-for-authenticated-user",
    "activity/unstar-repo-for-authenticated-user",
];
const RATE_LIMIT: u32 = 5000;
const MEDIA_DOCS: &str = "https://docs.github.com/v3/media";
const RATE_LIMIT_DOCS: &str =
    "https://docs.github.com/rest/overview/resources-in-the-rest-api#rate-limiting";
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
    authenticated: bool,
}

impl Who {
    fn of(&mut self, actor: &Actor) {
        self.login = actor.login().map(str::to_owned);
        self.scopes = actor.oauth_scopes().map(|s| s.join(", "));
        self.authenticated = !matches!(actor, Actor::Anonymous);
    }
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

    pub(crate) async fn handle(&self, req: Request<Incoming>, peer: IpAddr) -> HttpResponse {
        let path = req.uri().path();
        if path == "/healthz" || path == "/readyz" {
            return plain(StatusCode::OK, "ok");
        }
        if path == "/_hubdouble" || path.starts_with("/_hubdouble/") {
            return self.admin.serve(req, self.services.clone()).await;
        }
        if let Some(id) = path.strip_prefix("/_blobs/") {
            let id = id.to_owned();
            return crate::blobs::serve(
                &self.services,
                req.method(),
                req.uri().query(),
                req.headers(),
                &id,
            );
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
        let graphql_read = parts.method == Method::GET && api_path == "/graphql";
        if (graphql || graphql_read) && self.anonymous(&parts.headers) {
            return self.graphql_rate_limited(&parts, peer);
        }
        if graphql_read {
            return self.graphql_schema(&parts);
        }
        let resource = if graphql { "graphql" } else { "core" };
        let version = if graphql {
            ""
        } else {
            match api_version(&parts.headers) {
                Ok(v) => v,
                Err(e) => return self.finish(error_call(&parts, "", resource, "", e)),
            }
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
        let read = (parts.method == Method::GET || parts.method == Method::HEAD) && !entry.writes;
        let request = RestRequest {
            entry,
            method: &parts.method,
            api_path,
            raw_query: parts.uri.query(),
            params: &route.params,
            query: &query,
            headers: &parts.headers,
            body,
            urls,
        };
        let mut who = Who::default();
        let store = &self.services.store;
        let result = catch_unwind(AssertUnwindSafe(|| {
            let jobs = ReadJobs::default();
            let reply = if read {
                store.view(|tx| self.run(&request, Access::Read(tx), &jobs, &mut who))?
            } else {
                store.update(|tx| {
                    self.run(&request, Access::Write(tx.reborrow()), &jobs, &mut who)
                })?
            };
            jobs.run(&self.services.git, store)?;
            Ok(reply)
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
        read_jobs: &ReadJobs,
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
        who.of(&actor);
        let served: &[&str] = if r.entry.operation_id == "repos/get-release-asset" {
            &["application/octet-stream"]
        } else {
            &[]
        };
        if let Some(unsupported) = unsupported_accept(r.headers, served) {
            if matches!(actor, Actor::Anonymous) && !r.entry.authenticates {
                let requirement = &r.entry.requirement;
                if matches!(
                    requirement.scope,
                    Scope::User | Scope::App | Scope::Installation
                ) {
                    hubdouble_auth::authorize(tx, &actor, requirement, Target::None)?;
                } else if *r.method != Method::GET && *r.method != Method::HEAD {
                    return Err(ApiError::new(401, "Requires authentication"));
                }
            }
            return Err(unsupported);
        }
        let resolved = resolve_target(tx, r.params)?;
        let (target, repo) = match &resolved {
            Resolved::Moved(repo) if hubdouble_auth::can_see(tx, &actor, repo) => {
                return moved(r, &resolved);
            }
            Resolved::Moved(_) => return Err(ApiError::not_found()),
            Resolved::Repo(repo) => (Target::Repo(repo), Some(repo)),
            Resolved::Org(org) => (Target::Org(org), None),
            Resolved::None => (Target::None, None),
        };
        hubdouble_auth::authorize(tx, &actor, &r.entry.requirement, target)?;
        if let (Access::Write(_), Some(repo)) = (&access, repo)
            && repo.archived
            && !ARCHIVED_WRITES.contains(&r.entry.operation_id)
        {
            return Err(ApiError::new(403, hubdouble_auth::ARCHIVED_REST)
                .with_docs(r.entry.documentation_url));
        }
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
            read_jobs,
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
        let parsed = serde_json::from_slice::<GraphqlBody>(body).or_else(|e| {
            if e.classify() != serde_json::error::Category::Data {
                return Err(e);
            }
            serde_json::from_slice::<serde_json::Value>(body).and_then(GraphqlBody::deserialize)
        });
        let Ok(request) = parsed else {
            let e = ApiError::bad_request("Problems parsing JSON");
            return self.finish(error_call(parts, "graphql", "graphql", version, e));
        };
        let operation = request.operation_name.as_deref().unwrap_or("graphql");
        if let Some(fault) = self.fault(parts, operation).await {
            return self.faulted(parts, "graphql", "graphql", version, fault);
        }
        let services = &self.services;
        let mut who = Who::default();
        let variables = match request.variables {
            Some(given) if hubdouble_gql::may_hold_wide_ints(&given) => {
                exact_variables(body).unwrap_or(given)
            }
            given => given.unwrap_or_default(),
        };
        let result = catch_unwind(AssertUnwindSafe(|| {
            let actor = services.store.view(|tx| {
                hubdouble_auth::authenticate(tx, services.clock.as_ref(), &parts.headers)
            })?;
            who.of(&actor);
            if matches!(actor, Actor::Anonymous | Actor::App { .. }) {
                return Err(
                    ApiError::new(401, "This endpoint requires you to be authenticated.")
                        .with_docs(GRAPHQL_AUTH_DOCS),
                );
            }
            if let Some(unsupported) = unsupported_accept(&parts.headers, &[]) {
                return Err(unsupported);
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

    fn graphql_schema(&self, parts: &RequestParts) -> HttpResponse {
        let services = &self.services;
        let mut who = Who::default();
        let result = services
            .store
            .view(|tx| hubdouble_auth::authenticate(tx, services.clock.as_ref(), &parts.headers))
            .and_then(|actor| {
                who.of(&actor);
                if matches!(actor, Actor::Anonymous | Actor::App { .. }) {
                    return Err(ApiError::new(
                        401,
                        "This endpoint requires you to be authenticated.",
                    )
                    .with_docs(GRAPHQL_AUTH_DOCS));
                }
                if let Some(unsupported) = unsupported_accept(&parts.headers, &[]) {
                    return Err(unsupported);
                }
                Ok(Reply::raw(
                    hubdouble_rest::JSON,
                    self.gql.schema().introspection(),
                ))
            });
        self.finish(Call {
            operation_id: "graphql",
            resource: "graphql",
            version: "",
            result,
            who,
            parts,
        })
    }

    fn anonymous(&self, headers: &HeaderMap) -> bool {
        let services = &self.services;
        services
            .store
            .view(|tx| hubdouble_auth::authenticate(tx, services.clock.as_ref(), headers))
            .is_ok_and(|actor| matches!(actor, Actor::Anonymous))
    }

    fn graphql_rate_limited(&self, parts: &RequestParts, peer: IpAddr) -> HttpResponse {
        #[derive(Serialize)]
        struct Body<'a> {
            message: String,
            documentation_url: &'a str,
        }
        let body = Body {
            message: format!(
                "API rate limit exceeded for {}. (But here's the good news: Authenticated \
                 requests get a higher rate limit. Check out the documentation for more \
                 details.)",
                peer.to_canonical()
            ),
            documentation_url: RATE_LIMIT_DOCS,
        };
        let mut reply = Reply::raw(
            hubdouble_rest::JSON,
            serde_json::to_vec(&body).unwrap_or_default(),
        )
        .status(StatusCode::FORBIDDEN);
        self.common_headers(&mut reply.headers, &HeaderMap::new(), "graphql", None);
        set(
            &mut reply.headers,
            "x-github-media-type",
            "github.v3; format=json",
        );
        for name in [
            "x-ratelimit-limit",
            "x-ratelimit-remaining",
            "x-ratelimit-used",
        ] {
            set(&mut reply.headers, name, "0");
        }
        self.record(parts, "graphql", None, reply.status);
        to_response(reply, false)
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
        let selected = (!version.is_empty()).then_some(version);
        self.common_headers(&mut reply.headers, &parts.headers, resource, selected);
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
        if let Some(at) = reply.modified
            && let Ok(value) = HeaderValue::try_from(at.http_date())
        {
            reply.headers.insert(header::LAST_MODIFIED, value);
        }
        let selected = (!version.is_empty()).then_some(version);
        let media = self.common_headers(&mut reply.headers, &parts.headers, resource, selected);
        if let Some(scopes) = &who.scopes {
            set(&mut reply.headers, "x-oauth-scopes", scopes);
            if resource == "graphql" {
                set(&mut reply.headers, "x-accepted-oauth-scopes", "repo");
            }
        }
        let read = parts.method == Method::GET || parts.method == Method::HEAD;
        let mut tag = None;
        if read && reply.status == StatusCode::OK {
            let strong = match reply
                .headers
                .get(header::ETAG)
                .and_then(|t| t.to_str().ok())
            {
                Some(t) => t.trim_start_matches("W/").to_owned(),
                None => etag(&media, &reply.body),
            };
            if not_modified(&parts.headers, reply.modified, &strong) {
                reply.status = StatusCode::NOT_MODIFIED;
                reply.body = Bytes::new();
                if who.authenticated {
                    reply.headers.remove(header::CONTENT_TYPE);
                }
            }
            set(&mut reply.headers, "etag", &strong);
            tag = Some(strong);
        }
        cache_headers(&mut reply, operation_id, read, who.authenticated);
        if gzip(&mut reply, &parts.headers, operation_id)
            && let Some(strong) = tag
        {
            set(&mut reply.headers, "etag", &format!("W/{strong}"));
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
        let default = if resource == "graphql" { "v4" } else { "v3" };
        let media = media_type(
            request.get(header::ACCEPT).and_then(|v| v.to_str().ok()),
            default,
        );
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
    method: &'r Method,
    api_path: &'r str,
    raw_query: Option<&'r str>,
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

#[derive(Deserialize)]
struct RawVariables<'a> {
    #[serde(borrow)]
    variables: &'a serde_json::value::RawValue,
}

fn exact_variables(body: &[u8]) -> Option<serde_json::Map<String, serde_json::Value>> {
    let raw = serde_json::from_slice::<RawVariables<'_>>(body).ok()?;
    hubdouble_gql::exact_variables(raw.variables).ok()
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    message: &'a str,
    #[serde(skip_serializing_if = "Errors::is_empty")]
    errors: Errors<'a>,
    #[serde(skip_serializing_if = "str::is_empty")]
    documentation_url: &'a str,
    status: String,
}

struct Errors<'a>(&'a [FieldError], Option<&'a str>);

impl Errors<'_> {
    fn is_empty(&self) -> bool {
        self.0.is_empty() && self.1.is_none()
    }
}

impl Serialize for Errors<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq as _;
        if let Some(text) = self.1 {
            return s.serialize_str(text);
        }
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
        errors: Errors(&e.errors, e.errors_text.as_deref()),
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
    Moved(Arc<Repo>),
    Org(Arc<Account>),
}

fn resolve_target(tx: &Tx, params: &Params) -> Result<Resolved, ApiError> {
    let accounts = tx.table::<Accounts>();
    if let (Some(owner), Some(name)) = (params.get("owner"), params.get("repo")) {
        let repos = tx.table::<Repos>();
        if let Some(repo) = accounts
            .by_login(owner)
            .and_then(|o| repos.by_name(o.id, name))
        {
            return Ok(Resolved::Repo(repo.clone()));
        }
        let moved = tx
            .table::<RepoRedirects>()
            .get(owner, name)
            .and_then(|id| repos.get(id))
            .ok_or_else(ApiError::not_found)?;
        return Ok(Resolved::Moved(moved.clone()));
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

#[derive(Serialize)]
struct Redirect<'a> {
    message: &'a str,
    url: &'a str,
    documentation_url: &'a str,
}

fn moved(r: &RestRequest<'_>, resolved: &Resolved) -> Result<Reply, ApiError> {
    let path = link_path(r.api_path, resolved).ok_or_else(ApiError::not_found)?;
    let mut url = format!("{}{path}", r.urls.api);
    if let Some(query) = r.raw_query {
        url = format!("{url}?{query}");
    }
    let (status, message) = if *r.method == Method::GET || *r.method == Method::HEAD {
        (StatusCode::MOVED_PERMANENTLY, "Moved Permanently")
    } else {
        (StatusCode::TEMPORARY_REDIRECT, "Temporary Redirect")
    };
    let mut reply = Reply::json(&Redirect {
        message,
        url: &url,
        documentation_url: "https://docs.github.com/rest/guides/best-practices-for-using-the-rest-api#follow-redirects",
    })?;
    reply.status = status;
    reply.headers.extend(Reply::redirect(&url)?.headers);
    Ok(reply)
}

enum Alias {
    Repo,
    Org,
}

fn link_path(api_path: &str, resolved: &Resolved) -> Option<String> {
    let (prefix, skip, alias, id) = match resolved {
        Resolved::Repo(repo) | Resolved::Moved(repo) => ("/repos/", 2, "/repositories/", repo.id),
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

#[derive(Clone, Copy)]
struct Range<'a> {
    text: &'a str,
    media: &'a str,
    q: f32,
}

#[derive(PartialEq)]
enum Kind<'a> {
    Vendor(&'a str),
    Json,
    Any,
    Unsupported,
}

impl<'a> Range<'a> {
    fn kind(self) -> Kind<'a> {
        const VENDOR: &str = "application/vnd.github";
        let media = self.media;
        if let Some(rest) = media
            .get(..VENDOR.len())
            .filter(|p| p.eq_ignore_ascii_case(VENDOR))
            .and_then(|_| media.get(VENDOR.len()..))
        {
            Kind::Vendor(rest)
        } else if ["application/json", "text/*", "application/*"]
            .iter()
            .any(|m| media.eq_ignore_ascii_case(m))
        {
            Kind::Json
        } else if media == "*/*" {
            Kind::Any
        } else {
            Kind::Unsupported
        }
    }
}

fn ranges(accept: &str) -> impl Iterator<Item = Range<'_>> + Clone {
    accept
        .split(',')
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| {
            let mut params = text.split(';');
            let media = params.next().unwrap_or_default().trim();
            let q = params
                .filter_map(|p| p.split_once('='))
                .find(|(name, _)| name.trim().eq_ignore_ascii_case("q"))
                .and_then(|(_, value)| value.trim().parse().ok())
                .unwrap_or(1.0);
            Range { text, media, q }
        })
}

fn unsupported_accept(headers: &HeaderMap, served: &[&str]) -> Option<ApiError> {
    let accept = headers.get(header::ACCEPT)?.to_str().ok()?;
    if ranges(accept).next().is_none()
        || ranges(accept).any(|r| {
            r.kind() != Kind::Unsupported || served.iter().any(|m| r.media.eq_ignore_ascii_case(m))
        })
    {
        return None;
    }
    let mut sorted: Vec<Range<'_>> = ranges(accept).collect();
    sorted.sort_by(|a, b| b.q.total_cmp(&a.q));
    let names: Vec<&str> = sorted.iter().map(|r| r.text).collect();
    Some(
        ApiError::new(
            415,
            format!(
                "Unsupported 'Accept' header: '{}'. Must accept 'application/json'.",
                names.join(", ")
            ),
        )
        .with_docs(MEDIA_DOCS),
    )
}

pub(crate) fn media_type(accept: Option<&str>, default: &str) -> String {
    let accept = accept.unwrap_or("");
    let first = ranges(accept).fold(None, |best: Option<Range<'_>>, r| match best {
        Some(b) if b.q >= r.q => Some(b),
        _ => Some(r),
    });
    let Some(first) = first else {
        return format!("github.{default}; format=json");
    };
    let vendor = ranges(accept).find_map(|r| match r.kind() {
        Kind::Vendor(rest) => Some(rest),
        _ => None,
    });
    let media = match vendor {
        Some(range) => vendor_media(range, default),
        None if ranges(accept).any(|r| r.kind() == Kind::Json) => format!("github.{default}"),
        None if ranges(accept).any(|r| r.kind() == Kind::Any) => {
            if ranges(accept).nth(1).is_some() {
                format!("github.{default}")
            } else {
                format!("github.{default}; format=json")
            }
        }
        None => return "unknown".to_owned(),
    };
    if first.kind() == Kind::Unsupported {
        format!("unknown, {media}")
    } else {
        media
    }
}

fn vendor_media(range: &str, default: &str) -> String {
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
        .unwrap_or(default);
    let mut out = format!("github.{version}");
    if let Some(p) = names.next() {
        let _ = write!(out, "; param={p}");
    }
    if let Some(f) = format {
        let _ = write!(out, "; format={f}");
    }
    out
}

const AUTHENTICATED_VARY: &str =
    "Accept, Authorization, Cookie, X-GitHub-OTP,Accept-Encoding, Accept, X-Requested-With";
const AUTHENTICATED_SPACED_VARY: &str =
    "Accept, Authorization, Cookie, X-GitHub-OTP, Accept-Encoding, Accept, X-Requested-With";
const ANONYMOUS_VARY: &str = "Accept,Accept-Encoding, Accept, X-Requested-With";
const ANONYMOUS_SPACED_VARY: &str = "Accept, Accept-Encoding, Accept, X-Requested-With";
const PLAIN_VARY: &str = "Accept-Encoding, Accept, X-Requested-With";
const SPACED_VARY: [&str; 2] = ["repos/get-content", "pulls/get"];
const ANONYMOUS_SPACED_ONLY: [&str; 1] = ["pulls/list"];
const DAY_CACHE: [&str; 1] = ["git/get-blob"];
const UNCOMPRESSED: [&str; 1] = ["repos/get-content"];
const GZIP_MIN_BYTES: usize = 20;

fn cache_headers(reply: &mut Reply, operation_id: &str, read: bool, authenticated: bool) {
    let cached = reply.status == StatusCode::OK || reply.status == StatusCode::NOT_MODIFIED;
    let vary = if read && cached {
        let spaced = SPACED_VARY.contains(&operation_id);
        if !reply.headers.contains_key(header::CACHE_CONTROL) {
            let value = match (authenticated, DAY_CACHE.contains(&operation_id)) {
                (true, false) => "private, max-age=60, s-maxage=60",
                (true, true) => "private, max-age=86400, s-maxage=86400",
                (false, false) => "public, max-age=60, s-maxage=60",
                (false, true) => "public, max-age=86400, s-maxage=86400",
            };
            reply
                .headers
                .insert(header::CACHE_CONTROL, HeaderValue::from_static(value));
        }
        match (authenticated, spaced) {
            (true, false) => AUTHENTICATED_VARY,
            (true, true) => AUTHENTICATED_SPACED_VARY,
            (false, false) if !ANONYMOUS_SPACED_ONLY.contains(&operation_id) => ANONYMOUS_VARY,
            (false, _) => ANONYMOUS_SPACED_VARY,
        }
    } else {
        PLAIN_VARY
    };
    reply
        .headers
        .insert(header::VARY, HeaderValue::from_static(vary));
}

fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|coding| {
            let mut parts = coding.split(';');
            let name = parts.next().unwrap_or_default().trim();
            let refused = parts.any(|p| {
                p.trim()
                    .strip_prefix("q=")
                    .and_then(|q| q.trim().parse::<f32>().ok())
                    == Some(0.0)
            });
            name.eq_ignore_ascii_case("gzip") && !refused
        })
}

fn gzip(reply: &mut Reply, request: &HeaderMap, operation_id: &str) -> bool {
    let compressible = reply
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|t| t.to_str().ok())
        .is_some_and(|t| t.starts_with("application/json") || t.starts_with("text/plain"));
    if !compressible
        || reply.body.len() < GZIP_MIN_BYTES
        || UNCOMPRESSED.contains(&operation_id)
        || !accepts_gzip(request)
    {
        return false;
    }
    let mut encoder = GzEncoder::new(
        Vec::with_capacity(reply.body.len() / 2),
        Compression::fast(),
    );
    let Ok(body) = encoder
        .write_all(&reply.body)
        .and_then(|()| encoder.finish())
    else {
        return false;
    };
    reply.body = Bytes::from(body);
    set(&mut reply.headers, "content-encoding", "gzip");
    true
}

pub(crate) fn etag(media: &str, body: &[u8]) -> String {
    let lane = |seed: u64, value: &dyn Fn(&mut FoldHasher<'static>)| {
        let mut hasher = FixedState::with_seed(seed).build_hasher();
        value(&mut hasher);
        hasher.finish()
    };
    let content = |h: &mut FoldHasher<'static>| (media, body).hash(h);
    let (a, b) = (lane(1, &content), lane(2, &content));
    let digest = |h: &mut FoldHasher<'static>| (a, b).hash(h);
    let (c, d) = (lane(3, &digest), lane(4, &digest));
    format!("\"{a:016x}{b:016x}{c:016x}{d:016x}\"")
}

fn if_none_match(headers: &HeaderMap, tag: &str) -> bool {
    let strip = |t: &str| t.trim().trim_start_matches("W/").to_owned();
    let tag = strip(tag);
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|t| t.trim() == "*" || strip(t) == tag)
}

fn not_modified(request: &HeaderMap, modified: Option<Timestamp>, tag: &str) -> bool {
    if request.contains_key(header::IF_NONE_MATCH) {
        return if_none_match(request, tag);
    }
    modified.is_some_and(|modified| {
        request
            .get(header::IF_MODIFIED_SINCE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| Timestamp::parse_http_date(v).ok())
            .is_some_and(|since| modified <= since)
    })
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
        ..
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
        for (accept, default, want) in [
            (None, "v3", "github.v3; format=json"),
            (Some("*/*"), "v3", "github.v3; format=json"),
            (Some("application/json"), "v3", "github.v3"),
            (Some("*/*, application/json"), "v3", "github.v3"),
            (Some("application/vnd.github"), "v3", "github.v3"),
            (Some("application/vnd.github.v3"), "v3", "github.v3"),
            (
                Some("application/vnd.github+json"),
                "v3",
                "github.v3; format=json",
            ),
            (
                Some("application/vnd.github.v3.sha"),
                "v3",
                "github.v3; param=sha",
            ),
            (
                Some("application/vnd.github.raw+json"),
                "v3",
                "github.v3; param=raw; format=json",
            ),
            (
                Some("text/html, application/vnd.github.v3.diff"),
                "v3",
                "unknown, github.v3; param=diff",
            ),
            (
                Some("*/*, application/vnd.github.raw"),
                "v3",
                "github.v3; param=raw",
            ),
            (Some("text/*"), "v3", "github.v3"),
            (Some("APPLICATION/JSON"), "v3", "github.v3"),
            (Some("*/*;q=0.8"), "v3", "github.v3; format=json"),
            (Some("text/html, */*"), "v3", "unknown, github.v3"),
            (Some("application/json, text/html"), "v3", "github.v3"),
            (
                Some("text/html, image/png, application/json"),
                "v3",
                "unknown, github.v3",
            ),
            (
                Some("application/vnd.github.v3+json;q=0.5, text/html"),
                "v3",
                "unknown, github.v3; format=json",
            ),
            (
                Some("text/html;q=0.5, */*;q=0.5"),
                "v3",
                "unknown, github.v3",
            ),
            (Some("application/json;q=0"), "v3", "github.v3"),
            (Some("text/html"), "v3", "unknown"),
            (
                Some("text/html, application/json"),
                "v4",
                "unknown, github.v4",
            ),
            (Some("application/json"), "v4", "github.v4"),
            (
                Some("application/vnd.github.v3+json"),
                "v4",
                "github.v3; format=json",
            ),
            (Some("*/*"), "v4", "github.v4; format=json"),
            (
                Some("application/vnd.github.merge-info-preview+json"),
                "v4",
                "github.v4; param=merge-info-preview; format=json",
            ),
        ] {
            assert_eq!(media_type(accept, default), want, "{accept:?}");
        }
    }

    #[test]
    fn the_etag_changes_with_the_media_type_and_the_body() {
        let a = etag("github.v3; format=json", b"{}");
        assert!(a.starts_with('"'));
        assert_eq!(a, etag("github.v3; format=json", b"{}"));
        assert_ne!(a, etag("github.v3; param=raw", b"{}"));
        assert_ne!(a, etag("github.v3; format=json", b"[]"));
    }

    #[test]
    fn the_api_prefix_is_split_off() {
        assert_eq!(split_prefix("/api/v3"), (true, "/"));
        assert_eq!(split_prefix("/api/v3/repos/o/r"), (true, "/repos/o/r"));
        assert_eq!(split_prefix("/api/v3x"), (false, "/api/v3x"));
        assert_eq!(split_prefix("/repos/o/r"), (false, "/repos/o/r"));
    }
}

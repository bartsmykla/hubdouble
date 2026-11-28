use std::borrow::Cow;
use std::collections::{HashMap, hash_map};
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
use serde_json::value::RawValue;

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
const WITHOUT_A_BODY_TO_A_TOKEN: [&str; 4] = [
    "link",
    "x-oauth-scopes",
    "x-accepted-oauth-scopes",
    "x-github-media-type",
];
const MEDIA_DOCS: &str = "https://docs.github.com/v3/media";
const RATE_LIMIT_DOCS: &str =
    "https://docs.github.com/rest/overview/resources-in-the-rest-api#rate-limiting";
const GRAPHQL_AUTH_DOCS: &str =
    "https://docs.github.com/graphql/guides/forming-calls-with-graphql#authenticating-with-graphql";
const GRAPHQL_DOCS: &str = "https://docs.github.com/graphql";
const GRAPHQL_NO_QUERY: &str = "A query attribute must be specified and must be a string.";
const GRAPHQL_BAD_VARIABLES: &str = "Failed to parse variables";
const GRAPHQL_VARIABLES_DEPTH: usize = 255;

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
        let graphql_read = (parts.method == Method::GET || parts.method == Method::HEAD)
            && (api_path == "/graphql" || parts.uri.path() == "/api/graphql");
        let mut who = Who::default();
        let mut graphql_actor = Actor::Anonymous;
        if graphql || graphql_read {
            match self.graphql_sign_in(&parts.headers, &mut who) {
                Ok(Actor::Anonymous) => return self.graphql_rate_limited(&parts, peer),
                Ok(actor) => graphql_actor = actor,
                Err(e) => {
                    return self.finish(Call {
                        operation_id: "graphql",
                        resource: "graphql",
                        version: "",
                        result: Err(e),
                        who,
                        parts: &parts,
                    });
                }
            }
        }
        if graphql_read {
            return self.graphql_schema(&parts, who);
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
            return self
                .graphql(&parts, &body, urls, (graphql_actor, who))
                .await;
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
        if r.entry.requirement.scope != hubdouble_model::Scope::App {
            hubdouble_auth::accepts_token(
                &actor,
                r.entry.tokens.installation,
                r.entry.tokens.user_to_server,
            )
            .map_err(|e| e.with_docs(r.entry.documentation_url))?;
        }
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
        (actor, who): (Actor, Who),
    ) -> HttpResponse {
        let mut request = GraphqlBody::parse(body);
        let operation = request
            .as_ref()
            .ok()
            .and_then(|r| r.operation_name.as_deref())
            .unwrap_or("graphql");
        if let Some(fault) = self.fault(parts, operation).await {
            return self.faulted(parts, "graphql", "graphql", "", fault);
        }
        let services = &self.services;
        let variables = request
            .as_mut()
            .ok()
            .and_then(|r| r.variables.take())
            .unwrap_or_default();
        let request_id = self.request_id();
        let result = catch_unwind(AssertUnwindSafe(|| {
            if let Some(unsupported) = unsupported_accept(&parts.headers, &[]) {
                return Err(unsupported);
            }
            let request = match &request {
                Ok(request) => request,
                Err(Unread::NotJson) => {
                    return Err(
                        ApiError::bad_request("Problems parsing JSON").with_docs(GRAPHQL_DOCS)
                    );
                }
                Err(Unread::NotObject) => {
                    return Err(ApiError::bad_request("Body should be a JSON object")
                        .with_docs(GRAPHQL_DOCS));
                }
                Err(Unread::TooDeep | Unread::BrokenQuery) => {
                    return Ok(Reply::raw(hubdouble_rest::JSON, Vec::new())
                        .status(StatusCode::INTERNAL_SERVER_ERROR));
                }
                Err(Unread::NoQuery) => return graphql_error(GRAPHQL_NO_QUERY),
                Err(Unread::BadVariables) => return graphql_error(GRAPHQL_BAD_VARIABLES),
            };
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
                    request_id: &request_id,
                },
            )?;
            let mut reply = Reply::json(&response)?;
            set(&mut reply.headers, "x-github-request-id", &request_id);
            Ok(reply)
        }))
        .unwrap_or_else(|_| Err(ApiError::internal("Internal Server Error")));
        self.finish(Call {
            operation_id: "graphql",
            resource: "graphql",
            version: "",
            result,
            who,
            parts,
        })
    }

    fn graphql_schema(&self, parts: &RequestParts, who: Who) -> HttpResponse {
        let result = match unsupported_accept(&parts.headers, &[]) {
            Some(unsupported) => Err(unsupported),
            None => Ok(Reply::raw(
                hubdouble_rest::JSON,
                self.gql.schema().introspection(),
            )),
        };
        self.finish(Call {
            operation_id: "graphql",
            resource: "graphql",
            version: "",
            result,
            who,
            parts,
        })
    }

    fn graphql_sign_in(&self, headers: &HeaderMap, who: &mut Who) -> Result<Actor, ApiError> {
        let services = &self.services;
        let actor = services
            .store
            .view(|tx| hubdouble_auth::authenticate(tx, services.clock.as_ref(), headers))?;
        who.of(&actor);
        if matches!(actor, Actor::App { .. }) {
            return Err(
                ApiError::new(401, "This endpoint requires you to be authenticated.")
                    .with_docs(GRAPHQL_AUTH_DOCS),
            );
        }
        Ok(actor)
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
        to_response(reply, parts.method == Method::HEAD)
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
            match precondition(&parts.headers, reply.modified, &strong, who.authenticated) {
                Some(StatusCode::NOT_MODIFIED) => {
                    reply.status = StatusCode::NOT_MODIFIED;
                    reply.body = Bytes::new();
                    if who.authenticated {
                        reply.headers.remove(header::CONTENT_TYPE);
                    }
                }
                Some(status) => {
                    reply.status = status;
                    reply.body = Bytes::new();
                    reply.headers.insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("text/html;charset=utf-8"),
                    );
                }
                None => {}
            }
            if who.authenticated && reply.status != StatusCode::OK {
                for name in WITHOUT_A_BODY_TO_A_TOKEN {
                    reply.headers.remove(name);
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

    fn request_id(&self) -> String {
        let seq = self.request_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let unix = self.services.clock.now().unix();
        format!("0000:{seq:04X}:{seq:06X}:{seq:06X}:{unix:08X}")
    }

    fn common_headers(
        &self,
        out: &mut HeaderMap,
        request: &HeaderMap,
        resource: &str,
        selected: Option<&str>,
    ) -> String {
        let now = self.services.clock.now();
        let default = if resource == "graphql" { "v4" } else { "v3" };
        let media = media_type(
            request.get(header::ACCEPT).and_then(|v| v.to_str().ok()),
            default,
        );
        if !out.contains_key("x-github-request-id") {
            set(out, "x-github-request-id", &self.request_id());
        }
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

struct GraphqlBody {
    query: String,
    operation_name: Option<String>,
    variables: Option<hubdouble_gql::Variables>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Unread {
    NotJson,
    NotObject,
    TooDeep,
    NoQuery,
    BrokenQuery,
    BadVariables,
}

impl GraphqlBody {
    fn parse(body: &[u8]) -> Result<Self, Unread> {
        let body = spaced(body);
        let members = match body.iter().copied().find(|b| !b.is_ascii_whitespace()) {
            None => return Err(Unread::NoQuery),
            Some(b'{') => unbounded::<Members<'_>>(&body).ok_or(Unread::NotJson)?,
            Some(_) => {
                let raw = unbounded::<&RawValue>(&body).ok_or(Unread::NotJson)?;
                return Err(match raw.get() {
                    "null" | "false" => Unread::NoQuery,
                    _ => Unread::NotObject,
                });
            }
        };
        let raw = members
            .variables
            .map(RawValue::get)
            .filter(|raw| !raw.starts_with('"'));
        let sent = raw.map(raw_variables).transpose()?.flatten();
        let Some((query, lone_low)) = members
            .query
            .and_then(|raw| hubdouble_gql::json_string(raw.get()))
        else {
            return Err(
                if raw.is_some_and(|raw| nesting(raw) > GRAPHQL_VARIABLES_DEPTH) {
                    Unread::TooDeep
                } else {
                    Unread::NoQuery
                },
            );
        };
        if lone_low {
            return Err(Unread::BrokenQuery);
        }
        let variables = match members.variables {
            Some(raw) if raw.get().starts_with('"') => {
                let (text, _) =
                    hubdouble_gql::json_string_of_json(raw.get()).ok_or(Unread::NotJson)?;
                let text = spaced(text.as_bytes());
                let text = std::str::from_utf8(&text)
                    .map_err(|_| Unread::NotJson)?
                    .trim_matches([' ', '\t', '\n', '\r']);
                if text.is_empty() {
                    None
                } else {
                    let inner =
                        unbounded::<&RawValue>(text.as_bytes()).ok_or(Unread::BadVariables)?;
                    raw_variables(inner.get())?
                }
            }
            _ => sent,
        };
        let operation_name = members.operation_name.and_then(|raw| ruby_text(raw.get()));
        Ok(Self {
            query,
            operation_name,
            variables,
        })
    }
}

fn graphql_error(message: &str) -> Result<Reply, ApiError> {
    Reply::json(&serde_json::json!({ "errors": [{ "message": message }] }))
}

fn spaced(body: &[u8]) -> Cow<'_, [u8]> {
    if !body.iter().any(|b| matches!(b, 0x0B | 0x0C)) {
        return Cow::Borrowed(body);
    }
    let mut out = body.to_vec();
    let (mut in_string, mut escaped) = (false, false);
    for b in &mut out {
        if in_string {
            match *b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
        } else if *b == b'"' {
            in_string = true;
        } else if matches!(*b, 0x0B | 0x0C) {
            *b = b' ';
        }
    }
    Cow::Owned(out)
}

fn raw_variables(text: &str) -> Result<Option<hubdouble_gql::Variables>, Unread> {
    if !text.starts_with('{') {
        return Ok(None);
    }
    if nesting(text) > GRAPHQL_VARIABLES_DEPTH {
        return Err(Unread::TooDeep);
    }
    hubdouble_gql::Variables::parse(text)
        .map(Some)
        .map_err(|_| Unread::BadVariables)
}

fn nesting(text: &str) -> usize {
    let (mut depth, mut deepest) = (0_usize, 0_usize);
    let (mut in_string, mut escaped) = (false, false);
    for b in text.bytes() {
        if in_string {
            match b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                deepest = deepest.max(depth);
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    deepest
}

enum RubyNode {
    Leaf(String),
    List(Vec<usize>),
    Hash(Vec<(String, usize)>),
}

fn ruby_text(raw: &str) -> Option<String> {
    if raw == "null" {
        return None;
    }
    if raw.starts_with('"') {
        return hubdouble_gql::json_string(raw).map(|(text, _)| text);
    }
    let mut nodes: Vec<RubyNode> = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    let mut keys: HashMap<(usize, String), usize> = HashMap::new();
    let mut key: Option<String> = None;
    let mut rest = raw;
    while let Some(&b) = rest.as_bytes().first() {
        let (len, node) = match b {
            b'[' => (1, RubyNode::List(Vec::new())),
            b'{' => (1, RubyNode::Hash(Vec::new())),
            b']' | b'}' => {
                open.pop();
                rest = rest.get(1..)?;
                continue;
            }
            b'"' => {
                let len = string_len(rest);
                (len, RubyNode::Leaf(ruby_inspect(rest.get(..len)?)?))
            }
            b'n' => (4, RubyNode::Leaf("nil".to_owned())),
            b't' => (4, RubyNode::Leaf("true".to_owned())),
            b'f' => (5, RubyNode::Leaf("false".to_owned())),
            b',' | b':' => {
                rest = rest.get(1..)?;
                continue;
            }
            _ if b.is_ascii_whitespace() => {
                rest = rest.get(1..)?;
                continue;
            }
            _ => {
                let len = rest
                    .bytes()
                    .position(|b| matches!(b, b',' | b']' | b'}') || b.is_ascii_whitespace())
                    .unwrap_or(rest.len());
                (len, RubyNode::Leaf(ruby_number(rest.get(..len)?)))
            }
        };
        rest = rest.get(len..)?;
        let id = nodes.len();
        let parent = open.last().copied();
        match parent.and_then(|p| nodes.get_mut(p)) {
            Some(RubyNode::List(items)) => items.push(id),
            Some(RubyNode::Hash(entries)) => {
                let Some(name) = key.take() else {
                    let RubyNode::Leaf(name) = node else {
                        return None;
                    };
                    key = Some(name);
                    continue;
                };
                match keys.entry((parent.unwrap_or_default(), name.clone())) {
                    hash_map::Entry::Occupied(at) => {
                        entries.get_mut(*at.get())?.1 = id;
                    }
                    hash_map::Entry::Vacant(at) => {
                        at.insert(entries.len());
                        entries.push((name, id));
                    }
                }
            }
            _ => {}
        }
        if matches!(node, RubyNode::List(_) | RubyNode::Hash(_)) {
            open.push(id);
        }
        nodes.push(node);
    }
    ruby_print(&nodes)
}

fn ruby_print(nodes: &[RubyNode]) -> Option<String> {
    let mut out = String::new();
    let mut steps = vec![Step::Node(0)];
    while let Some(step) = steps.pop() {
        let id = match step {
            Step::Node(id) => id,
            Step::Text(text) => {
                out.push_str(text);
                continue;
            }
        };
        match nodes.get(id)? {
            RubyNode::Leaf(text) => out.push_str(text),
            RubyNode::List(items) => {
                out.push('[');
                steps.push(Step::Text("]"));
                for (i, &item) in items.iter().enumerate().rev() {
                    steps.push(Step::Node(item));
                    if i > 0 {
                        steps.push(Step::Text(", "));
                    }
                }
            }
            RubyNode::Hash(entries) => {
                out.push('{');
                steps.push(Step::Text("}"));
                for (i, (name, value)) in entries.iter().enumerate().rev() {
                    steps.push(Step::Node(*value));
                    steps.push(Step::Text(" => "));
                    steps.push(Step::Text(name));
                    if i > 0 {
                        steps.push(Step::Text(", "));
                    }
                }
            }
        }
    }
    Some(out)
}

enum Step<'n> {
    Node(usize),
    Text(&'n str),
}

fn string_len(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut end = 1;
    while let Some(b) = bytes.get(end) {
        match b {
            b'\\' => end += 2,
            b'"' => return end + 1,
            _ => end += 1,
        }
    }
    bytes.len()
}

fn ruby_inspect(token: &str) -> Option<String> {
    let mut units: Vec<Result<char, u32>> = Vec::new();
    hubdouble_gql::json_pieces(token, |piece| match piece {
        hubdouble_gql::JsonPiece::Text(text) => units.extend(text.chars().map(Ok)),
        hubdouble_gql::JsonPiece::LoneLow(unit) => units.push(Err(unit)),
    })?;
    let mut out = String::from('"');
    for (i, unit) in units.iter().enumerate() {
        let c = match *unit {
            Ok(c) => c,
            Err(unit) => {
                for byte in [
                    0xE0 | (unit >> 12),
                    0x80 | ((unit >> 6) & 0x3F),
                    0x80 | (unit & 0x3F),
                ] {
                    let _ = write!(out, "\\x{byte:02X}");
                }
                continue;
            }
        };
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\u{c}' => out.push_str("\\f"),
            '\u{b}' => out.push_str("\\v"),
            '\u{8}' => out.push_str("\\b"),
            '\u{7}' => out.push_str("\\a"),
            '\u{1b}' => out.push_str("\\e"),
            '#' if matches!(units.get(i + 1), Some(Ok('{' | '$' | '@'))) => out.push_str("\\#"),
            c if c.is_ascii_control() => {
                let _ = write!(out, "\\u{:04X}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    Some(out)
}

fn ruby_number(token: &str) -> String {
    if !token.contains(['.', 'e', 'E']) {
        return if token == "-0" { "0" } else { token }.to_owned();
    }
    let Ok(x) = token.parse::<f64>() else {
        return token.to_owned();
    };
    let sign = if x.is_sign_negative() { "-" } else { "" };
    if x.is_infinite() {
        return format!("{sign}Infinity");
    }
    if x.classify() == std::num::FpCategory::Zero {
        return format!("{sign}0.0");
    }
    let shortest = format!("{:e}", x.abs());
    let Some((mantissa, exponent)) = shortest.split_once('e') else {
        return token.to_owned();
    };
    let digits = mantissa.replace('.', "");
    let exponent: i32 = exponent.parse().unwrap_or_default();
    let point = exponent + 1;
    match usize::try_from(point) {
        Ok(point @ 1..=15) => {
            if digits.len() <= point {
                format!("{sign}{digits}{}.0", "0".repeat(point - digits.len()))
            } else {
                let (whole, fraction) = digits.split_at(point);
                format!("{sign}{whole}.{fraction}")
            }
        }
        Err(_) | Ok(0) if point >= -3 => {
            let zeros = usize::try_from(-point).unwrap_or_default();
            format!("{sign}0.{}{digits}", "0".repeat(zeros))
        }
        _ => {
            let (first, rest) = digits.split_at(1);
            let rest = if rest.is_empty() { "0" } else { rest };
            let mark = if exponent < 0 { '-' } else { '+' };
            format!("{sign}{first}.{rest}e{mark}{:02}", exponent.unsigned_abs())
        }
    }
}

fn unbounded<'de, T: Deserialize<'de>>(body: &'de [u8]) -> Option<T> {
    let mut de = serde_json::Deserializer::from_slice(body);
    de.disable_recursion_limit();
    let read = T::deserialize(&mut de).ok()?;
    de.end().ok()?;
    Some(read)
}

#[derive(Default)]
struct Members<'b> {
    query: Option<&'b RawValue>,
    operation_name: Option<&'b RawValue>,
    variables: Option<&'b RawValue>,
}

impl<'de> Deserialize<'de> for Members<'de> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Fields;
        impl<'de> serde::de::Visitor<'de> for Fields {
            type Value = Members<'de>;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a GraphQL request body")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Members<'de>, A::Error> {
                let mut members = Members::default();
                while let Some(key) = map.next_key::<&RawValue>()? {
                    let key = key.get();
                    let name = match key.get(1..key.len() - 1) {
                        Some(plain) if !plain.contains('\\') => Cow::Borrowed(plain),
                        _ => Cow::Owned(
                            hubdouble_gql::json_string(key).map_or_default(|(text, _)| text),
                        ),
                    };
                    let last = match &*name {
                        "query" => &mut members.query,
                        "operationName" => &mut members.operation_name,
                        "variables" => &mut members.variables,
                        _ => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                            continue;
                        }
                    };
                    *last = Some(map.next_value()?);
                }
                Ok(members)
            }
        }
        d.deserialize_map(Fields)
    }
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
    let cached = matches!(
        reply.status,
        StatusCode::OK | StatusCode::NOT_MODIFIED | StatusCode::PRECONDITION_FAILED
    );
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

fn if_match(headers: &HeaderMap, tag: &str) -> bool {
    headers
        .get_all(header::IF_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|t| t.trim() == "*" || t.trim() == tag)
}

fn date_header(request: &HeaderMap, name: HeaderName) -> Option<Timestamp> {
    request
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| Timestamp::parse_http_date(v).ok())
}

fn precondition(
    request: &HeaderMap,
    modified: Option<Timestamp>,
    tag: &str,
    authenticated: bool,
) -> Option<StatusCode> {
    let has_none_match = request.contains_key(header::IF_NONE_MATCH);
    if has_none_match && if_none_match(request, tag) {
        return Some(StatusCode::NOT_MODIFIED);
    }
    if authenticated && request.contains_key(header::IF_MATCH) && !if_match(request, tag) {
        return Some(StatusCode::PRECONDITION_FAILED);
    }
    if !has_none_match
        && let Some(modified) = modified
        && date_header(request, header::IF_MODIFIED_SINCE).is_some_and(|since| modified <= since)
    {
        return Some(StatusCode::NOT_MODIFIED);
    }
    if authenticated
        && let Some(modified) = modified
        && date_header(request, header::IF_UNMODIFIED_SINCE).is_some_and(|since| modified > since)
    {
        return Some(StatusCode::PRECONDITION_FAILED);
    }
    None
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

    fn nested_body(arrays: usize, leaf: &str) -> String {
        format!(
            r#"{{"query":"q","variables":{{"a":{}{leaf}{}}}}}"#,
            "[".repeat(arrays),
            "]".repeat(arrays)
        )
    }

    #[test]
    fn variables_parse_up_to_255_levels_on_both_paths() {
        for leaf in ["1", "18446744073709551616", &"9".repeat(400)] {
            // Given variables of 255 and 256 levels, the variables object and `a` counted
            let (fits, deeper) = (nested_body(254, leaf), nested_body(255, leaf));
            // When the bodies are parsed
            let (fits, deeper) = (
                GraphqlBody::parse(fits.as_bytes()),
                GraphqlBody::parse(deeper.as_bytes()),
            );
            // Then 255 levels parse and 256 are too deep, around a small or a wide int
            assert!(fits.is_ok(), "{}", &leaf[..1]);
            assert_eq!(deeper.err(), Some(Unread::TooDeep), "{}", &leaf[..1]);
        }
    }

    #[test]
    fn a_body_nested_far_past_the_limit_is_too_deep() {
        for leaf in ["1", "18446744073709551616"] {
            // Given a variable nested 100 000 arrays deep, around a small or a wide int
            let body = nested_body(100_000, leaf);
            // When the body is parsed
            let parsed = GraphqlBody::parse(body.as_bytes());
            // Then it is too deep, without overflowing the stack
            assert_eq!(parsed.err(), Some(Unread::TooDeep), "{leaf}");
        }
    }

    #[test]
    fn a_deep_member_that_the_body_does_not_use_parses() {
        // Given a body with a member nested 100 000 levels that is not variables, and quotes,
        // escapes and brackets in strings
        let deep = format!("{}1{}", r#"{"v":["#.repeat(100_000), "]}".repeat(100_000));
        let body = format!(
            r#"{{"x":{deep},"variables2":[[[]]],"query":"q \"[{{\\\\","variables":{{"a":[1]}}}}"#
        );
        // When the body is parsed
        let parsed = GraphqlBody::parse(body.as_bytes());
        // Then it parses with its query and variables
        let parsed = parsed.ok().map(|b| (b.query, b.variables.is_some()));
        assert_eq!(parsed, Some((r#"q "[{\\"#.to_owned(), true)));
    }

    #[test]
    fn an_escaped_variables_key_is_counted() {
        // Given a variables key written with an escape, nested 256 levels
        let body =
            nested_body(255, "1").replacen("variables", &format!("vari{}u0061bles", '\\'), 1);
        // When the body is parsed
        let parsed = GraphqlBody::parse(body.as_bytes());
        // Then it is too deep
        assert_eq!(parsed.err(), Some(Unread::TooDeep));
    }

    #[test]
    fn variables_forms_parse_as_github_reads_them() {
        let objects = |n: usize| format!("{}1{}", r#"{"a":"#.repeat(n), "}".repeat(n));
        let quoted = |text: &str| serde_json::to_string(text).unwrap_or_default();
        let arrays = format!("{}{}", "[".repeat(1000), "]".repeat(1000));
        let used = Ok(true);
        let unused = Ok(false);
        for (variables, want) in [
            (objects(255), used),
            (objects(256), Err(Unread::TooDeep)),
            (arrays.clone(), unused),
            (format!("[{}]", objects(300)), unused),
            ("[[[1]]]".to_owned(), unused),
            (format!(r#"{},"variables":{{}}"#, objects(300)), used),
            (
                format!(r#"{{}},"variables":{}"#, objects(300)),
                Err(Unread::TooDeep),
            ),
            ("null".to_owned(), unused),
            ("1.5".to_owned(), unused),
            ("true".to_owned(), unused),
            ("[[1]]".to_owned(), unused),
            (quoted(r#"{"n":1}"#), used),
            (quoted(" {\"n\":1}\n"), used),
            (quoted(&objects(255)), used),
            (quoted(&objects(256)), Err(Unread::TooDeep)),
            (quoted(&arrays), unused),
            (quoted(&format!("[{}]", objects(300))), unused),
            (r#"{"x":1e309,"y":-1e309}"#.to_owned(), used),
            (quoted(r#"{"x":1e309}"#), used),
            (r#"{"a":"\ud800","\udc00":1}"#.to_owned(), used),
            (quoted(r#"{"a":"\ud800"}"#), used),
            (quoted(""), unused),
            (quoted("null"), unused),
            (quoted("[1]"), unused),
            (quoted("not json"), Err(Unread::BadVariables)),
            (quoted(r#"{"n":1} x"#), Err(Unread::BadVariables)),
            (quoted(r#"{"n":1,}"#), Err(Unread::BadVariables)),
            (format!(r#""not json","variables":{}"#, r#"{"n":1}"#), used),
            (
                format!(r#"{},"variables":"not json""#, r#"{"n":1}"#),
                Err(Unread::BadVariables),
            ),
        ] {
            // Given a body with this variables member, as live GitHub answered it
            let body = format!(r#"{{"query":"q","variables":{variables}}}"#);
            // When the body is parsed
            let parsed = GraphqlBody::parse(body.as_bytes()).map(|b| b.variables.is_some());
            // Then the variables are used, ignored or refused
            assert_eq!(parsed, want, "{}", &variables[..variables.len().min(40)]);
        }
    }

    #[test]
    fn a_body_is_checked_for_json_and_depth_before_its_query() {
        let deep = format!("{}1{}", r#"{"a":"#.repeat(300), "}".repeat(300));
        for (body, want) in [
            (
                format!(r#"{{"query":"q","variables":{deep},}}"#),
                Unread::NotJson,
            ),
            (
                format!(r#"{{"query":"q",,"variables":{deep}}}"#),
                Unread::NotJson,
            ),
            (r#"{"query":1,}"#.to_owned(), Unread::NotJson),
            (format!(r#"{{"variables":{deep}}}"#), Unread::TooDeep),
            ("{}".to_owned(), Unread::NoQuery),
            (r#"{"query":null}"#.to_owned(), Unread::NoQuery),
            (r#"{"query":1}"#.to_owned(), Unread::NoQuery),
            (r#"{"variables":{"a":1}}"#.to_owned(), Unread::NoQuery),
            (format!(r#"{{"variables":[{deep}]}}"#), Unread::TooDeep),
            (
                format!(
                    r#"{{"query":1,"variables":{}1{}}}"#,
                    "[".repeat(256),
                    "]".repeat(256)
                ),
                Unread::TooDeep,
            ),
            (
                format!(r#"{{"variables":{}1{}}}"#, "[".repeat(255), "]".repeat(255)),
                Unread::NoQuery,
            ),
            (
                format!(r#"{{"variables":{deep},"query":1}}"#),
                Unread::TooDeep,
            ),
            (r#"{"variables":"not json"}"#.to_owned(), Unread::NoQuery),
            (
                r#"{"query":1,"variables":"not json"}"#.to_owned(),
                Unread::NoQuery,
            ),
            (
                format!(
                    r#"{{"variables":{}}}"#,
                    serde_json::to_string(&deep).unwrap_or_default()
                ),
                Unread::NoQuery,
            ),
            (
                format!(
                    r#"{{"query":"q","variables":{}}}"#,
                    serde_json::to_string(&deep).unwrap_or_default()
                ),
                Unread::TooDeep,
            ),
            (r#"{"query":"q \udc00"}"#.to_owned(), Unread::BrokenQuery),
            (String::new(), Unread::NoQuery),
            ("   ".to_owned(), Unread::NoQuery),
            ("\n".to_owned(), Unread::NoQuery),
            ("null".to_owned(), Unread::NoQuery),
            (" null ".to_owned(), Unread::NoQuery),
            ("false".to_owned(), Unread::NoQuery),
            ("{bad".to_owned(), Unread::NotJson),
            ("[]x".to_owned(), Unread::NotJson),
            ("[]".to_owned(), Unread::NotObject),
            (r#"[{"query":"q"}]"#.to_owned(), Unread::NotObject),
            ("[[[[1]]]]".to_owned(), Unread::NotObject),
            (r#""x""#.to_owned(), Unread::NotObject),
            (r#""\ud800""#.to_owned(), Unread::NotObject),
            (r#""{\"query\":\"q\"}""#.to_owned(), Unread::NotObject),
            ("1".to_owned(), Unread::NotObject),
            ("-1".to_owned(), Unread::NotObject),
            ("1e400".to_owned(), Unread::NotObject),
            ("true".to_owned(), Unread::NotObject),
            (" \u{c}[]".to_owned(), Unread::NotObject),
        ] {
            // Given a body that GitHub refuses, as live GitHub answered it
            // When the body is parsed
            let parsed = GraphqlBody::parse(body.as_bytes());
            // Then the first failed check names the answer
            assert_eq!(parsed.err(), Some(want), "{}", &body[..body.len().min(40)]);
        }
    }

    #[test]
    fn a_vertical_tab_or_form_feed_between_tokens_is_space_and_other_bytes_are_not() {
        let positions: [fn(&str) -> String; 10] = [
            |x| format!(r#"{x}{{"query":"q"}}"#),
            |x| format!(r#"{{"query":"q"}}{x}"#),
            |x| format!(r#"{{{x}"query":"q"}}"#),
            |x| format!(r#"{{"query"{x}:"q"}}"#),
            |x| format!(r#"{{"query":{x}"q"}}"#),
            |x| format!(r#"{{"query":"q"{x},"variables":{{"n":1}}}}"#),
            |x| format!(r#"{{"query":"q",{x}"variables":{{"n":1}}}}"#),
            |x| format!(r#"{{"query":"q"{x}}}"#),
            |x| format!(r#"{{"query":"q","variables":{{"n":{x}1}}}}"#),
            |x| format!(r#"{{"query":"q","variables":{{"n":1{x}}}}}"#),
        ];
        for (at, make) in positions.iter().enumerate() {
            for byte in ["\u{b}", "\u{c}"] {
                // Given a body with a vertical tab or a form feed at this position
                let body = make(byte);
                // When the body is parsed
                let parsed = GraphqlBody::parse(body.as_bytes()).map(|b| b.query);
                // Then it parses as live GitHub took it
                assert_eq!(parsed, Ok("q".to_owned()), "{at} {byte:?}");
            }
            for byte in [
                "\0", "\u{8}", "\u{e}", "\u{1f}", "\u{7f}", "\u{a0}", "\u{feff}", "\u{2028}",
                "\u{3000}",
            ] {
                // Given a body with another byte that is not JSON whitespace at this position
                let body = make(byte);
                // When the body is parsed
                let parsed = GraphqlBody::parse(body.as_bytes()).map(|b| b.query);
                // Then it is not JSON, as live GitHub answered
                assert_eq!(parsed, Err(Unread::NotJson), "{at} {byte:?}");
            }
        }
        // Given a form feed inside the query string, escaped
        let body = r#"{"query":"a\fb"}"#;
        // When the body is parsed
        let parsed = GraphqlBody::parse(body.as_bytes()).map(|b| b.query);
        // Then the string keeps it
        assert_eq!(parsed, Ok("a\u{c}b".to_owned()));
    }

    #[test]
    fn an_operation_name_of_any_json_value_is_the_text_that_github_names() {
        for (sent, want) in [
            ("null", None),
            ("0", Some("0")),
            ("-0", Some("0")),
            ("-1", Some("-1")),
            (
                "123456789012345678901234567890",
                Some("123456789012345678901234567890"),
            ),
            ("1.5", Some("1.5")),
            ("1.0", Some("1.0")),
            ("-0.0", Some("-0.0")),
            ("1e3", Some("1000.0")),
            ("1E2", Some("100.0")),
            ("100.0", Some("100.0")),
            ("0.1", Some("0.1")),
            ("0.0001", Some("0.0001")),
            ("0.00001", Some("1.0e-05")),
            ("1.25e-7", Some("1.25e-07")),
            ("1e15", Some("1.0e+15")),
            ("1e16", Some("1.0e+16")),
            ("1e20", Some("1.0e+20")),
            ("1e308", Some("1.0e+308")),
            ("-2.5e+30", Some("-2.5e+30")),
            ("123456789012345678.0", Some("1.2345678901234568e+17")),
            ("1e400", Some("Infinity")),
            ("-1e309", Some("-Infinity")),
            ("true", Some("true")),
            ("false", Some("false")),
            ("[]", Some("[]")),
            ("[[]]", Some("[[]]")),
            (r#"[1,"a",null,true]"#, Some(r#"[1, "a", nil, true]"#)),
            (r#"["a\\b"]"#, Some(r#"["a\\b"]"#)),
            (r#"["é"]"#, Some(r#"["é"]"#)),
            ("{}", Some("{}")),
            (r#"{"a":1}"#, Some(r#"{"a" => 1}"#)),
            (r#"{"":1}"#, Some(r#"{"" => 1}"#)),
            (r#"{"a":1.5}"#, Some(r#"{"a" => 1.5}"#)),
            (r#"{"a":[]}"#, Some(r#"{"a" => []}"#)),
            (r#"{"a":{}}"#, Some(r#"{"a" => {}}"#)),
            (r#"{"a":1,"b":"x"}"#, Some(r#"{"a" => 1, "b" => "x"}"#)),
            (
                r#"{"a":{"b":[1,null,true,"c"]}}"#,
                Some(r#"{"a" => {"b" => [1, nil, true, "c"]}}"#),
            ),
            (r#"{"a":"q\"r"}"#, Some(r#"{"a" => "q\"r"}"#)),
            (
                r#"{"a":"\n\t\u0001\u007f#{x}#$y#@z"}"#,
                Some(r#"{"a" => "\n\t\u0001\u007F\#{x}\#$y\#@z"}"#),
            ),
            (r#""0""#, Some("0")),
            (r#""""#, Some("")),
            (r#""x\"y""#, Some(r#"x"y"#)),
            (r#""a\\b""#, Some(r"a\b")),
            (r#""a\nb""#, Some("a\nb")),
            (r#""\ud800""#, Some("?")),
            (r#""a\ud800b""#, Some("a?b")),
            (r#""\ud800\ud800""#, Some("\u{10000}")),
            (r#""􏿿""#, Some("\u{10ffff}")),
            (r#""😀""#, Some("\u{1f600}")),
            (r#""\udc00""#, Some("\u{fffd}\u{fffd}\u{fffd}")),
        ] {
            // Given a body whose operationName member is this value
            let body = format!(r#"{{"query":"q","operationName":{sent}}}"#);
            // When the body is parsed
            let parsed = GraphqlBody::parse(body.as_bytes()).map(|b| b.operation_name);
            // Then the name is the text in live GitHub's "No operation named" answer
            assert_eq!(parsed, Ok(want.map(str::to_owned)), "{sent}");
        }
    }

    #[test]
    fn a_lone_surrogate_decodes_as_github_decodes_it() {
        for (sent, want) in [
            (r"\ud800", "?"),
            (r"a\ud800b", "a?b"),
            (r"\ud800\ud800", "\u{10000}"),
            (r"\udc00", "\u{fffd}\u{fffd}\u{fffd}"),
        ] {
            // Given a variable and a query string that hold this escape
            let body = format!(
                r#"{{"query":"{{repository(owner:\"{sent}\")}}","variables":{{"s":"{sent}"}}}}"#
            );
            // When the body is parsed
            let parsed = GraphqlBody::parse(body.as_bytes());
            // Then the variable holds the text that live GitHub echoed, and the query is read
            // unless it holds a lone low surrogate, which live GitHub answers with an empty 500
            let echoed = serde_json::json!({ "s": want }).to_string();
            let variables = parsed.as_ref().ok().and_then(|b| b.variables.clone());
            if sent == r"\udc00" {
                assert_eq!(parsed.err(), Some(Unread::BrokenQuery));
            } else {
                assert_eq!(
                    variables,
                    hubdouble_gql::Variables::parse(&echoed).ok(),
                    "{sent}"
                );
            }
        }
    }

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

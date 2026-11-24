//! The check every REST answer the test kit receives goes through. The operation comes from the
//! request method and path through the pinned description's own path templates, never through
//! hubdouble's router; the status must be one the operation documents or one GitHub's docs
//! prose gives every operation; the body must match that status's schema; and the headers must
//! be the ones GitHub's docs say every answer has.

use std::sync::LazyLock;

use http::HeaderMap;
use serde_json::Value;

use crate::validate::{OPENAPI, openapi_schema, resolve};

const MANY: [&str; 6] = ["ref", "path", "branch", "basehead", "tag", "tree_sha"];

const PROSE_STATUSES: [u16; 7] = [304, 400, 401, 403, 404, 422, 429];

const RAW_BY_CONTENT: [(&str, [&str; 2]); 1] =
    [("git/get-blob", ["text/plain", "application/octet-stream"])];

const EVERY_ANSWER: [&str; 7] = [
    "x-github-request-id",
    "x-github-media-type",
    "x-ratelimit-limit",
    "x-ratelimit-remaining",
    "x-ratelimit-reset",
    "x-ratelimit-used",
    "x-ratelimit-resource",
];

enum Segment {
    Literal(String),
    One,
    Many,
}

struct Template {
    path: &'static str,
    segments: Vec<Segment>,
    methods: Vec<(String, &'static Value)>,
}

static TEMPLATES: LazyLock<Vec<Template>> = LazyLock::new(|| {
    let doc: &'static Value = &OPENAPI.doc;
    doc.get("paths")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(path, item)| Template {
            path: path.as_str(),
            segments: path
                .trim_start_matches('/')
                .split('/')
                .map(
                    |s| match s.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                        Some(name) if MANY.contains(&name) => Segment::Many,
                        Some(_) => Segment::One,
                        None => Segment::Literal(s.to_owned()),
                    },
                )
                .collect(),
            methods: item
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(_, op)| op.get("operationId").is_some())
                .map(|(m, op)| (m.to_ascii_uppercase(), op))
                .collect(),
        })
        .collect()
});

fn matches(segments: &[Segment], parts: &[&str]) -> bool {
    let Some((segment, rest)) = segments.split_first() else {
        return parts.is_empty();
    };
    match segment {
        Segment::Literal(l) => parts.first() == Some(&l.as_str()) && matches(rest, &parts[1..]),
        Segment::One => parts.first().is_some_and(|p| !p.is_empty()) && matches(rest, &parts[1..]),
        Segment::Many => (1..=parts.len()).any(|n| {
            let (last, before) = parts[..n].split_last().unwrap_or((&"", &[]));
            let last_ok = !last.is_empty() || rest.is_empty();
            last_ok && before.iter().all(|p| !p.is_empty()) && matches(rest, &parts[n..])
        }),
    }
}

fn rank(t: &Template) -> Vec<u8> {
    t.segments
        .iter()
        .map(|s| match s {
            Segment::Literal(_) => 2,
            Segment::One => 1,
            Segment::Many => 0,
        })
        .collect()
}

/// An operation of the pinned description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Operation {
    pub id: &'static str,
    /// The description's path template, `/repos/{owner}/{repo}`.
    pub template: &'static str,
    op: &'static Value,
}

impl Operation {
    /// Whether the pinned description lists `status` among the operation's responses.
    #[must_use]
    pub fn documents(&self, status: u16) -> bool {
        self.op
            .get("responses")
            .and_then(|r| r.get(status.to_string()))
            .is_some()
    }
}

/// The operation `method` and the escaped API `path` (no `/api/v3` prefix, no query) resolve
/// to: among the templates that match and take the method, the one with a literal segment
/// where the others have a parameter, earliest first, and the longest on a tie.
#[must_use]
pub fn resolve_operation(method: &str, path: &str) -> Option<Operation> {
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    TEMPLATES
        .iter()
        .filter(|t| matches(&t.segments, &parts))
        .filter_map(|t| {
            t.methods
                .iter()
                .find(|(m, _)| m == method)
                .map(|(_, op)| (rank(t), t.path, *op))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .and_then(|(_, template, op)| {
            Some(Operation {
                id: op.get("operationId")?.as_str()?,
                template,
                op,
            })
        })
}

/// One REST request and its answer, as the client saw them.
pub struct Exchange<'a> {
    pub method: &'a str,
    /// The URL path as sent, escaped, with the `/api/v3` prefix when the client has it.
    pub path: &'a str,
    /// The URL query as sent, without the `?`.
    pub query: &'a str,
    pub request_headers: &'a HeaderMap,
    pub request_body: &'a [u8],
    pub status: u16,
    pub headers: &'a HeaderMap,
    pub body: &'a [u8],
}

/// A checked answer: the route as `docs/inventory/rest-routes.txt` writes it, and the status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub route: String,
    pub status: u16,
}

pub(crate) fn header<'h>(headers: &'h HeaderMap, name: &str) -> &'h str {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

pub(crate) fn media_type(value: &str) -> &str {
    value.split(';').next().unwrap_or("").trim()
}

pub(crate) fn is_json(media: &str) -> bool {
    media == "application/json" || media.ends_with("+json")
}

fn github_media_variant(media: &str) -> Option<&str> {
    let media = media.strip_prefix("application/vnd.github.")?;
    Some(media.strip_prefix("v3.").unwrap_or(media))
}

/// The API path: the URL path without the `/api/v3` prefix of an enterprise base URL.
#[must_use]
pub fn api_path(path: &str) -> &str {
    match path.strip_prefix("/api/v3") {
        Some("") => "/",
        Some(rest) if rest.starts_with('/') => rest,
        _ => path,
    }
}

/// Checks one REST answer. `registered` says whether hubdouble implements an operation, the
/// only case where its `501` is allowed.
///
/// The rules and where they come from:
///
/// - Path parameters `ref`, `path`, `branch`, `basehead` and `tag` hold slashes, as on GitHub
///   (plan section 6.2).
/// - Every operation may also answer what the docs prose gives every endpoint: `304` for a
///   conditional `GET` with an empty body
///   (<https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api#use-conditional-requests>);
///   `400` for a body that is not JSON or not an object and for an unknown
///   `X-GitHub-Api-Version`, `403` and `429` for rate limits, `403` for a token without the
///   permission, `404` for a resource the actor cannot see or a method the route does not take,
///   `422` for a request that fails validation
///   (<https://docs.github.com/en/rest/using-the-rest-api/troubleshooting-the-rest-api>); and
///   `401` for invalid credentials
///   (<https://docs.github.com/en/rest/authentication/authenticating-to-the-rest-api>).
/// - Every error body has `message`, `documentation_url` and `status` as text, and on a `422`
///   each of `errors` has `resource`, `code`, and `field` unless the code is `custom` (plan
///   section 4.7, the troubleshooting page's "Validation failed").
/// - Every answer has `X-GitHub-Request-Id`, `X-GitHub-Media-Type` and the five
///   `X-RateLimit-*` headers, `X-RateLimit-Remaining` never `0`
///   (<https://docs.github.com/en/rest/using-the-rest-api/getting-started-with-the-rest-api#about-the-response-code-and-headers>,
///   <https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api#checking-the-status-of-your-rate-limit>,
///   plan section 4.7), and `Location` where the operation documents it.
///   `X-GitHub-Api-Version-Selected` names the selected version unless GitHub rejects the version.
/// - A non-JSON body answers only a documented media type or the one `Accept` asked for.
/// - Routes outside the bundle are checked against their own docs: `/_blobs/{id}` (plan
///   sections 4.1 and 4.8), the Actions OIDC discovery (`OpenID` Connect Discovery 1.0 section
///   3), its JWKS (RFC 7517) and the runtime token `{"value"}`
///   (<https://docs.github.com/en/actions/reference/security/oidc#methods-for-requesting-the-oidc-token>),
///   OAuth and the device flow
///   (<https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps>),
///   and `GET /repositories/{repository_id}` against `repos/get`, which go-github v93 calls
///   it (`Repositories.GetByID`, `github/repos.go`). GraphQL is checked by its own client.
/// - `/repositories/{id}/...` and `/organizations/{id}/...` are checked against the
///   `/repos/{owner}/{repo}/...` and `/orgs/{org}/...` operation they alias, and every `Link`
///   URL of a repository or organization answer has that by-ID form, as api.github.com writes
///   it; any other `Link` URL keeps the request path.
///
/// # Errors
///
/// Every way the answer differs from GitHub's docs.
pub fn check(x: &Exchange<'_>, registered: &dyn Fn(&str) -> bool) -> Result<Checked, Vec<String>> {
    let path = api_path(x.path);
    if let Some(result) = outside_the_bundle(x, path) {
        return result;
    }
    if x.method == "GET" && segments_eq(path, "/repositories/{repository_id}") {
        let op = resolve_operation("GET", "/repos/o/r")
            .ok_or_else(|| vec!["the description has no repos/get".to_owned()])?;
        return check_operation(x, op, registered).map(|status| Checked {
            route: "GET /repositories/{repository_id}".to_owned(),
            status,
        });
    }
    let canonical = by_id_alias(path);
    let resolved = canonical.as_deref().unwrap_or(path);
    let op = resolve_operation(x.method, resolved)
        .ok_or_else(|| vec![format!("{} {path} resolves to no operation", x.method)])?;
    check_operation(x, op, registered).map(|status| Checked {
        route: format!("{} {}", x.method, op.template),
        status,
    })
}

fn by_id_alias(path: &str) -> Option<String> {
    let numbered = |prefix: &str| {
        let (id, tail) = path.strip_prefix(prefix)?.split_once('/')?;
        (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then_some(tail)
    };
    numbered("/repositories/")
        .map(|tail| format!("/repos/o/r/{tail}"))
        .or_else(|| numbered("/organizations/").map(|tail| format!("/orgs/o/{tail}")))
}

fn by_id_form(path: &str) -> Option<(&'static str, &str)> {
    let tail = |prefix: &str, skip: usize| {
        path.strip_prefix(prefix)
            .and_then(|rest| rest.splitn(skip + 1, '/').nth(skip))
    };
    tail("/repos/", 2)
        .or_else(|| tail("/repositories/", 1))
        .map(|t| ("/repositories/", t))
        .or_else(|| {
            tail("/orgs/", 1)
                .or_else(|| tail("/organizations/", 1))
                .map(|t| ("/organizations/", t))
        })
}

fn link_errors(x: &Exchange<'_>, id: &str) -> Vec<String> {
    let request = api_path(x.path);
    let mut errors = Vec::new();
    for part in header(x.headers, "link")
        .split(", ")
        .filter(|p| !p.is_empty())
    {
        let url = part
            .split_once('>')
            .and_then(|(url, _)| url.strip_prefix('<'))
            .unwrap_or(part);
        let target = url
            .split_once("://")
            .and_then(|(_, rest)| rest.find('/').map(|i| &rest[i..]))
            .unwrap_or("");
        let target = api_path(target.split('?').next().unwrap_or(""));
        let github_form = match by_id_form(request) {
            Some((alias, tail)) => target
                .strip_prefix(alias)
                .and_then(|rest| rest.split_once('/'))
                .is_some_and(|(n, t)| {
                    !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && t == tail
                }),
            None => target == request,
        };
        if !github_form {
            errors.push(format!(
                "{id}: the Link URL {url} is not the form GitHub uses for {request}"
            ));
        }
    }
    errors
}

fn segments_eq(path: &str, template: &str) -> bool {
    let p: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let t: Vec<&str> = template.trim_start_matches('/').split('/').collect();
    p.len() == t.len()
        && p.iter()
            .zip(&t)
            .all(|(p, t)| t.starts_with('{') && !p.is_empty() || p == t)
}

fn check_operation(
    x: &Exchange<'_>,
    op: Operation,
    registered: &dyn Fn(&str) -> bool,
) -> Result<u16, Vec<String>> {
    let mut errors = Vec::new();
    for name in EVERY_ANSWER {
        if header(x.headers, name).is_empty() {
            errors.push(format!("{}: no {name} header", op.id));
        }
    }
    if let Some(error) = version_error(x, op.id) {
        errors.push(error);
    }
    if header(x.headers, "x-ratelimit-remaining") == "0" {
        errors.push(format!("{}: x-ratelimit-remaining is 0", op.id));
    }
    errors.extend(link_errors(x, op.id));
    let doc = &OPENAPI.doc;
    let responses = op.op.get("responses");
    let documented = responses
        .and_then(|r| r.get(x.status.to_string()))
        .map(|r| resolve(doc, r));
    match documented {
        Some(response) => errors.extend(documented_answer(x, op.id, response)),
        None if x.status == 501 => {
            if registered(op.id) {
                errors.push(format!("{}: 501 for a registered operation", op.id));
            }
            errors.extend(error_body(x, op.id));
        }
        None if PROSE_STATUSES.contains(&x.status) => {
            if x.status == 304 {
                errors.extend(not_modified(x, op.id));
            } else {
                errors.extend(error_body(x, op.id));
            }
        }
        None => errors.push(format!(
            "{}: {} is neither documented for the operation nor by the docs prose",
            op.id, x.status
        )),
    }
    if errors.is_empty() {
        Ok(x.status)
    } else {
        Err(errors)
    }
}

fn version_error(x: &Exchange<'_>, id: &str) -> Option<String> {
    const SELECTED: &str = "x-github-api-version-selected";
    let requested = x.request_headers.get("x-github-api-version");
    let version = requested.map_or(Some("2022-11-28"), |v| v.to_str().ok());
    let selected = header(x.headers, SELECTED);
    match version {
        Some(version @ ("2022-11-28" | "2026-03-10")) if selected == version => None,
        Some(version @ ("2022-11-28" | "2026-03-10")) => Some(format!(
            "{id}: expected {SELECTED} {version}, got {selected:?}"
        )),
        _ if x.status == 400 && !x.headers.contains_key(SELECTED) => None,
        _ => Some(format!(
            "{id}: unsupported API version requires 400 without {SELECTED}"
        )),
    }
}

fn not_modified(x: &Exchange<'_>, id: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if x.method != "GET" && x.method != "HEAD" {
        errors.push(format!("{id}: 304 for a {}", x.method));
    }
    if header(x.request_headers, "if-none-match").is_empty() {
        errors.push(format!("{id}: 304 for a request without If-None-Match"));
    }
    if !x.body.is_empty() {
        errors.push(format!("{id}: 304 with a body"));
    }
    errors
}

fn documented_answer(x: &Exchange<'_>, id: &str, response: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    if response.pointer("/headers/Location").is_some() && header(x.headers, "location").is_empty() {
        errors.push(format!("{id} {}: no Location header", x.status));
    }
    if x.status == 304 {
        errors.extend(not_modified(x, id));
        return errors;
    }
    if x.method == "HEAD" {
        return errors;
    }
    let answered = header(x.headers, "content-type");
    let media = media_type(answered);
    let asked = header(x.request_headers, "accept");
    let accepts = |variant| {
        asked
            .split(',')
            .any(|range| github_media_variant(media_type(range)) == Some(variant))
    };
    let content_success = id == "repos/get-content" && x.status == 200;
    let raw_json = content_success && github_media_variant(media) == Some("raw+json");
    let is_json = !raw_json && is_json(media);
    let schema_type = if content_success && (accepts("object") || accepts("object+json")) {
        "application/vnd.github.object"
    } else {
        "application/json"
    };
    let content = response.get("content").and_then(Value::as_object);
    match content {
        None if x.status >= 400 => errors.extend(error_body(x, id)),
        None => {
            if !x.body.is_empty() {
                errors.push(format!(
                    "{id} {}: a body where the docs give none",
                    x.status
                ));
            }
        }
        Some(content) if is_json => match content.get(schema_type) {
            Some(media) => match serde_json::from_slice::<Value>(x.body) {
                Ok(body) => {
                    if let Some(schema) = media.get("schema")
                        && let Err(e) = openapi_schema(id, x.status, schema, &body)
                    {
                        errors.extend(e.into_iter().map(|e| format!("{id} {}: {e}", x.status)));
                    }
                    if x.status >= 400 {
                        errors.extend(error_body_with(x, id, true));
                    }
                }
                Err(e) => errors.push(format!("{id} {}: the body is not JSON: {e}", x.status)),
            },
            None => errors.push(format!("{id} {}: JSON where the docs give none", x.status)),
        },
        Some(content) => {
            let documented = content.contains_key(media)
                || (accepts("raw")
                    && RAW_BY_CONTENT
                        .iter()
                        .any(|(op, m)| *op == id && m.contains(&media)));
            let custom = asked.split(',').any(|a| {
                let a = media_type(a);
                let same = a == media
                    || github_media_variant(a)
                        .is_some_and(|v| Some(v) == github_media_variant(media));
                same && (raw_json || !a.ends_with("json"))
            });
            if !documented && !custom {
                errors.push(format!(
                    "{id} {}: {answered:?} answered to Accept {asked:?}",
                    x.status
                ));
            }
        }
    }
    errors
}

fn error_body(x: &Exchange<'_>, id: &str) -> Vec<String> {
    error_body_with(x, id, false)
}

fn error_body_with(x: &Exchange<'_>, id: &str, schema_checked: bool) -> Vec<String> {
    let s = x.status;
    let body: Value = match serde_json::from_slice(x.body) {
        Ok(b) => b,
        Err(e) => return vec![format!("{id} {s}: the error body is not JSON: {e}")],
    };
    let mut errors = Vec::new();
    for field in ["message", "documentation_url"] {
        if !body.get(field).is_some_and(Value::is_string) {
            errors.push(format!("{id} {s}: no {field} in the error body"));
        }
    }
    if body.get("status").and_then(Value::as_str) != Some(s.to_string().as_str()) {
        errors.push(format!("{id} {s}: the error body's status is not \"{s}\""));
    }
    if let Some(items) = body.get("errors").and_then(Value::as_array) {
        for (i, item) in items.iter().enumerate() {
            if schema_checked && item.is_string() {
                continue;
            }
            let code = item.get("code").and_then(Value::as_str);
            let mut need = vec!["resource", "code"];
            if code != Some("custom") {
                need.push("field");
            }
            for field in need {
                if !item.get(field).is_some_and(Value::is_string) {
                    errors.push(format!("{id} {s}: errors[{i}] has no {field}"));
                }
            }
        }
    }
    errors
}

fn obj(x: &Exchange<'_>) -> Result<Value, Vec<String>> {
    if is_json(media_type(header(x.headers, "content-type"))) {
        serde_json::from_slice::<Value>(x.body).map_err(|e| vec![format!("not JSON: {e}")])
    } else {
        let text = String::from_utf8_lossy(x.body);
        let map: serde_json::Map<String, Value> = text
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_owned(), Value::String(v.to_owned())))
            .collect();
        Ok(Value::Object(map))
    }
}

fn has(body: &Value, fields: &[&str]) -> Vec<String> {
    fields
        .iter()
        .filter(|f| body.get(**f).is_none_or(Value::is_null))
        .map(|f| format!("no {f}"))
        .collect()
}

fn outside_the_bundle(x: &Exchange<'_>, path: &str) -> Option<Result<Checked, Vec<String>>> {
    let route = |template: &str| format!("{} {template}", x.method);
    let (route, errors) = if path == "/graphql" || x.path == "/api/graphql" {
        (route("/graphql"), Vec::new())
    } else if path.starts_with("/_hubdouble/") || path == "/healthz" || path == "/readyz" {
        (route(path), Vec::new())
    } else if path.starts_with("/_blobs/") {
        (route("/_blobs/{id}"), blob(x))
    } else if path.starts_with("/_actions/") {
        (route(path), actions(x, path))
    } else if path.starts_with("/login/") {
        (route(path), login(x, path))
    } else if path.starts_with("/orgs/") && path.ends_with("/sso") && segments(path) == 3 {
        let errors = if (300..400).contains(&x.status) && header(x.headers, "location").is_empty() {
            vec!["an SSO redirect without Location".to_owned()]
        } else {
            Vec::new()
        };
        (route("/orgs/{org}/sso"), errors)
    } else {
        return None;
    };
    Some(if errors.is_empty() {
        Ok(Checked {
            route,
            status: x.status,
        })
    } else {
        Err(errors
            .into_iter()
            .map(|e| format!("{route}: {e}"))
            .collect())
    })
}

fn segments(path: &str) -> usize {
    path.trim_start_matches('/').split('/').count()
}

fn blob(x: &Exchange<'_>) -> Vec<String> {
    let mut errors = Vec::new();
    match x.status {
        200 if header(x.headers, "content-length").is_empty() => {
            errors.push("no Content-Length".to_owned());
        }
        206 if header(x.headers, "content-range").is_empty() => {
            errors.push("a 206 without Content-Range".to_owned());
        }
        403 if header(x.headers, "x-ms-error-code") != "AuthenticationFailed"
            || header(x.headers, "content-type") != "application/xml" =>
        {
            errors.push("a 403 that is not the storage's AuthenticationFailed".to_owned());
        }
        200 | 206 | 403 | 404 | 416 => {}
        s => errors.push(format!("status {s}")),
    }
    errors
}

fn actions(x: &Exchange<'_>, path: &str) -> Vec<String> {
    if x.status != 200 {
        return Vec::new();
    }
    let body = match obj(x) {
        Ok(b) => b,
        Err(e) => return e,
    };
    if path.ends_with("/.well-known/openid-configuration") {
        has(
            &body,
            &[
                "issuer",
                "jwks_uri",
                "response_types_supported",
                "subject_types_supported",
                "id_token_signing_alg_values_supported",
            ],
        )
    } else if path.ends_with("/.well-known/jwks") {
        match body.get("keys").and_then(Value::as_array) {
            Some(keys) => keys
                .iter()
                .flat_map(|k| has(k, &["kty", "kid", "n", "e"]))
                .collect(),
            None => vec!["no keys".to_owned()],
        }
    } else if path == "/_actions/token" {
        has(&body, &["value"])
    } else {
        Vec::new()
    }
}

fn login(x: &Exchange<'_>, path: &str) -> Vec<String> {
    match (path, x.status) {
        ("/login/oauth/authorize" | "/login/device", 302) => {
            if header(x.headers, "location").is_empty() {
                vec!["a redirect without Location".to_owned()]
            } else {
                Vec::new()
            }
        }
        ("/login/oauth/access_token", 200) => match obj(x) {
            Ok(body) if body.get("error").is_some() => has(&body, &["error"]),
            Ok(body) => has(&body, &["access_token", "token_type", "scope"]),
            Err(e) => e,
        },
        ("/login/device/code", 200) => match obj(x) {
            Ok(body) if body.get("error").is_some() => has(&body, &["error"]),
            Ok(body) => has(
                &body,
                &[
                    "device_code",
                    "user_code",
                    "verification_uri",
                    "expires_in",
                    "interval",
                ],
            ),
            Err(e) => e,
        },
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(method: &str, path: &str) -> Option<&'static str> {
        resolve_operation(method, path).map(|o| o.id)
    }

    #[test]
    fn the_description_alone_resolves_github_routes() {
        assert_eq!(
            id("GET", "/repos/o/r/issues/comments/7"),
            Some("issues/get-comment")
        );
        assert_eq!(
            id("GET", "/repos/o/r/issues/comments/events"),
            Some("issues/get-comment")
        );
        assert_eq!(
            id("GET", "/repos/o/r/issues/7/events"),
            Some("issues/list-events")
        );
        assert_eq!(
            id("GET", "/repos/o/r/commits/feat/a/status"),
            Some("repos/get-combined-status-for-ref")
        );
        assert_eq!(
            id("GET", "/repos/o/r/commits/feat/a"),
            Some("repos/get-commit")
        );
        assert_eq!(
            id("GET", "/repos/o/r/commits/abc/pulls"),
            Some("repos/list-pull-requests-associated-with-commit")
        );
        assert_eq!(
            id("GET", "/repos/o/r/branches/feature%2Fprotection"),
            Some("repos/get-branch")
        );
        assert_eq!(
            id("GET", "/repos/o/r/branches/main/protection"),
            Some("repos/get-branch-protection")
        );
        assert_eq!(
            id("GET", "/repos/o/r/git/ref/heads/feat/a"),
            Some("git/get-ref")
        );
        assert_eq!(
            id("GET", "/repos/o/r/contents/docs/a.md"),
            Some("repos/get-content")
        );
        assert_eq!(id("GET", "/repos/o/r/contents/"), Some("repos/get-content"));
        assert_eq!(
            id("GET", "/repos/o/r/git/matching-refs/tags/v/"),
            Some("git/list-matching-refs")
        );
        assert_eq!(id("GET", "/repos/o/r/git/matching-refs//v"), None);
        assert_eq!(id("DELETE", "/repos/o/r/issues"), None);
        assert_eq!(id("GET", "/repos/o/r/nope"), None);
        assert_eq!(id("GET", "/repos/o/r/issues/"), None);
    }

    fn exchange<'a>(
        method: &'a str,
        path: &'a str,
        status: u16,
        headers: &'a HeaderMap,
        body: &'a [u8],
    ) -> Exchange<'a> {
        static EMPTY: LazyLock<HeaderMap> = LazyLock::new(HeaderMap::new);
        Exchange {
            method,
            path,
            query: "",
            request_headers: &EMPTY,
            request_body: b"",
            status,
            headers,
            body,
        }
    }

    fn github_headers() -> HeaderMap {
        let mut h = HeaderMap::new();
        for (n, v) in [
            ("x-github-request-id", "0000:0001:000001:000001:00000000"),
            ("x-github-media-type", "github.v3; format=json"),
            ("x-github-api-version-selected", "2022-11-28"),
            ("x-ratelimit-limit", "5000"),
            ("x-ratelimit-remaining", "4999"),
            ("x-ratelimit-reset", "1"),
            ("x-ratelimit-used", "1"),
            ("x-ratelimit-resource", "core"),
            ("content-type", "application/json; charset=utf-8"),
        ] {
            h.insert(n, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn statuses_come_from_the_operation_or_the_prose() {
        let h = github_headers();
        let not_found = br#"{"message":"Not Found","documentation_url":"https://docs.github.com/rest","status":"404"}"#;
        let yes = |_: &str| true;
        let no = |_: &str| false;
        assert!(
            check(
                &exchange("GET", "/repos/o/r/git/ref/heads/x", 404, &h, not_found),
                &yes
            )
            .is_ok()
        );
        assert!(
            check(
                &exchange("GET", "/repos/o/r/actions/runs/1", 404, &h, not_found),
                &yes
            )
            .is_ok()
        );
        let teapot = br#"{"message":"x","documentation_url":"d","status":"418"}"#;
        assert!(
            check(
                &exchange("GET", "/repos/o/r/actions/runs/1", 418, &h, teapot),
                &yes
            )
            .is_err()
        );
        let ni = br#"{"message":"x","documentation_url":"d","status":"501"}"#;
        assert!(check(&exchange("GET", "/zen", 501, &h, ni), &no).is_ok());
        assert!(check(&exchange("GET", "/zen", 501, &h, ni), &yes).is_err());
        let bare = br#"{"message":"Not Found"}"#;
        assert!(
            check(
                &exchange("GET", "/repos/o/r/actions/runs/1", 404, &h, bare),
                &yes
            )
            .is_err()
        );
        let field_less = br#"{"message":"Validation Failed","documentation_url":"d","status":"422","errors":[{"resource":"Issue","code":"missing_field"}]}"#;
        assert!(
            check(
                &exchange("POST", "/repos/o/r/issues", 422, &h, field_less),
                &yes
            )
            .is_err()
        );
    }

    #[test]
    fn a_body_must_match_its_schema_and_every_answer_carries_githubs_headers() {
        let h = github_headers();
        let yes = |_: &str| true;
        let ok = check(&exchange("GET", "/repos/o/r/labels", 200, &h, b"[]"), &yes);
        assert_eq!(
            ok,
            Ok(Checked {
                route: "GET /repos/{owner}/{repo}/labels".to_owned(),
                status: 200
            })
        );
        assert!(check(&exchange("GET", "/repos/o/r/labels", 200, &h, b"{}"), &yes).is_err());
        let mut bare = h.clone();
        bare.remove("x-github-request-id");
        assert!(
            check(
                &exchange("GET", "/repos/o/r/labels", 200, &bare, b"[]"),
                &yes
            )
            .is_err()
        );
        let created = check(
            &exchange("POST", "/repos/o/r/labels", 201, &h, br"{}"),
            &yes,
        );
        let errors = created.unwrap_err();
        assert!(errors.iter().any(|e| e.contains("Location")), "{errors:?}");
        let empty = HeaderMap::new();
        let unknown = exchange("GET", "/repos/o/r/nope", 404, &empty, b"");
        assert!(check(&unknown, &yes).is_err());
    }

    #[test]
    fn an_answer_has_the_selected_rest_api_version() {
        let mut headers = github_headers();
        let yes = |_: &str| true;
        headers.remove("x-github-api-version-selected");
        let missing = exchange("GET", "/repos/o/r/labels", 200, &headers, b"[]");
        assert!(check(&missing, &yes).is_err());
        headers.insert(
            "x-github-api-version-selected",
            "2022-11-28".parse().unwrap(),
        );
        let request_headers = HeaderMap::from_iter([(
            http::HeaderName::from_static("x-github-api-version"),
            "2026-03-10".parse().unwrap(),
        )]);
        let wrong = Exchange {
            request_headers: &request_headers,
            ..exchange("GET", "/repos/o/r/labels", 200, &headers, b"[]")
        };
        assert!(check(&wrong, &yes).is_err());
        headers.insert(
            "x-github-api-version-selected",
            "2026-03-10".parse().unwrap(),
        );
        let right = Exchange {
            request_headers: &request_headers,
            ..exchange("GET", "/repos/o/r/labels", 200, &headers, b"[]")
        };
        assert!(check(&right, &yes).is_ok());
    }

    #[test]
    fn by_id_aliases_check_as_their_operation_and_links_take_githubs_by_id_form() {
        // Given api.github.com's Link forms, read from live answers on cli/cli and cli
        let yes = |_: &str| true;
        let with_link = |link: &str| {
            let mut h = github_headers();
            h.insert("link", http::HeaderValue::from_str(link).unwrap());
            h
        };
        let by_id = with_link(
            r#"<https://api.github.com/repositories/212613049/labels?per_page=1&page=2>; rel="next""#,
        );
        let by_name =
            with_link(r#"<https://api.github.com/repos/o/r/labels?per_page=1&page=2>; rel="next""#);
        let org_by_id = with_link(
            r#"<https://api.github.com/organizations/59704711/repos?per_page=1&page=2>; rel="next""#,
        );
        let other_tail = with_link(
            r#"<https://api.github.com/repositories/212613049/issues?per_page=1&page=2>; rel="next""#,
        );
        // When
        let listed = check(
            &exchange("GET", "/repos/o/r/labels", 200, &by_id, b"[]"),
            &yes,
        );
        let aliased = check(
            &exchange("GET", "/repositories/212613049/labels", 200, &by_id, b"[]"),
            &yes,
        );
        let org = check(
            &exchange("GET", "/orgs/o/repos", 200, &org_by_id, b"[]"),
            &yes,
        );
        // Then
        let labels = Checked {
            route: "GET /repos/{owner}/{repo}/labels".to_owned(),
            status: 200,
        };
        assert_eq!(listed, Ok(labels.clone()));
        assert_eq!(aliased, Ok(labels));
        assert!(org.is_ok(), "{org:?}");
        assert!(
            check(
                &exchange("GET", "/repos/o/r/labels", 200, &by_name, b"[]"),
                &yes
            )
            .is_err()
        );
        assert!(
            check(
                &exchange("GET", "/repos/o/r/labels", 200, &other_tail, b"[]"),
                &yes
            )
            .is_err()
        );
        assert!(check(&exchange("GET", "/orgs/o/repos", 200, &by_id, b"[]"), &yes).is_err());
    }
}

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use serde_json::Value;

use hubdouble_admin::Services;
use hubdouble_auth::{authenticate, can_see};
use hubdouble_model::Repo;
use hubdouble_store::{Accounts, Repos};

use crate::TestServer;
use crate::answer::{self, Checked, Exchange};
use crate::coverage::{self, Case, Recorder};

/// go-github v93's `User-Agent` (`github/github.go:47`, `Version` at `:33`).
pub const USER_AGENT: &str = "go-github/v93.0.0";
/// go-github v93's `Accept` on every request (`mediaTypeV3`, `github/github.go:56`, set at `:1026`).
pub const ACCEPT: &str = "application/vnd.github.v3+json";
/// go-github v93's default `X-Github-Api-Version` (`api20221128`, `github/github.go:43`, `:584`).
pub const API_VERSION: &str = "2022-11-28";

/// An answer, read whole.
#[derive(Debug, Clone)]
pub struct Answer {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Answer {
    /// # Panics
    ///
    /// When the body is not JSON.
    #[must_use]
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "the {} answer is not JSON ({e}): {}",
                self.status,
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    /// A header as text, empty when it is missing.
    #[must_use]
    pub fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }
}

pub(crate) async fn send(request: reqwest::RequestBuilder) -> Answer {
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("send the request: {e:?}"));
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .bytes()
        .await
        .unwrap_or_else(|e| panic!("read the answer: {e}"));
    Answer {
        status,
        headers,
        body,
    }
}

/// Checks one answer with [`crate::answer::check`] and records it for the inventory gate, for
/// a test that sends a request without [`GoGithub`]. `registered` says whether hubdouble
/// implements an operation.
///
/// # Panics
///
/// When the answer differs from GitHub's docs.
pub fn check_answer(x: &Exchange<'_>, registered: &dyn Fn(&str) -> bool) {
    checked(x, registered, &Recorder::from_process_env(), None);
}

fn checked(
    x: &Exchange<'_>,
    registered: &dyn Fn(&str) -> bool,
    recorder: &Recorder,
    services: Option<&Services>,
) {
    match answer::check(x, registered) {
        Ok(c) => recorder.record(&c, case(x, &c, services), &coverage::facts(x)),
        Err(errors) => panic!(
            "{} {} answered {}, which differs from GitHub's docs:\n  {}\nbody: {}",
            x.method,
            x.path,
            x.status,
            errors.join("\n  "),
            String::from_utf8_lossy(x.body)
        ),
    }
}

fn case(x: &Exchange<'_>, c: &Checked, services: Option<&Services>) -> Case {
    match x.status {
        200..=399 => Case::Success,
        403 if x.headers.contains_key("x-accepted-github-permissions") => Case::MissingPermission,
        422 => Case::Invalid,
        404 if services.is_some_and(|s| names_hidden_repository(s, &c.route, x)) => {
            Case::UnseenRepository
        }
        _ => Case::Other,
    }
}

fn names_hidden_repository(services: &Services, route: &str, x: &Exchange<'_>) -> bool {
    let parts: Vec<&str> = answer::api_path(x.path)
        .trim_start_matches('/')
        .split('/')
        .collect();
    let hidden = |find: &dyn Fn(&hubdouble_store::Tx) -> Option<Arc<Repo>>| {
        services
            .store
            .view(|tx| {
                let actor = authenticate(tx, &*services.clock, x.request_headers)?;
                Ok(find(tx).is_some_and(|repo| !can_see(tx, &actor, &repo)))
            })
            .unwrap_or(false)
    };
    if parts.first() == Some(&"repositories") {
        return parts
            .get(1)
            .and_then(|id| id.parse::<i64>().ok())
            .is_some_and(|id| hidden(&|tx| tx.table::<Repos>().get(id).cloned()));
    }
    let template: Vec<&str> = route
        .split_once(' ')
        .map_or("", |(_, t)| t)
        .trim_start_matches('/')
        .split('/')
        .collect();
    let at = |name: &str| {
        template
            .iter()
            .position(|s| *s == name)
            .and_then(|i| parts.get(i).copied())
    };
    let (Some(owner), Some(repo)) = (at("{owner}"), at("{repo}")) else {
        return false;
    };
    hidden(&|tx| {
        tx.table::<Accounts>()
            .by_login(owner)
            .and_then(|o| tx.table::<Repos>().by_name(o.id, repo))
            .cloned()
    })
}

/// A client that sends what go-github v93's `Client.NewRequest` sends: `Accept`, `User-Agent`,
/// `X-Github-Api-Version`, `Authorization: Bearer <token>` (`github/github.go:650`), and a JSON
/// body written by Go's `json.Encoder`, which ends it with a newline.
///
/// Every answer goes through [`crate::answer::check`], and a test fails on the first that
/// differs from GitHub's docs. Redirects are not followed, so each answer is checked against
/// the operation that gave it; a test follows a `Location` with another call.
#[derive(Clone)]
pub struct GoGithub {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
    registered: Arc<HashSet<&'static str>>,
    recorder: Recorder,
    services: Arc<Services>,
}

impl GoGithub {
    /// A client whose base URL is the server root, as `github.NewClient` with a custom base URL.
    ///
    /// # Panics
    ///
    /// When the HTTP client cannot be built.
    #[must_use]
    pub fn new(server: &TestServer) -> GoGithub {
        GoGithub {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_else(|e| panic!("build the HTTP client: {e}")),
            base: format!("{}/", server.base),
            token: None,
            registered: server.registered.clone(),
            recorder: server.recorder.clone(),
            services: server.server.services().clone(),
        }
    }

    /// A client whose base URL ends in `/api/v3/`, as `WithEnterpriseURLs` makes it.
    #[must_use]
    pub fn enterprise(server: &TestServer) -> GoGithub {
        GoGithub {
            base: format!("{}/api/v3/", server.base),
            ..GoGithub::new(server)
        }
    }

    #[must_use]
    pub fn with_token(mut self, token: &str) -> GoGithub {
        self.token = Some(token.to_owned());
        self
    }

    /// The base URL, with its trailing `/`.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let mut request = self
            .http
            .request(
                method,
                format!("{}{}", self.base, path.trim_start_matches('/')),
            )
            .header(http::header::ACCEPT, ACCEPT)
            .header(http::header::USER_AGENT, USER_AGENT)
            .header("X-Github-Api-Version", API_VERSION);
        if let Some(token) = &self.token {
            request = request.header(http::header::AUTHORIZATION, format!("Bearer {token}"));
        }
        request
    }

    async fn exchange(&self, request: reqwest::RequestBuilder) -> Answer {
        let (http, request) = request.build_split();
        let request = request.unwrap_or_else(|e| panic!("build the request: {e}"));
        let method = request.method().as_str().to_owned();
        let path = request.url().path().to_owned();
        let query = request.url().query().unwrap_or("").to_owned();
        let request_headers = request.headers().clone();
        let request_body = request
            .body()
            .and_then(reqwest::Body::as_bytes)
            .map_or_else(Bytes::new, Bytes::copy_from_slice);
        let answer = send(reqwest::RequestBuilder::from_parts(http, request)).await;
        checked(
            &Exchange {
                method: &method,
                path: &path,
                query: &query,
                request_headers: &request_headers,
                request_body: &request_body,
                status: answer.status.as_u16(),
                headers: &answer.headers,
                body: &answer.body,
            },
            &|id| self.registered.contains(id),
            &self.recorder,
            Some(&self.services),
        );
        answer
    }

    /// Sends a request to `path`, relative to the base URL and already escaped as go-github
    /// escapes it (see [`path_escape`], [`ref_escape`], [`contents_path`]), without a body.
    pub async fn call(&self, method: Method, path: &str) -> Answer {
        self.exchange(self.request(method, path)).await
    }

    /// As [`GoGithub::call`], with `headers` set over go-github's own, as a go-github method
    /// that sets its own `Accept` or a raw call with its own headers sends them.
    ///
    /// # Panics
    ///
    /// When the request cannot be built or the answer differs from GitHub's docs.
    pub async fn call_headers(&self, method: Method, path: &str, headers: HeaderMap) -> Answer {
        let (http, request) = self.request(method, path).build_split();
        let mut request = request.unwrap_or_else(|e| panic!("build the request: {e}"));
        for (name, value) in headers {
            if let Some(name) = name {
                request.headers_mut().insert(name, value);
            }
        }
        self.exchange(reqwest::RequestBuilder::from_parts(http, request))
            .await
    }

    /// Sends a request with `body` encoded as go-github encodes it.
    ///
    /// # Panics
    ///
    /// When `body` cannot be encoded.
    pub async fn call_json(&self, method: Method, path: &str, body: &Value) -> Answer {
        let mut bytes = serde_json::to_vec(body).unwrap_or_else(|e| panic!("encode the body: {e}"));
        bytes.push(b'\n');
        let request = self
            .request(method, path)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(bytes);
        self.exchange(request).await
    }

    pub async fn get(&self, path: &str) -> Answer {
        self.call(Method::GET, path).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Answer {
        self.call_json(Method::POST, path, body).await
    }
}

fn escape(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'_' | b'.' | b'~' | b'$' | b'&' | b'+' | b'=' | b':' | b'@'
            )
            || (keep_slash && matches!(b, b'/' | b';' | b','));
        if keep {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Go's `url.PathEscape`, which go-github applies to a branch name (`github/repos.go:1426`):
/// `/` becomes `%2F`.
#[must_use]
pub fn path_escape(s: &str) -> String {
    escape(s, false)
}

/// go-github's `refURLEscape` (`github/git_refs.go:77`): each segment path escaped, `/` kept.
#[must_use]
pub fn ref_escape(r: &str) -> String {
    r.split('/').map(path_escape).collect::<Vec<_>>().join("/")
}

/// How `Repositories.GetContents` escapes a file path (`github/repos_contents.go:231`):
/// `(&url.URL{Path: path}).String()`, which keeps `/`, `;` and `,`.
#[must_use]
pub fn contents_path(p: &str) -> String {
    escape(p.trim_end_matches('/'), true)
}

/// A GraphQL client that posts `{"query", "variables"}` to `graphql` under the base URL, as
/// go-github's `NewRequest("POST", "graphql", ...)` does.
#[derive(Clone)]
pub struct Graphql {
    client: GoGithub,
}

impl Graphql {
    #[must_use]
    pub fn new(client: GoGithub) -> Graphql {
        Graphql { client }
    }

    /// Runs `query` with `variables`.
    ///
    /// # Panics
    ///
    /// When the response does not match GitHub's GraphQL API.
    pub async fn query(&self, query: &str, variables: &Value) -> Answer {
        let body = serde_json::json!({ "query": query, "variables": variables });
        let answer = self.client.post("graphql", &body).await;
        crate::graphql_answer::check(query, variables, &answer);
        answer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_follows_go() {
        assert_eq!(path_escape("feature/protection"), "feature%2Fprotection");
        assert_eq!(path_escape("a b?#%"), "a%20b%3F%23%25");
        assert_eq!(path_escape("v1.0+x@y"), "v1.0+x@y");
        assert_eq!(ref_escape("heads/feat/a b"), "heads/feat/a%20b");
        assert_eq!(contents_path("docs/a b;c,d.md/"), "docs/a%20b;c,d.md");
        assert_eq!(contents_path("a?b"), "a%3Fb");
    }
}

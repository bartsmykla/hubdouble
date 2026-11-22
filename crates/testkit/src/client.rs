use std::fmt::Write as _;

use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use serde_json::Value;

use crate::TestServer;

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
        .unwrap_or_else(|e| panic!("send the request: {e}"));
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

/// A client that sends what go-github v93's `Client.NewRequest` sends: `Accept`, `User-Agent`,
/// `X-Github-Api-Version`, `Authorization: Bearer <token>` (`github/github.go:650`), and a JSON
/// body written by Go's `json.Encoder`, which ends it with a newline.
#[derive(Clone)]
pub struct GoGithub {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

impl GoGithub {
    /// A client whose base URL is the server root, as `github.NewClient` with a custom base URL.
    #[must_use]
    pub fn new(server: &TestServer) -> GoGithub {
        GoGithub {
            http: reqwest::Client::new(),
            base: format!("{}/", server.base),
            token: None,
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

    /// A request to `path`, relative to the base URL and already escaped as go-github escapes it
    /// (see [`path_escape`], [`ref_escape`], [`contents_path`]).
    pub fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
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

    /// Sends a request without a body.
    pub async fn call(&self, method: Method, path: &str) -> Answer {
        send(self.request(method, path)).await
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
        send(request).await
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
    pub async fn query(&self, query: &str, variables: &Value) -> Answer {
        let body = serde_json::json!({ "query": query, "variables": variables });
        self.client.post("graphql", &body).await
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

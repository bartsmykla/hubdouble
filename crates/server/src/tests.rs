use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http::header::{self, HeaderMap};
use http::{Method, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hubdouble_admin::Registrar;
use hubdouble_model::{ApiError, Config, ManualClock, Requirement, Timestamp};
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::{Blob, Blobs};
use hyper::client::conn::http1::{SendRequest, handshake};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::{Registries, Server};

fn root(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    Reply::json(&json!({ "current_user_url": format!("{}/user", ctx.urls().api) }))
}

#[expect(clippy::unnecessary_wraps, reason = "a handler returns a Result")]
fn zen(_: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    Ok(Reply::raw(
        "text/plain;charset=utf-8",
        "Keep it logically awesome.",
    ))
}

fn boom(_: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    panic!("the handler fails")
}

#[expect(clippy::unnecessary_wraps, reason = "a handler returns a Result")]
fn bearer(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    Ok(Reply::raw(
        "text/plain",
        ctx.header("authorization").unwrap_or_default().to_owned(),
    ))
}

fn register(r: &mut Registrar<'_>) {
    r.rest.extra_authenticating(
        Method::GET,
        "/_test/own-bearer",
        "test/own-bearer",
        Requirement::public(),
        bearer,
    );
    r.rest.extra(
        Method::GET,
        "/_test/github-bearer",
        "test/github-bearer",
        Requirement::public(),
        bearer,
    );
    r.rest.handle("meta/root", Requirement::public(), root);
    r.rest.handle("meta/get-zen", Requirement::public(), zen);
    r.rest
        .handle("meta/get-octocat", Requirement::public(), boom);
}

struct Running {
    addr: SocketAddr,
    server: Server,
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

async fn start() -> Running {
    let config = Config {
        addr: "127.0.0.1:0".parse().unwrap(),
        base_url: "http://hubdouble.test".to_owned(),
        max_body_bytes: 64,
        ..Config::default()
    };
    let clock = Arc::new(ManualClock::new(Timestamp::SEED));
    let server = Server::start(config, clock, &|_| None, Registries::new(register)).unwrap();
    let listener = server.bind().await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let serving = server.clone();
    let task = tokio::spawn(async move {
        serving
            .serve(listener, async {
                let _ = stopped.await;
            })
            .await;
    });
    Running {
        addr,
        server,
        stop,
        task,
    }
}

async fn connect(addr: SocketAddr) -> SendRequest<Full<Bytes>> {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (sender, conn) = handshake(TokioIo::new(stream)).await.unwrap();
    tokio::spawn(conn);
    sender
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }

    fn header(&self, name: &str) -> &str {
        self.headers.get(name).map_or("", |v| v.to_str().unwrap())
    }
}

async fn send(
    sender: &mut SendRequest<Full<Bytes>>,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: &'static [u8],
) -> Answer {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "hubdouble.test");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = sender
        .send_request(request.body(Full::new(Bytes::from_static(body))).unwrap())
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    Answer {
        status: parts.status,
        headers: parts.headers,
        body: body.collect().await.unwrap().to_bytes(),
    }
}

async fn get(addr: SocketAddr, path: &str, headers: &[(&str, &str)]) -> Answer {
    send(&mut connect(addr).await, Method::GET, path, headers, b"").await
}

#[tokio::test]
async fn health_and_readiness_answer_ok() {
    let s = start().await;
    for path in ["/healthz", "/readyz"] {
        assert_eq!(
            get(s.addr, path, &[]).await.status,
            StatusCode::OK,
            "{path}"
        );
    }
}

#[tokio::test]
async fn only_a_self_authenticating_route_takes_a_bearer_github_does_not_know() {
    let s = start().await;
    let runtime = [("authorization", "Bearer actions-runtime-token")];
    let own = get(s.addr, "/_test/own-bearer", &runtime).await;
    assert_eq!(own.status, StatusCode::OK);
    assert_eq!(own.body, "Bearer actions-runtime-token");
    let github = get(s.addr, "/_test/github-bearer", &runtime).await;
    assert_eq!(github.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_unregistered_operation_answers_501_naming_it() {
    // Given a server where no track registers pulls/list
    let s = start().await;
    // When a client lists pull requests
    let a = get(s.addr, "/repos/o/r/pulls?state=open", &[]).await;
    // Then the answer is GitHub's error shape with 501, and it names the operation
    assert_eq!(a.status, StatusCode::NOT_IMPLEMENTED);
    let body = a.json();
    assert_eq!(body["status"], "501");
    assert!(body["documentation_url"].is_string());
    assert!(body["message"].as_str().unwrap().contains("pulls/list"));
}

#[tokio::test]
async fn a_path_no_operation_matches_answers_404_in_github_shape() {
    let s = start().await;
    let a = get(s.addr, "/no/such/route/here", &[]).await;
    assert_eq!(a.status, StatusCode::NOT_FOUND);
    assert_eq!(a.json()["status"], "404");
    assert!(a.header("content-type").starts_with("application/json"));
}

#[tokio::test]
async fn the_api_version_header_selects_a_published_version() {
    let s = start().await;
    let default = get(s.addr, "/zen", &[]).await;
    assert_eq!(
        default.header("x-github-api-version-selected"),
        "2022-11-28"
    );
    let newer = get(s.addr, "/zen", &[("X-GitHub-Api-Version", "2026-03-10")]).await;
    assert_eq!(newer.status, StatusCode::OK);
    assert_eq!(newer.header("x-github-api-version-selected"), "2026-03-10");
    let unknown = get(s.addr, "/zen", &[("X-GitHub-Api-Version", "2019-01-01")]).await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    assert_eq!(unknown.json()["status"], "400");
}

#[tokio::test]
async fn a_matching_if_none_match_answers_304() {
    // Given a first GET that returns a weak ETag
    let s = start().await;
    let first = get(s.addr, "/zen", &[]).await;
    assert_eq!(first.status, StatusCode::OK);
    let tag = first.header("etag").to_owned();
    assert!(tag.starts_with("W/\""));
    // When the client sends it back
    let again = get(s.addr, "/zen", &[("If-None-Match", &tag)]).await;
    // Then the answer is 304 with no body and the same tag
    assert_eq!(again.status, StatusCode::NOT_MODIFIED);
    assert!(again.body.is_empty());
    assert_eq!(again.header("etag"), tag);
    // And another media type gets another tag
    let raw = get(
        s.addr,
        "/zen",
        &[
            ("Accept", "application/vnd.github.raw"),
            ("If-None-Match", &tag),
        ],
    )
    .await;
    assert_eq!(raw.status, StatusCode::OK);
    assert_ne!(raw.header("etag"), tag);
}

#[tokio::test]
async fn every_api_answer_has_githubs_headers() {
    let s = start().await;
    for a in [
        get(s.addr, "/zen", &[]).await,
        get(s.addr, "/repos/o/r/pulls", &[]).await,
    ] {
        assert_ne!(a.header("x-github-request-id"), "");
        assert_eq!(a.header("x-github-media-type"), "github.v3; format=json");
        assert_eq!(a.header("x-ratelimit-limit"), "5000");
        let remaining: u32 = a.header("x-ratelimit-remaining").parse().unwrap();
        assert!(remaining > 0);
        assert_ne!(a.header("x-ratelimit-reset"), "");
    }
}

#[tokio::test]
async fn the_api_v3_prefix_serves_the_same_api_with_its_own_urls() {
    let s = start().await;
    let plain = get(s.addr, "/", &[]).await.json();
    assert_eq!(plain["current_user_url"], "http://hubdouble.test/user");
    let v3 = get(s.addr, "/api/v3/", &[]).await.json();
    assert_eq!(v3["current_user_url"], "http://hubdouble.test/api/v3/user");
    assert_eq!(get(s.addr, "/api/v3/zen", &[]).await.status, StatusCode::OK);
}

#[tokio::test]
async fn a_panicking_handler_answers_500_and_the_connection_stays_open() {
    let s = start().await;
    let mut conn = connect(s.addr).await;
    let failed = send(&mut conn, Method::GET, "/octocat", &[], b"").await;
    assert_eq!(failed.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(failed.json()["status"], "500");
    let next = send(&mut conn, Method::GET, "/zen", &[], b"").await;
    assert_eq!(next.status, StatusCode::OK);
}

#[tokio::test]
async fn a_body_over_the_limit_answers_413() {
    let s = start().await;
    let mut conn = connect(s.addr).await;
    let a = send(&mut conn, Method::POST, "/markdown", &[], &[b'x'; 100]).await;
    assert_eq!(a.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(a.json()["status"], "413");
}

#[tokio::test]
async fn graphql_paths_reach_the_graphql_endpoint() {
    let s = start().await;
    for path in ["/graphql", "/api/graphql", "/api/v3/graphql"] {
        let a = send(
            &mut connect(s.addr).await,
            Method::POST,
            path,
            &[],
            b"{not json",
        )
        .await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST, "{path}");
        assert_eq!(a.header("x-ratelimit-resource"), "graphql", "{path}");
    }
    assert_eq!(
        get(s.addr, "/graphql", &[]).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn blobs_answer_head_and_ranges() {
    // Given a stored 10-byte blob
    let s = start().await;
    s.server
        .services()
        .store
        .update(|tx| {
            let blob = Blob {
                content_type: "text/plain".to_owned(),
                data: Bytes::from_static(b"0123456789"),
            };
            tx.table_mut::<Blobs>().insert("log-1".to_owned(), blob);
            Ok(())
        })
        .unwrap();
    let mut conn = connect(s.addr).await;
    // When the client asks for its size, for part of it and for a range past its end
    let head = send(&mut conn, Method::HEAD, "/_blobs/log-1", &[], b"").await;
    let slice = send(
        &mut conn,
        Method::GET,
        "/_blobs/log-1",
        &[("Range", "bytes=2-4")],
        b"",
    )
    .await;
    let past = send(
        &mut conn,
        Method::GET,
        "/_blobs/log-1",
        &[("Range", "bytes=20-")],
        b"",
    )
    .await;
    let missing = send(&mut conn, Method::GET, "/_blobs/none", &[], b"").await;
    // Then HEAD gives the length, a range gives 206 with that part, and past the end gives 416
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.header("content-length"), "10");
    assert!(head.body.is_empty());
    assert_eq!(slice.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(&slice.body[..], b"234");
    assert_eq!(slice.header("content-range"), "bytes 2-4/10");
    assert_eq!(past.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(past.header("content-range"), "bytes */10");
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn shutdown_stops_the_server() {
    let s = start().await;
    assert_eq!(get(s.addr, "/zen", &[]).await.status, StatusCode::OK);
    s.stop.send(()).unwrap();
    s.task.await.unwrap();
    assert!(TcpStream::connect(s.addr).await.is_err());
}

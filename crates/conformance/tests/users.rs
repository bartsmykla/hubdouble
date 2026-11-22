use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;

use http::{Method, StatusCode};
use hubdouble_admin::Registrar;
use hubdouble_gql::{ErrorType, GqlError, Resolved};
use hubdouble_model::{
    Access, Account, AccountKind, ApiError, Config, ManualClock, Requirement, Scope, Timestamp,
    Token, TokenKind, node_id,
};
use hubdouble_rest::Reply;
use hubdouble_server::{Registries, Server};
use hubdouble_store::{Accounts, Tokens};
use hubdouble_testkit::{Answer, GoGithub, Graphql, TestServer, fixture};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const CLIENT_ID: &str = "Iv1.signin";
const CLIENT_SECRET: &str = "signin-secret";
const CALLBACK: &str = "https://agent.example/callback";
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const INSTALLATION_TOKEN: &str = "ghs_users00000000000000000000000000000000";

fn org_repos(r: &mut Registrar<'_>) {
    r.rest.handle(
        "repos/list-for-org",
        Requirement {
            any_of: &[],
            scope: Scope::Org,
            scopes: &[],
        },
        |_| Reply::json(&Vec::<Value>::new()),
    );
}

fn bot_standin(r: &mut Registrar<'_>) {
    r.gql.object::<Account>("Bot");
    r.gql.node(node_id::BOT, "Bot", |ctx, id| {
        ctx.tx
            .table::<Accounts>()
            .get(id)
            .map(|a| Resolved::Object {
                type_name: "Bot",
                value: a.clone(),
            })
            .ok_or_else(|| GqlError::new(ErrorType::NotFound, "No such node"))
    });
}

fn signin_entries() -> Vec<Value> {
    vec![
        json!({"kind": "user", "id": 1010, "login": "dave", "verified_email": false}),
        json!({"kind": "app", "id": 4010, "slug": "signin-app", "owner": fixture::ORG,
               "client_id": CLIENT_ID, "client_secret": CLIENT_SECRET,
               "public_key": fixture::APP_PUBLIC_KEY, "device_flow": true}),
        json!({"kind": "app", "id": 4011, "slug": "no-device", "owner": fixture::ORG,
               "client_id": "Iv1.nodevice", "client_secret": "nodevice-secret",
               "public_key": fixture::APP_PUBLIC_KEY}),
        json!({"kind": "oauth", "default_user": fixture::ALICE}),
    ]
}

async fn start() -> TestServer {
    let mut builder = TestServer::builder()
        .register(hubdouble_api_users::register)
        .register(org_repos)
        .register(bot_standin)
        .fixture();
    for entry in signin_entries() {
        builder = builder.entry(entry);
    }
    builder.start().await
}

struct Raw {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Raw {
    fn header(&self, name: &str) -> &str {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map_or("", |(_, v)| v.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }

    fn location_query(&self, name: &str) -> String {
        let location = self.header("location");
        let query = location.split_once('?').map_or("", |(_, q)| q);
        hubdouble_rest::Query::parse(query)
            .get(name)
            .unwrap_or_else(|| panic!("no {name} in {location}"))
            .to_owned()
    }
}

async fn raw(
    addr: SocketAddr,
    method: &str,
    target: &str,
    headers_sent: &[(&str, &str)],
    body: &str,
) -> Raw {
    let headers = headers_sent;
    let body_sent = body;
    let mut stream = TcpStream::connect(addr)
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"));
    let mut request = format!(
        "{method} {target} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        let _ = write!(request, "{name}: {value}\r\n");
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream
        .write_all(request.as_bytes())
        .await
        .unwrap_or_else(|e| panic!("write: {e}"));
    let mut answer = String::new();
    stream
        .read_to_string(&mut answer)
        .await
        .unwrap_or_else(|e| panic!("read: {e}"));
    let (head, body) = answer
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("no head: {answer}"));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status: {head}"));
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(": "))
        .map(|(n, v)| (n.to_owned(), v.to_owned()))
        .collect();
    let header_map = |pairs: &mut dyn Iterator<Item = (&str, &str)>| {
        pairs
            .filter_map(|(n, v)| {
                Some((
                    http::HeaderName::from_bytes(n.as_bytes()).ok()?,
                    http::HeaderValue::from_str(v).ok()?,
                ))
            })
            .collect::<http::HeaderMap>()
    };
    hubdouble_testkit::check_answer(
        &hubdouble_testkit::answer::Exchange {
            method,
            path: target.split('?').next().unwrap_or(target),
            query: target.split_once('?').map_or("", |(_, q)| q),
            request_headers: &header_map(&mut headers_sent.iter().copied()),
            request_body: body_sent.as_bytes(),
            status,
            headers: &header_map(&mut headers.iter().map(|(n, v)| (n.as_str(), v.as_str()))),
            body: body.as_bytes(),
        },
        &|_| true,
    );
    Raw {
        status,
        headers,
        body: body.to_owned(),
    }
}

fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", hubdouble_render::escape(v)))
        .collect::<Vec<_>>()
        .join("&")
}

async fn post_form(addr: SocketAddr, path: &str, pairs: &[(&str, &str)]) -> Raw {
    raw(
        addr,
        "POST",
        path,
        &[
            ("Content-Type", "application/x-www-form-urlencoded"),
            ("Accept", "application/json"),
        ],
        &form(pairs),
    )
    .await
}

fn authorize_path(state: &str, login: Option<&str>) -> String {
    let mut pairs = vec![
        ("client_id", CLIENT_ID),
        ("redirect_uri", CALLBACK),
        ("state", state),
        ("code_challenge", CHALLENGE),
        ("code_challenge_method", "S256"),
    ];
    if let Some(login) = login {
        pairs.push(("login", login));
    }
    format!("/login/oauth/authorize?{}", form(&pairs))
}

async fn authorize(addr: SocketAddr, login: Option<&str>) -> Raw {
    raw(addr, "GET", &authorize_path("st-1", login), &[], "").await
}

async fn exchange(addr: SocketAddr, code: &str, verifier: &str, redirect: &str) -> Raw {
    post_form(
        addr,
        "/login/oauth/access_token",
        &[
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect),
            ("client_id", CLIENT_ID),
            ("client_secret", CLIENT_SECRET),
        ],
    )
    .await
}

async fn sign_in(server: &TestServer, login: &str) -> Value {
    let code = authorize(server.addr, Some(login))
        .await
        .location_query("code");
    exchange(server.addr, &code, VERIFIER, CALLBACK)
        .await
        .json()
}

fn error(answer: &Raw) -> String {
    assert_eq!(answer.status, 200, "{}", answer.body);
    answer.json()["error"].as_str().unwrap_or("").to_owned()
}

fn ok(answer: &Answer, operation: &str) -> Value {
    assert_eq!(
        answer.status,
        StatusCode::OK,
        "{operation}: {:?}",
        answer.body
    );
    answer.json()
}

#[tokio::test]
async fn the_web_flow_with_pkce_redirects_with_code_and_state_and_mints_a_user_token() {
    // Given
    let server = start().await;

    // When
    let authorized = authorize(server.addr, None).await;
    let code = authorized.location_query("code");
    let tokens = exchange(server.addr, &code, VERIFIER, CALLBACK).await;
    let spent = exchange(server.addr, &code, VERIFIER, CALLBACK).await;

    // Then
    assert_eq!(authorized.status, 302);
    assert!(
        authorized
            .header("location")
            .starts_with(&format!("{CALLBACK}?"))
    );
    assert_eq!(authorized.location_query("state"), "st-1");
    let tokens = tokens.json();
    let access = tokens["access_token"]
        .as_str()
        .unwrap_or_else(|| panic!("{tokens}"));
    assert!(access.starts_with("ghu_"));
    assert!(
        tokens["refresh_token"]
            .as_str()
            .expect("refresh")
            .starts_with("ghr_")
    );
    assert_eq!(tokens["expires_in"], 28_800);
    assert_eq!(tokens["refresh_token_expires_in"], 15_897_600);
    assert_eq!(tokens["token_type"], "bearer");
    assert_eq!(error(&spent), "bad_verification_code");
    let me = GoGithub::new(&server).with_token(access).get("user").await;
    let me = ok(&me, "users/get-authenticated");
    assert_eq!(me["login"], fixture::ALICE);
    assert_eq!(me["id"], fixture::ALICE_ID);
}

#[tokio::test]
async fn the_signed_in_user_comes_from_login_then_cookie_then_the_default() {
    // Given
    let server = start().await;

    // When
    let by_login = authorize(server.addr, Some(fixture::BOB)).await;
    let cookie = by_login
        .header("set-cookie")
        .split(';')
        .next()
        .unwrap_or("")
        .to_owned();
    let by_cookie = raw(
        server.addr,
        "GET",
        &authorize_path("st-2", None),
        &[("Cookie", &cookie)],
        "",
    )
    .await;
    let unknown = authorize(server.addr, Some("nobody")).await;

    // Then
    for (answer, login) in [(&by_login, fixture::BOB), (&by_cookie, fixture::BOB)] {
        let code = answer.location_query("code");
        let access = exchange(server.addr, &code, VERIFIER, CALLBACK)
            .await
            .json()["access_token"]
            .as_str()
            .expect("access")
            .to_owned();
        let me = GoGithub::new(&server).with_token(&access).get("user").await;
        assert_eq!(me.json()["login"], login);
    }
    assert_eq!(by_cookie.location_query("state"), "st-2");
    assert_eq!(unknown.status, 422);
}

#[tokio::test]
async fn the_code_exchange_checks_what_github_checks() {
    // Given
    let server = start().await;
    let code = || async { authorize(server.addr, None).await.location_query("code") };

    // When
    let wrong_verifier = exchange(server.addr, &code().await, "another-verifier", CALLBACK).await;
    let wrong_redirect = exchange(
        server.addr,
        &code().await,
        VERIFIER,
        "https://other.example/cb",
    )
    .await;
    let wrong_secret = post_form(
        server.addr,
        "/login/oauth/access_token",
        &[
            ("code", &code().await),
            ("code_verifier", VERIFIER),
            ("client_id", CLIENT_ID),
            ("client_secret", "nope"),
        ],
    )
    .await;
    let unknown_client = post_form(
        server.addr,
        "/login/oauth/access_token",
        &[("code", &code().await), ("client_id", "Iv1.unknown")],
    )
    .await;
    let unsupported = post_form(
        server.addr,
        "/login/oauth/access_token",
        &[("grant_type", "password"), ("client_id", CLIENT_ID)],
    )
    .await;
    let dave = authorize(server.addr, Some("dave"))
        .await
        .location_query("code");
    let unverified = exchange(server.addr, &dave, VERIFIER, CALLBACK).await;
    let stale = authorize(server.addr, None).await.location_query("code");
    server.clock.advance(601);
    let too_late = exchange(server.addr, &stale, VERIFIER, CALLBACK).await;

    // Then
    assert_eq!(error(&wrong_verifier), "bad_verification_code");
    assert_eq!(error(&wrong_redirect), "redirect_uri_mismatch");
    assert_eq!(error(&wrong_secret), "incorrect_client_credentials");
    assert_eq!(error(&unknown_client), "incorrect_client_credentials");
    assert_eq!(error(&unsupported), "unsupported_grant_type");
    assert_eq!(error(&unverified), "unverified_user_email");
    assert_eq!(error(&too_late), "bad_verification_code");
}

#[tokio::test]
async fn the_token_endpoint_answers_form_encoded_without_a_json_accept() {
    // Given
    let server = start().await;
    let code = authorize(server.addr, None).await.location_query("code");

    // When
    let answer = raw(
        server.addr,
        "POST",
        "/login/oauth/access_token",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        &form(&[
            ("code", &code),
            ("code_verifier", VERIFIER),
            ("client_id", CLIENT_ID),
            ("client_secret", CLIENT_SECRET),
        ]),
    )
    .await;

    // Then
    assert_eq!(answer.status, 200);
    assert!(
        answer
            .header("content-type")
            .starts_with("application/x-www-form-urlencoded")
    );
    let fields = hubdouble_rest::Query::parse(&answer.body);
    assert!(
        fields
            .get("access_token")
            .is_some_and(|t| t.starts_with("ghu_"))
    );
    assert_eq!(fields.get("token_type"), Some("bearer"));
}

#[tokio::test]
async fn the_device_flow_polls_pending_slows_down_and_mints_once_approved() {
    // Given
    let server = start().await;
    let started = post_form(
        server.addr,
        "/login/device/code",
        &[("client_id", CLIENT_ID)],
    )
    .await
    .json();
    let device = started["device_code"].as_str().expect("device").to_owned();
    let user_code = started["user_code"].as_str().expect("user code").to_owned();
    let poll = || async {
        post_form(
            server.addr,
            "/login/oauth/access_token",
            &[
                ("client_id", CLIENT_ID),
                ("device_code", &device),
                ("grant_type", DEVICE_GRANT),
            ],
        )
        .await
    };

    // When
    let first = poll().await;
    let early = poll().await;
    server.clock.advance(10);
    let later = poll().await;
    let approved = server
        .admin(
            Method::POST,
            &format!("oauth/device/{user_code}"),
            Some(&json!({"login": fixture::BOB})),
        )
        .await;
    let minted = poll().await;
    let after = poll().await;

    // Then
    assert_eq!(
        started["verification_uri"],
        format!("{}/login/device", server.base)
    );
    assert_eq!(started["expires_in"], 900);
    assert_eq!(started["interval"], 5);
    assert_eq!(error(&first), "authorization_pending");
    assert_eq!(error(&early), "slow_down");
    assert_eq!(early.json()["interval"], 10);
    assert_eq!(error(&later), "authorization_pending");
    assert_eq!(approved.status, StatusCode::NO_CONTENT);
    let access = minted.json()["access_token"]
        .as_str()
        .expect("access")
        .to_owned();
    let me = GoGithub::new(&server).with_token(&access).get("user").await;
    assert_eq!(me.json()["login"], fixture::BOB);
    assert_eq!(error(&after), "incorrect_device_code");
}

#[tokio::test]
async fn the_device_flow_answers_denied_expired_unknown_and_disabled() {
    // Given
    let server = start().await;
    let start_code = || async {
        post_form(
            server.addr,
            "/login/device/code",
            &[("client_id", CLIENT_ID)],
        )
        .await
        .json()
    };
    let poll = |device: String| {
        let addr = server.addr;
        async move {
            post_form(
                addr,
                "/login/oauth/access_token",
                &[
                    ("client_id", CLIENT_ID),
                    ("device_code", &device),
                    ("grant_type", DEVICE_GRANT),
                ],
            )
            .await
        }
    };
    let denied = start_code().await;
    let expiring = start_code().await;

    // When
    let page = raw(
        server.addr,
        "POST",
        "/login/device",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        &form(&[
            ("user_code", denied["user_code"].as_str().expect("code")),
            ("approve", "false"),
        ]),
    )
    .await;
    let refused = poll(denied["device_code"].as_str().expect("device").to_owned()).await;
    server.clock.advance(901);
    let expired = poll(expiring["device_code"].as_str().expect("device").to_owned()).await;
    let late = server
        .admin(
            Method::POST,
            &format!(
                "oauth/device/{}",
                expiring["user_code"].as_str().expect("code")
            ),
            Some(&json!({})),
        )
        .await;
    let unknown = poll("0000".to_owned()).await;
    let disabled = post_form(
        server.addr,
        "/login/device/code",
        &[("client_id", "Iv1.nodevice")],
    )
    .await;
    let missing = server
        .admin(Method::POST, "oauth/device/ZZZZ-ZZZZ", Some(&json!({})))
        .await;

    // Then
    assert_eq!(page.status, 200);
    assert_eq!(error(&refused), "access_denied");
    assert_eq!(error(&expired), "expired_token");
    assert_eq!(late.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error(&unknown), "incorrect_device_code");
    assert_eq!(error(&disabled), "device_flow_disabled");
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_device_page_approves_for_the_signed_in_user() {
    // Given
    let server = start().await;
    let started = post_form(
        server.addr,
        "/login/device/code",
        &[("client_id", CLIENT_ID)],
    )
    .await
    .json();
    let user_code = started["user_code"].as_str().expect("code");

    // When
    let page = raw(
        server.addr,
        "GET",
        &format!("/login/device?user_code={user_code}"),
        &[],
        "",
    )
    .await;
    let approved = raw(
        server.addr,
        "POST",
        "/login/device",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        &form(&[
            ("user_code", &user_code.to_ascii_lowercase()),
            ("login", fixture::CAROL),
        ]),
    )
    .await;
    let minted = post_form(
        server.addr,
        "/login/oauth/access_token",
        &[
            ("client_id", CLIENT_ID),
            (
                "device_code",
                started["device_code"].as_str().expect("device"),
            ),
            ("grant_type", DEVICE_GRANT),
        ],
    )
    .await;

    // Then
    assert_eq!(page.status, 200);
    assert!(page.header("content-type").starts_with("text/html"));
    assert!(page.body.contains(user_code));
    assert_eq!(approved.status, 200);
    let access = minted.json()["access_token"]
        .as_str()
        .expect("access")
        .to_owned();
    let me = GoGithub::new(&server).with_token(&access).get("user").await;
    assert_eq!(me.json()["login"], fixture::CAROL);
}

#[tokio::test]
async fn refresh_rotates_both_tokens_and_refuses_the_old_refresh_token() {
    // Given
    let server = start().await;
    let first = sign_in(&server, fixture::ALICE).await;
    let old = first["refresh_token"].as_str().expect("refresh").to_owned();
    let refresh = |token: String, secret: &'static str| {
        let addr = server.addr;
        async move {
            post_form(
                addr,
                "/login/oauth/access_token",
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &token),
                    ("client_id", CLIENT_ID),
                    ("client_secret", secret),
                ],
            )
            .await
        }
    };

    // When
    let wrong_secret = refresh(old.clone(), "nope").await;
    let rotated = refresh(old.clone(), CLIENT_SECRET).await.json();
    let again = refresh(old.clone(), CLIENT_SECRET).await;
    let as_bearer = GoGithub::new(&server).with_token(&old).get("user").await;

    // Then
    assert_eq!(error(&wrong_secret), "incorrect_client_credentials");
    assert_ne!(rotated["access_token"], first["access_token"]);
    assert_ne!(rotated["refresh_token"], first["refresh_token"]);
    assert_eq!(error(&again), "bad_refresh_token");
    assert_eq!(as_bearer.status, StatusCode::UNAUTHORIZED);
    let access = rotated["access_token"].as_str().expect("access");
    let me = GoGithub::new(&server).with_token(access).get("user").await;
    assert_eq!(me.json()["login"], fixture::ALICE);
}

#[tokio::test]
async fn sso_is_required_until_the_user_signs_in_through_the_org_sso_page() {
    // Given
    let server = start().await;
    let access = sign_in(&server, fixture::ALICE).await["access_token"]
        .as_str()
        .expect("access")
        .to_owned();
    let client = GoGithub::new(&server).with_token(&access);
    let return_to = authorize_path("st-sso", None);

    // When
    let blocked = client.get("orgs/acme-sso/repos?per_page=1").await;
    let followed = raw(
        server.addr,
        "GET",
        &format!(
            "/orgs/acme-sso/sso?{}",
            form(&[("return_to", &return_to), ("login", fixture::ALICE)])
        ),
        &[],
        "",
    )
    .await;
    let reached = client.get("orgs/acme-sso/repos?per_page=1").await;
    let recorded = raw(
        server.addr,
        "POST",
        "/orgs/acme-sso/sso",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        &form(&[("return_to", &return_to), ("login", fixture::ALICE)]),
    )
    .await;
    let foreign = raw(
        server.addr,
        "POST",
        "/orgs/acme-sso/sso",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        &form(&[("return_to", "https://evil.example/")]),
    )
    .await;
    let unknown = raw(server.addr, "POST", "/orgs/nope/sso", &[], "").await;

    // Then
    assert_eq!(blocked.status, StatusCode::FORBIDDEN);
    assert_eq!(
        blocked.header("x-github-sso"),
        format!("required; url={}/orgs/acme-sso/sso", server.base)
    );
    assert_eq!(followed.status, 302);
    assert_eq!(
        followed.header("location"),
        format!("{}{return_to}", server.base)
    );
    assert_eq!(reached.status, StatusCode::OK);
    assert_eq!(recorded.status, 302);
    assert_eq!(
        recorded.header("location"),
        format!("{}{return_to}", server.base)
    );
    assert_eq!(foreign.status, 422);
    assert_eq!(unknown.status, 404);
}

#[tokio::test]
async fn user_reads_answer_public_and_private_profiles() {
    // Given
    let server = start().await;
    let alice = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    let anonymous = GoGithub::enterprise(&server);

    // When
    let me = alice.get("user").await;
    let own = alice.get("users/alice").await;
    let other = alice.get("users/BOB").await;
    let by_id = anonymous.get(&format!("user/{}", fixture::BOB_ID)).await;
    let org = anonymous.get("users/acme").await;
    let bot = anonymous.get("users/fixture-app%5Bbot%5D").await;
    let missing_login = anonymous.get("users/nobody").await;
    let missing_id = anonymous.get("user/999999").await;
    let unauthenticated = anonymous.get("user").await;

    // Then
    let me = ok(&me, "users/get-authenticated");
    assert_eq!(me["user_view_type"], "private");
    assert_eq!(me["name"], "Alice");
    assert_eq!(me["email"], "alice@example.com");
    assert_eq!(me["type"], "User");
    assert_eq!(
        ok(&own, "users/get-by-username")["user_view_type"],
        "private"
    );
    let other = ok(&other, "users/get-by-username");
    assert_eq!(other["user_view_type"], "public");
    assert_eq!(other["id"], fixture::BOB_ID);
    assert!(other.get("plan").is_none());
    let by_id = ok(&by_id, "users/get-by-id");
    assert_eq!(by_id["login"], fixture::BOB);
    assert_eq!(by_id["url"], format!("{}/api/v3/users/bob", server.base));
    let org = ok(&org, "users/get-by-username");
    assert_eq!(org["type"], "Organization");
    assert_eq!(org["public_repos"], 1);
    let bot = ok(&bot, "users/get-by-username");
    assert_eq!(bot["type"], "Bot");
    assert_eq!(bot["login"], "fixture-app[bot]");
    assert_eq!(missing_login.status, StatusCode::NOT_FOUND);
    assert_eq!(missing_id.status, StatusCode::NOT_FOUND);
    assert_eq!(unauthenticated.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_authenticated_user_is_refused_to_an_installation_token() {
    // Given
    let server = start().await;
    server
        .server
        .services()
        .store
        .update(|tx| {
            let id = tx.next_id();
            tx.table_mut::<Tokens>().insert(Token {
                id,
                token: INSTALLATION_TOKEN.into(),
                kind: TokenKind::Installation,
                app_id: Some(fixture::APP_ID),
                installation_id: Some(fixture::INSTALLATION_ID),
                user_id: None,
                repo_ids: None,
                permissions: BTreeMap::from([("metadata".to_owned(), Access::Read)]),
                scopes: vec![],
                expires_at: None,
                revoked: false,
                owner_seeded: false,
                created_at: Timestamp::SEED,
            })?;
            Ok::<_, ApiError>(())
        })
        .expect("token");

    // When
    let answer = GoGithub::new(&server)
        .with_token(INSTALLATION_TOKEN)
        .get("user")
        .await;

    // Then
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn graphql_viewer_user_nodes_and_actor_login() {
    // Given
    let server = start().await;
    let alice = Graphql::new(GoGithub::new(&server).with_token(fixture::ALICE_TOKEN));
    let (user_node, bot_node) = server
        .server
        .services()
        .store
        .view(|tx| {
            let accounts = tx.table::<Accounts>();
            let bot = accounts
                .by_login("fixture-app[bot]")
                .filter(|a| a.kind == AccountKind::Bot)
                .ok_or_else(ApiError::not_found)?;
            let user = accounts
                .get(fixture::BOB_ID)
                .ok_or_else(ApiError::not_found)?;
            Ok((user.node_id.clone(), bot.node_id.clone()))
        })
        .expect("nodes");

    // When
    let answer = alice
        .query(
            "query($u: ID!, $b: ID!) { viewer { __typename id login databaseId } \
             u: node(id: $u) { __typename ... on Actor { login } ... on User { databaseId } } \
             b: node(id: $b) { __typename ... on Actor { login } } }",
            &json!({"u": user_node, "b": bot_node}),
        )
        .await;

    // Then
    let value = answer.json();
    assert!(value.get("errors").is_none(), "{value}");
    let data = &value["data"];
    assert_eq!(data["viewer"]["__typename"], "User");
    assert_eq!(data["viewer"]["login"], fixture::ALICE);
    assert_eq!(data["viewer"]["databaseId"], fixture::ALICE_ID);
    assert_eq!(
        data["viewer"]["id"],
        node_id::entity(node_id::USER, fixture::ALICE_ID)
    );
    assert_eq!(data["u"]["login"], fixture::BOB);
    assert_eq!(data["u"]["databaseId"], fixture::BOB_ID);
    assert_eq!(data["b"]["__typename"], "Bot");
    assert_eq!(data["b"]["login"], "fixture-app");
}

fn frame(r: &mut Registrar<'_>) {
    hubdouble_admin::register(r);
    hubdouble_api_users::register(r);
    org_repos(r);
}

#[tokio::test]
async fn pages_and_the_sso_header_use_the_base_origin_when_the_web_origin_differs() {
    // Given a server whose web origin is another host
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let base = format!("http://{addr}");
    let config = Config {
        addr,
        base_url: base.clone(),
        web_url: "https://github.example".to_owned(),
        oidc_issuer: format!("{base}/_actions"),
        admin_token: "admin".to_owned(),
        ..Config::default()
    };
    let clock = Arc::new(ManualClock::new(Timestamp::SEED));
    let server = Server::start(config, clock, &|_| None, Registries::new(frame)).expect("start");
    let serving = server.clone();
    tokio::spawn(async move { serving.serve(listener, std::future::pending()).await });
    let admin = [
        ("Authorization", "Bearer admin"),
        ("Content-Type", "application/json"),
    ];
    for entry in [
        json!({"login": "erin"}),
        json!({"login": "sso-org", "sso_enforced": true, "members": [{"login": "erin"}]}),
        json!({"slug": "web-app", "owner": "sso-org", "client_id": CLIENT_ID,
               "client_secret": CLIENT_SECRET, "public_key": fixture::APP_PUBLIC_KEY,
               "device_flow": true}),
    ]
    .iter()
    .zip(["user", "org", "app"])
    {
        let made = raw(
            addr,
            "POST",
            &format!("/_hubdouble/{}", entry.1),
            &admin,
            &entry.0.to_string(),
        )
        .await;
        assert!(made.status < 300, "{}", made.body);
    }

    // When
    let device = post_form(addr, "/login/device/code", &[("client_id", CLIENT_ID)])
        .await
        .json();
    let authorized = authorize(addr, Some("erin")).await;
    let code = authorized.location_query("code");
    let access = exchange(addr, &code, VERIFIER, CALLBACK).await.json()["access_token"]
        .as_str()
        .expect("access")
        .to_owned();
    let bearer = format!("Bearer {access}");
    let profile = raw(
        addr,
        "GET",
        "/users/erin",
        &[("Authorization", &bearer)],
        "",
    )
    .await;
    let blocked = raw(
        addr,
        "GET",
        "/orgs/sso-org/repos",
        &[("Authorization", &bearer)],
        "",
    )
    .await;
    let signed_on = raw(
        addr,
        "GET",
        "/orgs/sso-org/sso?login=erin&return_to=/after",
        &[],
        "",
    )
    .await;

    // Then
    assert_eq!(blocked.status, 403);
    assert_eq!(
        blocked.header("x-github-sso"),
        format!("required; url={base}/orgs/sso-org/sso")
    );
    assert_eq!(device["verification_uri"], format!("{base}/login/device"));
    assert!(
        authorized
            .header("location")
            .starts_with(&format!("{CALLBACK}?"))
    );
    assert_eq!(authorized.location_query("state"), "st-1");
    assert_eq!(signed_on.header("location"), format!("{base}/after"));
    assert_eq!(profile.json()["html_url"], "https://github.example/erin");
}

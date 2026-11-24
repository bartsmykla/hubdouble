use http::{Method, StatusCode};
use hubdouble_auth::{self as auth, Target};
use hubdouble_model::{Grant, Requirement, Scope};
use hubdouble_render::query_escape;
use hubdouble_store::Repos;
use hubdouble_testkit::{Answer, Builder, GoGithub, Graphql, TestServer, fixture, validate};
use serde_json::{Value, json};

const JWT: &str = concat!(
    "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOjQwMDEsImlhdCI6MTc2NzIyNTU0MCwiZXhwIjoxNzY3MjI2MjAwfQ.",
    "Fb_Cv3LZFiOrzUD6Lno5Qf6-AANwusj3b1QaF1VaRQfOYIsKZGViN0GQFeGjdguShxI5K_LMgk1PIrCPCOSXqcP6-606bfAyxfZ4",
    "Fy4ud8oR6GLiB3fpnGL4Nek6rVcnFA-WgRgjJzEIocsWRjfeZysfk2Wk3xo4kd2-cy5p7KJIiTYxl9EGWz91jd-_rsRJ1ChtmRZx",
    "7D7uE6bxtphwoO8NfojSuCue9yrK1tyUwQFCeTH0FafdsF-TsWjQeC15nOdrhGU-QU9eYHge0Cd5v7Pke48VZcefC27B812TTExpB",
    "RAAlwwQvlUrAxc_ss-RjTXEqvhU2UZCKyYLpreDhA"
);
const OTHER_JWT: &str = concat!(
    "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOjQwMDIsImlhdCI6MTc2NzIyNTU0MCwiZXhwIjoxNzY3MjI2MjAwfQ.",
    "gobO4qnik78K0sFVCrmbD6FG02vW0XMALsekmCW_PF0t0GZsJUx0isvZDY84Zp54-2zB9ciCUTM48qfwPNVeTsO9B_8jFncvfp6",
    "f0uwi0SS8nQWyh7pZJs_7h_T2DlbIVHBjTTgXMtLvWpP5VIe9YjgYQp1TbepKztie1tg6DX61xly-vxhKH-nNZ6_D9zMFbgvEV",
    "GvNYzQl4KSX0BwtEHs-CFj_DunR-bAuDDaKKeKB2kC99uqfPNpWnlp03z1KYEiH3B7d9zj7nVMCE4RHf9NmXifV0Emikpkqsx2",
    "hWlO65sop7GoeCOQuKxK6QoYJjtCXQ14lIQyGZIpOOlEpDA"
);

const TOKEN_PATH: &str = "app/installations/5001/access_tokens";
const DELIVERY_PATH: &str = "app/hook/deliveries";
const REPO_WRITE: Requirement = Requirement {
    any_of: &[Grant::write("issues")],
    scope: Scope::Repo,
    scopes: &[],
};

fn setup() -> Builder {
    TestServer::builder()
        .register(hubdouble_api_apps::register)
        .fixture()
        .entry(json!({
            "kind": "app", "id": 4002, "slug": "other-app", "owner": "alice",
            "public_key": fixture::APP_PUBLIC_KEY,
            "webhook": { "url": "{{env \"HUBDOUBLE_TEST_WEBHOOK_URL\"}}" }
        }))
        .entry(json!({
            "kind": "installation", "id": 5002, "app": "fixture-app", "account": "acme-sso",
            "repository_selection": "all", "suspended": true
        }))
        .entry(json!({
            "kind": "installation", "id": 5003, "app": "other-app", "account": "acme"
        }))
}

fn app(server: &TestServer) -> GoGithub {
    GoGithub::enterprise(server).with_token(JWT)
}

fn body(answer: &Answer, operation: &str, status: StatusCode) -> Value {
    assert_eq!(answer.status, status, "{operation}: {:?}", answer.body);
    answer.json()
}

async fn get(client: &GoGithub, path: &str, operation: &str) -> Value {
    body(&client.get(path).await, operation, StatusCode::OK)
}

fn status(answer: &Answer, expected: StatusCode) {
    assert_eq!(answer.status, expected, "{:?}", answer.body);
}

async fn admin_no_content(server: &TestServer, method: Method, path: &str) {
    status(
        &server.admin(method, path, None).await,
        StatusCode::NO_CONTENT,
    );
}

fn next_path<'a>(answer: &'a Answer, client: &GoGithub) -> &'a str {
    answer
        .header("link")
        .split_once(">; rel=\"next\"")
        .and_then(|(link, _)| link.strip_prefix('<')?.strip_prefix(client.base()))
        .unwrap_or_else(|| panic!("missing next link: {}", answer.header("link")))
}

#[tokio::test]
async fn graphql_needs_a_user_or_installation_token() {
    // Given
    let server = setup().start().await;
    let query = json!({"query": "{viewer{login}}"});
    // When
    let anonymous = GoGithub::new(&server).post("graphql", &query).await;
    let as_app = app(&server).post("graphql", &query).await;
    // Then
    for answer in [anonymous, as_app] {
        let body = body(&answer, "graphql", StatusCode::UNAUTHORIZED);
        assert!(body["message"].is_string(), "{body}");
        assert!(body.get("data").is_none(), "{body}");
        assert!(body.get("errors").is_none(), "{body}");
    }
}

#[tokio::test]
async fn apps_and_installation_lookups_use_the_authenticated_apps_grant() {
    let server = setup().start().await;
    let app = app(&server);
    let public = GoGithub::enterprise(&server);

    // When
    let slug = get(&app, "app", "apps/get-authenticated").await;
    let by_slug = get(&public, "apps/FIXTURE-APP", "apps/get-by-slug").await;
    let first = app.get("app/installations?page=1&per_page=1").await;
    let second = app.get(next_path(&first, &app)).await;
    let installation = get(&app, "orgs/acme/installation", "apps/get-org-installation").await;
    let repo = get(
        &app,
        "repos/acme/public/installation",
        "apps/get-repo-installation",
    )
    .await;

    // Then
    assert_eq!(slug["slug"], fixture::APP_SLUG);
    assert_eq!(slug["id"], fixture::APP_ID);
    assert_eq!(slug["installations_count"], 2);
    assert_eq!(by_slug["id"], slug["id"]);
    assert_eq!(by_slug["owner"]["login"], "acme");
    assert_eq!(
        body(&first, "apps/list-installations", StatusCode::OK)[0]["id"],
        fixture::INSTALLATION_ID
    );
    assert_eq!(
        body(&second, "apps/list-installations", StatusCode::OK)[0]["id"],
        5002
    );
    assert_eq!(installation["id"], fixture::INSTALLATION_ID);
    assert_eq!(
        installation["access_tokens_url"],
        format!("{}{TOKEN_PATH}", app.base())
    );
    assert_eq!(repo, installation);
    let bot = server
        .admin(Method::GET, "app/fixture-app", None)
        .await
        .json();
    assert_eq!(
        bot["bot"]["login"],
        format!("{}[bot]", slug["slug"].as_str().unwrap())
    );

    for path in [
        "app",
        "app/installations",
        "orgs/acme/installation",
        "repos/acme/public/installation",
    ] {
        status(&public.get(path).await, StatusCode::UNAUTHORIZED);
    }
    for path in [
        "apps/absent",
        "orgs/absent/installation",
        "repos/acme/absent/installation",
        "repos/acme/private/installation",
    ] {
        status(&app.get(path).await, StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn token_exchange_limits_repositories_and_permissions_and_revokes_the_token() {
    // Given
    let server = setup().entry(json!({
        "kind": "repository", "owner": "acme", "name": "third", "visibility": "private", "id": 3003
    })).start().await;
    let app = app(&server);
    let other = GoGithub::enterprise(&server).with_token(OTHER_JWT);
    patch(
        &server,
        "installation/5001",
        &json!({"repository_selection": "all"}),
    )
    .await;

    // When
    let request = json!({ "repositories": ["public"], "repository_ids": [3001, 3002], "permissions": { "issues": "read" } });
    let token = body(
        &app.post(TOKEN_PATH, &request).await,
        "apps/create-installation-access-token",
        StatusCode::CREATED,
    );
    let secret = token["token"].as_str().unwrap();
    let installation = GoGithub::enterprise(&server).with_token(secret);
    let first = installation
        .get("installation/repositories?per_page=1&page=1")
        .await;
    let second = installation.get(next_path(&first, &installation)).await;
    let last = installation
        .get("installation/repositories?per_page=1&page=3")
        .await;

    // Then
    assert!(secret.starts_with("ghs_"));
    assert_eq!(secret.len(), 40);
    assert_eq!(token["expires_at"], "2026-01-01T01:00:00Z");
    assert_eq!(token["repository_selection"], "selected");
    assert_eq!(token["permissions"]["issues"], "read");
    assert_eq!(token["permissions"]["contents"], Value::Null);
    assert_eq!(token["repositories"].as_array().unwrap().len(), 2);
    for (answer, id) in [(&first, 3001), (&second, 3002)] {
        let page = body(
            answer,
            "apps/list-repos-accessible-to-installation",
            StatusCode::OK,
        );
        assert_eq!(page["total_count"], 2);
        assert_eq!(page["repository_selection"], "selected");
        assert_eq!(page["repositories"][0]["id"], id);
    }
    assert_eq!(last.json()["total_count"], 2);
    assert_eq!(last.json()["repositories"], json!([]));
    assert_gate(&server, secret);
    mint_denials(&app, &other, &installation).await;

    admin_no_content(&server, Method::POST, "reset").await;
    assert_eq!(
        installation.get("installation/repositories").await.json()["total_count"],
        1
    );
    let revoked = installation
        .call(Method::DELETE, "installation/token")
        .await;
    status(&revoked, StatusCode::NO_CONTENT);
    assert!(revoked.body.is_empty());
    status(
        &installation.get("installation/repositories").await,
        StatusCode::UNAUTHORIZED,
    );
}

async fn mint_denials(app: &GoGithub, other: &GoGithub, installation: &GoGithub) {
    for request in [
        json!({ "permissions": {"contents": "write"} }),
        json!({ "repository_ids": [9000] }),
        json!({ "repositories": ["absent"] }),
    ] {
        let answer = app.post(TOKEN_PATH, &request).await;
        assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(answer.json()["status"], "422");
    }
    for (client, path, expected) in [
        (other, TOKEN_PATH, StatusCode::NOT_FOUND),
        (
            app,
            "app/installations/5002/access_tokens",
            StatusCode::FORBIDDEN,
        ),
        (installation, TOKEN_PATH, StatusCode::UNAUTHORIZED),
    ] {
        status(&client.post(path, &json!({})).await, expected);
    }
    status(
        &app.get("installation/repositories").await,
        StatusCode::FORBIDDEN,
    );
    status(
        &app.call(Method::DELETE, "installation/token").await,
        StatusCode::FORBIDDEN,
    );
}

#[expect(
    clippy::unwrap_used,
    reason = "a test fixture must contain the token and both repositories"
)]
fn assert_gate(server: &TestServer, token: &str) {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::AUTHORIZATION,
        format!("Bearer {token}").parse().unwrap(),
    );
    server
        .server
        .services()
        .store
        .view(|tx| {
            let actor = auth::authenticate(tx, server.clock.as_ref(), &headers)?;
            let repos = tx.table::<Repos>();
            let allowed = repos.get(3002).unwrap();
            let hidden = repos.get(3003).unwrap();
            let missing =
                auth::authorize(tx, &actor, &REPO_WRITE, Target::Repo(allowed)).unwrap_err();
            assert_eq!(missing.status, 403);
            assert_eq!(
                missing.headers,
                [("X-Accepted-GitHub-Permissions", "issues=write".to_owned())]
            );
            assert_eq!(
                auth::authorize(tx, &actor, &REPO_WRITE, Target::Repo(hidden))
                    .unwrap_err()
                    .status,
                404
            );
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn bot_database_id_and_app_node_resolve_with_public_fields() {
    let server = setup().start().await;
    let admin = server
        .admin(Method::GET, "app/fixture-app", None)
        .await
        .json();
    let bot_id = admin["bot"]["id"].as_i64().unwrap();
    let bot_node = hubdouble_model::node_id::entity(hubdouble_model::node_id::BOT, bot_id);
    let gql = Graphql::new(GoGithub::enterprise(&server).with_token(fixture::ALICE_TOKEN));
    let response = gql
        .query(
            "query($ids: [ID!]!) { nodes(ids: $ids) {
            ... on Bot { databaseId id login }
            ... on App { databaseId id slug }
        } }",
            &json!({ "ids": [bot_node, admin["node_id"]] }),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.json(),
        json!({"data": {"nodes": [
            {"databaseId": bot_id, "id": bot_node, "login": "fixture-app[bot]"},
            {"databaseId": fixture::APP_ID, "id": admin["node_id"], "slug": fixture::APP_SLUG}
        ]}})
    );
}

async fn drain(server: &TestServer) {
    admin_no_content(server, Method::POST, "hooks/drain").await;
}

async fn patch(server: &TestServer, path: &str, value: &Value) {
    let answer = server.admin(Method::PATCH, path, Some(value)).await;
    assert_eq!(answer.status, StatusCode::OK, "{:?}", answer.body);
}

#[tokio::test]
async fn delivery_pages_filter_resume_after_reset_and_redeliver_only_for_the_app() {
    let server = setup().start().await;
    let app = app(&server);
    let other = GoGithub::enterprise(&server).with_token(OTHER_JWT);
    assert_eq!(app.get(DELIVERY_PATH).await.json(), json!([]));

    patch(&server, "installation/5001", &json!({"suspended": true})).await;
    drain(&server).await;
    let ok = app.get(DELIVERY_PATH).await.json()[0]["id"]
        .as_i64()
        .unwrap();
    let url = format!("{}/missing-webhook", server.base);
    patch(
        &server,
        "app/fixture-app",
        &json!({"webhook": {"url": url}}),
    )
    .await;
    patch(&server, "installation/5001", &json!({"suspended": false})).await;
    drain(&server).await;

    // When
    let failures = app
        .get("app/hook/deliveries?per_page=1&status=failure")
        .await;
    let next = next_path(&failures, &app);
    let row = &body(&failures, "apps/list-webhook-deliveries", StatusCode::OK)[0];
    let id = row["id"].as_i64().unwrap();
    let path = format!("{DELIVERY_PATH}/{id}");
    let redelivery = format!("{path}/attempts");
    let detail = get(&app, &path, "apps/get-webhook-delivery").await;

    // Then
    assert_eq!(row["status_code"], 404);
    assert_eq!(row["event"], "installation");
    assert_eq!(row["action"], "unsuspend");
    assert_eq!(
        detail["request"]["payload"]["installation"]["id"],
        fixture::INSTALLATION_ID
    );
    let reply: Value =
        serde_json::from_str(detail["response"]["payload"].as_str().unwrap()).unwrap();
    assert_eq!(reply["status"], "404");
    assert_eq!(
        app.get("app/hook/deliveries?status=success").await.json()[0]["id"],
        ok
    );
    assert_eq!(other.get(&path).await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        other.call(Method::POST, &redelivery).await.status,
        StatusCode::NOT_FOUND
    );
    status(&other.get(next).await, StatusCode::UNPROCESSABLE_ENTITY);
    for query in [format!("cursor={id}"), "status=unknown".to_owned()] {
        let invalid = app.get(&format!("{DELIVERY_PATH}?{query}")).await;
        status(&invalid, StatusCode::UNPROCESSABLE_ENTITY);
    }

    admin_no_content(&server, Method::POST, "reset").await;
    let continuation = app.get(next).await.json();
    assert_eq!(continuation[0]["event"], "ping");
    assert_eq!(continuation[0]["id"], id - 1);
    let ping = app.get(&format!("{DELIVERY_PATH}/{}", id - 1)).await.json();
    validate::webhook_payload("ping", None, &ping["request"]["payload"])
        .unwrap_or_else(|errors| panic!("ping: {errors:?}"));
    let rel = app.call(Method::POST, &redelivery).await;
    assert_eq!(rel.status, StatusCode::ACCEPTED);
    assert_eq!(rel.json(), serde_json::json!({}));
    drain(&server).await;
    let newest = &app.get(DELIVERY_PATH).await.json()[0];
    assert_eq!(newest["id"], id + 1);
    assert_eq!(newest["guid"], row["guid"]);
    assert_eq!(newest["redelivery"], true);
    let repeated = app.get(&format!("{DELIVERY_PATH}/{}", id + 1)).await.json();
    assert_eq!(repeated["request"]["payload"], detail["request"]["payload"]);
}

async fn event(server: &TestServer, name: &str, action: &str) -> Value {
    drain(server).await;
    let deliveries = server.receiver.received().await;
    assert_eq!(deliveries.len(), 1, "{deliveries:?}");
    let delivery = &deliveries[0];
    assert_eq!(delivery.header("X-GitHub-Event"), name);
    assert!(delivery.signed_with(fixture::WEBHOOK_SECRET));
    let payload = delivery.json();
    assert_eq!(payload["action"], action);
    validate::webhook_payload(name, Some(action), &payload)
        .unwrap_or_else(|errors| panic!("{name}.{action}: {errors:?}"));
    payload
}

#[tokio::test]
async fn installation_admin_changes_emit_app_specific_events() {
    let server = setup().start().await;
    let hook_url = format!("{}?ping=1", server.receiver.url());
    patch(
        &server,
        "app/fixture-app",
        &json!({"webhook": {
            "url": hook_url, "secret": fixture::WEBHOOK_SECRET,
        }}),
    )
    .await;
    drain(&server).await;
    let deliveries = server.receiver.received().await;
    let [ping] = &deliveries[..] else {
        panic!("expected one ping, got {deliveries:?}");
    };
    assert_eq!(ping.header("x-github-event"), "ping");
    assert!(ping.signed_with(fixture::WEBHOOK_SECRET));
    assert_eq!(ping.json()["hook_id"], fixture::APP_ID);

    patch(&server, "installation/5001", &json!({"suspended": true})).await;
    assert_eq!(
        event(&server, "installation", "suspend").await["installation"]["suspended_at"],
        "2026-01-01T00:00:00Z"
    );
    patch(&server, "installation/5001", &json!({"suspended": false})).await;
    assert_eq!(
        event(&server, "installation", "unsuspend").await["installation"]["suspended_at"],
        Value::Null
    );

    patch(
        &server,
        "installation/5001",
        &json!({"permissions": {"checks": "write"}}),
    )
    .await;
    assert_eq!(
        event(&server, "installation", "new_permissions_accepted").await["installation"]["permissions"]
            ["checks"],
        "write"
    );
    patch(
        &server,
        "installation/5001",
        &json!({"repositories": ["public", "private"]}),
    )
    .await;
    let added = event(&server, "installation_repositories", "added").await;
    assert_eq!(added["repositories_added"][0]["full_name"], "acme/private");
    patch(
        &server,
        "installation/5001",
        &json!({"repositories": ["private"]}),
    )
    .await;
    let removed = event(&server, "installation_repositories", "removed").await;
    assert_eq!(
        removed["repositories_removed"][0]["full_name"],
        "acme/public"
    );

    admin_no_content(&server, Method::DELETE, "installation/5003").await;
    drain(&server).await;
    let deliveries = server.receiver.received().await;
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].json()["installation"]["app_id"], 4002);

    patch(
        &server,
        "installation-target/acme",
        &json!({"login": "acme-next"}),
    )
    .await;
    let renamed = event(&server, "installation_target", "renamed").await;
    assert_eq!(renamed["account"]["login"], "acme-next");
    assert_eq!(renamed["changes"]["login"]["from"], "acme");

    admin_no_content(
        &server,
        Method::DELETE,
        "app/fixture-app/authorizations/alice",
    )
    .await;
    assert_eq!(
        event(&server, "github_app_authorization", "revoked").await["sender"]["login"],
        "alice"
    );
    admin_no_content(&server, Method::DELETE, "installation/5001").await;
    let deleted = event(&server, "installation", "deleted").await;
    assert_eq!(deleted["repositories"][0]["full_name"], "acme-next/private");
    assert_eq!(
        app(&server).get("orgs/acme-next/installation").await.status,
        StatusCode::NOT_FOUND
    );
    let input = json!({"app": "fixture-app", "account": "acme-next", "repositories": ["private"]});
    status(
        &server
            .admin(Method::POST, "installation", Some(&input))
            .await,
        StatusCode::CREATED,
    );
    let created = event(&server, "installation", "created").await;
    assert_eq!(created["repositories"], deleted["repositories"]);
}

#[tokio::test]
async fn the_installation_list_reads_since_as_the_other_lists_do() {
    // Given the two installations of the app, updated at 2026-01-01T00:00:00Z
    let server = setup().start().await;
    let app = app(&server);

    // When the list reads a since at, or just past, that second
    for (since, listed) in [
        ("2026-01-01T00:00:00Z", 2),
        ("2025-12-31 16:00 PST", 2),
        ("Dec 31 2025", 2),
        ("2026-01-01T00:00:00.000001Z", 0),
        ("Jan 1 2026", 0),
    ] {
        let answer = app
            .get(&format!("app/installations?since={}", query_escape(since)))
            .await;

        // Then hubdouble reads it as the comment lists do (docs/fidelity.md)
        let listed_ids = body(&answer, "apps/list-installations", StatusCode::OK);
        assert_eq!(listed_ids.as_array().map(Vec::len), Some(listed), "{since}");
    }

    // When the list reads a since that Ruby cannot read
    let answer = app.get("app/installations?since=yesterday").await;

    // Then the refusal has the body of the other lists
    let refusal = body(
        &answer,
        "apps/list-installations",
        StatusCode::UNPROCESSABLE_ENTITY,
    );
    assert_eq!(
        refusal["documentation_url"],
        "https://docs.github.com/rest/apps/apps#list-installations-for-the-authenticated-app"
    );
}

use std::collections::{BTreeMap, BTreeSet};

use http::{Method, StatusCode};
use hubdouble_hooks::{Hooks, LatestPushes};
use hubdouble_model::{Clock, Token, TokenKind};
use hubdouble_store::{Deliveries, Tokens};
use hubdouble_testkit::fixture::{
    ALICE_ID, ALICE_TOKEN, APP_PUBLIC_KEY, BOB_TOKEN, ORG, ORG_ID, PUBLIC_REPO_ID,
};
use hubdouble_testkit::{Answer, Delivery, GoGithub, TestServer};
use serde_json::{Value, json};

const REPO_HOOKS: &str = "repos/acme/public/hooks";
const ORG_HOOKS: &str = "orgs/acme/hooks";
const HOOK_ADMIN_TOKEN: &str = "ghp_a11ce000000000000000000000000000hooks";

async fn server() -> TestServer {
    with_hook_admin(
        TestServer::builder()
            .frame(hubdouble_api::register_all)
            .fixture()
            .start()
            .await,
    )
}

fn with_hook_admin(server: TestServer) -> TestServer {
    add_token(
        &server,
        HOOK_ADMIN_TOKEN,
        TokenKind::Personal,
        (None, None, Some(ALICE_ID)),
        &["repo", "admin:org_hook"],
    );
    server
}

fn add_token(
    server: &TestServer,
    token: &str,
    kind: TokenKind,
    (app_id, installation_id, user_id): (Option<i64>, Option<i64>, Option<i64>),
    scopes: &[&str],
) {
    let now = server.clock.now();
    server
        .server
        .services()
        .store
        .update(|tx| {
            let id = tx.next_id();
            tx.table_mut::<Tokens>().insert(Token {
                id,
                token: token.to_owned(),
                kind,
                app_id,
                installation_id,
                user_id,
                repo_ids: None,
                permissions: BTreeMap::new(),
                scopes: scopes.iter().map(|&s| s.to_owned()).collect(),
                expires_at: None,
                revoked: false,
                owner_seeded: false,
                created_at: now,
            })?;
            Ok(())
        })
        .unwrap_or_else(|e| panic!("add the token {token}: {e:?}"));
}

fn alice(server: &TestServer) -> GoGithub {
    GoGithub::new(server).with_token(HOOK_ADMIN_TOKEN)
}

fn hook(server: &TestServer, secret: &str, events: &[&str], active: bool) -> Value {
    json!({
        "name": "web",
        "active": active,
        "events": events,
        "config": {"url": server.receiver.url(), "content_type": "json", "secret": secret},
    })
}

async fn create(client: &GoGithub, path: &str, body: &Value) -> i64 {
    let created = client.post(path, body).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json());
    created.json()["id"]
        .as_i64()
        .unwrap_or_else(|| panic!("a hook without an id"))
}

async fn drained(server: &TestServer) -> Vec<Delivery> {
    let drained = server.admin(Method::POST, "hooks/drain", None).await;
    assert_eq!(drained.status, StatusCode::NO_CONTENT);
    server.receiver.received().await
}

fn to_hook<'a>(deliveries: &'a [Delivery], hook_id: i64, event: &str) -> Vec<&'a Delivery> {
    deliveries
        .iter()
        .filter(|d| {
            d.header("x-github-hook-id") == hook_id.to_string()
                && d.header("x-github-event") == event
        })
        .collect()
}

fn one<'a>(deliveries: &'a [Delivery], hook_id: i64, event: &str) -> &'a Delivery {
    let got = to_hook(deliveries, hook_id, event);
    assert_eq!(got.len(), 1, "{event} deliveries to hook {hook_id}");
    got[0]
}

#[tokio::test]
async fn a_repository_hook_is_created_read_changed_and_deleted() {
    // Given
    let server = server().await;
    let alice = alice(&server);

    // When
    let created = alice
        .post(REPO_HOOKS, &hook(&server, "repo-secret", &["issues"], true))
        .await;

    // Then
    assert_eq!(created.status, StatusCode::CREATED);
    let body = created.json();
    let id = body["id"].as_i64().unwrap();
    assert!(
        created
            .header("location")
            .ends_with(&format!("/repos/acme/public/hooks/{id}"))
    );
    assert_eq!(body["type"], "Repository");
    assert_eq!(body["name"], "web");
    assert_eq!(body["active"], true);
    assert_eq!(body["events"], json!(["issues"]));
    assert_eq!(body["config"]["content_type"], "json");
    assert_eq!(body["config"]["secret"], "********");
    assert_eq!(body["config"]["url"], server.receiver.url());

    let listed = alice.get(REPO_HOOKS).await;
    assert_eq!(listed.status, StatusCode::OK);
    let ids: Vec<_> = listed
        .json()
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["id"].clone())
        .collect();
    assert_eq!(ids, [json!(id)]);
    let path = format!("{REPO_HOOKS}/{id}");
    assert_eq!(alice.get(&path).await.json()["id"], id);

    let updated = alice
        .call_json(
            Method::PATCH,
            &path,
            &json!({"add_events": ["push"], "active": false}),
        )
        .await;
    assert_eq!(updated.status, StatusCode::OK);
    let events: BTreeSet<_> = updated.json()["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    assert_eq!(events, BTreeSet::from(["issues".into(), "push".into()]));
    assert_eq!(updated.json()["active"], false);
    let removed = alice
        .call_json(Method::PATCH, &path, &json!({"remove_events": ["issues"]}))
        .await;
    assert_eq!(removed.json()["events"], json!(["push"]));

    let config = alice.get(&format!("{path}/config")).await;
    assert_eq!(config.status, StatusCode::OK);
    assert_eq!(config.json()["url"], server.receiver.url());
    assert_eq!(config.json()["insecure_ssl"], "0");
    let changed = alice
        .call_json(
            Method::PATCH,
            &format!("{path}/config"),
            &json!({"content_type": "form", "insecure_ssl": "1"}),
        )
        .await;
    assert_eq!(changed.status, StatusCode::OK);
    assert_eq!(changed.json()["content_type"], "form");
    assert_eq!(changed.json()["insecure_ssl"], "1");
    assert_eq!(changed.json()["url"], server.receiver.url());

    let deleted = alice.call(Method::DELETE, &path).await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert_eq!(alice.get(&path).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn creating_or_pinging_a_hook_delivers_a_signed_ping() {
    // Given
    let server = server().await;
    let alice = alice(&server);
    let repo_hook = create(
        &alice,
        REPO_HOOKS,
        &hook(&server, "repo-secret", &["issues"], true),
    )
    .await;
    let org_hook = create(
        &alice,
        ORG_HOOKS,
        &hook(&server, "org-secret", &["*"], true),
    )
    .await;

    // When
    let created = drained(&server).await;
    let pinged = alice
        .call(Method::POST, &format!("{REPO_HOOKS}/{repo_hook}/pings"))
        .await;
    let org_pinged = alice
        .call(Method::POST, &format!("{ORG_HOOKS}/{org_hook}/pings"))
        .await;

    // Then
    assert_eq!(pinged.status, StatusCode::NO_CONTENT);
    assert_eq!(org_pinged.status, StatusCode::NO_CONTENT);
    let pings = drained(&server).await;
    for deliveries in [&created, &pings] {
        let repo = one(deliveries, repo_hook, "ping");
        assert!(repo.signed_with("repo-secret"));
        assert_eq!(
            repo.header("x-github-hook-installation-target-type"),
            "repository"
        );
        assert_eq!(
            repo.header("x-github-hook-installation-target-id"),
            PUBLIC_REPO_ID.to_string()
        );
        let payload = repo.json();
        assert_eq!(payload["hook_id"], repo_hook);
        assert_eq!(payload["hook"]["id"], repo_hook);
        assert_eq!(payload["hook"]["type"], "Repository");
        assert!(payload["zen"].as_str().is_some_and(|z| !z.is_empty()));
        assert_eq!(payload["repository"]["id"], PUBLIC_REPO_ID);
        assert_eq!(payload["sender"]["id"], ALICE_ID);
        assert!(payload.get("installation").is_none());

        let org = one(deliveries, org_hook, "ping");
        assert!(org.signed_with("org-secret"));
        assert_eq!(
            org.header("x-github-hook-installation-target-type"),
            "organization"
        );
        assert_eq!(
            org.header("x-github-hook-installation-target-id"),
            ORG_ID.to_string()
        );
        let payload = org.json();
        assert_eq!(payload["hook"]["type"], "Organization");
        assert_eq!(payload["organization"]["id"], ORG_ID);
    }
}

#[tokio::test]
async fn an_organization_hook_is_created_read_changed_and_deleted() {
    // Given
    let server = server().await;
    let alice = alice(&server);

    // When
    let created = alice
        .post(ORG_HOOKS, &hook(&server, "org-secret", &["push"], true))
        .await;

    // Then
    assert_eq!(created.status, StatusCode::CREATED);
    let id = created.json()["id"].as_i64().unwrap();
    assert!(
        created
            .header("location")
            .ends_with(&format!("/orgs/acme/hooks/{id}"))
    );
    assert_eq!(created.json()["type"], "Organization");
    let listed = alice.get(ORG_HOOKS).await;
    assert_eq!(listed.status, StatusCode::OK);
    assert_eq!(listed.json()[0]["id"], id);
    let path = format!("{ORG_HOOKS}/{id}");
    assert_eq!(alice.get(&path).await.json()["events"], json!(["push"]));
    let updated = alice
        .call_json(Method::PATCH, &path, &json!({"events": ["issues"]}))
        .await;
    assert_eq!(updated.status, StatusCode::OK);
    assert_eq!(updated.json()["events"], json!(["issues"]));
    let config = alice.get(&format!("{path}/config")).await;
    assert_eq!(config.json()["content_type"], "json");
    let changed = alice
        .call_json(
            Method::PATCH,
            &format!("{path}/config"),
            &json!({"url": "https://example.com/hook"}),
        )
        .await;
    assert_eq!(changed.status, StatusCode::OK);
    assert_eq!(changed.json()["url"], "https://example.com/hook");
    assert_eq!(
        alice.call(Method::DELETE, &path).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(alice.get(&path).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_event_reaches_every_active_hook_that_subscribes_to_it() {
    // Given
    let server = server().await;
    let alice = alice(&server);
    let repo_hook = create(
        &alice,
        REPO_HOOKS,
        &hook(&server, "repo-secret", &["issues"], true),
    )
    .await;
    let org_hook = create(
        &alice,
        ORG_HOOKS,
        &hook(&server, "org-secret", &["*"], true),
    )
    .await;
    let push_only = create(
        &alice,
        REPO_HOOKS,
        &json!({"config": {"url": format!("{}?push", server.receiver.url())},
                "events": ["push"]}),
    )
    .await;
    let inactive = create(
        &alice,
        REPO_HOOKS,
        &json!({"active": false, "events": ["issues"],
                "config": {"url": format!("{}?inactive", server.receiver.url())}}),
    )
    .await;
    drained(&server).await;

    // When
    let issue = alice
        .post(
            "repos/acme/public/issues",
            &json!({"title": "hooks see this"}),
        )
        .await;
    assert_eq!(issue.status, StatusCode::CREATED);

    // Then
    let deliveries = drained(&server).await;
    for (hook_id, secret) in [(repo_hook, "repo-secret"), (org_hook, "org-secret")] {
        let delivery = one(&deliveries, hook_id, "issues");
        assert!(delivery.signed_with(secret));
        let payload = delivery.json();
        assert_eq!(payload["action"], "opened");
        assert_eq!(payload["issue"]["number"], issue.json()["number"]);
        assert_eq!(payload["repository"]["id"], PUBLIC_REPO_ID);
        assert_eq!(payload["sender"]["id"], ALICE_ID);
        assert!(payload.get("installation").is_none());
    }
    for hook_id in [push_only, inactive] {
        let hook_id = hook_id.to_string();
        assert!(
            deliveries
                .iter()
                .all(|d| d.header("x-github-hook-id") != hook_id)
        );
    }
}

#[tokio::test]
async fn a_form_hook_gets_the_payload_as_a_signed_form_parameter() {
    // Given
    let server = server().await;
    let alice = alice(&server);
    let id = create(
        &alice,
        REPO_HOOKS,
        &json!({"events": ["issues"],
                "config": {"url": server.receiver.url(), "content_type": "form",
                           "secret": "form-secret"}}),
    )
    .await;

    // When
    let deliveries = drained(&server).await;

    // Then
    let ping = one(&deliveries, id, "ping");
    assert_eq!(
        ping.header("content-type"),
        "application/x-www-form-urlencoded"
    );
    assert!(ping.body.starts_with(b"payload="));
    assert!(ping.signed_with("form-secret"));
    assert_eq!(ping.json()["hook_id"], id);
}

#[tokio::test]
async fn a_hook_keeps_its_own_delivery_log_and_redelivers_from_it() {
    // Given
    let server = server().await;
    let alice = alice(&server);
    let id = create(
        &alice,
        REPO_HOOKS,
        &hook(&server, "repo-secret", &["issues"], true),
    )
    .await;
    let other = create(
        &alice,
        ORG_HOOKS,
        &hook(&server, "org-secret", &["issues"], true),
    )
    .await;
    drained(&server).await;
    let log = format!("{REPO_HOOKS}/{id}/deliveries");

    // When
    let listed = alice.get(&log).await;

    // Then
    assert_eq!(listed.status, StatusCode::OK);
    let entries = listed.json();
    let entries = entries.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["event"], "ping");
    assert_eq!(entries[0]["redelivery"], false);
    let delivery_id = entries[0]["id"].as_i64().unwrap();
    let org_log = alice
        .get(&format!("{ORG_HOOKS}/{other}/deliveries"))
        .await
        .json();
    assert_ne!(org_log[0]["id"], delivery_id);
    let org_delivery = format!("{ORG_HOOKS}/{other}/deliveries/{}", org_log[0]["id"]);
    let org_got = alice.get(&org_delivery).await;
    assert_eq!(org_got.status, StatusCode::OK);
    assert_eq!(org_got.json()["request"]["payload"]["hook_id"], other);
    assert_eq!(
        alice
            .call(Method::POST, &format!("{org_delivery}/attempts"))
            .await
            .status,
        StatusCode::ACCEPTED
    );
    for list in [log.clone(), format!("{ORG_HOOKS}/{other}/deliveries")] {
        assert_eq!(
            alice.get(&format!("{list}?cursor=bogus")).await.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{list}"
        );
    }
    assert_eq!(
        alice
            .get(&format!("{ORG_HOOKS}/{other}/deliveries/{delivery_id}"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    let got = alice.get(&format!("{log}/{delivery_id}")).await;
    assert_eq!(got.status, StatusCode::OK);
    assert_eq!(got.json()["id"], delivery_id);
    assert_eq!(got.json()["event"], "ping");
    assert_eq!(got.json()["request"]["payload"]["hook_id"], id);

    let redelivered = alice
        .call(Method::POST, &format!("{log}/{delivery_id}/attempts"))
        .await;
    assert_eq!(redelivered.status, StatusCode::ACCEPTED);
    let again = drained(&server).await;
    let ping = one(&again, id, "ping");
    assert!(ping.signed_with("repo-secret"));
    assert_eq!(ping.json()["hook_id"], id);
    let entries = alice.get(&log).await.json();
    let redeliveries: Vec<_> = entries
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["redelivery"].clone())
        .collect();
    assert_eq!(redeliveries.len(), 2);
    assert!(redeliveries.contains(&json!(true)));
}

#[tokio::test]
async fn the_push_test_sends_a_push_only_to_a_hook_that_subscribes_to_push() {
    // Given
    let server = server().await;
    let alice = alice(&server);
    let push = create(
        &alice,
        REPO_HOOKS,
        &hook(&server, "push-secret", &["push"], true),
    )
    .await;
    let issues = create(
        &alice,
        REPO_HOOKS,
        &json!({"events": ["issues"],
                "config": {"url": format!("{}?issues", server.receiver.url())}}),
    )
    .await;
    drained(&server).await;

    // When
    let tested = alice
        .call(Method::POST, &format!("{REPO_HOOKS}/{push}/tests"))
        .await;
    let skipped = alice
        .call(Method::POST, &format!("{REPO_HOOKS}/{issues}/tests"))
        .await;

    // Then
    assert_eq!(tested.status, StatusCode::NO_CONTENT);
    assert_eq!(skipped.status, StatusCode::NO_CONTENT);
    let deliveries = drained(&server).await;
    let delivery = one(&deliveries, push, "push");
    assert!(delivery.signed_with("push-secret"));
    assert_eq!(delivery.json()["repository"]["id"], PUBLIC_REPO_ID);
    assert!(to_hook(&deliveries, issues, "push").is_empty());
}

#[tokio::test]
async fn the_test_url_of_a_hook_sends_the_push_test() {
    // Given
    let server = server().await;
    let alice = alice(&server);
    let created = alice
        .post(REPO_HOOKS, &hook(&server, "s", &["push"], true))
        .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let id = created.json()["id"].as_i64().unwrap();
    let test_url = created.json()["test_url"].as_str().unwrap().to_owned();
    drained(&server).await;

    // When
    let path = test_url.strip_prefix(&format!("{}/", server.base)).unwrap();
    let tested = alice.call(Method::POST, path).await;

    // Then
    assert!(
        test_url.ends_with(&format!("/hooks/{id}/test")),
        "{test_url}"
    );
    assert_eq!(tested.status, StatusCode::NO_CONTENT);
    assert_eq!(to_hook(&drained(&server).await, id, "push").len(), 1);
}

#[tokio::test]
async fn hook_links_keep_the_api_prefix_of_the_request() {
    // Given
    let server = server().await;
    let enterprise = GoGithub::enterprise(&server).with_token(HOOK_ADMIN_TOKEN);
    let prefix = format!("{}/api/v3/", server.base);

    // When
    let repo_hook = enterprise
        .post(REPO_HOOKS, &hook(&server, "s", &["push"], true))
        .await;
    let org_hook = enterprise
        .post(ORG_HOOKS, &hook(&server, "s", &["push"], true))
        .await;
    let id = repo_hook.json()["id"].as_i64().unwrap();
    let got = enterprise.get(&format!("{REPO_HOOKS}/{id}")).await;
    let listed = enterprise.get(ORG_HOOKS).await;

    // Then
    assert_eq!(
        repo_hook.header("location"),
        format!("{prefix}{REPO_HOOKS}/{id}")
    );
    assert!(org_hook.header("location").starts_with(&prefix));
    for body in [&repo_hook.json(), &got.json(), &listed.json()[0]] {
        for link in ["url", "ping_url", "deliveries_url"] {
            assert!(body[link].as_str().unwrap().starts_with(&prefix), "{body}");
        }
    }
    assert!(
        got.json()["test_url"]
            .as_str()
            .unwrap()
            .starts_with(&prefix)
    );
}

fn assert_denied(answer: &Answer, permission: &str) {
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    assert!(
        answer
            .header("x-accepted-github-permissions")
            .contains(permission)
    );
}

#[tokio::test]
async fn hooks_are_hidden_from_callers_who_cannot_administer_them() {
    // Given
    let server = with_hook_admin(
        TestServer::builder()
            .frame(hubdouble_api::register_all)
            .fixture()
            .entry(
                json!({"kind": "app", "id": 4009, "slug": "metadata-only", "owner": ORG,
                       "public_key": APP_PUBLIC_KEY,
                       "permissions": {"metadata": "read"}, "events": []}),
            )
            .entry(
                json!({"kind": "installation", "id": 5009, "app": "metadata-only",
                       "account": ORG, "repository_selection": "all",
                       "permissions": {"metadata": "read"}}),
            )
            .start()
            .await,
    );
    add_token(
        &server,
        "ghs_metadata_only",
        TokenKind::Installation,
        (Some(4009), Some(5009), None),
        &[],
    );
    let alice = alice(&server);
    let default_scopes = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let repo_hook = create(
        &alice,
        REPO_HOOKS,
        &hook(&server, "repo-secret", &["issues"], true),
    )
    .await;
    let org_hook = create(
        &alice,
        ORG_HOOKS,
        &hook(&server, "org-secret", &["issues"], true),
    )
    .await;
    let bob = GoGithub::new(&server).with_token(BOB_TOKEN);
    let anonymous = GoGithub::new(&server);
    let installation = GoGithub::new(&server).with_token("ghs_metadata_only");

    // When
    let unknown_repo = alice.get("repos/acme/missing/hooks").await;
    let unknown_hook = alice.get(&format!("{REPO_HOOKS}/999999999")).await;
    let other_owner = alice.get(&format!("{ORG_HOOKS}/{repo_hook}")).await;
    let member = bob.get(&format!("{REPO_HOOKS}/{repo_hook}")).await;
    let org_member = bob.get(&format!("{ORG_HOOKS}/{org_hook}")).await;
    let nobody = anonymous.get(REPO_HOOKS).await;
    let nobody_org = anonymous.get(ORG_HOOKS).await;
    let repo_denied = installation.get(REPO_HOOKS).await;
    let org_denied = installation.get(ORG_HOOKS).await;
    let scope_missing = default_scopes.get(ORG_HOOKS).await;

    // Then
    for answer in [
        &unknown_repo,
        &unknown_hook,
        &other_owner,
        &member,
        &org_member,
        &nobody,
        &nobody_org,
    ] {
        assert_eq!(answer.status, StatusCode::NOT_FOUND);
    }
    assert_denied(&repo_denied, "repository_hooks");
    assert_denied(&org_denied, "organization_hooks");
    assert_eq!(scope_missing.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn invalid_hooks_are_rejected() {
    // Given
    let server = server().await;
    let alice = alice(&server);
    let url = server.receiver.url().to_owned();
    let id = create(&alice, REPO_HOOKS, &json!({"config": {"url": url}})).await;
    let org_id = create(
        &alice,
        ORG_HOOKS,
        &json!({"name": "web", "config": {"url": url}}),
    )
    .await;
    let bodies = [
        json!({"config": {}}),
        json!({"name": "email", "config": {"url": "https://example.com/a"}}),
        json!({"config": {"url": "ftp://example.com/a"}}),
        json!({"config": {"url": "https://example.com/a", "content_type": "xml"}}),
        json!({"config": {"url": url}}),
    ];

    // When
    let mut answers = Vec::new();
    for body in &bodies {
        answers.push(alice.post(REPO_HOOKS, body).await);
    }
    answers.push(
        alice
            .post(ORG_HOOKS, &json!({"config": {"url": url}}))
            .await,
    );
    answers.push(
        alice
            .call_json(
                Method::PATCH,
                &format!("{REPO_HOOKS}/{id}/config"),
                &json!({"url": "not a url"}),
            )
            .await,
    );
    answers.push(
        alice
            .call_json(
                Method::PATCH,
                &format!("{REPO_HOOKS}/{id}"),
                &json!({"config": {"content_type": "xml"}}),
            )
            .await,
    );
    answers.push(
        alice
            .call_json(
                Method::PATCH,
                &format!("{ORG_HOOKS}/{org_id}"),
                &json!({"config": {"content_type": "xml"}}),
            )
            .await,
    );

    // Then
    for answer in &answers {
        assert_eq!(
            answer.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            answer.json()
        );
    }
    assert_eq!(
        alice.get(REPO_HOOKS).await.json().as_array().unwrap().len(),
        1
    );
}

#[tokio::test]
async fn an_update_may_not_give_a_hook_the_url_of_another_hook_of_its_owner() {
    // Given
    let server = server().await;
    let alice = alice(&server);
    let url = server.receiver.url().to_owned();
    let other = format!("{url}?other");
    let mut ids = Vec::new();
    for path in [REPO_HOOKS, ORG_HOOKS] {
        create(
            &alice,
            path,
            &json!({"name": "web", "config": {"url": url}}),
        )
        .await;
        ids.push(
            create(
                &alice,
                path,
                &json!({"name": "web", "config": {"url": other}}),
            )
            .await,
        );
    }
    let (repo_id, org_id) = (ids[0], ids[1]);

    // When
    let refused = [
        alice
            .call_json(
                Method::PATCH,
                &format!("{REPO_HOOKS}/{repo_id}"),
                &json!({"config": {"url": url}}),
            )
            .await,
        alice
            .call_json(
                Method::PATCH,
                &format!("{REPO_HOOKS}/{repo_id}/config"),
                &json!({"url": url}),
            )
            .await,
        alice
            .call_json(
                Method::PATCH,
                &format!("{ORG_HOOKS}/{org_id}"),
                &json!({"config": {"url": url}}),
            )
            .await,
        alice
            .call_json(
                Method::PATCH,
                &format!("{ORG_HOOKS}/{org_id}/config"),
                &json!({"url": url}),
            )
            .await,
    ];
    let kept = alice
        .call_json(
            Method::PATCH,
            &format!("{REPO_HOOKS}/{repo_id}"),
            &json!({"config": {"url": other}, "active": false}),
        )
        .await;

    // Then
    for answer in &refused {
        assert_invalid_hook(answer);
    }
    assert_eq!(kept.status, StatusCode::OK, "{}", kept.json());
    for (path, id) in [(REPO_HOOKS, repo_id), (ORG_HOOKS, org_id)] {
        let config = alice.get(&format!("{path}/{id}/config")).await.json();
        assert_eq!(config["url"], json!(other));
    }
}

fn assert_invalid_hook(answer: &Answer) {
    assert_eq!(
        answer.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        answer.json()
    );
    let errors = &answer.json()["errors"];
    assert_eq!(errors.as_array().map(Vec::len), Some(1), "{errors}");
    assert_eq!(errors[0]["resource"], "Hook");
    assert_eq!(errors[0]["code"], "custom");
}

#[tokio::test]
async fn a_hook_accepts_only_known_event_names() {
    // Given
    let server = server().await;
    let alice = alice(&server);
    let url = server.receiver.url().to_owned();
    let id = create(
        &alice,
        REPO_HOOKS,
        &json!({"events": ["push", "workflow_run", "security_and_analysis"],
                "config": {"url": url}}),
    )
    .await;
    let org_id = create(
        &alice,
        ORG_HOOKS,
        &json!({"name": "web", "events": ["team", "membership"], "config": {"url": url}}),
    )
    .await;

    // When
    let refused = [
        alice
            .post(
                REPO_HOOKS,
                &json!({"events": ["push", "pushes"],
                        "config": {"url": "https://example.com/a"}}),
            )
            .await,
        alice
            .post(
                ORG_HOOKS,
                &json!({"name": "web", "events": ["Issues"],
                        "config": {"url": "https://example.com/a"}}),
            )
            .await,
        alice
            .call_json(
                Method::PATCH,
                &format!("{REPO_HOOKS}/{id}"),
                &json!({"events": ["not_an_event"]}),
            )
            .await,
        alice
            .call_json(
                Method::PATCH,
                &format!("{REPO_HOOKS}/{id}"),
                &json!({"add_events": ["issues", "nope"]}),
            )
            .await,
        alice
            .call_json(
                Method::PATCH,
                &format!("{ORG_HOOKS}/{org_id}"),
                &json!({"events": [""]}),
            )
            .await,
        alice
            .post(
                REPO_HOOKS,
                &json!({"events": ["*", "push"],
                        "config": {"url": "https://example.com/b"}}),
            )
            .await,
        alice
            .call_json(
                Method::PATCH,
                &format!("{REPO_HOOKS}/{id}"),
                &json!({"add_events": ["*"]}),
            )
            .await,
    ];
    let changed = alice
        .call_json(
            Method::PATCH,
            &format!("{REPO_HOOKS}/{id}"),
            &json!({"add_events": ["issues"], "remove_events": ["security_and_analysis"]}),
        )
        .await;

    // Then
    for answer in &refused {
        assert_invalid_hook(answer);
    }
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.json());
    assert_eq!(
        changed.json()["events"],
        json!(["push", "workflow_run", "issues"])
    );
    assert_eq!(
        alice.get(REPO_HOOKS).await.json().as_array().unwrap().len(),
        1
    );
}

#[tokio::test]
async fn last_response_follows_the_latest_delivery_attempt() {
    // Given
    let server = server().await;
    let alice = alice(&server);
    let url = server.receiver.url().to_owned();

    // When
    let ok = alice
        .post(REPO_HOOKS, &json!({"config": {"url": url}}))
        .await;
    let failing = alice
        .post(
            REPO_HOOKS,
            &json!({"config": {"url": format!("{url}?status=404")}}),
        )
        .await;
    drained(&server).await;

    // Then
    for created in [&ok, &failing] {
        assert_eq!(created.status, StatusCode::CREATED);
        assert_eq!(
            created.json()["last_response"],
            json!({"code": null, "status": "unused", "message": null})
        );
    }
    let ok = alice
        .get(&format!("{REPO_HOOKS}/{}", ok.json()["id"]))
        .await
        .json();
    assert_eq!(
        ok["last_response"],
        json!({"code": 200, "status": "active", "message": "OK"})
    );
    let failing = alice
        .get(&format!("{REPO_HOOKS}/{}", failing.json()["id"]))
        .await
        .json();
    assert_eq!(failing["last_response"]["code"], 404);
    assert_eq!(failing["last_response"]["status"], "misconfigured");
    let listed = alice.get(REPO_HOOKS).await.json();
    let statuses: BTreeSet<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["last_response"]["status"].as_str())
        .collect();
    assert_eq!(statuses, BTreeSet::from(["active", "misconfigured"]));
}

async fn sha(client: &GoGithub, branch: &str) -> Value {
    let found = client
        .get(&format!("repos/acme/public/git/ref/heads/{branch}"))
        .await;
    assert_eq!(found.status, StatusCode::OK, "{}", found.json());
    found.json()["object"]["sha"].clone()
}

#[tokio::test]
async fn the_push_test_sends_the_latest_push_of_the_repository() {
    // Given
    let server = server().await;
    let alice = alice(&server);
    let id = create(
        &alice,
        REPO_HOOKS,
        &hook(&server, "push-secret", &["push"], true),
    )
    .await;
    let main = sha(&alice, "main").await;
    let created = alice
        .post(
            "repos/acme/public/git/refs",
            &json!({"ref": "refs/heads/topic", "sha": main}),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json());
    let moved = alice
        .call_json(
            Method::PATCH,
            "repos/acme/public/git/refs/heads/feature",
            &json!({"sha": main, "force": true}),
        )
        .await;
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.json());
    let pushes = drained(&server).await;
    let pushes = to_hook(&pushes, id, "push");
    assert_eq!(pushes.len(), 2);
    let latest = pushes[1].json();
    assert_eq!(latest["ref"], "refs/heads/feature");

    // When
    let tested = alice
        .call(Method::POST, &format!("{REPO_HOOKS}/{id}/tests"))
        .await;

    // Then
    assert_eq!(tested.status, StatusCode::NO_CONTENT);
    let deliveries = drained(&server).await;
    let delivery = one(&deliveries, id, "push");
    assert!(delivery.signed_with("push-secret"));
    let payload = delivery.json();
    for field in ["ref", "before", "after", "commits", "head_commit", "forced"] {
        assert_eq!(payload[field], latest[field], "{field}");
    }
    assert_eq!(payload["sender"]["login"], latest["sender"]["login"]);
    assert_eq!(payload["repository"]["id"], PUBLIC_REPO_ID);
}

#[tokio::test]
async fn deleting_a_repository_drops_its_hooks_and_its_latest_push() {
    // Given
    let server = server().await;
    let alice = alice(&server);
    create(&alice, REPO_HOOKS, &hook(&server, "s", &["push"], true)).await;
    let main = sha(&alice, "main").await;
    let created = alice
        .post(
            "repos/acme/public/git/refs",
            &json!({"ref": "refs/heads/topic", "sha": main}),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let rows = |server: &TestServer| {
        server
            .server
            .services()
            .store
            .view(|tx| {
                Ok((
                    tx.table::<Hooks>().children(PUBLIC_REPO_ID).count(),
                    tx.table::<LatestPushes>().in_repo(PUBLIC_REPO_ID).count(),
                ))
            })
            .unwrap()
    };
    assert_eq!(rows(&server), (1, 1));

    // When
    let deleted = server
        .admin(Method::DELETE, "repos/acme/public", None)
        .await;

    // Then
    assert!(deleted.status.is_success(), "{}", deleted.status);
    assert_eq!(rows(&server), (0, 0));
}

#[tokio::test]
async fn deleting_a_hook_drops_its_deliveries() {
    // Given
    let server = server().await;
    let alice = alice(&server);
    let id = create(&alice, REPO_HOOKS, &hook(&server, "s", &["push"], true)).await;
    let kept = create(&alice, ORG_HOOKS, &hook(&server, "s", &["push"], true)).await;
    drained(&server).await;
    let logged = |server: &TestServer, hook_id: i64| {
        server
            .server
            .services()
            .store
            .view(|tx| Ok(tx.table::<Deliveries>().children(hook_id).count()))
            .unwrap()
    };
    assert_eq!((logged(&server, id), logged(&server, kept)), (1, 1));

    // When
    let deleted = alice
        .call(Method::DELETE, &format!("{REPO_HOOKS}/{id}"))
        .await;

    // Then
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert_eq!((logged(&server, id), logged(&server, kept)), (0, 1));
}

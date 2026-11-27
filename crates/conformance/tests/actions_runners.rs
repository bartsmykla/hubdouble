use std::collections::BTreeMap;

use http::StatusCode;
use hubdouble_model::{Timestamp, Token, TokenKind};
use hubdouble_store::{Accounts, Tokens};
use hubdouble_testkit::fixture::APP_PUBLIC_KEY;
use hubdouble_testkit::{GoGithub, TestServer};
use serde_json::json;

const RUNNER: &str = "orgs/acme/actions/runners/23";
const INSTALLATION_TOKEN: &str = "ghs_org_runner";
const ADMIN_TOKEN: &str = "ghp_org_admin";
const MEMBER_TOKEN: &str = "ghp_org_member";
const APP_USER_TOKEN: &str = "ghu_org_admin";

#[tokio::test]
async fn self_hosted_runner_reads_require_access_to_its_organization() {
    let runner = json!({
        "id": 23,
        "name": "local-runner",
        "os": "Linux",
        "status": "online",
        "busy": true,
        "labels": [
            {"id": 5, "name": "self-hosted", "type": "read-only"},
            {"name": "local", "type": "custom"}
        ],
        "ephemeral": true,
        "runner_group_id": 1,
        "version": null
    });
    let mut seed = runner.clone();
    seed["kind"] = json!("actions_runner");
    seed["organization"] = json!("acme");
    let mut other_seed = seed.clone();
    other_seed["organization"] = json!("other");
    other_seed["id"] = json!(24);
    let server = TestServer::builder()
        .frame(hubdouble_api::register_all)
        .entry(json!({
            "kind": "user", "login": "alice",
            "tokens": [{"token": ADMIN_TOKEN, "scopes": ["admin:org"]}]
        }))
        .entry(json!({
            "kind": "user", "login": "bob",
            "tokens": [{"token": MEMBER_TOKEN, "scopes": ["admin:org"]}]
        }))
        .entry(json!({
            "kind": "org", "login": "acme",
            "members": [{"login": "alice", "role": "admin"}, {"login": "bob"}]
        }))
        .entry(json!({
            "kind": "org", "login": "other",
            "members": [{"login": "alice", "role": "admin"}]
        }))
        .entry(json!({
            "kind": "app", "id": 4001, "slug": "fixture-app", "owner": "acme",
            "public_key": APP_PUBLIC_KEY,
            "permissions": {"organization_self_hosted_runners": "read"}
        }))
        .entry(json!({
            "kind": "installation", "id": 5001, "app": "fixture-app", "account": "acme",
            "permissions": {"organization_self_hosted_runners": "read"}
        }))
        .entry(seed)
        .entry(other_seed)
        .start()
        .await;
    add_app_tokens(&server);
    let anonymous = GoGithub::enterprise(&server);
    let installation = anonymous.clone().with_token(INSTALLATION_TOKEN);
    let admin = anonymous.clone().with_token(ADMIN_TOKEN);
    let member = anonymous.clone().with_token(MEMBER_TOKEN);
    let app_user = anonymous.clone().with_token(APP_USER_TOKEN);

    let answer = installation.get("orgs/ACME/actions/runners/23").await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.json(), runner);
    assert_eq!(admin.get(RUNNER).await.status, StatusCode::OK);
    assert_eq!(app_user.get(RUNNER).await.status, StatusCode::OK);
    assert_eq!(anonymous.get(RUNNER).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(member.get(RUNNER).await.status, StatusCode::FORBIDDEN);

    let other_org_runner = "orgs/other/actions/runners/24";
    assert_eq!(admin.get(other_org_runner).await.status, StatusCode::OK);
    assert_eq!(
        app_user.get(other_org_runner).await.status,
        StatusCode::FORBIDDEN
    );
    let denied = installation.get(other_org_runner).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert_eq!(
        denied.header("x-accepted-github-permissions"),
        "organization_self_hosted_runners=read"
    );
    assert_eq!(
        installation
            .get("orgs/acme/actions/runners/24")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

fn add_app_tokens(server: &TestServer) {
    server
        .server
        .services()
        .store
        .update(|tx| {
            let installation = Token {
                id: tx.next_id(),
                token: INSTALLATION_TOKEN.to_owned(),
                kind: TokenKind::Installation,
                app_id: Some(4001),
                installation_id: Some(5001),
                user_id: None,
                repo_ids: None,
                permissions: BTreeMap::new(),
                scopes: Vec::new(),
                expires_at: None,
                revoked: false,
                owner_seeded: false,
                created_at: Timestamp::SEED,
            };
            let user_id = tx
                .table::<Accounts>()
                .by_login("alice")
                .unwrap_or_else(|| panic!("the seed has no alice account"))
                .id;
            let user = Token {
                id: tx.next_id(),
                token: APP_USER_TOKEN.to_owned(),
                kind: TokenKind::User,
                installation_id: None,
                user_id: Some(user_id),
                ..installation.clone()
            };
            let mut tokens = tx.table_mut::<Tokens>();
            tokens.insert(installation)?;
            tokens.insert(user)?;
            Ok(())
        })
        .unwrap_or_else(|error| panic!("add the App tokens: {error}"));
}

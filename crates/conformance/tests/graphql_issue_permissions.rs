use std::collections::BTreeMap;

use http::StatusCode;
use hubdouble_model::{Access, Clock, Token, TokenKind};
use hubdouble_store::Tokens;
use hubdouble_testkit::{GoGithub, Graphql, TestServer, fixture};
use serde_json::{Value, json};

const METADATA_ONLY: &str = "ghs_metadata_issue_test";
const PULLS_ONLY: &str = "ghs_pulls_only_issue_test";
const WITHOUT_SSO: &str = "ghu_without_sso_issue_test";

async fn server() -> TestServer {
    let mut builder = TestServer::builder().frame(hubdouble_api::register_all);
    for mut entry in fixture::seed() {
        match entry["id"].as_i64() {
            Some(fixture::ORG_ID) => entry["sso_enforced"] = json!(true),
            Some(fixture::PRIVATE_REPO_ID) => {
                entry["commits"] = json!([{
                    "branch": "feature", "message": "Add private file", "files": {"private.txt": "data"},
                }]);
            }
            Some(fixture::INSTALLATION_ID) => {
                entry["repositories"] = json!([fixture::PUBLIC_REPO, fixture::PRIVATE_REPO]);
                entry["permissions"]["pull_requests"] = json!("read");
            }
            _ => {}
        }
        builder = builder.entry(entry);
    }
    let server = builder
        .entry(json!({
            "kind": "pull_request", "repository": "acme/private", "user": "alice",
            "title": "Private pull", "head": "feature", "base": "main",
        }))
        .start()
        .await;
    server
        .server
        .services()
        .store
        .update_quiet(|tx| {
            for (token, kind, user_id, app_id, installation_id, permission) in [
                (
                    METADATA_ONLY,
                    TokenKind::Installation,
                    None,
                    Some(fixture::APP_ID),
                    Some(fixture::INSTALLATION_ID),
                    "metadata",
                ),
                (
                    PULLS_ONLY,
                    TokenKind::Installation,
                    None,
                    Some(fixture::APP_ID),
                    Some(fixture::INSTALLATION_ID),
                    "pull_requests",
                ),
                (
                    WITHOUT_SSO,
                    TokenKind::User,
                    Some(fixture::ALICE_ID),
                    None,
                    None,
                    "metadata",
                ),
            ] {
                let id = tx.next_id();
                tx.table_mut::<Tokens>().insert(Token {
                    id,
                    token: token.into(),
                    kind,
                    user_id,
                    app_id,
                    installation_id,
                    repo_ids: None,
                    permissions: BTreeMap::from([(permission.into(), Access::Read)]),
                    scopes: vec!["repo".into()],
                    expires_at: None,
                    revoked: false,
                    owner_seeded: false,
                    created_at: server.clock.now(),
                })?;
            }
            Ok(())
        })
        .unwrap_or_else(|error| panic!("test token setup failed: {error}"));
    server
}

#[tokio::test]
async fn graphql_issue_reads_check_repository_grants_and_sso() {
    let server = server().await;
    let owner = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    let issues = "repos/acme/private/issues";
    let issue = owner.post(issues, &json!({"title":"Private issue"})).await;
    assert_eq!(issue.status, StatusCode::CREATED);
    let issue = issue.json();
    let comment = owner
        .post(
            &format!("{issues}/{}/comments", issue["number"]),
            &json!({"body":"Private comment"}),
        )
        .await;
    assert_eq!(comment.status, StatusCode::CREATED);
    let query = r#"query($number: Int!, $issue: ID!, $comment: ID!) {
        repository(owner: "acme", name: "private") { name issue(number: $number) { title } }
        issue: node(id: $issue) { ... on Issue { title } }
        comment: node(id: $comment) { ... on IssueComment { body } }
    }"#;
    let vars = json!({"number": issue["number"], "issue": issue["node_id"], "comment": comment.json()["node_id"]});
    for token in [METADATA_ONLY, WITHOUT_SSO] {
        let denied = Graphql::new(GoGithub::new(&server).with_token(token))
            .query(query, &vars)
            .await
            .json();
        assert_eq!(denied["data"]["repository"]["name"], "private");
        assert_eq!(denied["data"]["repository"]["issue"], Value::Null);
        assert_eq!(denied["data"]["issue"], Value::Null);
        assert_eq!(denied["data"]["comment"], Value::Null);
        let errors = denied["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 3);
        assert!(
            errors.iter().all(|error| error["type"] == "FORBIDDEN"),
            "{denied}"
        );
    }
    let allowed = Graphql::new(owner).query(query, &vars).await.json();
    assert!(allowed.get("errors").is_none(), "{allowed}");
    assert_eq!(
        allowed["data"]["repository"]["issue"]["title"],
        "Private issue"
    );
    assert_eq!(allowed["data"]["issue"]["title"], "Private issue");
    assert_eq!(allowed["data"]["comment"]["body"], "Private comment");
}

#[tokio::test]
async fn graphql_pull_commit_tree_requires_contents_read() {
    let server = server().await;
    let client = Graphql::new(GoGithub::new(&server).with_token(PULLS_ONLY));
    let pull = client
        .query(
            r#"{ repository(owner: "acme", name: "private") {
        pullRequest(number: 1) { number }
    } }"#,
            &json!({}),
        )
        .await
        .json();
    assert_eq!(pull["data"]["repository"]["pullRequest"]["number"], 1);

    let query = r#"{ repository(owner: "acme", name: "private") {
        pullRequest(number: 1) { commits(first: 1) { nodes { commit { tree { oid } } } } }
    } }"#;
    let denied = client.query(query, &json!({})).await.json();
    assert_eq!(denied["errors"][0]["type"], "FORBIDDEN", "{denied}");
    assert_eq!(
        denied["errors"][0]["path"],
        json!([
            "repository",
            "pullRequest",
            "commits",
            "nodes",
            0,
            "commit",
            "tree"
        ])
    );

    let allowed = Graphql::new(GoGithub::new(&server).with_token(fixture::ALICE_TOKEN))
        .query(query, &json!({}))
        .await
        .json();
    assert!(allowed.get("errors").is_none(), "{allowed}");
    assert!(allowed["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]["tree"]["oid"].is_string());
}

#[tokio::test]
async fn graphql_collaborators_require_sso_for_a_private_repository() {
    let server = server().await;
    let query = r#"{ repository(owner: "acme", name: "private") {
        collaborators(first: 1) { edges { permission } }
    } }"#;
    let denied = Graphql::new(GoGithub::new(&server).with_token(WITHOUT_SSO))
        .query(query, &json!({}))
        .await
        .json();
    assert_eq!(denied["errors"][0]["type"], "FORBIDDEN", "{denied}");
    let allowed = Graphql::new(GoGithub::new(&server).with_token(fixture::ALICE_TOKEN))
        .query(query, &json!({}))
        .await
        .json();
    assert!(allowed.get("errors").is_none(), "{allowed}");
    assert!(allowed["data"]["repository"]["collaborators"]["edges"][0]["permission"].is_string());
}

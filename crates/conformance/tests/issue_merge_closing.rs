use http::StatusCode;
use hubdouble_testkit::fixture::{BOB_ID, CAROL_TOKEN, PUBLIC_REPO_ID};
use hubdouble_testkit::{GoGithub, TestServer};
use serde_json::json;

#[tokio::test]
async fn a_sibling_fork_head_update_closes_the_pulls_linked_issue() {
    const HEAD_REPO: i64 = 3100;
    const BASE: &str = "repos/carol/public";
    const FEATURE: &str = "refs/heads/feature";
    let server = TestServer::builder()
        .fixture()
        .frame(hubdouble_api::register_all)
        .entry(json!({
            "kind": "repository", "id": HEAD_REPO, "owner": "bob", "name": "public",
            "fork_of": "acme/public", "visibility": "public"
        }))
        .entry(json!({
            "kind": "repository", "owner": "carol", "name": "public",
            "fork_of": "acme/public", "visibility": "public", "has_issues": true
        }))
        .start()
        .await;
    let client = GoGithub::new(&server).with_token(CAROL_TOKEN);
    let issue = client
        .post(&format!("{BASE}/issues"), &json!({"title": "Fix a fault"}))
        .await;
    assert_eq!(issue.status, StatusCode::CREATED, "{}", issue.json());
    let n = issue.json()["number"].as_i64().unwrap();
    let pull = client
        .post(
            &format!("{BASE}/pulls"),
            &json!({
                "title": "Fix the fault", "head": "bob:feature", "base": "main",
                "body": format!("Fixes #{n}")
            }),
        )
        .await;
    assert_eq!(pull.status, StatusCode::CREATED, "{}", pull.json());
    let pull_number = pull.json()["number"].as_i64().unwrap();
    let services = server.server.services();
    services
        .store
        .update(|tx| {
            let git = &services.git;
            let before = git.resolve_ref(tx, HEAD_REPO, FEATURE)?;
            let base = git.resolve_ref(tx, PUBLIC_REPO_ID, "refs/heads/main")?;
            Ok(git.update_ref(
                tx,
                HEAD_REPO,
                BOB_ID,
                FEATURE,
                Some(before),
                Some(base),
                true,
            )?)
        })
        .unwrap();

    let pull = client.get(&format!("{BASE}/pulls/{pull_number}")).await;
    assert_eq!(pull.status, StatusCode::OK);
    assert_eq!(pull.json()["merged"], true);
    let issue = client.get(&format!("{BASE}/issues/{n}")).await;
    assert_eq!(issue.status, StatusCode::OK);
    assert_eq!(issue.json()["state"], "closed");
    assert_eq!(issue.json()["closed_by"]["login"], "bob");
}

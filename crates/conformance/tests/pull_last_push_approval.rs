use std::sync::Arc;

use http::{Method, StatusCode};
use hubdouble_git::{EntryMode, FileChange, Signature};
use hubdouble_testkit::fixture::{self, ALICE_ID, ALICE_TOKEN, BOB_TOKEN, PUBLIC_REPO_ID};
use hubdouble_testkit::{GoGithub, Graphql, TestServer};
use serde_json::{Value, json};

const PULL: &str = "repos/acme/public/pulls/1";

async fn server() -> TestServer {
    let mut entries = fixture::seed();
    for entry in &mut entries {
        if entry["kind"] == "repository" && entry["name"] == fixture::PUBLIC_REPO {
            entry["collaborators"] = json!([{"login": "bob", "permission": "write"}]);
        }
    }
    entries.push(
        json!({"kind": "ruleset", "repository": "acme/public", "name": "rules",
        "target": "branch", "enforcement": "active", "bypass_actors": [],
        "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}},
        "rules": [{"type": "pull_request", "parameters": {
            "required_approving_review_count": 1, "dismiss_stale_reviews_on_push": false,
            "require_code_owner_review": false, "require_last_push_approval": true,
            "required_review_thread_resolution": false}}]}),
    );
    entries.push(json!({"kind": "mergeability", "repository": "acme/public", "reads": 0}));
    entries.push(
        json!({"kind": "pull_request", "repository": "acme/public", "user": "alice",
        "title": "feature", "head": "feature", "base": "main"}),
    );
    let mut builder = TestServer::builder().frame(hubdouble_api::register_all);
    for entry in entries {
        builder = builder.entry(entry);
    }
    builder.start().await
}

fn push_feature(server: &TestServer, path: &str) {
    let services = server.server.services();
    let sig = Signature::seed();
    services
        .store
        .update(|tx| {
            let parent = services
                .git
                .resolve_ref(tx, PUBLIC_REPO_ID, "refs/heads/feature")?;
            Ok(services.git.commit(
                tx,
                PUBLIC_REPO_ID,
                ALICE_ID,
                "feature",
                Some(parent),
                &[FileChange::Write {
                    path: path.into(),
                    data: Arc::from(&b"more\n"[..]),
                    mode: EntryMode::File,
                }],
                "more",
                &sig,
                &sig,
                false,
            )?)
        })
        .unwrap_or_else(|e| panic!("push feature: {e:?}"));
}

async fn decision(client: &GoGithub) -> (Value, Value) {
    let pull = client.get(PULL).await.json();
    let answer = Graphql::new(client.clone())
        .query(
            "query($id: ID!) {node(id: $id) {... on PullRequest {reviewDecision}}}",
            &json!({"id": pull["node_id"]}),
        )
        .await
        .json();
    (
        pull["mergeable_state"].clone(),
        answer["data"]["node"]["reviewDecision"].clone(),
    )
}

#[tokio::test]
async fn a_pending_review_started_before_the_last_push_approves_it_when_submitted_after() {
    // Given a main that needs the last push approved, and a pending review that bob started on the old head
    let server = server().await;
    let alice = GoGithub::enterprise(&server).with_token(ALICE_TOKEN);
    let bob = GoGithub::enterprise(&server).with_token(BOB_TOKEN);
    let pending = bob
        .post(&format!("{PULL}/reviews"), &json!({"body": "looking"}))
        .await;
    assert_eq!(pending.status, StatusCode::OK, "{}", pending.json());
    assert_eq!(pending.json()["state"], "PENDING");
    // When alice pushes a change in the same second and bob then submits his pending review as an approval
    push_feature(&server, "more.txt");
    let submitted = bob
        .call_json(
            Method::POST,
            &format!("{PULL}/reviews/{}/events", pending.json()["id"]),
            &json!({"event": "APPROVE"}),
        )
        .await;
    assert_eq!(submitted.status, StatusCode::OK, "{}", submitted.json());
    // Then the approval came after the last push, so it approves that push
    assert_eq!(decision(&alice).await, (json!("clean"), json!("APPROVED")));
    // When alice pushes another change in the same second
    push_feature(&server, "again.txt");
    // Then the approval is older than the last push
    assert_eq!(
        decision(&alice).await,
        (json!("blocked"), json!("REVIEW_REQUIRED"))
    );
}

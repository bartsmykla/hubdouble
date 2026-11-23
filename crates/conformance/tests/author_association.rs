use std::sync::Arc;

use http::{Method, StatusCode};
use hubdouble_git::{EntryMode, FileChange, Signature};
use hubdouble_model::Timestamp;
use hubdouble_testkit::fixture::{
    self, ALICE_ID, ALICE_TOKEN, BOB_TOKEN, CAROL_TOKEN, PUBLIC_REPO_ID,
};
use hubdouble_testkit::{Delivery, GoGithub, Graphql, TestServer};
use serde_json::{Value, json};

const DAVE_TOKEN: &str = "ghp_da7e0000000000000000000000000000000000";
const ERIN_TOKEN: &str = "ghp_e41n0000000000000000000000000000000000";
const FRANK_TOKEN: &str = "ghp_f4a70000000000000000000000000000000000";
const ISSUES: &str = "repos/acme/public/issues";

fn fork(owner: &str, branch: &str, email: &str) -> Value {
    json!({"kind": "repository", "owner": owner, "name": "public", "fork_of": "acme/public",
           "commits": [{"branch": branch, "message": format!("{owner}'s change"),
                        "files": {format!("{owner}.txt"): "change\n"},
                        "author": {"name": owner, "email": email}}]})
}

async fn server() -> TestServer {
    let mut entries = fixture::seed();
    for entry in &mut entries {
        if entry["kind"] == "app" {
            entry["events"] = json!([
                "issue_comment",
                "pull_request",
                "pull_request_review",
                "pull_request_review_comment"
            ]);
        }
        if entry["kind"] == "repository" && entry["name"] == fixture::PUBLIC_REPO {
            entry["collaborators"] = json!([{"login": "carol", "permission": "read"}]);
        }
    }
    entries.extend([
        json!({"kind": "user", "login": "dave", "email": "dave@example.com",
               "tokens": [{"token": DAVE_TOKEN}]}),
        json!({"kind": "user", "login": "erin", "email": "erin@example.com",
               "tokens": [{"token": ERIN_TOKEN}]}),
        json!({"kind": "user", "login": "frank", "email": "frank@example.com",
               "tokens": [{"token": FRANK_TOKEN}]}),
        json!({"kind": "repository", "owner": "dave", "name": "tools", "visibility": "public",
               "has_issues": true, "files": {"README.md": "# tools\n"}}),
        fork("dave", "dave-work", "dave@example.com"),
        fork("erin", "erin-work", "erin@example.com"),
        json!({"kind": "pull_request", "repository": "acme/public", "user": "dave",
               "title": "dave's change", "head": "dave:dave-work", "base": "main"}),
        json!({"kind": "pull_request", "repository": "acme/public", "user": "erin",
               "title": "erin's change", "head": "erin:erin-work", "base": "main"}),
    ]);
    let mut builder = TestServer::builder().frame(hubdouble_api::register_all);
    for entry in entries {
        builder = builder.entry(entry);
    }
    builder.start().await
}

fn client(server: &TestServer, token: &str) -> GoGithub {
    GoGithub::enterprise(server).with_token(token)
}

async fn created(client: &GoGithub, path: &str, body: &Value) -> Value {
    let answer = client.post(path, body).await;
    assert!(
        matches!(answer.status, StatusCode::CREATED | StatusCode::OK),
        "{path}: {}",
        answer.json()
    );
    answer.json()
}

async fn comment(client: &GoGithub, number: i64) -> Value {
    created(
        client,
        &format!("{ISSUES}/{number}/comments"),
        &json!({"body": "a comment"}),
    )
    .await
}

async fn graphql_association(client: &GoGithub, node_id: &Value, kind: &str) -> Value {
    let answer = Graphql::new(client.clone())
        .query(
            &format!("query($id: ID!) {{node(id: $id) {{... on {kind} {{authorAssociation}}}}}}"),
            &json!({"id": node_id}),
        )
        .await
        .json();
    assert_eq!(answer.get("errors"), None, "{kind}: {answer}");
    answer["data"]["node"]["authorAssociation"].clone()
}

async fn deliveries(server: &TestServer) -> Vec<Delivery> {
    let drained = server.admin(Method::POST, "hooks/drain", None).await;
    assert_eq!(drained.status, StatusCode::NO_CONTENT);
    server.receiver.received().await
}

fn hook_associations(all: &[Delivery], event: &str, action: &str, object: &str) -> Vec<Value> {
    all.iter()
        .filter(|d| d.header("x-github-event") == event)
        .map(Delivery::json)
        .filter(|payload| payload["action"] == action)
        .map(|payload| payload[object]["author_association"].clone())
        .collect()
}

fn push_to_main(server: &TestServer, email: &str) {
    let services = server.server.services();
    let author = Signature {
        name: "pusher".into(),
        email: email.into(),
        time: Timestamp::SEED,
        offset_minutes: 0,
    };
    services
        .store
        .update(|tx| {
            let parent = services
                .git
                .resolve_ref(tx, PUBLIC_REPO_ID, "refs/heads/main")?;
            Ok(services.git.commit(
                tx,
                PUBLIC_REPO_ID,
                ALICE_ID,
                "main",
                Some(parent),
                &[FileChange::Write {
                    path: "pushed.txt".into(),
                    data: Arc::from(&b"pushed\n"[..]),
                    mode: EntryMode::File,
                }],
                "a pushed change",
                &author,
                &Signature::seed(),
                false,
            )?)
        })
        .unwrap_or_else(|e| panic!("push to main: {e:?}"));
}

#[tokio::test]
async fn a_merged_pull_request_makes_its_author_a_contributor_on_every_surface() {
    // Given open first pull requests by dave and erin, and an issue by alice
    let server = server().await;
    let alice = client(&server, ALICE_TOKEN);
    let dave = client(&server, DAVE_TOKEN);
    let issue = created(&alice, ISSUES, &json!({"title": "an issue"})).await;
    let number = issue["number"].as_i64().unwrap_or_default();
    let first = comment(&dave, number).await;
    // Then an author with only an open first pull request has no association, as live GitHub answers
    assert_eq!(first["author_association"], "NONE");
    assert_eq!(
        alice.get("repos/acme/public/pulls/1").await.json()["author_association"],
        "NONE"
    );
    deliveries(&server).await;
    // When alice merges dave's pull request
    let merged = alice
        .call_json(Method::PUT, "repos/acme/public/pulls/1/merge", &json!({}))
        .await;
    assert_eq!(merged.status, StatusCode::OK, "{}", merged.json());
    let second = comment(&dave, number).await;
    let head = alice.get("repos/acme/public/pulls/2").await.json()["head"]["sha"].clone();
    let review = created(
        &dave,
        "repos/acme/public/pulls/2/reviews",
        &json!({"event": "COMMENT", "body": "looks fine", "commit_id": head,
                "comments": [{"body": "this line", "path": "erin.txt", "line": 1, "side": "RIGHT"}]}),
    )
    .await;
    let review_comment = alice.get("repos/acme/public/pulls/2/comments").await.json()[0].clone();
    // Then dave is a contributor on every REST answer, on his earlier comment too
    let pull = alice.get("repos/acme/public/pulls/1").await.json();
    let comments = alice
        .get(&format!("{ISSUES}/{number}/comments"))
        .await
        .json();
    let read_review = alice
        .get(&format!(
            "repos/acme/public/pulls/2/reviews/{}",
            review["id"]
        ))
        .await
        .json();
    let read_comment = alice
        .get(&format!(
            "repos/acme/public/pulls/comments/{}",
            review_comment["id"]
        ))
        .await
        .json();
    assert_eq!(
        [
            &pull["author_association"],
            &comments[0]["author_association"],
            &comments[1]["author_association"],
            &second["author_association"],
            &read_review["author_association"],
            &read_comment["author_association"],
        ],
        [&json!("CONTRIBUTOR"); 6]
    );
    // Then the webhooks name him a contributor
    let all = deliveries(&server).await;
    let contributor = vec![json!("CONTRIBUTOR")];
    assert_eq!(
        hook_associations(&all, "pull_request", "closed", "pull_request"),
        contributor
    );
    assert_eq!(
        hook_associations(&all, "issue_comment", "created", "comment"),
        contributor
    );
    assert_eq!(
        hook_associations(&all, "pull_request_review", "submitted", "review"),
        contributor
    );
    assert_eq!(
        hook_associations(&all, "pull_request_review_comment", "created", "comment"),
        contributor
    );
    // Then GraphQL authorAssociation names him a contributor
    for (node, kind) in [
        (&pull["node_id"], "PullRequest"),
        (&second["node_id"], "IssueComment"),
        (&read_review["node_id"], "PullRequestReview"),
        (&read_comment["node_id"], "PullRequestReviewComment"),
    ] {
        assert_eq!(
            graphql_association(&alice, node, kind).await,
            "CONTRIBUTOR",
            "{kind}"
        );
    }
    // Then erin's open first pull request still has no association
    let erin = alice.get("repos/acme/public/pulls/2").await.json();
    assert_eq!(erin["author_association"], "NONE");
    assert_eq!(
        graphql_association(&alice, &erin["node_id"], "PullRequest").await,
        "NONE"
    );
}

#[tokio::test]
async fn each_association_follows_the_authors_relation_to_the_repository() {
    // Given an issue on acme/public and one on dave's own repository
    let server = server().await;
    let alice = client(&server, ALICE_TOKEN);
    let issue = created(&alice, ISSUES, &json!({"title": "an issue"})).await;
    let number = issue["number"].as_i64().unwrap_or_default();
    let dave = client(&server, DAVE_TOKEN);
    let own = created(&dave, "repos/dave/tools/issues", &json!({"title": "mine"})).await;
    // Then the owner of a user repository is its owner
    assert_eq!(own["author_association"], "OWNER");
    assert_eq!(
        graphql_association(&dave, &own["node_id"], "Issue").await,
        "OWNER"
    );
    // Then org members, a collaborator and an outsider get their relation
    for (token, want) in [
        (ALICE_TOKEN, "MEMBER"),
        (BOB_TOKEN, "MEMBER"),
        (CAROL_TOKEN, "COLLABORATOR"),
        (FRANK_TOKEN, "NONE"),
    ] {
        let written = comment(&client(&server, token), number).await;
        assert_eq!(written["author_association"], want, "{token}");
        assert_eq!(
            graphql_association(&alice, &written["node_id"], "IssueComment").await,
            want,
            "{token}"
        );
    }
    // When a commit by frank lands on the default branch
    push_to_main(&server, "frank@example.com");
    // Then frank is a contributor
    let written = comment(&client(&server, FRANK_TOKEN), number).await;
    assert_eq!(written["author_association"], "CONTRIBUTOR");
}

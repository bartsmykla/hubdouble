use std::sync::Arc;

use http::{Method, StatusCode};
use hubdouble_git::{EntryMode, FileChange, ObjectId, Signature};
use hubdouble_testkit::fixture::{ALICE_ID, ALICE_TOKEN, PUBLIC_REPO_ID, WEBHOOK_SECRET};
use hubdouble_testkit::validate::webhook_payload;
use hubdouble_testkit::{Delivery, GoGithub, Graphql, TestServer};
use serde_json::{Value, json};

const PULL: &str = "repos/acme/public/pulls/1";
const ISSUE: &str = "repos/acme/public/issues/1";

#[derive(Clone, Copy, Debug)]
enum Via {
    Pulls,
    Issues,
    Graphql,
}

const EVERY_WAY: [Via; 3] = [Via::Pulls, Via::Issues, Via::Graphql];

enum Outcome {
    Reopened,
    Refused,
}

async fn server(entries: &[Value]) -> TestServer {
    let mut builder = TestServer::builder()
        .fixture()
        .register(hubdouble_api_pulls::register)
        .register(hubdouble_api_issues::register);
    for entry in entries {
        builder = builder.entry(entry.clone());
    }
    builder.start().await
}

fn pull_request(number_hint: &str, base: &str) -> Value {
    json!({"kind": "pull_request", "repository": "acme/public", "user": "alice",
        "title": format!("Feature {number_hint}"), "head": "feature", "base": base})
}

fn alice(server: &TestServer) -> GoGithub {
    GoGithub::enterprise(server).with_token(ALICE_TOKEN)
}

async fn close(client: &GoGithub, path: &str) {
    let closed = client
        .call_json(Method::PATCH, path, &json!({"state": "closed"}))
        .await;
    assert_eq!(closed.status, StatusCode::OK, "{}", closed.json());
}

async fn reopen(client: &GoGithub, via: Via, number: i64) -> Outcome {
    let rest = |kind: &str| format!("repos/acme/public/{kind}/{number}");
    let path = match via {
        Via::Pulls => rest("pulls"),
        Via::Issues => rest("issues"),
        Via::Graphql => {
            let id = client.get(&rest("pulls")).await.json()["node_id"].clone();
            let answer = Graphql::new(client.clone())
                .query(
                    "mutation($id: ID!) {reopenPullRequest(input: {pullRequestId: $id, clientMutationId: \"m1\"}) {clientMutationId pullRequest {state}}}",
                    &json!({"id": id}),
                )
                .await
                .json();
            if let Some(errors) = answer.get("errors") {
                assert_eq!(errors[0]["type"], "UNPROCESSABLE", "{via:?}: {answer}");
                assert_eq!(answer["data"]["reopenPullRequest"], Value::Null);
                return Outcome::Refused;
            }
            assert_eq!(
                answer["data"]["reopenPullRequest"],
                json!({"clientMutationId": "m1", "pullRequest": {"state": "OPEN"}})
            );
            return Outcome::Reopened;
        }
    };
    let answer = client
        .call_json(Method::PATCH, &path, &json!({"state": "open"}))
        .await;
    if answer.status == StatusCode::OK {
        return Outcome::Reopened;
    }
    assert_eq!(
        answer.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{via:?}: {}",
        answer.json()
    );
    let error = &answer.json()["errors"][0];
    assert_eq!(
        (&error["resource"], &error["field"], &error["code"]),
        (&json!("PullRequest"), &json!("state"), &json!("custom")),
        "{via:?}"
    );
    assert!(error["message"].is_string(), "{via:?}: {error}");
    Outcome::Refused
}

fn tip(server: &TestServer, branch: &str) -> ObjectId {
    let services = server.server.services();
    services
        .store
        .view(|tx| {
            Ok(services
                .git
                .resolve_ref(tx, PUBLIC_REPO_ID, &format!("refs/heads/{branch}"))?)
        })
        .unwrap_or_else(|e| panic!("read {branch}: {e:?}"))
}

fn push(server: &TestServer, branch: &str, path: &str, text: &str) -> ObjectId {
    let services = server.server.services();
    let parent = tip(server, branch);
    let sig = Signature::seed();
    services
        .store
        .update(|tx| {
            Ok(services.git.commit(
                tx,
                PUBLIC_REPO_ID,
                ALICE_ID,
                branch,
                Some(parent),
                &[FileChange::Write {
                    path: path.into(),
                    data: Arc::from(text.as_bytes()),
                    mode: EntryMode::File,
                }],
                "change while closed",
                &sig,
                &sig,
                false,
            )?)
        })
        .unwrap_or_else(|e| panic!("push {branch}: {e:?}"))
}

fn move_branch(server: &TestServer, branch: &str, to: Option<ObjectId>) {
    let services = server.server.services();
    let from = tip(server, branch);
    services
        .store
        .update(|tx| {
            Ok(services.git.update_ref(
                tx,
                PUBLIC_REPO_ID,
                ALICE_ID,
                &format!("refs/heads/{branch}"),
                Some(from),
                to,
                true,
            )?)
        })
        .unwrap_or_else(|e| panic!("move {branch}: {e:?}"));
}

async fn deliveries(server: &TestServer) -> Vec<Delivery> {
    let drained = server.admin(Method::POST, "hooks/drain", None).await;
    assert_eq!(drained.status, StatusCode::NO_CONTENT);
    server.receiver.received().await
}

fn of(all: &[Delivery], event: &str, action: &str) -> Vec<Value> {
    all.iter()
        .filter(|d| d.header("x-github-event") == event && d.json()["action"] == action)
        .map(|d| {
            assert!(d.signed_with(WEBHOOK_SECRET));
            let payload = d.json();
            webhook_payload(event, Some(action), &payload)
                .unwrap_or_else(|errors| panic!("{event}.{action}: {errors:?}"));
            payload
        })
        .collect()
}

#[tokio::test]
async fn every_reopen_takes_the_live_branches_and_sends_the_same_webhook() {
    let mut answers = Vec::new();
    for via in EVERY_WAY {
        // Given a closed pull request whose head and base both moved, to conflicting files, while it was closed
        let server = server(&[pull_request("one", "main")]).await;
        let client = alice(&server);
        close(&client, PULL).await;
        let head = push(&server, "feature", "README.md", "# from the feature\n");
        let base = push(&server, "main", "README.md", "# from main\n");
        server.clock.advance(60);
        // When it is reopened
        let outcome = reopen(&client, via, 1).await;
        // Then it is open on the new tips, not mergeable, with no merge commit
        assert!(matches!(outcome, Outcome::Reopened), "{via:?}");
        let pull = client.get(PULL).await.json();
        assert_eq!(pull["state"], "open", "{via:?}");
        assert_eq!(pull["closed_at"], Value::Null, "{via:?}");
        assert_eq!(pull["head"]["sha"], json!(head), "{via:?}");
        assert_eq!(pull["base"]["sha"], json!(base), "{via:?}");
        assert_eq!(pull["mergeable"], false, "{via:?}");
        assert_eq!(pull["merge_commit_sha"], Value::Null, "{via:?}");
        assert_eq!(pull["commits"], 2, "{via:?}");
        let issue = client.get(ISSUE).await.json();
        assert_eq!(issue["state_reason"], Value::Null, "{via:?}");
        // And the timeline lists the close and one reopened event by alice, with GitHub's fields
        let timeline = client.get(&format!("{ISSUE}/events")).await.json();
        let kinds: Vec<_> = timeline
            .as_array()
            .into_iter()
            .flatten()
            .map(|e| e["event"].clone())
            .collect();
        assert_eq!(kinds, [json!("closed"), json!("reopened")], "{via:?}");
        let reopened_event = &timeline[1];
        assert_eq!(reopened_event["actor"]["login"], "alice", "{via:?}");
        assert_eq!(reopened_event["state_reason"], "reopened", "{via:?}");
        assert!(reopened_event.get("intent").is_none(), "{via:?}");
        assert_eq!(reopened_event["created_at"], pull["updated_at"], "{via:?}");
        // And one pull_request reopened webhook carries that pull request, and no issues event is sent
        let all = deliveries(&server).await;
        let reopened = of(&all, "pull_request", "reopened");
        assert_eq!(reopened.len(), 1, "{via:?}");
        assert_eq!(reopened[0]["number"], 1, "{via:?}");
        assert_eq!(reopened[0]["sender"]["login"], "alice", "{via:?}");
        assert_eq!(reopened[0]["pull_request"]["head"]["sha"], json!(head));
        assert_eq!(reopened[0]["pull_request"]["mergeable"], false);
        assert!(
            all.iter().all(|d| d.header("x-github-event") != "issues"),
            "{via:?}"
        );
        let host = client
            .base()
            .trim_start_matches("http://")
            .trim_end_matches("/api/v3/");
        let portable = |v: &Value| -> Value {
            serde_json::from_str(&v.to_string().replace(host, "host")).unwrap()
        };
        answers.push((
            portable(&pull),
            portable(&reopened[0]["pull_request"]),
            portable(&timeline),
        ));
    }
    // And every way gives the same pull request and the same webhook
    for (other, via) in answers.iter().zip(EVERY_WAY).skip(1) {
        assert_eq!(answers[0], *other, "{via:?}");
    }
}

async fn assert_refused(server: &TestServer, via: Via, number: i64) {
    let client = alice(server);
    let path = format!("repos/acme/public/pulls/{number}");
    let before = client.get(&path).await.json();
    server.clock.advance(60);
    assert!(
        matches!(reopen(&client, via, number).await, Outcome::Refused),
        "{via:?}"
    );
    let pull = client.get(&path).await.json();
    assert_eq!(pull["state"], "closed", "{via:?}");
    assert_eq!(pull, before, "{via:?}");
    let all = deliveries(server).await;
    assert!(of(&all, "pull_request", "reopened").is_empty(), "{via:?}");
}

#[tokio::test]
async fn a_reopen_is_refused_while_another_pull_request_is_open_from_the_same_head_to_the_same_base()
 {
    for via in EVERY_WAY {
        // Given a closed pull request and a newer open one from feature to main
        let server = server(&[pull_request("one", "main")]).await;
        let client = alice(&server);
        close(&client, PULL).await;
        let newer = client
            .post(
                "repos/acme/public/pulls",
                &json!({"title": "Again", "head": "feature", "base": "main"}),
            )
            .await;
        assert_eq!(newer.status, StatusCode::CREATED, "{}", newer.json());
        // When the old one is reopened, then it is refused and stays closed
        assert_refused(&server, via, 1).await;
    }
}

#[tokio::test]
async fn a_reopen_is_refused_once_the_head_or_the_base_branch_is_deleted() {
    for via in EVERY_WAY {
        // Given a closed pull request into main and one into develop
        let server = server(&[pull_request("one", "main"), pull_request("two", "develop")]).await;
        let client = alice(&server);
        close(&client, PULL).await;
        close(&client, "repos/acme/public/pulls/2").await;
        // When develop is deleted, then the one into develop cannot be reopened
        move_branch(&server, "develop", None);
        assert_refused(&server, via, 2).await;
        // When feature is deleted, then the one into main cannot be reopened
        move_branch(&server, "feature", None);
        assert_refused(&server, via, 1).await;
    }
}

#[tokio::test]
async fn a_reopen_is_refused_after_the_head_branch_is_force_pushed() {
    for via in EVERY_WAY {
        // Given a closed pull request whose head branch was moved back to main
        let server = server(&[pull_request("one", "main")]).await;
        let client = alice(&server);
        close(&client, PULL).await;
        let main = tip(&server, "main");
        move_branch(&server, "feature", Some(main));
        push(&server, "feature", "src/other.txt", "other\n");
        // When it is reopened, then it is refused and stays closed
        assert_refused(&server, via, 1).await;
    }
}

#[tokio::test]
async fn a_pull_request_closed_and_reopened_through_issues_has_no_state_reason() {
    // Given an open pull request
    let server = server(&[pull_request("one", "main")]).await;
    let client = alice(&server);
    // When it is closed as not planned through the issues endpoint
    let closed = client
        .call_json(
            Method::PATCH,
            ISSUE,
            &json!({"state": "closed", "state_reason": "not_planned"}),
        )
        .await;
    let reopened = client
        .call_json(Method::PATCH, ISSUE, &json!({"state": "open"}))
        .await;
    // Then neither answer has a state reason, as on GitHub
    assert_eq!(closed.status, StatusCode::OK);
    assert_eq!(closed.json()["state"], "closed");
    assert_eq!(closed.json()["state_reason"], Value::Null);
    assert_eq!(reopened.status, StatusCode::OK);
    assert_eq!(reopened.json()["state"], "open");
    assert_eq!(reopened.json()["state_reason"], Value::Null);
}

#[tokio::test]
async fn a_merged_pull_request_stays_closed_through_every_way() {
    for via in EVERY_WAY {
        // Given a pull request merged by a push of its head to main
        let server = server(&[pull_request("one", "main")]).await;
        let feature = tip(&server, "feature");
        let main = tip(&server, "main");
        let services = server.server.services();
        services
            .store
            .update(|tx| {
                Ok(services.git.update_ref(
                    tx,
                    PUBLIC_REPO_ID,
                    ALICE_ID,
                    "refs/heads/main",
                    Some(main),
                    Some(feature),
                    false,
                )?)
            })
            .unwrap();
        assert_eq!(alice(&server).get(PULL).await.json()["merged"], true);
        // When it is reopened, then it is refused and stays closed
        assert_refused(&server, via, 1).await;
    }
}

use std::sync::Arc;

use http::{Method, StatusCode};
use hubdouble_git::{EntryMode, FileChange, ObjectId, Signature};
use hubdouble_testkit::fixture::{self, ALICE_ID, ALICE_TOKEN, PUBLIC_REPO_ID};
use hubdouble_testkit::{GoGithub, Graphql, TestServer};
use serde_json::{Value, json};

const REPO: &str = "repos/acme/public";

const COMMON_KEYS: [&str; 9] = [
    "actor",
    "commit_id",
    "commit_url",
    "created_at",
    "event",
    "id",
    "node_id",
    "performed_via_github_app",
    "url",
];

const TEAM_KEYS: [&str; 15] = [
    "description",
    "html_url",
    "id",
    "members_url",
    "name",
    "node_id",
    "notification_setting",
    "organization_id",
    "parent",
    "permission",
    "privacy",
    "repositories_url",
    "slug",
    "type",
    "url",
];

async fn server() -> TestServer {
    let mut entries = fixture::seed();
    for entry in &mut entries {
        if entry["kind"] == "repository" && entry["name"] == fixture::PUBLIC_REPO {
            entry["collaborators"] = json!([{"login": "bob", "permission": "write"}]);
        }
    }
    entries.push(json!({"kind": "team", "org": "acme", "name": "Reviewers",
        "privacy": "closed", "members": [{"login": "bob"}],
        "repositories": [{"repository": "acme/public", "permission": "write"}]}));
    let mut builder = TestServer::builder().frame(hubdouble_api::register_all);
    for entry in entries {
        builder = builder.entry(entry);
    }
    builder.start().await
}

fn alice(server: &TestServer) -> GoGithub {
    GoGithub::enterprise(server).with_token(ALICE_TOKEN)
}

async fn open(client: &GoGithub, draft: bool) -> Value {
    let opened = client
        .post(
            &format!("{REPO}/pulls"),
            &json!({"title": "feature", "head": "feature", "base": "main", "draft": draft}),
        )
        .await;
    assert_eq!(opened.status, StatusCode::CREATED, "{}", opened.json());
    opened.json()
}

async fn events(client: &GoGithub, number: &Value) -> Vec<Value> {
    let answer = client.get(&format!("{REPO}/issues/{number}/events")).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
    answer.json().as_array().cloned().unwrap_or_default()
}

fn named<'a>(events: &'a [Value], name: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["event"] == name).collect()
}

fn keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<_> = value
        .as_object()
        .map_or_default(|o| o.keys().cloned().collect());
    keys.sort();
    keys
}

fn keys_with(extra: &[&str]) -> Vec<String> {
    let mut keys: Vec<String> = COMMON_KEYS
        .iter()
        .chain(extra)
        .map(|k| (*k).to_owned())
        .collect();
    keys.sort();
    keys
}

fn prefix(event: &Value) -> Option<&str> {
    event["node_id"].as_str()?.split_once('_').map(|(p, _)| p)
}

async fn reviewers(client: &GoGithub, number: &Value, method: Method) {
    let expected = if method == Method::POST {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let answer = client
        .call_json(
            method,
            &format!("{REPO}/pulls/{number}/requested_reviewers"),
            &json!({"reviewers": ["bob"], "team_reviewers": ["reviewers"]}),
        )
        .await;
    assert_eq!(answer.status, expected, "{}", answer.json());
}

fn assert_review_request(events: &[Value], name: &str, node_prefix: &str) {
    let found = named(events, name);
    assert_eq!(found.len(), 2, "{name}: {events:?}");
    let (user, team) = (found[0], found[1]);
    assert_eq!(
        keys(user),
        keys_with(&["requested_reviewer", "review_requester"])
    );
    assert_eq!(
        keys(team),
        keys_with(&["requested_team", "review_requester"])
    );
    for event in [user, team] {
        assert_eq!(prefix(event), Some(node_prefix), "{name}");
        assert_eq!(event["review_requester"]["login"], "alice", "{name}");
        assert_eq!(event["actor"]["login"], "alice", "{name}");
        assert_eq!(event["commit_id"], Value::Null, "{name}");
        assert_eq!(event["commit_url"], Value::Null, "{name}");
    }
    assert_eq!(user["requested_reviewer"]["login"], "bob", "{name}");
    assert_eq!(keys(&team["requested_team"]), TEAM_KEYS.map(String::from));
    assert_eq!(team["requested_team"]["slug"], "reviewers", "{name}");
    assert_eq!(team["requested_team"]["type"], "organization", "{name}");
    assert_eq!(
        team["requested_team"]["organization_id"],
        json!(fixture::ORG_ID),
        "{name}"
    );
}

#[tokio::test]
async fn a_review_request_records_review_requested_for_a_user_and_a_team() {
    // Given an open pull request of alice
    let server = server().await;
    let alice = alice(&server);
    let pull = open(&alice, false).await;
    // When alice requests reviews from bob and the team reviewers
    reviewers(&alice, &pull["number"], Method::POST).await;
    // Then the issue events list one review_requested event for each, by alice
    assert_review_request(
        &events(&alice, &pull["number"]).await,
        "review_requested",
        "RRE",
    );
}

#[tokio::test]
async fn removing_a_review_request_records_review_request_removed_for_a_user_and_a_team() {
    // Given an open pull request of alice with reviews requested from bob and a team
    let server = server().await;
    let alice = alice(&server);
    let pull = open(&alice, false).await;
    reviewers(&alice, &pull["number"], Method::POST).await;
    // When alice removes both requests
    reviewers(&alice, &pull["number"], Method::DELETE).await;
    // Then the issue events list one review_request_removed event for each, after the requests
    let recorded = events(&alice, &pull["number"]).await;
    assert_review_request(&recorded, "review_request_removed", "RRRE");
    let names: Vec<&Value> = recorded.iter().map(|e| &e["event"]).collect();
    assert_eq!(
        names,
        [
            "review_requested",
            "review_requested",
            "review_request_removed",
            "review_request_removed"
        ]
    );
}

async fn set_draft(client: &GoGithub, pull: &Value, draft: bool) {
    let mutation = if draft {
        "convertPullRequestToDraft"
    } else {
        "markPullRequestReadyForReview"
    };
    let answer = Graphql::new(client.clone())
        .query(
            &format!(
                "mutation($id: ID!) {{ {mutation}(input: {{pullRequestId: $id}}) \
                 {{ pullRequest {{ isDraft }} }} }}"
            ),
            &json!({"id": pull["node_id"]}),
        )
        .await
        .json();
    assert_eq!(
        answer["data"][mutation]["pullRequest"]["isDraft"], draft,
        "{answer}"
    );
}

fn assert_plain(events: &[Value], name: &str, node_prefix: &str) {
    let found = named(events, name);
    assert_eq!(found.len(), 1, "{name}: {events:?}");
    let event = found[0];
    assert_eq!(keys(event), keys_with(&[]), "{name}");
    assert_eq!(prefix(event), Some(node_prefix), "{name}");
    assert_eq!(event["actor"]["login"], "alice", "{name}");
    assert_eq!(event["commit_id"], Value::Null, "{name}");
}

#[tokio::test]
async fn marking_a_draft_ready_records_ready_for_review() {
    // Given a draft pull request of alice
    let server = server().await;
    let alice = alice(&server);
    let pull = open(&alice, true).await;
    // When alice marks it ready for review, and again when it is ready
    set_draft(&alice, &pull, false).await;
    set_draft(&alice, &pull, false).await;
    // Then the issue events list one ready_for_review event, by alice
    let recorded = events(&alice, &pull["number"]).await;
    assert_plain(&recorded, "ready_for_review", "RFRE");
    let changed = alice.get(&format!("{REPO}/pulls/{}", pull["number"])).await;
    assert_eq!(
        named(&recorded, "ready_for_review")[0]["created_at"],
        changed.json()["updated_at"]
    );
}

#[tokio::test]
async fn converting_a_ready_pull_request_to_a_draft_records_convert_to_draft() {
    // Given a ready pull request of alice
    let server = server().await;
    let alice = alice(&server);
    let pull = open(&alice, false).await;
    // When alice converts it to a draft, and again when it is a draft
    set_draft(&alice, &pull, true).await;
    set_draft(&alice, &pull, true).await;
    // Then the issue events list one convert_to_draft event, by alice
    assert_plain(
        &events(&alice, &pull["number"]).await,
        "convert_to_draft",
        "CTDE",
    );
}

fn commit(server: &TestServer, branch: &str, parent: ObjectId, path: &str) -> ObjectId {
    let services = server.server.services();
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
                    data: Arc::from(b"changed\n".as_slice()),
                    mode: EntryMode::File,
                }],
                &format!("change {path}"),
                &sig,
                &sig,
                false,
            )?)
        })
        .unwrap_or_else(|e| panic!("commit on {branch}: {e:?}"))
}

async fn tip(client: &GoGithub, branch: &str) -> ObjectId {
    let answer = client.get(&format!("{REPO}/git/ref/heads/{branch}")).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
    answer.json()["object"]["sha"]
        .as_str()
        .and_then(|sha| sha.parse().ok())
        .unwrap_or(ObjectId::NULL)
}

#[tokio::test]
async fn a_head_push_that_drops_the_old_head_records_head_ref_force_pushed_at_the_new_head() {
    // Given an open pull request of feature
    let server = server().await;
    let alice = alice(&server);
    let pull = open(&alice, false).await;
    // When feature moves forward, and then is forced to a commit on main that drops its head
    let forward = commit(&server, "feature", tip(&alice, "feature").await, "a.txt");
    let created = alice
        .post(
            &format!("{REPO}/git/refs"),
            &json!({"ref": "refs/heads/rewrite", "sha": tip(&alice, "main").await.to_string()}),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json());
    let rewrite = commit(&server, "rewrite", tip(&alice, "main").await, "b.txt");
    let forced = alice
        .call_json(
            Method::PATCH,
            &format!("{REPO}/git/refs/heads/feature"),
            &json!({"sha": rewrite.to_string(), "force": true}),
        )
        .await;
    assert_eq!(forced.status, StatusCode::OK, "{}", forced.json());
    // Then only the forced push records head_ref_force_pushed, with the new head as its commit
    let recorded = events(&alice, &pull["number"]).await;
    let found = named(&recorded, "head_ref_force_pushed");
    assert_eq!(found.len(), 1, "{recorded:?}");
    let event = found[0];
    assert_eq!(keys(event), keys_with(&[]));
    assert_eq!(prefix(event), Some("HRFPE"));
    assert_eq!(event["actor"]["login"], "alice");
    assert_eq!(event["commit_id"], json!(rewrite.to_string()));
    assert_ne!(event["commit_id"], json!(forward.to_string()));
    let url = event["commit_url"].as_str().unwrap_or_default();
    assert!(
        url.ends_with(&format!("/repos/acme/public/commits/{rewrite}")),
        "{url}"
    );
}

use std::collections::BTreeMap;
use std::sync::Arc;

use http::{Method, StatusCode};
use hubdouble_git::{EntryMode, FileChange, ObjectId, Signature};
use hubdouble_model::{Repo, Timestamp, Token, TokenKind};
use hubdouble_store::Repos;
use hubdouble_testkit::fixture::{self, ALICE_ID, ALICE_TOKEN, PUBLIC_REPO_ID, WEBHOOK_SECRET};
use hubdouble_testkit::validate::webhook_payload;
use hubdouble_testkit::{Delivery, GoGithub, Graphql, TestServer};
use serde_json::{Value, json};

const PULL: &str = "repos/acme/public/pulls/1";
const ISSUE: &str = "repos/acme/public/issues/1";
const MERGE_REF: &str = "repos/acme/public/git/ref/pull/1/merge";
const HEAD_REF: &str = "repos/acme/public/git/ref/pull/1/head";

async fn server(entries: &[Value]) -> TestServer {
    let mut builder = TestServer::builder()
        .fixture()
        .register(hubdouble_api_gitdata::register)
        .register(hubdouble_api_repos::register)
        .register(hubdouble_api_pulls::register)
        .register(hubdouble_api_issues::register);
    for entry in entries {
        builder = builder.entry(entry.clone());
    }
    builder.start().await
}

fn pull_request(head: &str, base: &str) -> Value {
    json!({"kind": "pull_request", "repository": "acme/public", "user": "alice",
        "title": format!("{head} into {base}"), "head": head, "base": base})
}

fn alice(server: &TestServer) -> GoGithub {
    GoGithub::enterprise(server).with_token(ALICE_TOKEN)
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
                &format!("change {path}"),
                &sig,
                &sig,
                false,
            )?)
        })
        .unwrap_or_else(|e| panic!("push {branch}: {e:?}"))
}

async fn ref_target(client: &GoGithub, path: &str) -> Option<String> {
    let answer = client.get(path).await;
    if answer.status == StatusCode::NOT_FOUND {
        return None;
    }
    assert_eq!(answer.status, StatusCode::OK, "{path}: {}", answer.json());
    let body = answer.json();
    assert_eq!(body["object"]["type"], "commit", "{path}");
    body["object"]["sha"].as_str().map(str::to_owned)
}

async fn merge_commit_sha(client: &GoGithub) -> Value {
    client.get(PULL).await.json()["merge_commit_sha"].clone()
}

async fn graphql_merge_commit(client: &GoGithub) -> Value {
    let id = client.get(PULL).await.json()["node_id"].clone();
    let answer = Graphql::new(client.clone())
        .query(
            "query($id: ID!) {node(id: $id) {... on PullRequest {mergeCommit {oid}}}}",
            &json!({"id": id}),
        )
        .await
        .json();
    assert_eq!(answer.get("errors"), None, "{answer}");
    answer["data"]["node"]["mergeCommit"].clone()
}

async fn parents(client: &GoGithub, sha: &str) -> Vec<Value> {
    let commit = client
        .get(&format!("repos/acme/public/git/commits/{sha}"))
        .await
        .json();
    commit["parents"]
        .as_array()
        .map_or_default(|all| all.iter().map(|p| p["sha"].clone()).collect())
}

async fn deliveries(server: &TestServer) -> Vec<Delivery> {
    let drained = server.admin(Method::POST, "hooks/drain", None).await;
    assert_eq!(drained.status, StatusCode::NO_CONTENT);
    server.receiver.received().await
}

fn pull_hooks(all: &[Delivery], number: i64) -> Vec<(String, Value)> {
    all.iter()
        .filter(|d| d.header("x-github-event") == "pull_request")
        .map(|d| {
            assert!(d.signed_with(WEBHOOK_SECRET));
            let payload = d.json();
            let action = payload["action"].as_str().unwrap_or_default().to_owned();
            webhook_payload("pull_request", Some(&action), &payload)
                .unwrap_or_else(|errors| panic!("pull_request.{action}: {errors:?}"));
            (action, payload)
        })
        .filter(|(_, payload)| payload["number"] == number)
        .collect()
}

async fn events(client: &GoGithub, number: i64) -> Vec<Value> {
    let answer = client
        .get(&format!("repos/acme/public/issues/{number}/events"))
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
    answer.json().as_array().cloned().unwrap_or_default()
}

fn names(events: &[Value]) -> Vec<&str> {
    events.iter().filter_map(|e| e["event"].as_str()).collect()
}

fn assert_refused(answer: &hubdouble_testkit::Answer, field: &str, code: &str) {
    assert_eq!(
        answer.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        answer.json()
    );
    let errors = answer.json()["errors"].clone();
    assert_eq!(errors.as_array().map(Vec::len), Some(1), "{errors}");
    let error = &errors[0];
    assert_eq!(
        (&error["resource"], &error["field"], &error["code"]),
        (&json!("PullRequest"), &json!(field), &json!(code)),
        "{error}"
    );
}

#[tokio::test]
async fn a_refused_reopen_keeps_only_the_body_on_both_edit_endpoints() {
    for path in [PULL, ISSUE] {
        // Given a closed pull request while another one is open from the same head into the same base
        let server = server(&[pull_request("feature", "main")]).await;
        let client = alice(&server);
        let closed = client
            .call_json(Method::PATCH, PULL, &json!({"state": "closed"}))
            .await;
        assert_eq!(closed.status, StatusCode::OK);
        let other = client
            .call_json(
                Method::POST,
                "repos/acme/public/pulls",
                &json!({"title": "again", "head": "feature", "base": "main"}),
            )
            .await;
        assert_eq!(other.status, StatusCode::CREATED, "{}", other.json());
        deliveries(&server).await;
        // When a reopen is sent with a new title and a new body
        let answer = client
            .call_json(
                Method::PATCH,
                path,
                &json!({"state": "open", "title": "New title", "body": "New body"}),
            )
            .await;
        // Then the reopen is refused, and only the body is written
        assert_refused(&answer, "state", "custom");
        let pull = client.get(PULL).await.json();
        assert_eq!(
            (&pull["state"], &pull["title"], &pull["body"]),
            (
                &json!("closed"),
                &json!("feature into main"),
                &json!("New body")
            ),
            "{path}"
        );
        // Then the kept body sends its edited webhook, with no rename or reopen
        let all = deliveries(&server).await;
        let hooks = pull_hooks(&all, 1);
        let issue_hooks: Vec<_> = all
            .iter()
            .filter(|d| d.header("x-github-event") == "issues")
            .map(|d| d.json()["changes"].clone())
            .collect();
        let changes: Vec<_> = hooks
            .iter()
            .map(|(action, payload)| (action.as_str(), payload["changes"].clone()))
            .chain(
                issue_hooks
                    .iter()
                    .map(|changes| ("edited", changes.clone())),
            )
            .collect();
        assert_eq!(
            changes,
            [("edited", json!({"body": {"from": ""}}))],
            "{path}"
        );
        assert!(
            !names(&events(&client, 1).await).contains(&"renamed"),
            "{path}"
        );
    }
}

#[tokio::test]
async fn a_blank_title_is_refused_before_anything_is_written() {
    // Given an open pull request
    let server = server(&[pull_request("feature", "main")]).await;
    let client = alice(&server);
    // When a blank title is sent with a new body and a close
    let answer = client
        .call_json(
            Method::PATCH,
            PULL,
            &json!({"title": " ", "body": "New body", "state": "closed"}),
        )
        .await;
    // Then it is refused and the pull request is unchanged
    assert_refused(&answer, "title", "missing_field");
    let pull = client.get(PULL).await.json();
    assert_eq!(
        (&pull["state"], &pull["body"]),
        (&json!("open"), &Value::Null)
    );
}

#[tokio::test]
async fn a_refused_base_keeps_the_other_writes_of_the_edit() {
    let cases = [
        json!({"state": "closed", "base": "develop"}),
        json!({"base": "missing"}),
        json!({"base": "feature"}),
        json!({"base": "develop"}),
    ];
    for case in cases {
        // Given an open pull request from feature into main, and another from feature into develop
        let server = server(&[
            pull_request("feature", "main"),
            pull_request("feature", "develop"),
        ])
        .await;
        let client = alice(&server);
        deliveries(&server).await;
        // When a base change that GitHub refuses is sent with a new title and body
        let mut body = case.clone();
        body["title"] = json!("New title");
        body["body"] = json!("New body");
        let answer = client.call_json(Method::PATCH, PULL, &body).await;
        // Then the base is refused and stays, while the title, the body and the state change are kept
        assert_refused(&answer, "base", "invalid");
        let pull = client.get(PULL).await.json();
        let state = case["state"].as_str().unwrap_or("open");
        assert_eq!(
            (
                &pull["base"]["ref"],
                &pull["title"],
                &pull["body"],
                &pull["state"]
            ),
            (
                &json!("main"),
                &json!("New title"),
                &json!("New body"),
                &json!(state)
            ),
            "{case}"
        );
        // Then the kept parts send their webhooks and record their events
        let hooks = pull_hooks(&deliveries(&server).await, 1);
        let actions: Vec<_> = hooks.iter().map(|(action, _)| action.as_str()).collect();
        let expected: &[&str] = if state == "closed" {
            &["edited", "closed"]
        } else {
            &["edited"]
        };
        assert_eq!(actions, expected, "{case}");
        assert_eq!(
            hooks[0].1["changes"],
            json!({"title": {"from": "feature into main"}, "body": {"from": ""}}),
            "{case}"
        );
        let recorded = events(&client, 1).await;
        let expected: &[&str] = if state == "closed" {
            &["renamed", "closed"]
        } else {
            &["renamed"]
        };
        assert_eq!(names(&recorded), expected, "{case}");
    }
}

#[tokio::test]
async fn a_reopen_with_a_refused_base_answers_the_base_refusal_and_keeps_the_body() {
    // Given a merged pull request, which cannot be reopened
    let server = server(&[pull_request("feature", "main")]).await;
    let client = alice(&server);
    let merged = client
        .call_json(Method::PUT, &format!("{PULL}/merge"), &json!({}))
        .await;
    assert_eq!(merged.status, StatusCode::OK);
    // When a reopen is sent with a base change and a new body
    let answer = client
        .call_json(
            Method::PATCH,
            PULL,
            &json!({"state": "open", "base": "develop", "body": "New body"}),
        )
        .await;
    // Then the base, judged against the pull request left closed, is the refusal, and the body is kept
    assert_refused(&answer, "base", "invalid");
    let pull = client.get(PULL).await.json();
    assert_eq!(
        (&pull["state"], &pull["base"]["ref"], &pull["body"]),
        (&json!("closed"), &json!("main"), &json!("New body"))
    );
}

#[tokio::test]
async fn a_reopen_with_a_rename_and_a_new_base_records_them_in_github_order() {
    // Given a closed pull request from feature into main
    let server = server(&[pull_request("feature", "main")]).await;
    let client = alice(&server);
    let closed = client
        .call_json(Method::PATCH, PULL, &json!({"state": "closed"}))
        .await;
    assert_eq!(closed.status, StatusCode::OK);
    deliveries(&server).await;
    // When it is reopened with a new title and the base develop
    let answer = client
        .call_json(
            Method::PATCH,
            PULL,
            &json!({"state": "open", "title": "New title", "base": "develop"}),
        )
        .await;
    // Then all three are written, and the timeline has the rename, the reopen and the base change in that order
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
    assert_eq!(answer.json()["base"]["ref"], "develop");
    let recorded = events(&client, 1).await;
    let tail: Vec<_> = names(&recorded).into_iter().skip(1).collect();
    assert_eq!(tail, ["renamed", "reopened", "base_ref_changed"]);
    let changed = &recorded[3];
    assert_eq!(changed["actor"]["login"], "alice");
    assert_eq!(
        changed.as_object().map(|o| {
            let mut keys: Vec<_> = o.keys().cloned().collect();
            keys.sort();
            keys
        }),
        Some(
            [
                "actor",
                "commit_id",
                "commit_url",
                "created_at",
                "event",
                "id",
                "node_id",
                "performed_via_github_app",
                "url"
            ]
            .map(String::from)
            .to_vec()
        )
    );
    // Then the edited webhook names the old title and the old base
    let hooks = pull_hooks(&deliveries(&server).await, 1);
    let edited = hooks
        .iter()
        .find(|(action, _)| action == "edited")
        .map_or_default(|(_, payload)| payload["changes"].clone());
    assert_eq!(edited["title"], json!({"from": "feature into main"}));
    assert_eq!(edited["base"]["ref"], json!({"from": "main"}));
}

#[tokio::test]
async fn an_open_pull_request_names_a_signed_test_merge_commit_at_its_merge_ref() {
    // Given a pull request from feature into main, opened by alice
    let server = server(&[pull_request("feature", "main")]).await;
    let client = alice(&server);
    let (head, base) = (tip(&server, "feature"), tip(&server, "main"));
    // When the pull request is read
    let pull = client.get(PULL).await.json();
    // Then merge_commit_sha names the commit at refs/pull/1/merge, and refs/pull/1/head is the head
    assert_eq!(pull["mergeable"], true);
    let sha = pull["merge_commit_sha"]
        .as_str()
        .unwrap_or_else(|| panic!("no merge_commit_sha: {pull}"))
        .to_owned();
    assert_eq!(ref_target(&client, MERGE_REF).await, Some(sha.clone()));
    assert_eq!(ref_target(&client, HEAD_REF).await, Some(head.to_string()));
    let listed = client
        .get("repos/acme/public/git/matching-refs/pull/1")
        .await
        .json();
    let names: Vec<_> = listed
        .as_array()
        .map_or_default(|all| all.iter().map(|r| r["ref"].clone()).collect());
    assert_eq!(
        names,
        [json!("refs/pull/1/head"), json!("refs/pull/1/merge")]
    );
    // Then the commit merges the head into the base tip as GitHub writes it, signed by web-flow
    let commit = client
        .get(&format!("repos/acme/public/git/commits/{sha}"))
        .await
        .json();
    assert_eq!(parents(&client, &sha).await, [json!(base), json!(head)]);
    assert_eq!(commit["message"], format!("Merge {head} into {base}"));
    assert_eq!(
        (&commit["author"]["name"], &commit["author"]["email"]),
        (&json!("Alice"), &json!("alice@example.com"))
    );
    assert_eq!(
        (&commit["committer"]["name"], &commit["committer"]["email"]),
        (&json!("GitHub"), &json!("noreply@github.com"))
    );
    assert_eq!(commit["author"]["date"], commit["committer"]["date"]);
    assert_eq!(
        (
            &commit["verification"]["verified"],
            &commit["verification"]["reason"]
        ),
        (&json!(true), &json!("valid"))
    );
    // Then GraphQL mergeCommit stays null until the pull request merges
    assert_eq!(graphql_merge_commit(&client).await, Value::Null);
}

#[tokio::test]
async fn a_base_push_gives_every_open_pull_request_on_it_its_own_test_merge() {
    // Given three open pull requests into main, the third changing the README
    let mut entries = fixture::seed();
    for entry in &mut entries {
        if entry["kind"] == "repository" && entry["name"] == fixture::PUBLIC_REPO {
            entry["commits"] = json!([
                {"branch": "one", "message": "one", "files": {"one.txt": "one\n"}},
                {"branch": "two", "message": "two", "files": {"two.txt": "two\n"}},
                {"branch": "clash", "message": "clash", "files": {"README.md": "# clash\n"}},
            ]);
        }
    }
    let mut builder = TestServer::builder()
        .register(hubdouble_api_gitdata::register)
        .register(hubdouble_api_repos::register)
        .register(hubdouble_api_pulls::register)
        .register(hubdouble_api_issues::register);
    for entry in entries.into_iter().chain(
        ["one", "two", "clash"]
            .into_iter()
            .map(|head| pull_request(head, "main")),
    ) {
        builder = builder.entry(entry);
    }
    let server = builder.start().await;
    let client = alice(&server);

    // When main changes the README
    let base = push(&server, "main", "README.md", "# main\n");

    // Then each clean pull request names its own test merge of the new base tip and its head
    let mut merges = Vec::new();
    for (number, head) in [(1, "one"), (2, "two")] {
        let pull = client
            .get(&format!("repos/acme/public/pulls/{number}"))
            .await
            .json();
        assert_eq!(pull["mergeable"], true, "pull request {number}");
        let sha = pull["merge_commit_sha"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert_eq!(
            parents(&client, &sha).await,
            [json!(base), json!(tip(&server, head))],
            "pull request {number}"
        );
        let merge_ref = format!("repos/acme/public/git/ref/pull/{number}/merge");
        assert_eq!(ref_target(&client, &merge_ref).await, Some(sha.clone()));
        merges.push(sha);
    }
    assert_ne!(merges[0], merges[1]);
    // Then the pull request with a conflict has no test merge
    let clash = client.get("repos/acme/public/pulls/3").await.json();
    assert_eq!(clash["mergeable"], false);
    assert_eq!(clash["merge_commit_sha"], Value::Null);
}

#[tokio::test]
async fn the_test_merge_follows_the_head_and_the_base_and_stays_behind_on_a_conflict() {
    // Given an open pull request from feature into main
    let server = server(&[pull_request("feature", "main")]).await;
    let client = alice(&server);
    // When the head moves
    let head = push(&server, "feature", "src/second.txt", "second\n");
    // Then a new test merge has the new head as its second parent
    let after_head = merge_commit_sha(&client).await;
    let sha = after_head.as_str().unwrap_or_default().to_owned();
    assert_eq!(
        parents(&client, &sha).await,
        [json!(tip(&server, "main")), json!(head)]
    );
    assert_eq!(ref_target(&client, HEAD_REF).await, Some(head.to_string()));
    // When the base moves
    let base = push(&server, "main", "docs/notes.md", "notes\n");
    // Then a new test merge has the new base tip as its first parent
    let after_base = merge_commit_sha(&client).await;
    let sha = after_base.as_str().unwrap_or_default().to_owned();
    assert_ne!(after_base, after_head);
    assert_eq!(parents(&client, &sha).await, [json!(base), json!(head)]);
    assert_eq!(ref_target(&client, MERGE_REF).await, Some(sha.clone()));
    // When the base changes a file and the head then changes the same file differently
    push(&server, "main", "README.md", "# from main\n");
    let sha = merge_commit_sha(&client)
        .await
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let head = push(&server, "feature", "README.md", "# from the feature\n");
    // Then merge_commit_sha is null while the merge ref keeps the last test merge
    let pull = client.get(PULL).await.json();
    assert_eq!(pull["mergeable"], false);
    assert_eq!(pull["merge_commit_sha"], Value::Null);
    assert_eq!(ref_target(&client, MERGE_REF).await, Some(sha));
    assert_eq!(ref_target(&client, HEAD_REF).await, Some(head.to_string()));
}

#[tokio::test]
async fn a_closed_pull_request_keeps_its_head_ref_and_loses_its_merge_ref() {
    for path in [PULL, ISSUE] {
        // Given an open pull request with a test merge commit
        let server = server(&[pull_request("feature", "main")]).await;
        let client = alice(&server);
        let open_sha = merge_commit_sha(&client).await;
        // When it is closed
        let closed = client
            .call_json(Method::PATCH, path, &json!({"state": "closed"}))
            .await;
        assert_eq!(closed.status, StatusCode::OK, "{path}");
        // Then the merge ref is gone, while merge_commit_sha and the head ref stay
        assert_eq!(ref_target(&client, MERGE_REF).await, None, "{path}");
        assert_eq!(merge_commit_sha(&client).await, open_sha, "{path}");
        let head = tip(&server, "feature").to_string();
        assert_eq!(ref_target(&client, HEAD_REF).await, Some(head), "{path}");
        // When it is reopened
        let reopened = client
            .call_json(Method::PATCH, path, &json!({"state": "open"}))
            .await;
        assert_eq!(reopened.status, StatusCode::OK, "{path}");
        // Then the merge ref names the test merge again
        let sha = merge_commit_sha(&client).await;
        assert_eq!(
            ref_target(&client, MERGE_REF).await.map(Value::from),
            Some(sha),
            "{path}"
        );
    }
}

#[tokio::test]
async fn a_merged_pull_request_names_its_merge_commit_and_loses_its_merge_ref() {
    // Given an open pull request with a test merge commit
    let server = server(&[pull_request("feature", "main")]).await;
    let client = alice(&server);
    let test_merge = merge_commit_sha(&client).await;
    // When it is merged
    let merged = client
        .call_json(Method::PUT, &format!("{PULL}/merge"), &json!({}))
        .await;
    assert_eq!(merged.status, StatusCode::OK, "{}", merged.json());
    let sha = merged.json()["sha"].clone();
    // Then merge_commit_sha and GraphQL mergeCommit name the merge commit, and the merge ref is gone
    assert_ne!(sha, test_merge);
    assert_eq!(merge_commit_sha(&client).await, sha);
    assert_eq!(graphql_merge_commit(&client).await, json!({"oid": sha}));
    assert_eq!(ref_target(&client, MERGE_REF).await, None);
    let head = tip(&server, "feature").to_string();
    assert_eq!(ref_target(&client, HEAD_REF).await, Some(head));
}

#[derive(Clone, Copy, Debug)]
enum Deletion {
    Rest,
    Graphql,
}

async fn delete_branch(client: &GoGithub, how: Deletion, branch: &str) {
    match how {
        Deletion::Rest => {
            let answer = client
                .call_json(
                    Method::DELETE,
                    &format!("repos/acme/public/git/refs/heads/{branch}"),
                    &json!({}),
                )
                .await;
            assert_eq!(answer.status, StatusCode::NO_CONTENT, "{}", answer.json());
        }
        Deletion::Graphql => {
            let repository = client.get(PULL).await.json()["base"]["repo"]["node_id"].clone();
            let answer = Graphql::new(client.clone())
                .query(
                    "mutation($input: UpdateRefsInput!) {updateRefs(input: $input) {clientMutationId}}",
                    &json!({"input": {"repositoryId": repository, "refUpdates": [
                        {"name": format!("refs/heads/{branch}"), "afterOid": ObjectId::NULL.to_string()}
                    ]}}),
                )
                .await
                .json();
            assert_eq!(answer.get("errors"), None, "{answer}");
        }
    }
}

fn keys(event: &Value) -> Vec<String> {
    let mut keys: Vec<_> = event
        .as_object()
        .map_or_default(|o| o.keys().cloned().collect());
    keys.sort();
    keys
}

const REF_EVENT_KEYS: [&str; 9] = [
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

#[tokio::test]
async fn deleting_a_base_branch_closes_its_open_pull_requests_on_every_path() {
    for how in [Deletion::Rest, Deletion::Graphql] {
        // Given an open pull request into develop
        let server = server(&[pull_request("feature", "develop")]).await;
        let client = alice(&server);
        deliveries(&server).await;
        // When develop is deleted
        delete_branch(&client, how, "develop").await;
        // Then the pull request is closed by the deleter and keeps its base
        let pull = client.get(PULL).await.json();
        assert_eq!(
            (&pull["state"], &pull["merged"], &pull["base"]["ref"]),
            (&json!("closed"), &json!(false), &json!("develop")),
            "{how:?}"
        );
        assert_eq!(ref_target(&client, MERGE_REF).await, None, "{how:?}");
        // Then the timeline records the base deletion and then the close, as GitHub does
        let recorded = events(&client, 1).await;
        assert_eq!(names(&recorded), ["base_ref_deleted", "closed"], "{how:?}");
        assert_eq!(
            keys(&recorded[0]),
            REF_EVENT_KEYS.map(String::from),
            "{how:?}"
        );
        assert_eq!(
            recorded[0]["node_id"].as_str().map(|id| &id[..5]),
            Some("BRDE_")
        );
        assert_eq!(
            (&recorded[1]["actor"]["login"], &recorded[1]["state_reason"]),
            (&json!("alice"), &Value::Null),
            "{how:?}"
        );
        let hooks = pull_hooks(&deliveries(&server).await, 1);
        let actions: Vec<_> = hooks.iter().map(|(action, _)| action.as_str()).collect();
        assert_eq!(actions, ["closed"], "{how:?}");
        assert_eq!(hooks[0].1["sender"]["login"], "alice", "{how:?}");
    }
}

#[tokio::test]
async fn deleting_a_head_branch_records_it_and_leaves_the_pull_request_open() {
    for how in [Deletion::Rest, Deletion::Graphql] {
        // Given an open pull request from feature
        let server = server(&[pull_request("feature", "main")]).await;
        let client = alice(&server);
        let head = tip(&server, "feature").to_string();
        deliveries(&server).await;
        // When feature is deleted
        delete_branch(&client, how, "feature").await;
        // Then the pull request stays open on its last head, with no mergeability
        let pull = client.get(PULL).await.json();
        assert_eq!(
            (&pull["state"], &pull["head"]["sha"], &pull["mergeable"]),
            (&json!("open"), &json!(head), &Value::Null),
            "{how:?}"
        );
        assert_eq!(ref_target(&client, HEAD_REF).await, Some(head), "{how:?}");
        // Then the timeline records only the head deletion, with no pull_request webhook
        let recorded = events(&client, 1).await;
        assert_eq!(names(&recorded), ["head_ref_deleted"], "{how:?}");
        assert_eq!(
            keys(&recorded[0]),
            REF_EVENT_KEYS.map(String::from),
            "{how:?}"
        );
        assert_eq!(
            recorded[0]["node_id"].as_str().map(|id| &id[..5]),
            Some("HRDE_")
        );
        assert!(
            pull_hooks(&deliveries(&server).await, 1).is_empty(),
            "{how:?}"
        );
    }
}

fn delete_branch_on_merge(server: &TestServer) {
    let store = &server.server.services().store;
    store
        .update(|tx| {
            let mut repos = tx.table_mut::<Repos>();
            let mut repo = repos
                .get(PUBLIC_REPO_ID)
                .map(|r| Repo::clone(r))
                .ok_or_else(hubdouble_model::ApiError::not_found)?;
            repo.delete_branch_on_merge = true;
            repos
                .replace(repo)
                .map_err(|e| hubdouble_model::ApiError::internal(format!("{e:?}")))?;
            Ok(())
        })
        .unwrap_or_else(|e| panic!("set delete_branch_on_merge: {e:?}"));
}

#[tokio::test]
async fn a_merge_that_deletes_its_head_branch_retargets_the_pull_requests_based_on_it() {
    // Given a repository that deletes head branches on merge, a pull request from one into main, and one from two into one
    let server = server(&[]).await;
    delete_branch_on_merge(&server);
    let client = alice(&server);
    for (branch, from) in [("one", "main"), ("two", "one")] {
        let sha = tip(&server, from).to_string();
        let created = client
            .post(
                "repos/acme/public/git/refs",
                &json!({"ref": format!("refs/heads/{branch}"), "sha": sha}),
            )
            .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.json());
        push(&server, branch, &format!("{branch}.txt"), "text\n");
    }
    for (head, base) in [("one", "main"), ("two", "one")] {
        let opened = client
            .post(
                "repos/acme/public/pulls",
                &json!({"title": head, "head": head, "base": base}),
            )
            .await;
        assert_eq!(opened.status, StatusCode::CREATED, "{}", opened.json());
    }
    deliveries(&server).await;
    // When the first pull request is merged
    let merged = client
        .call_json(Method::PUT, "repos/acme/public/pulls/1/merge", &json!({}))
        .await;
    assert_eq!(merged.status, StatusCode::OK, "{}", merged.json());
    // Then one is deleted, and the second pull request is open on main
    let gone = client.get("repos/acme/public/git/ref/heads/one").await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    let second = client.get("repos/acme/public/pulls/2").await.json();
    assert_eq!(
        (&second["state"], &second["base"]["ref"]),
        (&json!("open"), &json!("main"))
    );
    // Then the timelines record the deletion and the automatic base change as GitHub does
    let stack_events = |number: i64| {
        let client = client.clone();
        async move {
            client
                .get(&format!("repos/acme/public/issues/{number}/events"))
                .await
                .json()
                .as_array()
                .cloned()
                .unwrap_or_default()
        }
    };
    let first = stack_events(1).await;
    assert!(names(&first).contains(&"head_ref_deleted"), "{first:?}");
    let recorded = stack_events(2).await;
    assert_eq!(
        names(&recorded),
        ["automatic_base_change_succeeded", "base_ref_changed"]
    );
    assert_eq!(
        recorded[0]["node_id"].as_str().map(|id| &id[..6]),
        Some("ABCSE_")
    );
    assert_eq!(keys(&recorded[1]), REF_EVENT_KEYS.map(String::from));
    assert_eq!(recorded[1]["actor"]["login"], "alice");
    // Then the retarget sends an edited webhook with the old base, and the deletion a push
    let all = deliveries(&server).await;
    let edited: Vec<_> = pull_hooks(&all, 2)
        .into_iter()
        .filter(|(action, _)| action == "edited")
        .map(|(_, payload)| payload["changes"]["base"]["ref"].clone())
        .collect();
    assert_eq!(edited, [json!({"from": "one"})]);
    let deleted: Vec<_> = all
        .iter()
        .filter(|d| d.header("x-github-event") == "push")
        .map(|d| (d.json()["ref"].clone(), d.json()["deleted"].clone()))
        .filter(|(_, deleted)| *deleted == json!(true))
        .collect();
    assert_eq!(deleted, [(json!("refs/heads/one"), json!(true))]);
}

#[tokio::test]
async fn a_merge_that_deletes_its_head_branch_records_a_failed_base_change_on_merged_pull_requests_based_on_it()
 {
    // Given a repository that deletes head branches on merge, a pull request from one into main, and a merged one from two into one
    let server = server(&[]).await;
    delete_branch_on_merge(&server);
    let client = alice(&server);
    for (branch, from) in [("one", "main"), ("two", "one")] {
        create_branch(
            &client,
            Deletion::Rest,
            branch,
            &tip(&server, from).to_string(),
        )
        .await;
        push(&server, branch, &format!("{branch}.txt"), "text\n");
    }
    for (head, base) in [("one", "main"), ("two", "one")] {
        let opened = client
            .post(
                "repos/acme/public/pulls",
                &json!({"title": head, "head": head, "base": base}),
            )
            .await;
        assert_eq!(opened.status, StatusCode::CREATED, "{}", opened.json());
    }
    let merged = client
        .call_json(Method::PUT, "repos/acme/public/pulls/2/merge", &json!({}))
        .await;
    assert_eq!(merged.status, StatusCode::OK, "{}", merged.json());
    let before = client.get("repos/acme/public/pulls/2").await.json();
    let earlier: Vec<_> = events(&client, 2)
        .await
        .iter()
        .map(|e| e["id"].clone())
        .collect();
    deliveries(&server).await;
    // When the first pull request is merged
    let merged = client
        .call_json(Method::PUT, "repos/acme/public/pulls/1/merge", &json!({}))
        .await;
    assert_eq!(merged.status, StatusCode::OK, "{}", merged.json());
    // Then the merged pull request records automatic_base_change_failed by the merger and keeps its base and updated_at
    let recorded: Vec<_> = events(&client, 2)
        .await
        .into_iter()
        .filter(|e| !earlier.contains(&e["id"]))
        .collect();
    assert_eq!(names(&recorded), ["automatic_base_change_failed"]);
    let failed = &recorded[0];
    assert_eq!(keys(failed), REF_EVENT_KEYS.map(String::from));
    assert_eq!(
        failed["node_id"].as_str().map(|id| &id[..6]),
        Some("ABCFE_")
    );
    assert_eq!(failed["actor"]["login"], "alice");
    let after = client.get("repos/acme/public/pulls/2").await.json();
    assert_eq!(
        (&after["state"], &after["merged"], &after["base"]["ref"]),
        (&json!("closed"), &json!(true), &json!("one"))
    );
    assert_eq!(after["updated_at"], before["updated_at"]);
    assert_eq!(
        pull_hooks(&deliveries(&server).await, 2),
        Vec::<(String, Value)>::new()
    );
}

#[tokio::test]
async fn a_merge_records_a_failed_base_change_on_a_pull_request_whose_head_is_gone_and_the_deletion_closes_it()
 {
    // Given a repository that deletes head branches on merge, a pull request from one into main, and an open one from two into one whose head branch two was deleted
    let server = server(&[]).await;
    delete_branch_on_merge(&server);
    let client = alice(&server);
    for (branch, from) in [("one", "main"), ("two", "one")] {
        create_branch(
            &client,
            Deletion::Rest,
            branch,
            &tip(&server, from).to_string(),
        )
        .await;
        push(&server, branch, &format!("{branch}.txt"), "text\n");
    }
    for (head, base) in [("one", "main"), ("two", "one")] {
        let opened = client
            .post(
                "repos/acme/public/pulls",
                &json!({"title": head, "head": head, "base": base}),
            )
            .await;
        assert_eq!(opened.status, StatusCode::CREATED, "{}", opened.json());
    }
    delete_branch(&client, Deletion::Rest, "two").await;
    // When the first pull request is merged
    let merged = client
        .call_json(Method::PUT, "repos/acme/public/pulls/1/merge", &json!({}))
        .await;
    // Then the merge stands and deletes one
    assert_eq!(merged.status, StatusCode::OK, "{}", merged.json());
    assert_eq!(
        client
            .get("repos/acme/public/git/ref/heads/one")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(client.get(PULL).await.json()["merged"], true);
    // Then the second pull request records the failed base change, and the base deletion closes it on its base
    let recorded = events(&client, 2).await;
    assert_eq!(
        names(&recorded),
        [
            "head_ref_deleted",
            "automatic_base_change_failed",
            "base_ref_deleted",
            "closed"
        ]
    );
    let second = client.get("repos/acme/public/pulls/2").await.json();
    assert_eq!(
        (&second["state"], &second["merged"], &second["base"]["ref"]),
        (&json!("closed"), &json!(false), &json!("one"))
    );
}

fn merger_app() -> [Value; 2] {
    [
        json!({"kind": "app", "id": 4100, "slug": "merger", "name": "Merger", "owner": "acme",
               "public_key": hubdouble_testkit::fixture::APP_PUBLIC_KEY,
               "permissions": {"contents": "write", "pull_requests": "write", "metadata": "read"},
               "events": []}),
        json!({"kind": "installation", "id": 5100, "app": "merger", "account": "acme",
               "repositories": ["public"],
               "permissions": {"contents": "write", "pull_requests": "write", "metadata": "read"}}),
    ]
}

const MERGER_TOKEN: &str = "ghs_pull_lifecycle_merger_installation";

fn merger_token(server: &TestServer) {
    let store = &server.server.services().store;
    store
        .update(|tx| {
            tx.table_mut::<hubdouble_store::Tokens>()
                .insert(Token {
                    id: 7100,
                    token: MERGER_TOKEN.into(),
                    kind: TokenKind::Installation,
                    app_id: Some(4100),
                    installation_id: Some(5100),
                    user_id: None,
                    repo_ids: None,
                    permissions: BTreeMap::new(),
                    scopes: vec![],
                    expires_at: None,
                    revoked: false,
                    owner_seeded: false,
                    created_at: Timestamp::SEED,
                })
                .map_err(|e| hubdouble_model::ApiError::internal(format!("{e:?}")))?;
            Ok(())
        })
        .unwrap_or_else(|e| panic!("insert the merger token: {e:?}"));
}

fn fork(collaborators: &Value) -> Value {
    json!({"kind": "repository", "id": 3100, "owner": "bob", "name": "public",
           "fork_of": "acme/public", "delete_branch_on_merge": true,
           "collaborators": collaborators,
           "commits": [{"branch": "fork-work", "message": "fork change",
                        "files": {"fork.txt": "fork\n"}}]})
}

type DeletionCase = (
    &'static str,
    Vec<Value>,
    bool,
    &'static str,
    &'static str,
    bool,
);

fn fork_pull() -> Value {
    json!({"kind": "pull_request", "repository": "acme/public", "user": "bob",
           "title": "fork work", "head": "bob:fork-work", "base": "main"})
}

#[tokio::test]
async fn a_merge_deletes_a_head_branch_only_with_write_access_to_it_and_no_rule_or_open_pull_request_keeping_it()
 {
    let protect_feature = json!({"kind": "ruleset", "repository": "acme/public",
        "name": "keep feature", "target": "branch", "enforcement": "active",
        "conditions": {"ref_name": {"include": ["refs/heads/feature"], "exclude": []}},
        "rules": [{"type": "deletion"}]});
    let [app, installation] = merger_app();
    let alice_write = json!([{"login": "alice", "permission": "write"}]);
    let cases: [DeletionCase; 7] = [
        (
            "a fork head the merger can push to",
            vec![fork(&alice_write), fork_pull()],
            false,
            ALICE_TOKEN,
            "repos/bob/public/git/ref/heads/fork-work",
            true,
        ),
        (
            "a fork head the merger cannot push to",
            vec![fork(&json!([])), fork_pull()],
            false,
            ALICE_TOKEN,
            "repos/bob/public/git/ref/heads/fork-work",
            false,
        ),
        (
            "a head in the installation's repository",
            vec![
                app.clone(),
                installation.clone(),
                pull_request("feature", "main"),
            ],
            true,
            MERGER_TOKEN,
            "repos/acme/public/git/ref/heads/feature",
            true,
        ),
        (
            "a fork head outside the installation",
            vec![app, installation, fork(&alice_write), fork_pull()],
            false,
            MERGER_TOKEN,
            "repos/bob/public/git/ref/heads/fork-work",
            false,
        ),
        (
            "a head a ruleset keeps from deletion",
            vec![protect_feature, pull_request("feature", "main")],
            true,
            ALICE_TOKEN,
            "repos/acme/public/git/ref/heads/feature",
            false,
        ),
        (
            "a head of another open pull request",
            vec![
                pull_request("feature", "main"),
                pull_request("feature", "develop"),
            ],
            true,
            ALICE_TOKEN,
            "repos/acme/public/git/ref/heads/feature",
            false,
        ),
        (
            "a head with no rule or other pull request",
            vec![pull_request("feature", "main")],
            true,
            ALICE_TOKEN,
            "repos/acme/public/git/ref/heads/feature",
            true,
        ),
    ];
    for (case, entries, base_deletes, token, head_ref, deleted) in cases {
        // Given a merge setting on the head repository and a pull request from that head into main
        let server = server(&entries).await;
        if base_deletes {
            delete_branch_on_merge(&server);
        }
        merger_token(&server);
        // When the merger merges it
        let merged = GoGithub::enterprise(&server)
            .with_token(token)
            .call_json(Method::PUT, &format!("{PULL}/merge"), &json!({}))
            .await;
        // Then the merge succeeds, and the head branch is deleted only where GitHub deletes it
        assert_eq!(merged.status, StatusCode::OK, "{case}: {}", merged.json());
        let head = alice(&server).get(head_ref).await.status;
        let expected = if deleted {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::OK
        };
        assert_eq!(head, expected, "{case}");
    }
}

async fn create_branch(client: &GoGithub, how: Deletion, branch: &str, sha: &str) {
    let name = format!("refs/heads/{branch}");
    match how {
        Deletion::Rest => {
            let answer = client
                .post(
                    "repos/acme/public/git/refs",
                    &json!({"ref": name, "sha": sha}),
                )
                .await;
            assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.json());
        }
        Deletion::Graphql => {
            let repository = client.get(PULL).await.json()["base"]["repo"]["node_id"].clone();
            let answer = Graphql::new(client.clone())
                .query(
                    "mutation($input: UpdateRefsInput!) {updateRefs(input: $input) {clientMutationId}}",
                    &json!({"input": {"repositoryId": repository, "refUpdates": [
                        {"name": name, "afterOid": sha, "beforeOid": ObjectId::NULL.to_string()}
                    ]}}),
                )
                .await
                .json();
            assert_eq!(answer.get("errors"), None, "{answer}");
        }
    }
}

#[tokio::test]
async fn restoring_a_deleted_head_branch_records_it_on_open_and_closed_pull_requests() {
    for (how, closed) in [
        (Deletion::Rest, false),
        (Deletion::Rest, true),
        (Deletion::Graphql, false),
        (Deletion::Graphql, true),
    ] {
        // Given a pull request from feature, closed or open, whose head branch was deleted
        let server = server(&[pull_request("feature", "main")]).await;
        let client = alice(&server);
        let head = tip(&server, "feature").to_string();
        if closed {
            let answer = client
                .call_json(Method::PATCH, PULL, &json!({"state": "closed"}))
                .await;
            assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
        }
        delete_branch(&client, how, "feature").await;
        deliveries(&server).await;
        // When feature is created again at its old commit
        create_branch(&client, how, "feature", &head).await;
        // Then the timeline records the restore after the deletion, by the restorer
        let recorded = events(&client, 1).await;
        let expected: &[&str] = if closed {
            &["closed", "head_ref_deleted", "head_ref_restored"]
        } else {
            &["head_ref_deleted", "head_ref_restored"]
        };
        assert_eq!(names(&recorded), expected, "{how:?} closed={closed}");
        let restored = &recorded[recorded.len() - 1];
        assert_eq!(keys(restored), REF_EVENT_KEYS.map(String::from), "{how:?}");
        assert_eq!(
            restored["node_id"].as_str().map(|id| &id[..5]),
            Some("HRRE_")
        );
        assert_eq!(restored["actor"]["login"], "alice", "{how:?}");
        let pull = client.get(PULL).await.json();
        assert_eq!(pull["updated_at"], restored["created_at"], "{how:?}");
        assert_eq!(pull["head"]["sha"], json!(head), "{how:?}");
        // Then the restore at the old commit sends no pull_request webhook
        assert!(
            pull_hooks(&deliveries(&server).await, 1).is_empty(),
            "{how:?} closed={closed}"
        );
        // When feature is deleted and created again a second time
        delete_branch(&client, how, "feature").await;
        create_branch(&client, how, "feature", &head).await;
        // Then each restore follows its deletion
        let again = events(&client, 1).await;
        let tail: Vec<_> = names(&again).into_iter().rev().take(4).collect();
        assert_eq!(
            tail,
            [
                "head_ref_restored",
                "head_ref_deleted",
                "head_ref_restored",
                "head_ref_deleted"
            ],
            "{how:?}"
        );
    }
}

#[tokio::test]
async fn an_unknown_state_on_a_pull_request_edit_is_ignored_and_the_rest_is_written() {
    for closed in [false, true] {
        // Given a pull request, open or closed
        let server = server(&[pull_request("feature", "main")]).await;
        let client = alice(&server);
        if closed {
            let answer = client
                .call_json(Method::PATCH, PULL, &json!({"state": "closed"}))
                .await;
            assert_eq!(answer.status, StatusCode::OK);
        }
        let before = client.get(PULL).await.json()["state"].clone();
        // When an edit sends a state that is neither open nor closed, with a new title
        let answer = client
            .call_json(
                Method::PATCH,
                PULL,
                &json!({"state": "parked", "title": "renamed"}),
            )
            .await;
        // Then GitHub answers 200 with the new title and the state unchanged
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
        let pull = client.get(PULL).await.json();
        assert_eq!(
            (&pull["title"], &pull["state"]),
            (&json!("renamed"), &before),
            "closed={closed}"
        );
    }
}

async fn mergeability(client: &GoGithub) -> Value {
    let pull = client.get(PULL).await.json();
    json!([
        pull["mergeable"],
        pull["mergeable_state"],
        pull["rebaseable"]
    ])
}

#[tokio::test]
async fn a_closed_pull_request_keeps_the_mergeability_it_had_when_it_closed() {
    for (path, conflict) in [(PULL, false), (ISSUE, false), (PULL, true), (ISSUE, true)] {
        // Given an open pull request, clean or with a conflict
        let server = server(&[pull_request("feature", "main")]).await;
        let client = alice(&server);
        if conflict {
            push(&server, "main", "README.md", "# from main\n");
            push(&server, "feature", "README.md", "# from the feature\n");
        }
        let open = mergeability(&client).await;
        assert_eq!(open[0], json!(!conflict), "{path}");
        // When it is closed
        let closed = client
            .call_json(Method::PATCH, path, &json!({"state": "closed"}))
            .await;
        assert_eq!(closed.status, StatusCode::OK, "{path}");
        // Then mergeable and mergeable_state stay as they were, and rebaseable is false
        let expected = if conflict {
            json!([false, "dirty", false])
        } else {
            json!([true, "clean", false])
        };
        assert_eq!(mergeability(&client).await, expected, "{path}");
        // When its head branch is deleted after the close
        delete_branch(&client, Deletion::Rest, "feature").await;
        // Then they still stay as they were
        assert_eq!(mergeability(&client).await, expected, "{path}");
    }
}

#[tokio::test]
async fn a_base_deletion_closes_a_pull_request_as_not_mergeable_and_a_merge_as_unknown() {
    // Given an open pull request into develop
    let server = server(&[pull_request("feature", "develop")]).await;
    let client = alice(&server);
    // When develop is deleted
    delete_branch(&client, Deletion::Rest, "develop").await;
    // Then the closed pull request is not mergeable and has no merge_commit_sha
    assert_eq!(mergeability(&client).await, json!([false, "dirty", false]));
    assert_eq!(merge_commit_sha(&client).await, Value::Null);
    // Given another open pull request
    let server = self::server(&[pull_request("feature", "main")]).await;
    let client = alice(&server);
    // When it is merged
    let merged = client
        .call_json(Method::PUT, &format!("{PULL}/merge"), &json!({}))
        .await;
    assert_eq!(merged.status, StatusCode::OK, "{}", merged.json());
    // Then its mergeability is unknown
    assert_eq!(mergeability(&client).await, json!([null, "unknown", null]));
}

#[tokio::test]
async fn a_repeated_state_takes_its_last_value_and_an_unknown_last_value_is_ignored() {
    // Given an open pull request and a closed one
    let server = server(&[
        pull_request("feature", "main"),
        pull_request("feature", "develop"),
    ])
    .await;
    let client = alice(&server);
    let closed = client
        .call_json(
            Method::PATCH,
            "repos/acme/public/pulls/2",
            &json!({"state": "closed"}),
        )
        .await;
    assert_eq!(closed.status, StatusCode::OK);
    // When the pulls list repeats state, as live cli/cli answers state=closed&state=bogus with open ones and state=bogus&state=closed with closed ones
    for (query, number) in [
        ("state=closed&state=bogus", 1),
        ("state=bogus&state=closed", 2),
    ] {
        let listed = client
            .get(&format!("repos/acme/public/pulls?{query}"))
            .await
            .json();
        // Then the last value counts, and an unknown one means open
        let numbers: Vec<_> = listed
            .as_array()
            .map_or_default(|all| all.iter().map(|p| p["number"].clone()).collect());
        assert_eq!(numbers, [json!(number)], "{query}");
    }
    // When an edit body repeats state, with an unknown value last
    let answer = client
        .call_bytes(
            Method::PATCH,
            PULL,
            br#"{"state": "closed", "title": "renamed", "state": "parked"}"#.to_vec(),
        )
        .await;
    // Then the last value counts, so the state is unchanged and the title is written
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
    let pull = client.get(PULL).await.json();
    assert_eq!(
        (&pull["title"], &pull["state"]),
        (&json!("renamed"), &json!("open"))
    );
    // When an edit body repeats state with closed last
    let answer = client
        .call_bytes(
            Method::PATCH,
            PULL,
            br#"{"state": "parked", "state": "closed"}"#.to_vec(),
        )
        .await;
    // Then the pull request is closed
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
    assert_eq!(client.get(PULL).await.json()["state"], "closed");
}

#[tokio::test]
async fn writes_to_pull_request_refs_are_refused_on_every_ref_path() {
    // Given an open pull request with its head and merge refs
    let server = server(&[pull_request("feature", "main")]).await;
    let client = alice(&server);
    let main = tip(&server, "main").to_string();
    let (head, merge) = (
        ref_target(&client, HEAD_REF).await,
        ref_target(&client, MERGE_REF).await,
    );
    // When a client creates, moves or deletes a ref under refs/pull through REST
    let writes = [
        (
            Method::POST,
            "repos/acme/public/git/refs",
            json!({"ref": "refs/pull/2/head", "sha": main}),
            "create-a-reference",
        ),
        (
            Method::PATCH,
            "repos/acme/public/git/refs/pull/1/head",
            json!({"sha": main, "force": true}),
            "update-a-reference",
        ),
        (
            Method::DELETE,
            "repos/acme/public/git/refs/pull/1/merge",
            json!({}),
            "delete-a-reference",
        ),
    ];
    for (method, path, body, operation) in writes {
        let answer = client.call_json(method.clone(), path, &body).await;
        // Then GitHub's ref update refusal answers, with no field errors and the operation's docs
        assert_eq!(
            answer.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{method} {path}: {}",
            answer.json()
        );
        let refusal = answer.json();
        assert_eq!(refusal.get("errors"), None, "{method} {path}");
        assert_eq!(
            refusal["documentation_url"],
            format!("https://docs.github.com/rest/git/refs#{operation}"),
            "{method} {path}"
        );
    }
    // When a client moves or deletes one through GraphQL updateRefs
    let repository = client.get(PULL).await.json()["base"]["repo"]["node_id"].clone();
    for after in [main.clone(), ObjectId::NULL.to_string()] {
        let answer = Graphql::new(client.clone())
            .query(
                "mutation($input: UpdateRefsInput!) {updateRefs(input: $input) {clientMutationId}}",
                &json!({"input": {"repositoryId": repository, "refUpdates": [
                    {"name": "refs/pull/1/head", "afterOid": after, "force": true}
                ]}}),
            )
            .await
            .json();
        // Then it is refused as unprocessable
        assert_eq!(answer["errors"][0]["type"], "UNPROCESSABLE", "{answer}");
    }
    assert_eq!(ref_target(&client, HEAD_REF).await, head);
    assert_eq!(ref_target(&client, MERGE_REF).await, merge);
    let other = client.get("repos/acme/public/git/ref/pull/2/head").await;
    assert_eq!(other.status, StatusCode::NOT_FOUND);
}

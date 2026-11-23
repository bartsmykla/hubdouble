use std::sync::Arc;

use http::{Method, StatusCode};
use hubdouble_git::{EntryMode, FileChange, ObjectId, Signature};
use hubdouble_testkit::fixture::{ALICE_ID, ALICE_TOKEN, PUBLIC_REPO_ID};
use hubdouble_testkit::{GoGithub, Graphql, TestServer};
use serde_json::{Value, json};

const PULL: &str = "repos/acme/public/pulls/1";
const ISSUE: &str = "repos/acme/public/issues/1";
const MERGE_REF: &str = "repos/acme/public/git/ref/pull/1/merge";
const HEAD_REF: &str = "repos/acme/public/git/ref/pull/1/head";

async fn server(entries: &[Value]) -> TestServer {
    let mut builder = TestServer::builder()
        .fixture()
        .register(hubdouble_api_gitdata::register)
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
    // And the commit merges the head into the base tip as GitHub writes it, signed by web-flow
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
    // And GraphQL mergeCommit stays null until the pull request merges
    assert_eq!(graphql_merge_commit(&client).await, Value::Null);
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

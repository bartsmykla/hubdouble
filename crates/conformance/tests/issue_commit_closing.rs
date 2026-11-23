use http::{Method, StatusCode};
use hubdouble_git::{Commit, ObjectData, ObjectId, Signature};
use hubdouble_model::{Clock, node_id};
use hubdouble_testkit::fixture::{ALICE_ID, ALICE_TOKEN, CAROL_TOKEN, PUBLIC_REPO_ID};
use hubdouble_testkit::{Answer, GoGithub, Graphql, TestServer, validate};
use serde_json::{Value, json};

const MAIN: &str = "refs/heads/main";
const FEATURE: &str = "refs/heads/feature";
const PUBLIC: &str = "repos/acme/public";
const PRIVATE: &str = "repos/acme/private";
const BLOCKED: &str = "repos/carol/blocked";
const CREATE_COMMIT: &str = include_str!("../../../docs/inventory/graphql/createCommit.graphql");
const UPDATE_REFS: &str = include_str!("../../../docs/inventory/graphql/updateRefs.graphql");

fn checked(answer: &Answer, status: StatusCode) -> Value {
    assert_eq!(answer.status, status, "{}", answer.json());
    answer.json()
}

fn head(server: &TestServer, name: &str) -> ObjectId {
    let services = server.server.services();
    services
        .store
        .view(|tx| Ok(services.git.resolve_ref(tx, PUBLIC_REPO_ID, name)?))
        .unwrap_or_else(|e| panic!("resolve {name}: {e:?}"))
}

fn commit(server: &TestServer, parent: ObjectId, message: &str) -> ObjectId {
    let services = server.server.services();
    services
        .store
        .view(|tx| {
            let ObjectData::Commit(base) = services.git.object(tx, PUBLIC_REPO_ID, parent)?.data
            else {
                panic!("{parent} is not a commit");
            };
            let author = Signature {
                name: "Bob".to_owned(),
                email: "bob@example.com".to_owned(),
                time: server.clock.now(),
                offset_minutes: 0,
            };
            Ok(services.git.write_commit(
                tx,
                PUBLIC_REPO_ID,
                &Commit {
                    tree: base.tree,
                    parents: vec![parent],
                    committer: author.clone(),
                    author,
                    message: message.to_owned(),
                    gpg_signature: None,
                },
            )?)
        })
        .unwrap_or_else(|e| panic!("write a commit after {parent}: {e:?}"))
}

fn push(server: &TestServer, name: &str, before: ObjectId, after: ObjectId) {
    let services = server.server.services();
    services
        .store
        .update(|tx| {
            Ok(services.git.update_ref(
                tx,
                PUBLIC_REPO_ID,
                ALICE_ID,
                name,
                Some(before),
                Some(after),
                false,
            )?)
        })
        .unwrap_or_else(|e| panic!("move {name} to {after}: {e:?}"));
}

async fn patch_main(client: &GoGithub, new: ObjectId, force: bool) {
    checked(
        &client
            .call_json(
                Method::PATCH,
                &format!("{PUBLIC}/git/refs/heads/main"),
                &json!({"sha": new, "force": force}),
            )
            .await,
        StatusCode::OK,
    );
}

async fn open_issue(client: &GoGithub, repo: &str) -> (i64, String) {
    let list = format!("{repo}/issues");
    let opened = checked(
        &client.post(&list, &json!({"title": "A fault"})).await,
        StatusCode::CREATED,
    );
    let n = opened["number"]
        .as_i64()
        .unwrap_or_else(|| panic!("missing issue number: {opened}"));
    (n, format!("{list}/{n}"))
}

async fn assert_commit_closed(
    server: &TestServer,
    client: &GoGithub,
    issue_path: &str,
    sha: ObjectId,
) {
    let issue = checked(&client.get(issue_path).await, StatusCode::OK);
    assert_eq!(issue["state"], "closed", "{issue_path}");
    assert_eq!(issue["state_reason"], "completed");
    assert_eq!(issue["closed_by"]["login"], "alice");
    assert_eq!(issue["closed_at"], json!(server.clock.now()));
    let events = checked(
        &client.get(&format!("{issue_path}/events")).await,
        StatusCode::OK,
    );
    let events = events
        .as_array()
        .unwrap_or_else(|| panic!("expected a list of events: {events}"));
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["event"], "closed");
    assert_eq!(events[0]["actor"]["login"], "alice");
    assert_eq!(events[0]["state_reason"], Value::Null);
    assert_eq!(events[0]["commit_id"], json!(sha));
    assert_eq!(
        events[0]["commit_url"],
        format!("{}/{PUBLIC}/commits/{sha}", server.base)
    );
}

async fn closed_hook_numbers(server: &TestServer) -> Vec<Value> {
    assert_eq!(
        server.admin(Method::POST, "hooks/drain", None).await.status,
        StatusCode::NO_CONTENT
    );
    server
        .receiver
        .received()
        .await
        .into_iter()
        .filter(|delivery| delivery.header("x-github-event") == "issues")
        .map(|delivery| delivery.json())
        .filter(|payload| payload["action"] == "closed")
        .map(|payload| {
            validate::webhook_payload("issues", Some("closed"), &payload)
                .unwrap_or_else(|e| panic!("issues.closed: {e:?}"));
            payload["issue"]["number"].clone()
        })
        .collect()
}

#[tokio::test]
async fn a_merge_closes_issues_from_new_commit_messages_once() {
    let server = TestServer::builder()
        .fixture()
        .frame(hubdouble_api::register_all)
        .entry(
            json!({"kind": "repository", "owner": "carol", "name": "blocked",
                      "visibility": "public", "has_issues": true}),
        )
        .entry(
            json!({"kind": "pull_request", "repository": "acme/public", "user": "alice",
                      "title": "Fix a fault", "head": "feature", "base": "main"}),
        )
        .start()
        .await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let carol = GoGithub::new(&server).with_token(CAROL_TOKEN);
    let (local, local_path) = open_issue(&alice, PUBLIC).await;
    let (cross, cross_path) = open_issue(&alice, PRIVATE).await;
    let (blocked, blocked_path) = open_issue(&carol, BLOCKED).await;
    let main = head(&server, MAIN);
    let feature = head(&server, FEATURE);
    let fixes = commit(
        &server,
        feature,
        &format!("Fixes #{local}, fixes Acme/private#{cross}, fixes carol/blocked#{blocked}"),
    );
    let tip = commit(&server, fixes, "Follow-up");
    push(&server, FEATURE, feature, tip);
    assert_eq!(
        checked(&alice.get(&local_path).await, StatusCode::OK)["state"],
        "open"
    );
    server.clock.advance(60);

    let merge = checked(
        &alice
            .call_json(Method::PUT, &format!("{PUBLIC}/pulls/1/merge"), &json!({}))
            .await,
        StatusCode::OK,
    );
    assert_commit_closed(&server, &alice, &local_path, fixes).await;
    assert_commit_closed(&server, &alice, &cross_path, fixes).await;
    assert_eq!(
        checked(&carol.get(&blocked_path).await, StatusCode::OK)["state"],
        "open"
    );
    assert_eq!(closed_hook_numbers(&server).await, [json!(local)]);

    let graph = Graphql::new(alice.clone())
        .query(
            "query($n: Int!) { repository(owner: \"acme\", name: \"public\") { issue(number: $n) {
                closedByPullRequestsReferences(first: 10, includeClosedPrs: true) { nodes { id } }
            } } }",
            &json!({"n": local}),
        )
        .await
        .json();
    assert_eq!(
        graph["data"]["repository"]["issue"]["closedByPullRequestsReferences"]["nodes"],
        json!([])
    );

    let reopened = alice
        .call_json(Method::PATCH, &local_path, &json!({"state": "open"}))
        .await;
    assert_eq!(reopened.status, StatusCode::OK);
    patch_main(&alice, main, true).await;
    patch_main(
        &alice,
        merge["sha"].as_str().unwrap().parse().unwrap(),
        false,
    )
    .await;
    assert_eq!(
        checked(&alice.get(&local_path).await, StatusCode::OK)["state"],
        "open"
    );
    assert_eq!(closed_hook_numbers(&server).await, Vec::<Value>::new());
    let events = checked(
        &alice.get(&format!("{local_path}/events")).await,
        StatusCode::OK,
    );
    let kinds: Vec<_> = events
        .as_array()
        .unwrap()
        .iter()
        .map(|e| &e["event"])
        .collect();
    assert_eq!(kinds, [&json!("closed"), &json!("reopened")]);
}

#[tokio::test]
async fn each_ref_write_closes_issues_from_commit_messages() {
    let server = TestServer::builder()
        .fixture()
        .register(hubdouble_api_gitdata::register)
        .register(hubdouble_api_repos::register)
        .register(hubdouble_api_issues::register)
        .register(|r| {
            r.store.register_table::<hubdouble_api_pulls::StateEvents>();
            hubdouble_api_pulls::register_tables(r.store);
        })
        .start()
        .await;
    let client = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let graph = Graphql::new(client.clone());
    for method in ["push", "REST", "updateRefs", "createCommitOnBranch"] {
        let (n, path) = open_issue(&client, PUBLIC).await;
        let base = head(&server, MAIN);
        let message = format!("Closes #{n}");
        let mut new = commit(&server, base, &message);
        match method {
            "push" => push(&server, MAIN, base, new),
            "REST" => patch_main(&client, new, false).await,
            "updateRefs" => {
                let id = node_id::entity(node_id::REPOSITORY, PUBLIC_REPO_ID);
                let data = graph
                    .query(
                        UPDATE_REFS,
                        &json!({"input": {
                            "repositoryId": id,
                            "refUpdates": [{"name": MAIN, "beforeOid": base, "afterOid": new}],
                            "clientMutationId": "updated"
                        }}),
                    )
                    .await
                    .json();
                assert_eq!(
                    data["data"]["updateRefs"]["clientMutationId"], "updated",
                    "{data}"
                );
            }
            "createCommitOnBranch" => {
                let data = graph.query(CREATE_COMMIT, &json!({"input": {
                    "branch": {"repositoryNameWithOwner": "acme/public", "branchName": "main"},
                    "expectedHeadOid": base,
                    "message": {"headline": "Fix a fault", "body": message},
                    "fileChanges": {"additions": [{"path": "fault.txt", "contents": "YQo="}]}
                }})).await.json();
                assert!(data["errors"].is_null(), "{data}");
                new = data["data"]["createCommitOnBranch"]["commit"]["oid"]
                    .as_str()
                    .unwrap()
                    .parse()
                    .unwrap();
            }
            other => panic!("{other}"),
        }
        assert_commit_closed(&server, &client, &path, new).await;
    }
}

#[tokio::test]
async fn a_merged_pull_request_body_closes_another_repositorys_issue_only_with_push_access() {
    // Given a pull request whose body names an issue alice can push to and one she cannot
    let server = TestServer::builder()
        .fixture()
        .frame(hubdouble_api::register_all)
        .entry(
            json!({"kind": "repository", "owner": "carol", "name": "blocked",
                      "visibility": "public", "has_issues": true}),
        )
        .start()
        .await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let carol = GoGithub::new(&server).with_token(CAROL_TOKEN);
    let (cross, cross_path) = open_issue(&alice, PRIVATE).await;
    let (blocked, blocked_path) = open_issue(&carol, BLOCKED).await;
    let body = format!("Fixes acme/private#{cross}, fixes carol/blocked#{blocked}");
    checked(
        &alice
            .post(
                &format!("{PUBLIC}/pulls"),
                &json!({"title": "Fix", "head": "feature", "base": "main", "body": body}),
            )
            .await,
        StatusCode::CREATED,
    );
    // When alice merges it
    checked(
        &alice
            .call_json(Method::PUT, &format!("{PUBLIC}/pulls/1/merge"), &json!({}))
            .await,
        StatusCode::OK,
    );
    // Then only the issue in the repository she can push to closes
    assert_eq!(
        checked(&alice.get(&cross_path).await, StatusCode::OK)["state"],
        "closed"
    );
    assert_eq!(
        checked(&carol.get(&blocked_path).await, StatusCode::OK)["state"],
        "open"
    );
}

#[tokio::test]
async fn a_merged_event_has_no_close_reason_or_intent_as_github_gives_it() {
    // Given an open pull request, as microsoft/vscode and cli/cli merged events show
    let server = TestServer::builder()
        .fixture()
        .frame(hubdouble_api::register_all)
        .entry(
            json!({"kind": "pull_request", "repository": "acme/public", "user": "alice",
                      "title": "Fix a fault", "head": "feature", "base": "main"}),
        )
        .start()
        .await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);

    // When alice merges it and reads its events
    checked(
        &alice
            .call_json(Method::PUT, &format!("{PUBLIC}/pulls/1/merge"), &json!({}))
            .await,
        StatusCode::OK,
    );
    let events = checked(
        &alice.get(&format!("{PUBLIC}/issues/1/events")).await,
        StatusCode::OK,
    );

    // Then the merged event has neither key and the closed event has both
    let keys = |name: &str| {
        let event = events
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["event"] == name)
            .unwrap_or_else(|| panic!("no {name} event in {events}"));
        (
            event.get("state_reason").cloned(),
            event.get("intent").cloned(),
        )
    };
    assert_eq!(keys("merged"), (None, None));
    assert_eq!(keys("closed"), (Some(Value::Null), Some(Value::Null)));
}

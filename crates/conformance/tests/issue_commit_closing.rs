use std::collections::BTreeMap;

use http::{Method, StatusCode};
use hubdouble_git::{Commit, ObjectData, ObjectId, Signature};
use hubdouble_model::{Clock, Timestamp, Token, TokenKind, node_id};
use hubdouble_testkit::fixture::{
    ALICE_ID, ALICE_TOKEN, BOB_ID, CAROL_TOKEN, PRIVATE_REPO_ID, PUBLIC_REPO_ID,
};
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
    head_in(server, PUBLIC_REPO_ID, name)
}

fn head_in(server: &TestServer, repo: i64, name: &str) -> ObjectId {
    let services = server.server.services();
    services
        .store
        .view(|tx| Ok(services.git.resolve_ref(tx, repo, name)?))
        .unwrap_or_else(|e| panic!("resolve {name}: {e:?}"))
}

fn commit(server: &TestServer, parent: ObjectId, message: &str) -> ObjectId {
    commit_in(server, PUBLIC_REPO_ID, parent, message)
}

fn commit_in(server: &TestServer, repo: i64, parent: ObjectId, message: &str) -> ObjectId {
    let services = server.server.services();
    services
        .store
        .view(|tx| {
            let ObjectData::Commit(base) = services.git.object(tx, repo, parent)?.data else {
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
                repo,
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
    push_in(server, (PUBLIC_REPO_ID, ALICE_ID), name, (before, after));
}

fn push_in(
    server: &TestServer,
    (repo, pusher): (i64, i64),
    name: &str,
    (before, after): (ObjectId, ObjectId),
) {
    let services = server.server.services();
    services
        .store
        .update(|tx| {
            Ok(services
                .git
                .update_ref(tx, repo, pusher, name, Some(before), Some(after), false)?)
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
    let events: Vec<&Value> = events
        .as_array()
        .unwrap_or_else(|| panic!("expected a list of events: {events}"))
        .iter()
        .filter(|e| e["event"] != "referenced")
        .collect();
    assert_eq!(events.len(), 1, "{events:?}");
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
    assert_eq!(
        kinds,
        [&json!("referenced"), &json!("closed"), &json!("reopened")]
    );
}

#[tokio::test]
async fn each_ref_write_closes_issues_from_commit_messages() {
    let server = TestServer::builder()
        .fixture()
        .register(hubdouble_api_gitdata::register)
        .register(hubdouble_api_repos::register)
        .register(hubdouble_api_issues::register)
        .register(|r| hubdouble_api_pulls::register_tables(r.store))
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

#[tokio::test]
async fn a_pushed_commit_that_names_an_issue_without_closing_it_records_referenced_once() {
    // Given two open issues, and a branch at main
    let server = TestServer::builder()
        .fixture()
        .frame(hubdouble_api::register_all)
        .start()
        .await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let (named, named_path) = open_issue(&alice, PUBLIC).await;
    let (fixed, fixed_path) = open_issue(&alice, PUBLIC).await;
    let base = head(&server, MAIN);
    checked(
        &alice
            .post(
                &format!("{PUBLIC}/git/refs"),
                &json!({"ref": "refs/heads/topic", "sha": base}),
            )
            .await,
        StatusCode::CREATED,
    );
    // When a commit that names one issue twice and fixes the other goes to the branch, and
    // then main moves to it
    let named_twice = commit(
        &server,
        base,
        &format!("Touch #{named} (see #{named}), fixes #{fixed}"),
    );
    push(&server, "refs/heads/topic", base, named_twice);
    push(&server, MAIN, base, named_twice);
    // Then the named issue has one referenced event at the commit, by the pusher
    let events = checked(
        &alice.get(&format!("{named_path}/events")).await,
        StatusCode::OK,
    );
    let referenced: Vec<&Value> = events
        .as_array()
        .map_or_default(|a| a.iter().filter(|e| e["event"] == "referenced").collect());
    assert_eq!(referenced.len(), 1, "{events}");
    let event = referenced[0];
    let mut keys: Vec<&String> = event.as_object().map_or_default(|o| o.keys().collect());
    keys.sort();
    assert_eq!(
        keys,
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
    );
    assert!(
        event["node_id"]
            .as_str()
            .is_some_and(|n| n.starts_with("REFE_")),
        "{event}"
    );
    assert_eq!(event["actor"]["login"], "alice");
    assert_eq!(event["commit_id"], json!(named_twice));
    assert_eq!(
        event["commit_url"],
        format!("{}/{PUBLIC}/commits/{named_twice}", server.base)
    );
    assert_eq!(
        checked(&alice.get(&named_path).await, StatusCode::OK)["state"],
        "open"
    );
    // And the fixed issue, which the commit closes on main, is referenced only from the branch
    let fixed_events = checked(
        &alice.get(&format!("{fixed_path}/events")).await,
        StatusCode::OK,
    );
    let kinds: Vec<&Value> = fixed_events
        .as_array()
        .map_or_default(|a| a.iter().map(|e| &e["event"]).collect());
    assert_eq!(kinds, [&json!("referenced"), &json!("closed")]);
}

#[tokio::test]
async fn a_commit_in_a_private_repository_shows_only_to_a_viewer_who_can_read_it() {
    // Given a public issue, and a commit in the private repository that names it
    let server = TestServer::builder()
        .fixture()
        .frame(hubdouble_api::register_all)
        .start()
        .await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let (number, path) = open_issue(&alice, PUBLIC).await;
    let base = head_in(&server, PRIVATE_REPO_ID, MAIN);
    let named = commit_in(
        &server,
        PRIVATE_REPO_ID,
        base,
        &format!("Touch acme/public#{number}"),
    );
    // When the commit goes to the private main
    push_in(&server, (PRIVATE_REPO_ID, ALICE_ID), MAIN, (base, named));
    let reader = GoGithub::new(&server).with_token(CAROL_TOKEN);
    let stranger = GoGithub::new(&server);
    for (viewer, reads) in [(&reader, true), (&stranger, false)] {
        // Then each viewer gets the referenced event, with the commit only when it reads
        // the private repository
        let events = checked(&viewer.get(&format!("{path}/events")).await, StatusCode::OK);
        let referenced: Vec<&Value> = events
            .as_array()
            .map_or_default(|a| a.iter().filter(|e| e["event"] == "referenced").collect());
        assert_eq!(referenced.len(), 1, "{events}");
        let (id, url) = if reads {
            (
                json!(named),
                json!(format!("{}/{PRIVATE}/commits/{named}", server.base)),
            )
        } else {
            (Value::Null, Value::Null)
        };
        assert_eq!(referenced[0]["commit_id"], id, "{reads}");
        assert_eq!(referenced[0]["commit_url"], url, "{reads}");
    }
}

fn installation(id: i64, permissions: &Value) -> [Value; 2] {
    [
        json!({"kind": "app", "id": id, "slug": format!("reader-{id}"), "name": format!("Reader {id}"),
               "owner": "acme", "public_key": hubdouble_testkit::fixture::APP_PUBLIC_KEY,
               "permissions": permissions, "events": []}),
        json!({"kind": "installation", "id": id + 1, "app": format!("reader-{id}"), "account": "acme",
               "repositories": ["public", "private"], "permissions": permissions}),
    ]
}

fn installation_token(server: &TestServer, app: i64, secret: &str) {
    server
        .server
        .services()
        .store
        .update(|tx| {
            tx.table_mut::<hubdouble_store::Tokens>()
                .insert(Token {
                    id: app + 2,
                    token: secret.into(),
                    kind: TokenKind::Installation,
                    app_id: Some(app),
                    installation_id: Some(app + 1),
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
        .unwrap_or_else(|e| panic!("insert the installation token: {e:?}"));
}

#[tokio::test]
async fn a_commit_shows_only_to_a_viewer_who_may_read_the_commit_and_only_while_its_repository_exists()
 {
    // Given an installation that reads issues but not contents, one that reads both, and a
    // public issue that a commit in the private repository names
    let issues = json!({"issues": "read", "metadata": "read"});
    let contents = json!({"issues": "read", "contents": "read", "metadata": "read"});
    let [app, install] = installation(4200, &issues);
    let [other_app, other_install] = installation(4300, &contents);
    let server = TestServer::builder()
        .fixture()
        .entry(app)
        .entry(install)
        .entry(other_app)
        .entry(other_install)
        .frame(hubdouble_api::register_all)
        .start()
        .await;
    installation_token(&server, 4200, "ghs_issues_reader");
    installation_token(&server, 4300, "ghs_contents_reader");
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let (number, path) = open_issue(&alice, PUBLIC).await;
    let base = head_in(&server, PRIVATE_REPO_ID, MAIN);
    let named = commit_in(
        &server,
        PRIVATE_REPO_ID,
        base,
        &format!("Touch acme/public#{number}"),
    );
    push_in(&server, (PRIVATE_REPO_ID, ALICE_ID), MAIN, (base, named));
    let commit = |viewer: GoGithub| {
        let path = path.clone();
        async move {
            let events = checked(&viewer.get(&format!("{path}/events")).await, StatusCode::OK);
            let referenced: Vec<Value> = events.as_array().map_or_default(|a| {
                a.iter()
                    .filter(|e| e["event"] == "referenced")
                    .cloned()
                    .collect()
            });
            assert_eq!(referenced.len(), 1, "{events}");
            (
                referenced[0]["commit_id"].clone(),
                referenced[0]["commit_url"].clone(),
            )
        }
    };
    let shown = (
        json!(named),
        json!(format!("{}/{PRIVATE}/commits/{named}", server.base)),
    );
    let hidden = (Value::Null, Value::Null);
    // When each installation reads the events
    let without_contents = commit(GoGithub::new(&server).with_token("ghs_issues_reader")).await;
    let with_contents = commit(GoGithub::new(&server).with_token("ghs_contents_reader")).await;
    // Then only the one that may read the commit sees it, as GET commits/{sha} requires
    // Contents read
    assert_eq!(without_contents, hidden);
    assert_eq!(with_contents, shown);
    // When the private repository is deleted
    let deleted = server
        .admin(Method::DELETE, PRIVATE, Some(&json!({})))
        .await;
    assert!(deleted.status.is_success(), "{}", deleted.status);
    // Then a deleted private repository exposes neither commit field
    let without_contents = commit(GoGithub::new(&server).with_token("ghs_issues_reader")).await;
    assert_eq!(without_contents, hidden);
    assert_eq!(commit(alice).await, hidden);
}

#[tokio::test]
async fn a_reference_keeps_its_commit_fields_after_a_public_fork_is_deleted() {
    const FORK_ID: i64 = 4900;
    const FORK: &str = "repos/alice/fork";
    let server = TestServer::builder()
        .fixture()
        .entry(
            json!({"kind": "repository", "id": FORK_ID, "owner": "alice", "name": "fork",
                       "fork_of": "acme/public"}),
        )
        .frame(hubdouble_api::register_all)
        .start()
        .await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let (number, path) = open_issue(&alice, PUBLIC).await;
    let base = head_in(&server, FORK_ID, MAIN);
    let sha = commit_in(
        &server,
        FORK_ID,
        base,
        &format!("Touch acme/public#{number}"),
    );
    push_in(&server, (FORK_ID, ALICE_ID), MAIN, (base, sha));
    let deleted = server.admin(Method::DELETE, FORK, Some(&json!({}))).await;
    assert!(deleted.status.is_success(), "{}", deleted.status);
    let events = checked(&alice.get(&format!("{path}/events")).await, StatusCode::OK);
    let event = events
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["event"] == "referenced")
        .unwrap();
    assert_eq!(event["commit_id"], json!(sha));
    assert_eq!(
        event["commit_url"],
        json!(format!("{}/{FORK}/commits/{sha}", server.base))
    );
}

#[tokio::test]
async fn a_merge_and_a_force_push_show_their_commit_only_to_a_viewer_who_may_read_it() {
    // Given an installation that reads issues and pull requests but not contents, one that
    // reads contents too, and a pull request of feature
    let reads = |contents: bool| {
        let mut permissions =
            json!({"issues": "read", "pull_requests": "read", "metadata": "read"});
        if contents {
            permissions["contents"] = json!("read");
        }
        permissions
    };
    let [app, install] = installation(4200, &reads(false));
    let [other_app, other_install] = installation(4300, &reads(true));
    let server = TestServer::builder()
        .fixture()
        .entry(app)
        .entry(install)
        .entry(other_app)
        .entry(other_install)
        .frame(hubdouble_api::register_all)
        .start()
        .await;
    installation_token(&server, 4200, "ghs_issues_reader");
    installation_token(&server, 4300, "ghs_contents_reader");
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let pull = checked(
        &alice
            .post(
                &format!("{PUBLIC}/pulls"),
                &json!({"title": "feature", "head": "feature", "base": "main"}),
            )
            .await,
        StatusCode::CREATED,
    );
    let number = &pull["number"];
    // When feature is forced to a new commit on main, and the pull request merges
    let rewrite = commit(&server, head(&server, MAIN), "Rewrite the feature");
    checked(
        &alice
            .call_json(
                Method::PATCH,
                &format!("{PUBLIC}/git/refs/heads/feature"),
                &json!({"sha": rewrite.to_string(), "force": true}),
            )
            .await,
        StatusCode::OK,
    );
    let merged = checked(
        &alice
            .call_json(
                Method::PUT,
                &format!("{PUBLIC}/pulls/{number}/merge"),
                &json!({}),
            )
            .await,
        StatusCode::OK,
    );
    let commits = |token: &'static str| {
        let viewer = GoGithub::new(&server).with_token(token);
        async move {
            let events = checked(
                &viewer
                    .get(&format!("{PUBLIC}/issues/{number}/events"))
                    .await,
                StatusCode::OK,
            );
            ["head_ref_force_pushed", "merged"].map(|name| {
                let event = events
                    .as_array()
                    .and_then(|a| a.iter().find(|e| e["event"] == name))
                    .cloned()
                    .unwrap_or_else(|| panic!("no {name} in {events}"));
                (event["commit_id"].clone(), event["commit_url"].clone())
            })
        }
    };
    let shown = |sha: String| {
        (
            json!(sha),
            json!(format!("{}/{PUBLIC}/commits/{sha}", server.base)),
        )
    };
    let hidden = (Value::Null, Value::Null);
    // Then only the installation that reads contents sees either commit
    assert_eq!(commits("ghs_issues_reader").await, [hidden.clone(), hidden]);
    assert_eq!(
        commits("ghs_contents_reader").await,
        [
            shown(rewrite.to_string()),
            shown(merged["sha"].as_str().unwrap_or_default().to_owned())
        ]
    );
}

#[tokio::test]
async fn a_push_references_a_private_issue_only_when_the_pusher_can_read_it() {
    // Given an issue in the private repository, which bob cannot read
    let server = TestServer::builder()
        .fixture()
        .frame(hubdouble_api::register_all)
        .start()
        .await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let (number, path) = open_issue(&alice, PRIVATE).await;
    let base = head(&server, MAIN);
    let by_bob = commit(&server, base, &format!("Touch acme/private#{number}"));
    let by_alice = commit(
        &server,
        by_bob,
        &format!("Touch acme/private#{number} again"),
    );
    // When bob pushes the first commit, and alice the second
    push_in(&server, (PUBLIC_REPO_ID, BOB_ID), MAIN, (base, by_bob));
    push_in(
        &server,
        (PUBLIC_REPO_ID, ALICE_ID),
        MAIN,
        (by_bob, by_alice),
    );
    // Then only alice's push records a referenced event
    let events = checked(&alice.get(&format!("{path}/events")).await, StatusCode::OK);
    let commits: Vec<&Value> = events.as_array().map_or_default(|a| {
        a.iter()
            .filter(|e| e["event"] == "referenced")
            .map(|e| &e["commit_id"])
            .collect()
    });
    assert_eq!(commits, [&json!(by_alice)]);
}

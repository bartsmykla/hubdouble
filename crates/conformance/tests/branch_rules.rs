use std::sync::Arc;

use http::{Method, StatusCode};
use hubdouble_git::{EntryMode, FileChange, ObjectId, Signature};
use hubdouble_testkit::fixture::{self, ALICE_ID, ALICE_TOKEN, BOB_ID, BOB_TOKEN, PUBLIC_REPO_ID};
use hubdouble_testkit::{Answer, Delivery, GoGithub, Graphql, TestServer};
use serde_json::{Value, json};

const REPO: &str = "repos/acme/public";
const PULL: &str = "repos/acme/public/pulls/1";
const MERGE_DOCS: &str = "https://docs.github.com/rest/branches/branches#merge-a-branch";
const PULL_MERGE_DOCS: &str = "https://docs.github.com/rest/pulls/pulls#merge-a-pull-request";

async fn server(extra: &[Value]) -> TestServer {
    let mut entries = fixture::seed();
    for entry in &mut entries {
        if entry["kind"] == "app" {
            entry["events"] = json!(["pull_request_review"]);
        }
        if entry["kind"] == "repository" && entry["name"] == fixture::PUBLIC_REPO {
            entry["collaborators"] = json!([{"login": "bob", "permission": "write"}]);
        }
    }
    entries.extend(extra.iter().cloned());
    let mut builder = TestServer::builder().frame(hubdouble_api::register_all);
    for entry in entries {
        builder = builder.entry(entry);
    }
    builder.start().await
}

fn ruleset(include: &str, bypass: &Value, rules: &Value) -> Value {
    json!({"kind": "ruleset", "repository": "acme/public", "name": "rules",
           "target": "branch", "enforcement": "active", "bypass_actors": bypass,
           "conditions": {"ref_name": {"include": [include], "exclude": []}},
           "rules": rules})
}

fn on_main(rules: &Value) -> Value {
    ruleset("~DEFAULT_BRANCH", &json!([]), rules)
}

fn required_checks(contexts: &[&str], on_create: bool) -> Value {
    let checks: Vec<_> = contexts.iter().map(|c| json!({"context": c})).collect();
    json!({"type": "required_status_checks", "parameters": {
        "strict_required_status_checks_policy": false, "do_not_enforce_on_create": !on_create,
        "required_status_checks": checks}})
}

fn reviews_rule(count: u64, dismiss_stale: bool, last_push: bool) -> Value {
    json!({"type": "pull_request", "parameters": {
        "required_approving_review_count": count, "dismiss_stale_reviews_on_push": dismiss_stale,
        "require_code_owner_review": false, "require_last_push_approval": last_push,
        "required_review_thread_resolution": false}})
}

fn pull_request() -> Value {
    json!({"kind": "pull_request", "repository": "acme/public", "user": "alice",
           "title": "feature", "head": "feature", "base": "main"})
}

fn client(server: &TestServer, token: &str) -> GoGithub {
    GoGithub::enterprise(server).with_token(token)
}

async fn tip(client: &GoGithub, branch: &str) -> String {
    let answer = client.get(&format!("{REPO}/git/ref/heads/{branch}")).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
    answer.json()["object"]["sha"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

async fn status(client: &GoGithub, sha: &str, context: &str, state: &str) {
    let answer = client
        .post(
            &format!("{REPO}/statuses/{sha}"),
            &json!({"state": state, "context": context}),
        )
        .await;
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.json());
}

fn violations(list: &[&str]) -> String {
    let mut message = String::from("Repository rule violations found\n\n");
    for v in list {
        message.push_str(v);
        message.push_str("\n\n");
    }
    message
}

fn assert_refused(answer: &Answer, code: u16, docs: Option<&str>, list: &[&str], step: &str) {
    let body = answer.json();
    assert_eq!(answer.status.as_u16(), code, "{step}: {body}");
    assert_eq!(body["message"], violations(list), "{step}");
    assert_eq!(body["status"], code.to_string(), "{step}");
    if let Some(docs) = docs {
        assert_eq!(body["documentation_url"], docs, "{step}");
    }
}

async fn merge_feature(client: &GoGithub) -> Answer {
    client
        .post(
            &format!("{REPO}/merges"),
            &json!({"base": "main", "head": "feature"}),
        )
        .await
}

fn commit(server: &TestServer, sender: i64, branch: &str, path: Option<&str>) -> ObjectId {
    let services = server.server.services();
    let sig = Signature::seed();
    let changes: Vec<_> = path
        .map(|path| FileChange::Write {
            path: path.into(),
            data: Arc::from(&b"change\n"[..]),
            mode: EntryMode::File,
        })
        .into_iter()
        .collect();
    services
        .store
        .update(|tx| {
            let parent =
                services
                    .git
                    .resolve_ref(tx, PUBLIC_REPO_ID, &format!("refs/heads/{branch}"))?;
            Ok(services.git.commit(
                tx,
                PUBLIC_REPO_ID,
                sender,
                branch,
                Some(parent),
                &changes,
                "change",
                &sig,
                &sig,
                false,
            )?)
        })
        .unwrap_or_else(|e| panic!("commit to {branch}: {e:?}"))
}

#[tokio::test]
async fn post_merges_refuses_each_broken_rule_with_409_and_keeps_the_branch() {
    for (rules, list) in [
        (
            json!([{"type": "update"}]),
            vec!["Cannot update this protected ref."],
        ),
        (
            json!([reviews_rule(0, false, false)]),
            vec!["Changes must be made through a pull request."],
        ),
        (
            json!([{"type": "required_linear_history"}]),
            vec!["This branch must not contain merge commits."],
        ),
        (
            json!([{"type": "required_signatures"}]),
            vec!["Commits must have verified signatures."],
        ),
        (
            json!([required_checks(&["ci"], true)]),
            vec!["Required status check \"ci\" is expected."],
        ),
        (
            json!([required_checks(&["ci", "build"], true)]),
            vec!["2 of 2 required status checks are expected."],
        ),
        (
            json!([{"type": "update"}, {"type": "required_linear_history"}]),
            vec![
                "Cannot update this protected ref.",
                "This branch must not contain merge commits.",
            ],
        ),
    ] {
        // Given a main whose active ruleset holds these rules, with no bypass actor
        let server = server(&[on_main(&rules)]).await;
        let alice = client(&server, ALICE_TOKEN);
        let before = tip(&alice, "main").await;
        // When the repository admin merges feature into main through POST /merges
        let answer = merge_feature(&alice).await;
        // Then GitHub refuses with 409 and lists every broken rule, and main stays
        assert_refused(&answer, 409, Some(MERGE_DOCS), &list, &format!("{rules}"));
        assert_eq!(tip(&alice, "main").await, before, "{rules}");
    }
}

#[tokio::test]
async fn post_merges_reads_required_checks_on_the_merged_head_with_runs_and_statuses_together() {
    // Given a main that requires ci and a feature head whose ci status fails
    let server = server(&[on_main(&json!([required_checks(&["ci"], true)]))]).await;
    let alice = client(&server, ALICE_TOKEN);
    let head = tip(&alice, "feature").await;
    status(&alice, &head, "ci", "failure").await;
    // When it is merged
    let answer = merge_feature(&alice).await;
    // Then the failing check refuses the merge
    let failing = ["Required status check \"ci\" is expected."];
    assert_refused(&answer, 409, Some(MERGE_DOCS), &failing, "failing");
    // When ci passes on the head
    status(&alice, &head, "ci", "success").await;
    // Then the merge goes through
    let answer = merge_feature(&alice).await;
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.json());
}

#[tokio::test]
async fn a_ref_write_refuses_each_broken_rule_with_422() {
    // Given the ref writes of git/refs that move develop, which starts at main's tip
    for (rules, method, path, body, list) in [
        (
            json!([{"type": "creation"}]),
            Method::POST,
            "git/refs",
            json!({"ref": "refs/heads/develop-2", "sha": "main"}),
            vec!["Cannot create ref due to creations being restricted."],
        ),
        (
            json!([{"type": "deletion"}]),
            Method::DELETE,
            "git/refs/heads/develop",
            json!({}),
            vec!["Cannot delete this branch"],
        ),
        (
            json!([{"type": "update"}]),
            Method::PATCH,
            "git/refs/heads/develop",
            json!({"sha": "feature"}),
            vec!["Cannot update this protected ref."],
        ),
        (
            json!([reviews_rule(0, false, false)]),
            Method::PATCH,
            "git/refs/heads/develop",
            json!({"sha": "feature"}),
            vec!["Changes must be made through a pull request."],
        ),
        (
            json!([{"type": "required_signatures"}]),
            Method::PATCH,
            "git/refs/heads/develop",
            json!({"sha": "feature"}),
            vec!["Commits must have verified signatures."],
        ),
        (
            json!([required_checks(&["ci"], true)]),
            Method::PATCH,
            "git/refs/heads/develop",
            json!({"sha": "feature"}),
            vec!["Required status check \"ci\" is expected."],
        ),
        (
            json!([required_checks(&["ci"], true)]),
            Method::POST,
            "git/refs",
            json!({"ref": "refs/heads/develop-2", "sha": "feature"}),
            vec!["Required status check \"ci\" is expected."],
        ),
    ] {
        let server = server(&[ruleset("refs/heads/develop*", &json!([]), &rules)]).await;
        let alice = client(&server, ALICE_TOKEN);
        let mut body = body.clone();
        if let Some(branch) = body["sha"].as_str().map(str::to_owned) {
            body["sha"] = json!(tip(&alice, &branch).await);
        }
        let before = tip(&alice, "develop").await;
        // When the repository admin writes the ref
        let answer = alice
            .call_json(method.clone(), &format!("{REPO}/{path}"), &body)
            .await;
        // Then GitHub refuses with 422 and develop stays
        assert_refused(
            &answer,
            422,
            None,
            &list,
            &format!("{method} {path} {rules}"),
        );
        assert_eq!(tip(&alice, "develop").await, before, "{rules}");
    }
}

#[tokio::test]
async fn a_ref_write_refuses_a_force_push_and_a_merge_commit() {
    // Given a develop that forbids force pushes and merge commits
    let rules = json!([{"type": "non_fast_forward"}, {"type": "required_linear_history"}]);
    let server = server(&[ruleset("refs/heads/develop", &json!([]), &rules)]).await;
    let alice = client(&server, ALICE_TOKEN);
    let feature = tip(&alice, "feature").await;
    let develop = tip(&alice, "develop").await;
    let moved = alice
        .call_json(
            Method::PATCH,
            &format!("{REPO}/git/refs/heads/develop"),
            &json!({"sha": feature}),
        )
        .await;
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.json());
    // When develop is forced back to its old commit
    let answer = alice
        .call_json(
            Method::PATCH,
            &format!("{REPO}/git/refs/heads/develop"),
            &json!({"sha": develop, "force": true}),
        )
        .await;
    // Then the force push is refused
    assert_refused(
        &answer,
        422,
        None,
        &["Cannot force-push to this branch"],
        "force",
    );
    // When main gets a merge commit and develop moves to it
    let merged = merge_feature(&alice).await;
    assert_eq!(merged.status, StatusCode::CREATED, "{}", merged.json());
    let answer = alice
        .call_json(
            Method::PATCH,
            &format!("{REPO}/git/refs/heads/develop"),
            &json!({"sha": merged.json()["sha"]}),
        )
        .await;
    // Then the merge commit is refused
    let linear = ["This branch must not contain merge commits."];
    assert_refused(&answer, 422, None, &linear, "merge commit");
}

#[tokio::test]
async fn graphql_branch_writes_refuse_broken_rules_as_unprocessable() {
    // Given a main that requires a pull request
    let server = server(&[on_main(&json!([reviews_rule(0, false, false)]))]).await;
    let alice = client(&server, ALICE_TOKEN);
    let main = tip(&alice, "main").await;
    let before = main.clone();
    // When createCommitOnBranch writes to main
    let answer = Graphql::new(alice.clone())
        .query(
            "mutation($input: CreateCommitOnBranchInput!) {createCommitOnBranch(input: $input) {commit {oid}}}",
            &json!({"input": {"branch": {"repositoryNameWithOwner": "acme/public", "branchName": "main"},
                "expectedHeadOid": main, "message": {"headline": "direct"},
                "fileChanges": {"additions": [{"path": "x.txt", "contents": "eAo="}]}}}),
        )
        .await
        .json();
    // Then the mutation fails as unprocessable with the violation, and main stays
    assert_eq!(
        answer["data"]["createCommitOnBranch"],
        Value::Null,
        "{answer}"
    );
    assert_eq!(answer["errors"][0]["type"], "UNPROCESSABLE", "{answer}");
    assert_eq!(
        answer["errors"][0]["message"],
        violations(&["Changes must be made through a pull request."])
    );
    assert_eq!(tip(&alice, "main").await, before);
}

#[tokio::test]
async fn bypass_actors_skip_their_ruleset_by_mode() {
    let admins =
        |mode: &str| json!([{"actor_type": "RepositoryRole", "actor_id": 5, "bypass_mode": mode}]);
    // Given a main that forbids updates, which repository admins always bypass
    let rules = json!([{"type": "update"}]);
    let server = server(&[ruleset("~DEFAULT_BRANCH", &admins("always"), &rules)]).await;
    // When bob, who can only write, merges
    let answer = merge_feature(&client(&server, BOB_TOKEN)).await;
    // Then the rule holds for bob
    let update = ["Cannot update this protected ref."];
    assert_refused(&answer, 409, Some(MERGE_DOCS), &update, "bob");
    // When alice, an admin, merges
    let answer = merge_feature(&client(&server, ALICE_TOKEN)).await;
    // Then the admin bypasses it
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.json());

    // Given the same rule that admins bypass only for pull requests
    let server =
        server_with_pull(&[ruleset("~DEFAULT_BRANCH", &admins("pull_request"), &rules)]).await;
    let alice = client(&server, ALICE_TOKEN);
    // When alice merges outside a pull request
    let answer = merge_feature(&alice).await;
    // Then the rule holds
    assert_refused(&answer, 409, Some(MERGE_DOCS), &update, "pull_request mode");
    // When alice merges the pull request
    let answer = alice
        .call_json(Method::PUT, &format!("{PULL}/merge"), &json!({}))
        .await;
    // Then the admin bypasses it
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
}

async fn server_with_pull(extra: &[Value]) -> TestServer {
    let mut all = extra.to_vec();
    all.push(pull_request());
    server(&all).await
}

async fn merge_pull(client: &GoGithub) -> Answer {
    client
        .call_json(Method::PUT, &format!("{PULL}/merge"), &json!({}))
        .await
}

async fn approve(server: &TestServer) {
    let answer = client(server, BOB_TOKEN)
        .post(
            &format!("{PULL}/reviews"),
            &json!({"event": "APPROVE", "body": "ok"}),
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
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

async fn deliveries(server: &TestServer) -> Vec<Delivery> {
    let drained = server.admin(Method::POST, "hooks/drain", None).await;
    assert_eq!(drained.status, StatusCode::NO_CONTENT);
    server.receiver.received().await
}

#[tokio::test]
async fn a_pull_request_merge_refuses_missing_reviews_and_checks_with_405() {
    // Given a main that needs one approval and the ci check
    let rules = json!([
        reviews_rule(1, false, false),
        required_checks(&["ci"], true)
    ]);
    let server = server_with_pull(&[on_main(&rules)]).await;
    let alice = client(&server, ALICE_TOKEN);
    // When alice merges without either
    let answer = merge_pull(&alice).await;
    // Then GitHub refuses with 405 and both violations
    let list = [
        "Required status check \"ci\" is expected.",
        "At least 1 approving review is required by reviewers with write access.",
    ];
    assert_refused(&answer, 405, Some(PULL_MERGE_DOCS), &list, "nothing");
    // When bob approves and ci passes on the head
    approve(&server).await;
    status(&alice, &tip(&alice, "feature").await, "ci", "success").await;
    // Then the merge goes through
    let answer = merge_pull(&alice).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
}

#[tokio::test]
async fn a_push_that_changes_the_diff_dismisses_stale_approvals() {
    // Given a main that dismisses stale approvals, and bob's approval of the pull request
    let server = server_with_pull(&[on_main(&json!([reviews_rule(1, true, false)]))]).await;
    let alice = client(&server, ALICE_TOKEN);
    approve(&server).await;
    let approved = alice.get(&format!("{PULL}/reviews")).await.json()[0].clone();
    assert_eq!(decision(&alice).await, (json!("clean"), json!("APPROVED")));
    deliveries(&server).await;
    // When alice pushes a commit with the same tree
    commit(&server, ALICE_ID, "feature", None);
    // Then the diff is the same and the approval stays
    assert_eq!(decision(&alice).await, (json!("clean"), json!("APPROVED")));
    // When alice pushes a commit that changes a file
    let head = commit(&server, ALICE_ID, "feature", Some("more.txt")).to_string();
    // Then the approval is dismissed and keeps its commit and time
    let review = alice.get(&format!("{PULL}/reviews")).await.json()[0].clone();
    assert_eq!(review["state"], "DISMISSED");
    assert_eq!(review["commit_id"], approved["commit_id"]);
    assert_eq!(review["submitted_at"], approved["submitted_at"]);
    assert_eq!(
        decision(&alice).await,
        (json!("blocked"), json!("REVIEW_REQUIRED"))
    );
    // Then the issue events record the dismissal by the pusher, as live GitHub does
    let events = alice.get(&format!("{REPO}/issues/1/events")).await.json();
    let event = events
        .as_array()
        .and_then(|all| all.iter().find(|e| e["event"] == "review_dismissed"))
        .cloned()
        .unwrap_or_else(|| panic!("no review_dismissed event: {events}"));
    assert_eq!(event["actor"]["login"], "alice");
    assert_eq!(event["commit_id"], Value::Null);
    assert_eq!(event["commit_url"], Value::Null);
    assert_eq!(event["performed_via_github_app"], Value::Null);
    assert!(
        event["node_id"]
            .as_str()
            .is_some_and(|n| n.starts_with("RDE_")),
        "{event}"
    );
    assert_eq!(
        event["dismissed_review"],
        json!({"state": "approved", "review_id": approved["id"], "dismissal_message": null,
               "dismissal_commit_id": head})
    );
    // Then a pull_request_review webhook tells of the dismissal
    let hooks: Vec<_> = deliveries(&server)
        .await
        .iter()
        .filter(|d| d.header("x-github-event") == "pull_request_review")
        .map(Delivery::json)
        .collect();
    assert_eq!(hooks.len(), 1, "{hooks:?}");
    assert_eq!(hooks[0]["action"], "dismissed");
    assert_eq!(hooks[0]["review"]["state"], "dismissed");
    assert_eq!(hooks[0]["review"]["id"], approved["id"]);
    assert_eq!(hooks[0]["sender"]["login"], "alice");
}

#[tokio::test]
async fn the_last_push_needs_an_approval_from_someone_else() {
    // Given a main that needs the last push approved, and bob's approval
    let server = server_with_pull(&[on_main(&json!([reviews_rule(1, false, true)]))]).await;
    let alice = client(&server, ALICE_TOKEN);
    approve(&server).await;
    assert_eq!(decision(&alice).await, (json!("clean"), json!("APPROVED")));
    // When alice pushes a change
    commit(&server, ALICE_ID, "feature", Some("more.txt"));
    // Then the push waits for a new approval
    assert_eq!(
        decision(&alice).await,
        (json!("blocked"), json!("REVIEW_REQUIRED"))
    );
    let reapproval = ["Waiting on 1 reapproval from someone other than the last pusher."];
    assert_refused(
        &merge_pull(&alice).await,
        405,
        Some(PULL_MERGE_DOCS),
        &reapproval,
        "alice",
    );
    // When bob approves again
    approve(&server).await;
    // Then the merge state is clean
    assert_eq!(decision(&alice).await, (json!("clean"), json!("APPROVED")));
    // When bob pushes and then approves that push
    commit(&server, BOB_ID, "feature", Some("bob.txt"));
    approve(&server).await;
    // Then the last pusher's own approval does not count
    assert_eq!(
        decision(&alice).await,
        (json!("blocked"), json!("REVIEW_REQUIRED"))
    );
}

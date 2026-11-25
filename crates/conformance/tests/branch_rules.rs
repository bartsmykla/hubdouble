use std::sync::Arc;

use http::{Method, StatusCode};
use hubdouble_git::{EntryMode, FileChange, ObjectId, Signature};
use hubdouble_model::{Clock, Repo};
use hubdouble_store::Repos;
use hubdouble_testkit::fixture::{self, ALICE_ID, ALICE_TOKEN, BOB_ID, BOB_TOKEN, PUBLIC_REPO_ID};
use hubdouble_testkit::{Answer, Delivery, GoGithub, Graphql, TestServer};
use serde_json::{Value, json};

const REPO: &str = "repos/acme/public";
const PULL: &str = "repos/acme/public/pulls/1";
const MERGE_DOCS: &str = "https://docs.github.com/rest/branches/branches#merge-a-branch";
const PULL_MERGE_DOCS: &str = "https://docs.github.com/rest/pulls/pulls#merge-a-pull-request";

async fn server(extra: &[Value]) -> TestServer {
    server_where(extra, |_| {}).await
}

async fn server_where(extra: &[Value], edit: impl Fn(&mut Value)) -> TestServer {
    let mut entries = fixture::seed();
    for entry in &mut entries {
        if entry["kind"] == "app" {
            entry["events"] = json!(["pull_request_review", "push", "create", "delete", "status"]);
            entry["permissions"]["statuses"] = json!("read");
        }
        if entry["kind"] == "installation" {
            entry["permissions"]["statuses"] = json!("read");
        }
        if entry["kind"] == "repository" && entry["name"] == fixture::PUBLIC_REPO {
            entry["collaborators"] = json!([{"login": "bob", "permission": "write"}]);
            edit(entry);
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

fn assert_refused(answer: &Answer, code: u16, docs: Option<&str>, step: &str) {
    let body = answer.json();
    assert_eq!(answer.status.as_u16(), code, "{step}: {body}");
    assert_eq!(body["status"], code.to_string(), "{step}");
    assert!(
        body["message"].as_str().is_some_and(|m| !m.is_empty()),
        "{step}"
    );
    if let Some(docs) = docs {
        assert_eq!(body["documentation_url"], docs, "{step}");
    }
}

async fn assert_no_ref_events(server: &TestServer, step: &str) {
    let sent: Vec<String> = deliveries(server)
        .await
        .iter()
        .map(|d| d.header("x-github-event").to_owned())
        .filter(|e| ["push", "create", "delete"].contains(&e.as_str()))
        .collect();
    assert_eq!(sent, Vec::<String>::new(), "{step}");
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
    for rules in [
        json!([{"type": "update"}]),
        json!([reviews_rule(0, false, false)]),
        json!([{"type": "required_linear_history"}]),
        json!([{"type": "required_signatures"}]),
        json!([required_checks(&["ci"], true)]),
        json!([required_checks(&["ci", "build"], true)]),
        json!([{"type": "update"}, {"type": "required_linear_history"}]),
    ] {
        // Given a main whose active ruleset holds these rules, with no bypass actor
        let server = server(&[on_main(&rules)]).await;
        let alice = client(&server, ALICE_TOKEN);
        let before = tip(&alice, "main").await;
        deliveries(&server).await;
        // When the repository admin merges feature into main through POST /merges
        let answer = merge_feature(&alice).await;
        // Then GitHub refuses with 409, main stays and no push goes out
        assert_refused(&answer, 409, Some(MERGE_DOCS), &format!("{rules}"));
        assert_eq!(tip(&alice, "main").await, before, "{rules}");
        assert_no_ref_events(&server, &format!("{rules}")).await;
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
    assert_refused(&answer, 409, Some(MERGE_DOCS), "failing");
    // When ci passes on the head
    status(&alice, &head, "ci", "success").await;
    // Then the merge goes through
    let answer = merge_feature(&alice).await;
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.json());
}

#[tokio::test]
async fn a_ref_write_refuses_each_broken_rule_with_422() {
    // Given the ref writes of git/refs that move develop, which starts at main's tip
    for (rules, method, path, body) in [
        (
            json!([{"type": "creation"}]),
            Method::POST,
            "git/refs",
            json!({"ref": "refs/heads/develop-2", "sha": "main"}),
        ),
        (
            json!([{"type": "deletion"}]),
            Method::DELETE,
            "git/refs/heads/develop",
            json!({}),
        ),
        (
            json!([{"type": "update"}]),
            Method::PATCH,
            "git/refs/heads/develop",
            json!({"sha": "feature"}),
        ),
        (
            json!([reviews_rule(0, false, false)]),
            Method::PATCH,
            "git/refs/heads/develop",
            json!({"sha": "feature"}),
        ),
        (
            json!([{"type": "required_signatures"}]),
            Method::PATCH,
            "git/refs/heads/develop",
            json!({"sha": "feature"}),
        ),
        (
            json!([required_checks(&["ci"], true)]),
            Method::PATCH,
            "git/refs/heads/develop",
            json!({"sha": "feature"}),
        ),
        (
            json!([required_checks(&["ci"], true)]),
            Method::POST,
            "git/refs",
            json!({"ref": "refs/heads/develop-2", "sha": "feature"}),
        ),
    ] {
        let server = server(&[ruleset("refs/heads/develop*", &json!([]), &rules)]).await;
        let alice = client(&server, ALICE_TOKEN);
        let mut body = body.clone();
        if let Some(branch) = body["sha"].as_str().map(str::to_owned) {
            body["sha"] = json!(tip(&alice, &branch).await);
        }
        let before = tip(&alice, "develop").await;
        deliveries(&server).await;
        // When the repository admin writes the ref
        let answer = alice
            .call_json(method.clone(), &format!("{REPO}/{path}"), &body)
            .await;
        // Then GitHub refuses with 422, develop stays and no ref event goes out
        let step = format!("{method} {path} {rules}");
        assert_refused(&answer, 422, None, &step);
        assert_eq!(tip(&alice, "develop").await, before, "{step}");
        let created = alice.get(&format!("{REPO}/git/ref/heads/develop-2")).await;
        assert_eq!(created.status, StatusCode::NOT_FOUND, "{step}");
        assert_no_ref_events(&server, &step).await;
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
    assert_refused(&answer, 422, None, "force");
    assert_eq!(tip(&alice, "develop").await, feature);
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
    assert_refused(&answer, 422, None, "merge commit");
    assert_eq!(tip(&alice, "develop").await, feature);
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
    assert_refused(&answer, 409, Some(MERGE_DOCS), "bob");
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
    assert_refused(&answer, 409, Some(MERGE_DOCS), "pull_request mode");
    // When alice merges the pull request
    let answer = alice
        .call_json(Method::PUT, &format!("{PULL}/merge"), &json!({}))
        .await;
    // Then the admin bypasses it
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
}

fn computed_at_once() -> Value {
    json!({"kind": "mergeability", "repository": "acme/public", "reads": 0})
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

async fn approve(server: &TestServer, number: u64) {
    let answer = client(server, BOB_TOKEN)
        .post(
            &format!("{REPO}/pulls/{number}/reviews"),
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
    // Then GitHub refuses with 405
    assert_refused(&answer, 405, Some(PULL_MERGE_DOCS), "nothing");
    // When bob approves, the check still holds the merge
    approve(&server, 1).await;
    let answer = merge_pull(&alice).await;
    assert_refused(&answer, 405, Some(PULL_MERGE_DOCS), "approved only");
    // When ci passes on the head too
    status(&alice, &tip(&alice, "feature").await, "ci", "success").await;
    // Then the merge goes through
    let answer = merge_pull(&alice).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
}

#[tokio::test]
async fn a_push_that_changes_the_diff_dismisses_stale_approvals() {
    // Given a main that dismisses stale approvals, and bob's approval of the pull request
    let rules = on_main(&json!([reviews_rule(1, true, false)]));
    let server = server_with_pull(&[rules, computed_at_once()]).await;
    let alice = client(&server, ALICE_TOKEN);
    approve(&server, 1).await;
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
    let rules = on_main(&json!([reviews_rule(1, false, true)]));
    let server = server_with_pull(&[rules, computed_at_once()]).await;
    let alice = client(&server, ALICE_TOKEN);
    approve(&server, 1).await;
    assert_eq!(decision(&alice).await, (json!("clean"), json!("APPROVED")));
    // When alice pushes a change
    commit(&server, ALICE_ID, "feature", Some("more.txt"));
    // Then the push waits for a new approval
    assert_eq!(
        decision(&alice).await,
        (json!("blocked"), json!("REVIEW_REQUIRED"))
    );
    assert_refused(
        &merge_pull(&alice).await,
        405,
        Some(PULL_MERGE_DOCS),
        "alice",
    );
    // When bob approves again
    approve(&server, 1).await;
    // Then the merge state is clean
    assert_eq!(decision(&alice).await, (json!("clean"), json!("APPROVED")));
    // When bob pushes and then approves that push
    commit(&server, BOB_ID, "feature", Some("bob.txt"));
    approve(&server, 1).await;
    // Then the last pusher's own approval does not count
    assert_eq!(
        decision(&alice).await,
        (json!("blocked"), json!("REVIEW_REQUIRED"))
    );
}

fn branch_at(server: &TestServer, name: &str, at: ObjectId) {
    ref_at(server, &format!("refs/heads/{name}"), at);
}

fn ref_at(server: &TestServer, name: &str, at: ObjectId) {
    let services = server.server.services();
    services
        .store
        .update(|tx| {
            Ok(services.git.update_ref(
                tx,
                PUBLIC_REPO_ID,
                ALICE_ID,
                name,
                None,
                Some(at),
                false,
            )?)
        })
        .unwrap_or_else(|e| panic!("create {name}: {e:?}"));
}

async fn branch_tip(client: &GoGithub, branch: &str) -> ObjectId {
    let sha = tip(client, branch).await;
    sha.parse()
        .unwrap_or_else(|e| panic!("tip {sha} of {branch}: {e:?}"))
}

fn dismissing_rules_everywhere() -> [Value; 2] {
    [
        ruleset("~ALL", &json!([]), &json!([reviews_rule(0, true, false)])),
        computed_at_once(),
    ]
}

async fn retarget(client: &GoGithub, base: &str) {
    let answer = client
        .call_json(Method::PATCH, PULL, &json!({"base": base}))
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
}

async fn review_states(client: &GoGithub, number: u64) -> Vec<Value> {
    let reviews = client
        .get(&format!("{REPO}/pulls/{number}/reviews"))
        .await
        .json();
    let all = reviews
        .as_array()
        .unwrap_or_else(|| panic!("invalid reviews: {reviews}"));
    all.iter().map(|r| r["state"].clone()).collect()
}

async fn dismissals(client: &GoGithub, number: u64) -> Vec<Value> {
    let events = client
        .get(&format!("{REPO}/issues/{number}/events"))
        .await
        .json();
    let all = events
        .as_array()
        .unwrap_or_else(|| panic!("invalid events: {events}"));
    all.iter()
        .filter(|e| e["event"] == "review_dismissed")
        .cloned()
        .collect()
}

#[tokio::test]
async fn a_base_change_that_adds_changes_dismisses_stale_approvals() {
    // Given a feature of two commits, a first branch at its first commit, and bob's approval
    let server = server_with_pull(&dismissing_rules_everywhere()).await;
    let alice = client(&server, ALICE_TOKEN);
    let bob = client(&server, BOB_TOKEN);
    branch_at(&server, "first", branch_tip(&alice, "feature").await);
    let head = commit(&server, ALICE_ID, "feature", Some("more.txt")).to_string();
    approve(&server, 1).await;
    let approved = alice.get(&format!("{PULL}/reviews")).await.json()[0].clone();
    deliveries(&server).await;
    // When bob moves the base to first, so the diff only loses the first commit
    retarget(&bob, "first").await;
    // Then no new change came in and the approval stays
    assert_eq!(review_states(&alice, 1).await, [json!("APPROVED")]);
    // When bob moves the base back to main, so the diff gains the first commit
    retarget(&bob, "main").await;
    // Then the approval is dismissed by bob, as a push that adds changes dismisses it
    assert_eq!(review_states(&alice, 1).await, [json!("DISMISSED")]);
    let events = dismissals(&alice, 1).await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["actor"]["login"], "bob");
    assert_eq!(
        events[0]["dismissed_review"],
        json!({"state": "approved", "review_id": approved["id"], "dismissal_message": null,
               "dismissal_commit_id": head})
    );
    let hooks: Vec<_> = deliveries(&server)
        .await
        .iter()
        .filter(|d| d.header("x-github-event") == "pull_request_review")
        .map(Delivery::json)
        .collect();
    assert_eq!(hooks.len(), 1, "{hooks:?}");
    assert_eq!(hooks[0]["action"], "dismissed");
    assert_eq!(hooks[0]["review"]["id"], approved["id"]);
    assert_eq!(hooks[0]["sender"]["login"], "bob");
}

fn delete_branch_on_merge(server: &TestServer) {
    server
        .server
        .services()
        .store
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
async fn an_automatic_base_change_keeps_approvals_when_no_change_comes_in() {
    // Given a main one commit past feature's fork point, and a stacked branch on main's tip
    // whose pull request into feature bob approved
    let server = server_with_pull(&dismissing_rules_everywhere()).await;
    let alice = client(&server, ALICE_TOKEN);
    commit(&server, ALICE_ID, "main", Some("main.txt"));
    branch_at(&server, "stacked", branch_tip(&alice, "main").await);
    commit(&server, ALICE_ID, "stacked", Some("stacked.txt"));
    let opened = alice
        .post(
            &format!("{REPO}/pulls"),
            &json!({"title": "stacked", "head": "stacked", "base": "feature"}),
        )
        .await;
    assert_eq!(opened.status, StatusCode::CREATED, "{}", opened.json());
    approve(&server, 2).await;
    delete_branch_on_merge(&server);
    // When alice merges pull request 1 and its head branch goes
    let merged = merge_pull(&alice).await;
    assert_eq!(merged.status, StatusCode::OK, "{}", merged.json());
    // Then pull request 2 targets main and GitHub computes its mergeability
    let stacked = alice.get(&format!("{REPO}/pulls/2")).await.json();
    assert_eq!(stacked["base"]["ref"], "main");
    assert_eq!(stacked["mergeable"], true);
    // Then the merge base only moved forward, so the approval stays
    assert_eq!(review_states(&alice, 2).await, [json!("APPROVED")]);
    assert_eq!(dismissals(&alice, 2).await, Vec::<Value>::new());
}

const ZERO: &str = "0000000000000000000000000000000000000000";

async fn update_refs(client: &GoGithub, name: &str, before: &str, after: &str) -> Value {
    let repo = client.get(REPO).await.json();
    Graphql::new(client.clone())
        .query(
            "mutation($input: UpdateRefsInput!) {updateRefs(input: $input) {clientMutationId}}",
            &json!({"input": {"repositoryId": repo["node_id"], "refUpdates": [
                {"name": name, "beforeOid": before, "afterOid": after, "force": true}]}}),
        )
        .await
        .json()
}

#[tokio::test]
async fn the_default_branch_cannot_be_deleted_on_any_path() {
    // Given main, the default branch, with no ruleset, and a deletion ruleset on develop
    let rules = ruleset(
        "refs/heads/develop",
        &json!([]),
        &json!([{"type": "deletion"}]),
    );
    let server = server(&[rules]).await;
    let alice = client(&server, ALICE_TOKEN);
    let main = tip(&alice, "main").await;
    deliveries(&server).await;
    // When the repository admin deletes main through REST
    let answer = alice
        .call_json(
            Method::DELETE,
            &format!("{REPO}/git/refs/heads/main"),
            &json!({}),
        )
        .await;
    // Then GitHub refuses with 422 and the operation's docs
    let docs = "https://docs.github.com/rest/git/refs#delete-a-reference";
    assert_eq!(
        answer.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        answer.json()
    );
    assert_eq!(answer.json()["documentation_url"], docs);
    // When it deletes main through GraphQL updateRefs
    let answer = update_refs(&alice, "refs/heads/main", &main, ZERO).await;
    // Then the mutation fails as unprocessable
    assert_eq!(answer["errors"][0]["type"], "UNPROCESSABLE", "{answer}");
    // Then main stays and no ref event goes out
    assert_eq!(tip(&alice, "main").await, main);
    assert_no_ref_events(&server, "default branch").await;
    // When it deletes feature, which is not the default branch
    let answer = alice
        .call_json(
            Method::DELETE,
            &format!("{REPO}/git/refs/heads/feature"),
            &json!({}),
        )
        .await;
    // Then the branch goes and GitHub sends the push of the deletion
    assert_eq!(answer.status, StatusCode::NO_CONTENT);
    let events: Vec<_> = deliveries(&server)
        .await
        .iter()
        .map(|d| d.header("x-github-event").to_owned())
        .collect();
    assert!(events.iter().any(|e| e == "push"), "{events:?}");
}

fn strict_checks(contexts: &[&str]) -> Value {
    let mut rule = required_checks(contexts, true);
    rule["parameters"]["strict_required_status_checks_policy"] = json!(true);
    rule
}

async fn merge_state_status(client: &GoGithub) -> Value {
    let pull = client.get(PULL).await.json();
    Graphql::new(client.clone())
        .query(
            "query($id: ID!) {node(id: $id) {... on PullRequest {mergeStateStatus}}}",
            &json!({"id": pull["node_id"]}),
        )
        .await
        .json()["data"]["node"]["mergeStateStatus"]
        .clone()
}

#[tokio::test]
async fn a_strict_check_refuses_the_merge_of_a_pull_request_behind_its_base() {
    // Given a main with a strict ci check, and pull request 1 whose head passes ci
    let rules = on_main(&json!([strict_checks(&["ci"])]));
    let server = server_with_pull(&[rules, computed_at_once()]).await;
    let alice = client(&server, ALICE_TOKEN);
    status(&alice, &tip(&alice, "feature").await, "ci", "success").await;
    // When main moves on, so the head lacks the base tip
    commit(&server, ALICE_ID, "main", Some("main.txt"));
    let main = tip(&alice, "main").await;
    // Then the pull request reads as behind and its merge is refused with 405
    assert_eq!(alice.get(PULL).await.json()["mergeable_state"], "behind");
    assert_eq!(merge_state_status(&alice).await, "BEHIND");
    assert_refused(
        &merge_pull(&alice).await,
        405,
        Some(PULL_MERGE_DOCS),
        "behind",
    );
    assert_eq!(tip(&alice, "main").await, main);
}

#[tokio::test]
async fn a_required_check_reads_the_latest_head_and_not_an_older_test_merge() {
    // Given a main that requires ci, a test merge that stays pending for many reads, and pull
    // request 1 whose settled test merge passes ci
    let rules = on_main(&json!([required_checks(&["ci"], false)]));
    let slow = json!({"kind": "mergeability", "repository": "acme/public", "reads": 1000});
    let server = server_with_pull(&[rules, slow]).await;
    let alice = client(&server, ALICE_TOKEN);
    let settle = || server.admin(Method::POST, "repos/acme/public/pulls/1/mergeability", None);
    assert_eq!(settle().await.status, StatusCode::OK);
    let old_merge = alice.get(PULL).await.json()["merge_commit_sha"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert_ne!(old_merge, "");
    status(&alice, &old_merge, "ci", "success").await;
    // When a push moves the head, its test merge is still pending, and ci fails on the new
    // head
    commit(&server, ALICE_ID, "feature", Some("src/next.txt"));
    assert_eq!(alice.get(PULL).await.json()["merge_commit_sha"], old_merge);
    let head = tip(&alice, "feature").await;
    status(&alice, &head, "ci", "failure").await;
    let main = tip(&alice, "main").await;
    // Then the merge is refused with 405, and main stays
    assert_refused(&merge_pull(&alice).await, 405, Some(PULL_MERGE_DOCS), "ci");
    assert_eq!(tip(&alice, "main").await, main);
}

#[tokio::test]
async fn every_branch_and_tag_write_moves_pushed_at_and_leaves_updated_at() {
    // Given acme/public with pull request 1 from feature to main
    let server = server_with_pull(&[computed_at_once()]).await;
    let alice = client(&server, ALICE_TOKEN);
    let updated = alice.get(REPO).await.json()["updated_at"].clone();
    let main = tip(&alice, "main").await;
    let feature = tip(&alice, "feature").await;
    let writes: Vec<(&str, Method, String, Value)> = vec![
        (
            "branch create",
            Method::POST,
            "git/refs".into(),
            json!({"ref": "refs/heads/topic", "sha": main}),
        ),
        (
            "branch update",
            Method::PATCH,
            "git/refs/heads/topic".into(),
            json!({"sha": feature, "force": true}),
        ),
        (
            "tag create",
            Method::POST,
            "git/refs".into(),
            json!({"ref": "refs/tags/v9", "sha": main}),
        ),
        (
            "branch delete",
            Method::DELETE,
            "git/refs/heads/topic".into(),
            json!({}),
        ),
        (
            "pull merge",
            Method::PUT,
            "pulls/1/merge".into(),
            json!({"merge_method": "squash"}),
        ),
        (
            "branch merge",
            Method::POST,
            "merges".into(),
            json!({"base": "main", "head": "feature"}),
        ),
    ];
    for (step, method, path, body) in writes {
        // When the clock moves on and alice writes a branch or a tag
        server.clock.advance(60);
        let answer = alice
            .call_json(method, &format!("{REPO}/{path}"), &body)
            .await;
        assert!(answer.status.is_success(), "{step}: {}", answer.json());
        // Then pushed_at is the time of the write and updated_at stays
        let repo = alice.get(REPO).await.json();
        assert_eq!(repo["pushed_at"], server.clock.now().to_string(), "{step}");
        assert_eq!(repo["updated_at"], updated, "{step}");
    }
    // When createCommitOnBranch writes main
    server.clock.advance(60);
    let answer = Graphql::new(alice.clone())
        .query(
            "mutation($input: CreateCommitOnBranchInput!) {createCommitOnBranch(input: $input) {commit {oid}}}",
            &json!({"input": {"branch": {"repositoryNameWithOwner": "acme/public", "branchName": "main"},
                "expectedHeadOid": tip(&alice, "main").await, "message": {"headline": "direct"},
                "fileChanges": {"additions": [{"path": "x.txt", "contents": "eAo="}]}}}),
        )
        .await
        .json();
    assert_eq!(answer["errors"], Value::Null, "{answer}");
    // Then GraphQL pushedAt and REST pushed_at are its time, and updated_at stays
    let repo = alice.get(REPO).await.json();
    let now = server.clock.now().to_string();
    assert_eq!(repo["pushed_at"], now);
    assert_eq!(repo["updated_at"], updated);
    let read = Graphql::new(alice.clone())
        .query(
            "{repository(owner: \"acme\", name: \"public\") {pushedAt}}",
            &json!({}),
        )
        .await
        .json();
    assert_eq!(read["data"]["repository"]["pushedAt"], now, "{read}");
}

#[tokio::test]
async fn a_pull_request_merge_of_each_method_is_a_pr_merge_activity() {
    for method in ["merge", "squash", "rebase"] {
        // Given pull request 1 from feature to main
        let server = server_with_pull(&[computed_at_once()]).await;
        let alice = client(&server, ALICE_TOKEN);
        // When alice merges it with this method
        let merged = alice
            .call_json(
                Method::PUT,
                &format!("{PULL}/merge"),
                &json!({"merge_method": method}),
            )
            .await;
        assert_eq!(merged.status, StatusCode::OK, "{method}: {}", merged.json());
        // Then the activity of main names the move to the merge as pr_merge
        let activity = alice
            .get(&format!("{REPO}/activity?ref=main&per_page=1"))
            .await
            .json();
        assert_eq!(activity[0]["activity_type"], "pr_merge", "{method}");
        assert_eq!(activity[0]["after"], merged.json()["sha"], "{method}");
    }
}

fn owners_rule() -> Value {
    let mut rule = reviews_rule(0, false, false);
    rule["parameters"]["require_code_owner_review"] = json!(true);
    rule
}

async fn approve_as(server: &TestServer, token: &str) {
    let answer = client(server, token)
        .post(
            &format!("{PULL}/reviews"),
            &json!({"event": "APPROVE", "body": "ok"}),
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
}

#[tokio::test]
async fn a_code_owner_review_rule_waits_for_an_owner_of_every_changed_file() {
    // Given a CODEOWNERS file that gives src/ to bob, a main that needs code owner reviews,
    // and pull request 1 that changes src/feature.txt
    let rules = on_main(&json!([owners_rule()]));
    let server = server_where(&[rules, computed_at_once(), pull_request()], |repo| {
        repo["files"][".github/CODEOWNERS"] = json!("* @alice\n/src/ @bob\n");
        repo["collaborators"] = json!([{"login": "bob", "permission": "write"},
                                       {"login": "carol", "permission": "write"}]);
    })
    .await;
    let alice = client(&server, ALICE_TOKEN);
    let main = tip(&alice, "main").await;
    // Then the pull request waits for a review
    assert_eq!(
        decision(&alice).await,
        (json!("blocked"), json!("REVIEW_REQUIRED"))
    );
    assert_eq!(merge_state_status(&alice).await, "BLOCKED");
    // When carol, who writes but owns nothing, approves
    approve_as(&server, fixture::CAROL_TOKEN).await;
    // Then it still waits, and its merge is refused with 405
    assert_eq!(
        decision(&alice).await,
        (json!("blocked"), json!("REVIEW_REQUIRED"))
    );
    assert_refused(
        &merge_pull(&alice).await,
        405,
        Some(PULL_MERGE_DOCS),
        "carol",
    );
    assert_eq!(tip(&alice, "main").await, main);
    // When bob, the owner of src/, approves
    approve_as(&server, BOB_TOKEN).await;
    // Then the pull request is approved and merges
    assert_eq!(decision(&alice).await, (json!("clean"), json!("APPROVED")));
    assert_eq!(merge_state_status(&alice).await, "CLEAN");
    let merged = merge_pull(&alice).await;
    assert_eq!(merged.status, StatusCode::OK, "{}", merged.json());
}

#[tokio::test]
async fn a_merge_waits_for_the_code_owners_of_the_base_before_the_merge() {
    for method in ["merge", "squash", "rebase"] {
        // Given a main whose CODEOWNERS gives src/ to bob and needs code owner reviews, and
        // pull request 1 whose head also gives every file to carol
        let rules = on_main(&json!([owners_rule()]));
        let server = server_where(&[rules, computed_at_once(), pull_request()], |repo| {
            repo["files"][".github/CODEOWNERS"] = json!("* @alice\n/src/ @bob\n");
            repo["collaborators"] = json!([{"login": "bob", "permission": "write"},
                                           {"login": "carol", "permission": "write"}]);
        })
        .await;
        let alice = client(&server, ALICE_TOKEN);
        let head = tip(&alice, "feature").await;
        let moved = file_commit(&alice, &head, ".github/CODEOWNERS", "* @carol\n").await;
        let answer = alice
            .call_json(
                Method::PATCH,
                &format!("{REPO}/git/refs/heads/feature"),
                &json!({"sha": moved}),
            )
            .await;
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
        let main = tip(&alice, "main").await;
        // When carol, an owner only in the merge result, approves and alice merges
        approve_as(&server, fixture::CAROL_TOKEN).await;
        let merged = alice
            .call_json(
                Method::PUT,
                &format!("{PULL}/merge"),
                &json!({"merge_method": method}),
            )
            .await;
        // Then the merge waits for bob, the owner of src/ on main, and main stays
        assert_refused(&merged, 405, Some(PULL_MERGE_DOCS), method);
        assert_eq!(tip(&alice, "main").await, main, "{method}");
    }
}

fn tag_ruleset(rules: &Value) -> Value {
    json!({"kind": "ruleset", "repository": "acme/public", "name": "tags",
           "target": "tag", "enforcement": "active", "bypass_actors": [],
           "conditions": {"ref_name": {"include": ["refs/tags/v*"], "exclude": []}},
           "rules": rules})
}

async fn tag_tip(client: &GoGithub, tag: &str) -> Answer {
    client.get(&format!("{REPO}/git/ref/tags/{tag}")).await
}

#[tokio::test]
async fn tag_rulesets_refuse_each_broken_rule_on_ref_writes() {
    for (rules, method, path, body, tag) in [
        (
            json!([{"type": "creation"}]),
            Method::POST,
            "git/refs",
            json!({"ref": "refs/tags/v2.0.0", "sha": "main"}),
            "v2.0.0",
        ),
        (
            json!([{"type": "update"}]),
            Method::PATCH,
            "git/refs/tags/v1.0.0",
            json!({"sha": "feature", "force": true}),
            "v1.0.0",
        ),
        (
            json!([{"type": "deletion"}]),
            Method::DELETE,
            "git/refs/tags/v1.0.0",
            json!({}),
            "v1.0.0",
        ),
        (
            json!([{"type": "non_fast_forward"}]),
            Method::PATCH,
            "git/refs/tags/vf",
            json!({"sha": "main", "force": true}),
            "vf",
        ),
    ] {
        // Given a tag ruleset on v* with this rule, and a vf tag at feature's tip
        let server = server(&[tag_ruleset(&rules)]).await;
        let alice = client(&server, ALICE_TOKEN);
        let feature = tip(&alice, "feature").await;
        ref_at(
            &server,
            "refs/tags/vf",
            feature.parse().unwrap_or_else(|e| panic!("{e:?}")),
        );
        let mut body = body.clone();
        if let Some(branch) = body["sha"].as_str().map(str::to_owned) {
            body["sha"] = json!(tip(&alice, &branch).await);
        }
        let before = tag_tip(&alice, tag).await.json();
        deliveries(&server).await;
        // When the repository admin writes the tag
        let answer = alice
            .call_json(method.clone(), &format!("{REPO}/{path}"), &body)
            .await;
        // Then GitHub refuses with 422, the tag stays and no ref event goes out
        let step = format!("{method} {path} {rules}");
        assert_refused(&answer, 422, None, &step);
        assert_eq!(tag_tip(&alice, tag).await.json(), before, "{step}");
        assert_no_ref_events(&server, &step).await;
    }
}

#[tokio::test]
async fn refs_to_a_tree_pass_the_tag_and_push_rules_that_need_commits() {
    // Given tag rules on v* that read history, a push ruleset, and a tag of main's tree
    let push = json!({"kind": "ruleset", "repository": "acme/public", "name": "push",
                      "target": "push", "enforcement": "active", "bypass_actors": [],
                      "rules": [{"type": "file_path_restriction",
                                 "parameters": {"restricted_file_paths": ["secrets/**"]}}]});
    let tags = tag_ruleset(&json!([{"type": "non_fast_forward"},
                                   {"type": "required_linear_history"}]));
    let server = server(&[tags, push]).await;
    let alice = client(&server, ALICE_TOKEN);
    let main = tip(&alice, "main").await;
    let tree = alice
        .get(&format!("{REPO}/git/commits/{main}"))
        .await
        .json()["tree"]["sha"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    ref_at(
        &server,
        "refs/tags/snapshot",
        tree.parse().unwrap_or_else(|e| panic!("{e:?}")),
    );
    let fine = file_commit(&alice, &main, "ok.md", "ok\n").await;
    // When the repository admin creates a v* tag of the tree and moves develop
    let tagged = alice
        .post(
            &format!("{REPO}/git/refs"),
            &json!({"ref": "refs/tags/v-tree", "sha": tree}),
        )
        .await;
    let moved = alice
        .call_json(
            Method::PATCH,
            &format!("{REPO}/git/refs/heads/develop"),
            &json!({"sha": fine}),
        )
        .await;
    // Then both writes go through
    assert_eq!(tagged.status, StatusCode::CREATED, "{}", tagged.json());
    assert_eq!(tagged.json()["object"]["sha"], json!(tree));
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.json());
    assert_eq!(tip(&alice, "develop").await, fine);
}

async fn file_commit(client: &GoGithub, parent: &str, path: &str, content: &str) -> String {
    let base = client
        .get(&format!("{REPO}/git/commits/{parent}"))
        .await
        .json();
    let tree = client
        .post(
            &format!("{REPO}/git/trees"),
            &json!({"base_tree": base["tree"]["sha"],
                    "tree": [{"path": path, "mode": "100644", "type": "blob", "content": content}]}),
        )
        .await
        .json();
    let commit = client
        .post(
            &format!("{REPO}/git/commits"),
            &json!({"message": "add a file", "tree": tree["sha"], "parents": [parent]}),
        )
        .await
        .json();
    commit["sha"].as_str().unwrap_or_default().to_owned()
}

#[tokio::test]
async fn push_rulesets_refuse_restricted_files_on_every_write() {
    let big = "a".repeat(1024 * 1024 + 1);
    for (rule, path, content) in [
        (
            json!({"type": "file_path_restriction",
                   "parameters": {"restricted_file_paths": ["secrets/**"]}}),
            "secrets/key.txt",
            "key\n",
        ),
        (
            json!({"type": "max_file_path_length", "parameters": {"max_file_path_length": 10}}),
            "docs/a-long-name.md",
            "long\n",
        ),
        (
            json!({"type": "file_extension_restriction",
                   "parameters": {"restricted_file_extensions": ["*.exe"]}}),
            "tool.exe",
            "binary\n",
        ),
        (
            json!({"type": "max_file_size", "parameters": {"max_file_size": 1}}),
            "data.bin",
            big.as_str(),
        ),
    ] {
        // Given a push ruleset with this rule, and a commit on main that writes such a file
        let push = json!({"kind": "ruleset", "repository": "acme/public", "name": "push",
                          "target": "push", "enforcement": "active", "bypass_actors": [],
                          "rules": [rule]});
        let server = server(&[push]).await;
        let alice = client(&server, ALICE_TOKEN);
        let main = tip(&alice, "main").await;
        let sha = file_commit(&alice, &main, path, content).await;
        let fine = file_commit(&alice, &main, "ok.md", "ok\n").await;
        deliveries(&server).await;
        // When the repository admin moves develop to it through REST
        let answer = alice
            .call_json(
                Method::PATCH,
                &format!("{REPO}/git/refs/heads/develop"),
                &json!({"sha": sha}),
            )
            .await;
        // Then GitHub refuses with 422, develop stays and no push goes out
        let step = format!("{rule}");
        assert_refused(&answer, 422, None, &step);
        assert_eq!(tip(&alice, "develop").await, main, "{step}");
        assert_no_ref_events(&server, &step).await;
        // When it creates a tag at that commit
        let answer = alice
            .post(
                &format!("{REPO}/git/refs"),
                &json!({"ref": "refs/tags/blocked", "sha": sha}),
            )
            .await;
        // Then the tag is refused too
        assert_refused(&answer, 422, None, &step);
        // When it moves develop to a commit that writes an allowed file
        let answer = alice
            .call_json(
                Method::PATCH,
                &format!("{REPO}/git/refs/heads/develop"),
                &json!({"sha": fine}),
            )
            .await;
        // Then the write goes through
        assert_eq!(answer.status, StatusCode::OK, "{step}: {}", answer.json());
    }
}

#[tokio::test]
async fn a_push_ruleset_refuses_a_pull_request_merge_that_brings_in_a_restricted_file() {
    // Given a push ruleset on secrets/**, and a feature branch that adds a secret
    let push = json!({"kind": "ruleset", "repository": "acme/public", "name": "push",
                      "target": "push", "enforcement": "active", "bypass_actors": [],
                      "rules": [{"type": "file_path_restriction",
                                 "parameters": {"restricted_file_paths": ["secrets/**"]}}]});
    let server = server_with_pull(&[push, computed_at_once()]).await;
    let alice = client(&server, ALICE_TOKEN);
    commit(&server, ALICE_ID, "feature", Some("secrets/key.txt"));
    let main = tip(&alice, "main").await;
    // When alice merges the pull request
    let answer = merge_pull(&alice).await;
    // Then GitHub refuses with 405 and main stays
    assert_refused(&answer, 405, Some(PULL_MERGE_DOCS), "merge");
    assert_eq!(tip(&alice, "main").await, main);
}

#[tokio::test]
async fn a_branch_that_only_rulesets_cover_reads_as_protected_without_classic_protection() {
    // Given a ruleset on main only
    let server = server(&[on_main(&json!([{"type": "deletion"}]))]).await;
    let alice = client(&server, ALICE_TOKEN);
    // When main and develop are read
    let main = alice.get(&format!("{REPO}/branches/main")).await.json();
    let develop = alice.get(&format!("{REPO}/branches/develop")).await.json();
    // Then main is protected with classic protection off, as live GitHub answers
    let off = json!({"enabled": false, "required_status_checks":
                     {"enforcement_level": "off", "contexts": [], "checks": []}});
    assert_eq!(main["protected"], true);
    assert_eq!(main["protection"], off);
    assert_eq!(develop["protected"], false);
    assert_eq!(develop["protection"], off);
    deliveries(&server).await;
    // When a status is set on main's tip, which develop shares
    status(&alice, &tip(&alice, "main").await, "ci", "success").await;
    // Then the status webhook marks main protected and develop not
    let hooks: Vec<Value> = deliveries(&server)
        .await
        .iter()
        .filter(|d| d.header("x-github-event") == "status")
        .map(Delivery::json)
        .collect();
    assert_eq!(hooks.len(), 1, "{hooks:?}");
    let protected = |name: &str| {
        hooks[0]["branches"]
            .as_array()
            .and_then(|all| all.iter().find(|b| b["name"] == name))
            .map(|b| b["protected"].clone())
    };
    assert_eq!(protected("main"), Some(json!(true)));
    assert_eq!(protected("develop"), Some(json!(false)));
}

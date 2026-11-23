use std::collections::BTreeMap;
use std::sync::Arc;

use http::{Method, StatusCode};
use hubdouble_git::{EntryMode, FileChange, ObjectId, Signature};
use hubdouble_model::{Timestamp, Token, TokenKind};
use hubdouble_store::Tokens;
use hubdouble_testkit::fixture::{self, ALICE_ID, ALICE_TOKEN, BOB_TOKEN, PUBLIC_REPO_ID};
use hubdouble_testkit::{GoGithub, Graphql, TestServer};
use serde_json::{Value, json};

const PULL: &str = "repos/acme/public/pulls/1";
const CHECKS_TOKEN: &str = "ghs_pull_state_fixture_app";

fn entries(extra: &[Value]) -> Vec<Value> {
    let mut entries = fixture::seed();
    for entry in &mut entries {
        if entry["kind"] == "installation" {
            entry["permissions"]["checks"] = json!("write");
        }
        if entry["kind"] == "repository" && entry["name"] == fixture::PUBLIC_REPO {
            entry["collaborators"] = json!([{"login": "bob", "permission": "write"}]);
        }
    }
    entries.extend(extra.iter().cloned());
    entries
}

async fn server(extra: &[Value]) -> TestServer {
    let mut builder = TestServer::builder().frame(hubdouble_api::register_all);
    for entry in entries(extra) {
        builder = builder.entry(entry);
    }
    let server = builder.start().await;
    server
        .server
        .services()
        .store
        .update(|tx| {
            tx.table_mut::<Tokens>()
                .insert(Token {
                    id: 7200,
                    token: CHECKS_TOKEN.into(),
                    kind: TokenKind::Installation,
                    app_id: Some(fixture::APP_ID),
                    installation_id: Some(fixture::INSTALLATION_ID),
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
        .unwrap_or_else(|e| panic!("insert the checks token: {e:?}"));
    server
}

fn ruleset(rules: &Value) -> Value {
    json!({"kind": "ruleset", "repository": "acme/public", "name": "main rules",
           "target": "branch", "enforcement": "active",
           "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}},
           "rules": rules})
}

fn required_checks(strict: bool, checks: &Value) -> Value {
    json!({"type": "required_status_checks", "parameters": {
        "strict_required_status_checks_policy": strict, "required_status_checks": checks}})
}

fn pull_request(draft: bool) -> Value {
    json!({"kind": "pull_request", "repository": "acme/public", "user": "alice",
           "title": "feature", "head": "feature", "base": "main", "draft": draft})
}

fn alice(server: &TestServer) -> GoGithub {
    GoGithub::enterprise(server).with_token(ALICE_TOKEN)
}

async fn head(client: &GoGithub) -> String {
    client.get(PULL).await.json()["head"]["sha"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

async fn status(client: &GoGithub, sha: &str, context: &str, state: &str) {
    let answer = client
        .post(
            &format!("repos/acme/public/statuses/{sha}"),
            &json!({"state": state, "context": context}),
        )
        .await;
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.json());
}

async fn check_run(server: &TestServer, sha: &str, name: &str, conclusion: Option<&str>) {
    let app = GoGithub::enterprise(server).with_token(CHECKS_TOKEN);
    let body = match conclusion {
        Some(conclusion) => {
            json!({"name": name, "head_sha": sha, "status": "completed", "conclusion": conclusion})
        }
        None => json!({"name": name, "head_sha": sha, "status": "in_progress"}),
    };
    let answer = app.post("repos/acme/public/check-runs", &body).await;
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.json());
}

async fn state(client: &GoGithub) -> (Value, Value) {
    let pull = client.get(PULL).await.json();
    let answer = Graphql::new(client.clone())
        .query(
            "query($id: ID!) {node(id: $id) {... on PullRequest {mergeStateStatus}}}",
            &json!({"id": pull["node_id"]}),
        )
        .await
        .json();
    assert_eq!(answer.get("errors"), None, "{answer}");
    (
        pull["mergeable_state"].clone(),
        answer["data"]["node"]["mergeStateStatus"].clone(),
    )
}

async fn assert_state(client: &GoGithub, want: &str, step: &str) {
    assert_eq!(
        state(client).await,
        (json!(want), json!(want.to_ascii_uppercase())),
        "{step}"
    );
}

fn push(server: &TestServer, branch: &str, path: &str) {
    let services = server.server.services();
    let sig = Signature::seed();
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
                ALICE_ID,
                branch,
                Some(parent),
                &[FileChange::Write {
                    path: path.into(),
                    data: Arc::from(&b"change\n"[..]),
                    mode: EntryMode::File,
                }],
                &format!("change {path}"),
                &sig,
                &sig,
                false,
            )?)
        })
        .unwrap_or_else(|e| panic!("push {branch}: {e:?}"));
}

fn merge_main_into_feature(server: &TestServer) -> ObjectId {
    let services = server.server.services();
    services
        .store
        .update(|tx| {
            let main = services
                .git
                .resolve_ref(tx, PUBLIC_REPO_ID, "refs/heads/main")?;
            Ok(services
                .git
                .merge(
                    tx,
                    PUBLIC_REPO_ID,
                    ALICE_ID,
                    "feature",
                    &main.to_string(),
                    "update",
                )?
                .commit)
        })
        .unwrap_or_else(|e| panic!("update feature: {e:?}"))
}

#[tokio::test]
async fn required_checks_block_the_merge_state_and_other_failing_checks_make_it_unstable() {
    for draft in [false, true] {
        // Given a pull request, ready or draft, into a main whose ruleset requires the status ci and the fixture App's check run build
        let rules = ruleset(&json!([required_checks(
            false,
            &json!([{"context": "ci"}, {"context": "build", "integration_id": fixture::APP_ID}])
        )]));
        let server = server(&[rules, pull_request(draft)]).await;
        let client = alice(&server);
        let sha = head(&client).await;
        // Then with no check reported the merge is blocked, for a draft as for a ready pull request
        assert_state(&client, "blocked", &format!("draft={draft}: nothing")).await;
        // When only ci passes
        status(&client, &sha, "ci", "success").await;
        // Then the missing check run still blocks it
        assert_state(&client, "blocked", &format!("draft={draft}: ci")).await;
        // When the App's build passes too
        check_run(&server, &sha, "build", Some("success")).await;
        // Then the pull request is clean
        assert_state(&client, "clean", &format!("draft={draft}: ci and build")).await;
        // When a newer ci status is pending
        status(&client, &sha, "ci", "pending").await;
        // Then the latest status of the context counts, and it blocks the merge
        assert_state(&client, "blocked", &format!("draft={draft}: ci pending")).await;
        // When ci passes again and a check that no rule requires fails
        status(&client, &sha, "ci", "success").await;
        check_run(&server, &sha, "lint", Some("failure")).await;
        // Then the pull request is unstable
        assert_state(&client, "unstable", &format!("draft={draft}: lint")).await;
    }
}

#[tokio::test]
async fn without_rules_a_failing_or_running_check_makes_the_merge_state_unstable() {
    for (conclusion, want) in [
        (Some("failure"), "unstable"),
        (None, "unstable"),
        (Some("neutral"), "clean"),
    ] {
        // Given a pull request into a main without rules
        let server = server(&[pull_request(false)]).await;
        let client = alice(&server);
        let sha = head(&client).await;
        assert_state(&client, "clean", "no checks").await;
        // When a check run reports
        check_run(&server, &sha, "test", conclusion).await;
        // Then only a check that does not pass makes the pull request unstable
        assert_state(&client, want, &format!("{conclusion:?}")).await;
    }
}

#[tokio::test]
async fn a_strict_status_check_rule_makes_a_head_without_the_base_tip_behind() {
    for strict in [true, false] {
        // Given a pull request whose required ci passes, into a main whose rule is strict or not
        let rules = ruleset(&json!([required_checks(
            strict,
            &json!([{"context": "ci"}])
        )]));
        let server = server(&[rules, pull_request(false)]).await;
        let client = alice(&server);
        let sha = head(&client).await;
        status(&client, &sha, "ci", "success").await;
        assert_state(&client, "clean", &format!("strict={strict}: up to date")).await;
        // When main moves on without a conflict
        push(&server, "main", "other.txt");
        // Then a strict rule makes the pull request behind, and a loose one keeps it clean
        let moved = if strict { "behind" } else { "clean" };
        assert_state(&client, moved, &format!("strict={strict}: main moved")).await;
        // When ci fails on that head
        status(&client, &sha, "ci", "failure").await;
        // Then behind wins over the failing required check, as GitHub answers
        let failing = if strict { "behind" } else { "blocked" };
        assert_state(&client, failing, &format!("strict={strict}: ci failed")).await;
        // When main is merged into the head
        let updated = merge_main_into_feature(&server).to_string();
        // Then the new head is up to date and waits for its own required ci
        assert_state(&client, "blocked", &format!("strict={strict}: updated")).await;
        status(&client, &updated, "ci", "success").await;
        assert_state(&client, "clean", &format!("strict={strict}: updated ci")).await;
    }
}

#[tokio::test]
async fn required_approvals_and_thread_resolution_block_the_merge_state() {
    // Given a pull request by alice into a main whose ruleset requires one approval
    let rules = ruleset(&json!([{"type": "pull_request", "parameters": {
        "required_approving_review_count": 1, "dismiss_stale_reviews_on_push": false,
        "require_code_owner_review": false, "require_last_push_approval": false,
        "required_review_thread_resolution": false}}]));
    let server = server(&[rules, pull_request(false)]).await;
    let client = alice(&server);
    let bob = GoGithub::enterprise(&server).with_token(BOB_TOKEN);
    assert_state(&client, "blocked", "no review").await;
    for (event, want) in [
        ("APPROVE", "clean"),
        ("REQUEST_CHANGES", "blocked"),
        ("APPROVE", "clean"),
    ] {
        // When bob, who can write to the repository, submits a review
        let review = bob
            .post(
                &format!("{PULL}/reviews"),
                &json!({"event": event, "body": "review"}),
            )
            .await;
        assert_eq!(review.status, StatusCode::OK, "{}", review.json());
        // Then his latest review decides whether the merge is blocked
        assert_state(&client, want, event).await;
    }

    // Given a pull request into a main whose ruleset requires resolved conversations
    let rules = ruleset(&json!([{"type": "pull_request", "parameters": {
        "required_approving_review_count": 0, "dismiss_stale_reviews_on_push": false,
        "require_code_owner_review": false, "require_last_push_approval": false,
        "required_review_thread_resolution": true}}]));
    let server = self::server(&[rules, pull_request(false)]).await;
    let client = alice(&server);
    assert_state(&client, "clean", "no thread").await;
    // When bob comments on a line
    let sha = head(&client).await;
    let bob = GoGithub::enterprise(&server).with_token(BOB_TOKEN);
    let review = bob
        .post(
            &format!("{PULL}/reviews"),
            &json!({"event": "COMMENT", "body": "see the line", "commit_id": sha,
                    "comments": [{"body": "why", "path": "src/feature.txt", "line": 1, "side": "RIGHT"}]}),
        )
        .await;
    assert_eq!(review.status, StatusCode::OK, "{}", review.json());
    // Then the open conversation blocks the merge
    assert_state(&client, "blocked", "open thread").await;
    // When the conversation is resolved
    let node = client.get(PULL).await.json()["node_id"].clone();
    let threads = Graphql::new(client.clone())
        .query(
            "query($id: ID!) {node(id: $id) {... on PullRequest {reviewThreads(first: 10) {nodes {id}}}}}",
            &json!({"id": node}),
        )
        .await
        .json();
    let thread = threads["data"]["node"]["reviewThreads"]["nodes"][0]["id"].clone();
    let resolved = Graphql::new(client.clone())
        .query(
            "mutation($id: ID!) {resolveReviewThread(input: {threadId: $id}) {thread {isResolved}}}",
            &json!({"id": thread}),
        )
        .await
        .json();
    assert_eq!(resolved.get("errors"), None, "{resolved}");
    // Then the merge is no longer blocked
    assert_state(&client, "clean", "resolved thread").await;
}

#[tokio::test]
async fn a_closed_pull_request_keeps_the_blocked_state_it_had_when_it_closed() {
    // Given a pull request blocked by its missing required ci
    let rules = ruleset(&json!([required_checks(
        false,
        &json!([{"context": "ci"}])
    )]));
    let server = server(&[rules, pull_request(false)]).await;
    let client = alice(&server);
    let sha = head(&client).await;
    assert_state(&client, "blocked", "open").await;
    // When it is closed and ci passes afterwards
    let closed = client
        .call_json(Method::PATCH, PULL, &json!({"state": "closed"}))
        .await;
    assert_eq!(closed.status, StatusCode::OK, "{}", closed.json());
    status(&client, &sha, "ci", "success").await;
    // Then it keeps blocked, as live cli/cli 15146 does
    assert_state(&client, "blocked", "closed").await;
    let pull = client.get(PULL).await.json();
    assert_eq!(pull["mergeable"], true);
    // When it is reopened
    let reopened = client
        .call_json(Method::PATCH, PULL, &json!({"state": "open"}))
        .await;
    assert_eq!(reopened.status, StatusCode::OK, "{}", reopened.json());
    // Then it answers from the passing ci again
    assert_state(&client, "clean", "reopened").await;
}

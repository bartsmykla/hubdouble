use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use http::{Method, StatusCode};
use hubdouble_admin::Registrar;
use hubdouble_gql::{Args, Ctx, ErrorType, GqlError, Obj, Resolved};
use hubdouble_model::{
    Access, Account, ApiError, Issue, IssueState, Pull, PullRef, Repo, Timestamp, Token, TokenKind,
    node_id,
};
use hubdouble_store::{Accounts, Counters, Issues, Repos, Tokens};
use hubdouble_testkit::fixture::{
    ALICE, ALICE_ID, ALICE_TOKEN, APP_ID, APP_PUBLIC_KEY, APP_SLUG, BOB_TOKEN, CAROL_TOKEN,
    INSTALLATION_ID, ORG, PRIVATE_REPO_ID, PUBLIC_REPO, PUBLIC_REPO_ID, WEBHOOK_SECRET,
};
use hubdouble_testkit::validate::webhook_payload;
use hubdouble_testkit::{Answer, Delivery, GoGithub, Graphql, TestServer, standins};
use serde_json::{Value, json};

const PR_APP_ID: i64 = 4002;
const PR_INSTALLATION_ID: i64 = 5002;
const APP_TOKEN: &str = "ghs_fixtureapp0000000000000000000000000";
const PR_APP_TOKEN: &str = "ghs_prapp000000000000000000000000000000";
const METADATA_TOKEN: &str = "ghs_metadataonly0000000000000000000000";
const REPO: &str = "repos/acme/public";

fn must<T>(r: Result<T, ApiError>, what: &str) -> T {
    r.unwrap_or_else(|e| panic!("{what}: {e:?}"))
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "a field resolver returns a Result"
)]
fn leaf(v: impl Into<Value>) -> Result<Resolved, GqlError> {
    Ok(Resolved::Leaf(v.into()))
}

fn name_with_owner(cx: &Ctx<'_>, o: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let repo = o.get::<Repo>()?;
    let owner = cx
        .tx
        .table::<Accounts>()
        .get(repo.owner_id)
        .ok_or_else(|| GqlError::new(ErrorType::Internal, "no owner"))?;
    leaf(format!("{}/{}", owner.login, repo.name))
}

fn pull_repository(cx: &Ctx<'_>, o: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let issue = o.get::<Issue>()?;
    let repo = cx
        .tx
        .table::<Repos>()
        .get(issue.repo_id)
        .ok_or_else(|| GqlError::new(ErrorType::Internal, "no repository"))?;
    Ok(Resolved::Object {
        type_name: "Repository",
        value: Arc::<Repo>::clone(repo),
    })
}

fn stand_ins(r: &mut Registrar<'_>) {
    r.gql.object::<Repo>("Repository");
    r.gql.object::<Account>("User");
    r.gql.object::<Account>("Bot");
    r.gql.object::<Issue>("PullRequest");
    r.gql.field("Repository", "nameWithOwner", name_with_owner);
    r.gql
        .field("User", "databaseId", |_, o, _| leaf(o.get::<Account>()?.id));
    r.gql
        .field("Bot", "databaseId", |_, o, _| leaf(o.get::<Account>()?.id));
    r.gql.field("PullRequest", "repository", pull_repository);
    r.gql
        .field("PullRequest", "headRepository", pull_repository);
    r.gql.field("PullRequest", "state", |_, o, _| {
        leaf(json!(o.get::<Issue>()?.state))
    });
    r.gql.field("PullRequest", "headRefName", |_, o, _| {
        let issue = o.get::<Issue>()?;
        leaf(issue.pull.as_ref().map(|p| p.head.ref_name.clone()))
    });
}

async fn start() -> TestServer {
    let server = TestServer::builder()
        .fixture()
        .entry(
            json!({"kind": "app", "id": PR_APP_ID, "slug": "pr-app", "owner": ORG,
                      "public_key": APP_PUBLIC_KEY,
                      "permissions": {"pull_requests": "write", "metadata": "read"}}),
        )
        .entry(
            json!({"kind": "installation", "id": PR_INSTALLATION_ID, "app": "pr-app",
                      "account": ORG, "repositories": [PUBLIC_REPO],
                      "permissions": {"pull_requests": "write", "metadata": "read"}}),
        )
        .register(hubdouble_api_issues::register)
        .register(standins::query_repository)
        .register(stand_ins)
        .start()
        .await;
    token(&server, APP_TOKEN, APP_ID, INSTALLATION_ID, BTreeMap::new());
    token(
        &server,
        PR_APP_TOKEN,
        PR_APP_ID,
        PR_INSTALLATION_ID,
        BTreeMap::new(),
    );
    token(
        &server,
        METADATA_TOKEN,
        APP_ID,
        INSTALLATION_ID,
        BTreeMap::from([("metadata".to_owned(), Access::Read)]),
    );
    server
}

fn token(
    server: &TestServer,
    value: &str,
    app_id: i64,
    installation_id: i64,
    permissions: BTreeMap<String, Access>,
) {
    let store = &server.server.services().store;
    must(
        store.update(|tx| {
            let id = tx.next_id();
            tx.table_mut::<Tokens>().insert(Token {
                id,
                token: value.to_owned(),
                kind: TokenKind::Installation,
                app_id: Some(app_id),
                installation_id: Some(installation_id),
                user_id: None,
                repo_ids: None,
                permissions,
                scopes: Vec::new(),
                expires_at: Some(Timestamp::SEED.plus_seconds(3600)),
                revoked: false,
                owner_seeded: false,
                created_at: Timestamp::SEED,
            })?;
            Ok(())
        }),
        "add a token",
    );
}

fn client(server: &TestServer, token: &str) -> GoGithub {
    GoGithub::new(server).with_token(token)
}

fn as_alice(server: &TestServer) -> GoGithub {
    client(server, ALICE_TOKEN)
}

fn valid(operation: &str, a: &Answer) -> Value {
    serde_json::from_slice(&a.body)
        .unwrap_or_else(|e| panic!("{operation} {}: not JSON: {e}", a.status))
}

fn pull_request(server: &TestServer, title: &str, body: &str, state: IssueState) -> i64 {
    pull_request_on(server, title, body, state, (PUBLIC_REPO_ID, "main"))
}

fn pull_request_on(
    server: &TestServer,
    title: &str,
    body: &str,
    state: IssueState,
    (repo_id, branch): (i64, &str),
) -> i64 {
    let store = &server.server.services().store;
    must(
        store.update(|tx| {
            let id = tx.next_id();
            let pull_id = tx.next_id();
            let number = tx.table_mut::<Counters>().next_number(repo_id);
            let side = |name: &str| PullRef {
                repo_id: Some(repo_id),
                ref_name: name.to_owned(),
                sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            };
            tx.table_mut::<Issues>().insert(Issue {
                id,
                node_id: node_id::entity(node_id::ISSUE, id),
                repo_id,
                number,
                title: title.to_owned(),
                body: Some(body.to_owned()),
                user_id: ALICE_ID,
                state,
                state_reason: None,
                locked: false,
                active_lock_reason: None,
                label_ids: Vec::new(),
                assignee_ids: Vec::new(),
                closed_by: (state == IssueState::Closed).then_some(ALICE_ID),
                created_at: Timestamp::SEED,
                updated_at: Timestamp::SEED,
                closed_at: (state == IssueState::Closed).then_some(Timestamp::SEED),
                last_edited_at: None,
                pull: Some(Pull {
                    id: pull_id,
                    node_id: node_id::entity(node_id::PULL_REQUEST, pull_id),
                    head: side("feature"),
                    base: side(branch),
                    draft: false,
                    merged_at: None,
                    merge_commit_sha: None,
                    merged_by: None,
                    mergeable: Some(true),
                    maintainer_can_modify: false,
                    requested_reviewers: Vec::new(),
                    requested_teams: Vec::new(),
                }),
            })?;
            Ok(number)
        }),
        "add a pull request",
    )
}

async fn open_issue(gh: &GoGithub, body: &Value) -> Value {
    let a = gh.post(&format!("{REPO}/issues"), body).await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.json());
    valid("issues/create", &a)
}

async fn comment(gh: &GoGithub, number: i64, body: &str) -> Value {
    let a = gh
        .post(
            &format!("{REPO}/issues/{number}/comments"),
            &json!({"body": body}),
        )
        .await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.json());
    valid("issues/create-comment", &a)
}

async fn deliveries(server: &TestServer) -> Vec<Delivery> {
    let drained = server.admin(Method::POST, "hooks/drain", None).await;
    assert_eq!(drained.status, StatusCode::NO_CONTENT);
    server.receiver.received().await
}

fn of<'d>(all: &'d [Delivery], event: &str, action: &str) -> Vec<&'d Delivery> {
    all.iter()
        .filter(|d| d.header("x-github-event") == event && d.json()["action"] == json!(action))
        .collect()
}

fn octokit(d: &Delivery) -> Value {
    let p = d.json();
    let event = d.header("x-github-event");
    let action = p["action"].as_str();
    if let Err(errors) = webhook_payload(event, action, &p) {
        panic!("{event}.{action:?} breaks octokit's schema: {errors:?}");
    }
    assert!(d.signed_with(WEBHOOK_SECRET));
    p
}

#[tokio::test]
async fn every_operation_hides_an_unseen_repository_and_needs_an_issues_or_pull_request_grant() {
    // Given
    let server = start().await;
    let bob = client(&server, BOB_TOKEN);
    let metadata_only = client(&server, METADATA_TOKEN);
    let calls: [(Method, &str); 17] = [
        (Method::GET, "issues"),
        (Method::POST, "issues"),
        (Method::GET, "issues/1"),
        (Method::PATCH, "issues/1"),
        (Method::GET, "issues/1/events"),
        (Method::POST, "issues/1/labels"),
        (Method::GET, "labels"),
        (Method::POST, "labels"),
        (Method::PATCH, "labels/bug"),
        (Method::GET, "issues/1/comments"),
        (Method::GET, "issues/comments"),
        (Method::POST, "issues/1/comments"),
        (Method::GET, "issues/comments/1"),
        (Method::PATCH, "issues/comments/1"),
        (Method::DELETE, "issues/comments/1"),
        (Method::GET, "issues/comments/1/reactions"),
        (Method::POST, "issues/comments/1/reactions"),
    ];
    for (method, path) in calls {
        // When
        let hidden = bob
            .call_json(
                method.clone(),
                &format!("repos/acme/private/{path}"),
                &json!({}),
            )
            .await;
        let denied = metadata_only
            .call_json(method.clone(), &format!("{REPO}/{path}"), &json!({}))
            .await;
        // Then
        assert_eq!(hidden.status, StatusCode::NOT_FOUND, "{method} {path}");
        assert_eq!(denied.status, StatusCode::FORBIDDEN, "{method} {path}");
        assert!(
            denied
                .header("x-accepted-github-permissions")
                .contains("issues="),
            "{method} {path}"
        );
    }
}

#[tokio::test]
async fn a_comment_takes_issues_write_or_pull_requests_write() {
    // Given
    let server = start().await;
    let issue = open_issue(&as_alice(&server), &json!({"title": "t"})).await;
    let number = issue["number"].as_i64().unwrap();
    let path = format!("{REPO}/issues/{number}/comments");
    // When
    let by_issues_grant = client(&server, APP_TOKEN)
        .post(&path, &json!({"body": "a"}))
        .await;
    let by_pulls_grant = client(&server, PR_APP_TOKEN)
        .post(&path, &json!({"body": "b"}))
        .await;
    let by_neither = client(&server, METADATA_TOKEN)
        .post(&path, &json!({"body": "c"}))
        .await;
    // Then
    assert_eq!(by_issues_grant.status, StatusCode::CREATED);
    assert_eq!(by_pulls_grant.status, StatusCode::CREATED);
    assert_eq!(by_neither.status, StatusCode::FORBIDDEN);
    let a = valid("issues/create-comment", &by_issues_grant);
    assert_eq!(a["user"]["login"], json!(format!("{APP_SLUG}[bot]")));
    assert_eq!(a["user"]["type"], json!("Bot"));
    assert_eq!(a["performed_via_github_app"]["id"], json!(APP_ID));
    assert_eq!(by_pulls_grant.json()["user"]["login"], json!("pr-app[bot]"));
    let pulls = client(&server, PR_APP_TOKEN);
    for path in [format!("{REPO}/issues"), format!("{REPO}/issues/{number}")] {
        let denied = pulls.get(&path).await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN, "{path}");
        assert_eq!(
            denied.header("x-accepted-github-permissions"),
            "issues=read"
        );
    }
    let denied = pulls
        .post(
            &format!("{REPO}/issues/comments/{}/reactions", a["id"]),
            &json!({"content": "+1"}),
        )
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert_eq!(
        denied.header("x-accepted-github-permissions"),
        "issues=write"
    );
}

#[tokio::test]
async fn the_smoke_comment_is_delivered_and_the_app_reaction_reads_back_as_its_bot() {
    // Given
    let server = start().await;
    let pr = pull_request(&server, "a change", "", IssueState::Open);
    let issue = open_issue(&as_alice(&server), &json!({"title": "t"})).await;
    let issue_number = issue["number"].as_i64().unwrap();
    // When
    let on_pr = comment(&as_alice(&server), pr, "/agent review").await;
    let on_issue = comment(&as_alice(&server), issue_number, "plain").await;
    let id = on_pr["id"].as_i64().unwrap();
    let reactions = format!("{REPO}/issues/comments/{id}/reactions");
    let app = client(&server, APP_TOKEN);
    let first = app.post(&reactions, &json!({"content": "eyes"})).await;
    let again = app.post(&reactions, &json!({"content": "eyes"})).await;
    let bad = app.post(&reactions, &json!({"content": "wave"})).await;
    let listed = client(&server, ALICE_TOKEN).get(&reactions).await;
    let filtered = client(&server, ALICE_TOKEN)
        .get(&format!("{reactions}?content=heart"))
        .await;
    let all = deliveries(&server).await;
    // Then
    assert_eq!(first.status, StatusCode::CREATED);
    valid("reactions/create-for-issue-comment", &first);
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.json()["id"], first.json()["id"]);
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    let listed = valid("reactions/list-for-issue-comment", &listed);
    let [reaction] = listed.as_array().unwrap().as_slice() else {
        panic!("one reaction expected: {listed}");
    };
    assert_eq!(reaction["content"], json!("eyes"));
    assert_eq!(reaction["user"]["type"], json!("Bot"));
    assert_eq!(reaction["user"]["login"], json!(format!("{APP_SLUG}[bot]")));
    assert_eq!(filtered.json(), json!([]));

    let created = of(&all, "issue_comment", "created");
    assert_eq!(created.len(), 2);
    let pr_payload = octokit(created[0]);
    assert_eq!(pr_payload["comment"]["id"], json!(id));
    assert_eq!(pr_payload["comment"]["body"], json!("/agent review"));
    assert_eq!(pr_payload["installation"]["id"], json!(INSTALLATION_ID));
    let links = &pr_payload["issue"]["pull_request"];
    let base = &server.base;
    assert_eq!(links["url"], json!(format!("{base}/{REPO}/pulls/{pr}")));
    assert_eq!(
        links["html_url"],
        json!(format!("{base}/acme/public/pull/{pr}"))
    );
    assert_eq!(
        links["diff_url"],
        json!(format!("{base}/acme/public/pull/{pr}.diff"))
    );
    assert_eq!(
        links["patch_url"],
        json!(format!("{base}/acme/public/pull/{pr}.patch"))
    );
    let issue_payload = octokit(created[1]);
    assert_eq!(issue_payload["comment"]["id"], on_issue["id"]);
    assert_eq!(issue_payload["issue"].get("pull_request"), None);
}

#[tokio::test]
async fn issues_list_by_state_and_label_with_pages_as_findings_reads_them() {
    // Given
    let server = start().await;
    let alice = as_alice(&server);
    let mut want = Vec::new();
    for n in 0..3 {
        let i = open_issue(
            &alice,
            &json!({"title": format!("finding {n}"), "labels": ["agent"]}),
        )
        .await;
        want.push(i["number"].clone());
    }
    open_issue(&alice, &json!({"title": "unlabeled"})).await;
    let closed = open_issue(&alice, &json!({"title": "done", "labels": ["agent"]})).await;
    let closing = alice
        .call_json(
            Method::PATCH,
            &format!("{REPO}/issues/{}", closed["number"]),
            &json!({"state": "closed"}),
        )
        .await;
    want.reverse();
    // When
    let first = alice
        .get(&format!("{REPO}/issues?labels=agent&per_page=2&state=open"))
        .await;
    let second = alice
        .get(&format!(
            "{REPO}/issues?labels=agent&page=2&per_page=2&state=open"
        ))
        .await;
    let every = alice
        .get(&format!("{REPO}/issues?state=all&labels=AGENT"))
        .await;
    let unknown = alice.get(&format!("{REPO}/issues?labels=agent,nope")).await;
    // Then
    assert_eq!(closing.status, StatusCode::OK);
    assert_eq!(closing.json()["state"], json!("closed"));
    assert_eq!(closing.json()["state_reason"], json!("completed"));
    let first_page = valid("issues/list-for-repo", &first);
    let numbers: Vec<Value> = first_page
        .as_array()
        .unwrap()
        .iter()
        .chain(second.json().as_array().unwrap())
        .map(|i| i["number"].clone())
        .collect();
    assert_eq!(numbers, want);
    assert!(first.header("link").contains("rel=\"next\""));
    assert!(first.header("link").contains("labels=agent"));
    assert_eq!(every.json().as_array().unwrap().len(), 4);
    assert_eq!(unknown.json(), json!([]));
}

#[tokio::test]
async fn issue_list_uses_newest_first_for_an_unknown_sort() {
    let server = start().await;
    let alice = as_alice(&server);
    let first = open_issue(&alice, &json!({"title": "first"})).await;
    server.clock.advance(1);
    let last = open_issue(&alice, &json!({"title": "last"})).await;
    let answer = alice.get(&format!("{REPO}/issues?sort=bogus")).await;
    assert_eq!(answer.status, StatusCode::OK);
    let issues = answer.json();
    assert_eq!(issues[0]["id"], last["id"]);
    assert_eq!(issues[1]["id"], first["id"]);
}

#[tokio::test]
async fn a_writer_labels_a_new_issue_and_a_reader_gets_the_labels_dropped() {
    // Given
    let server = start().await;
    // When
    let by_writer = open_issue(
        &as_alice(&server),
        &json!({"title": 7, "body": "b", "labels": ["bug", {"name": "agent"}], "assignees": [ALICE]}),
    )
    .await;
    let by_reader = open_issue(
        &client(&server, BOB_TOKEN),
        &json!({"title": "r", "labels": ["bug"], "assignees": [ALICE]}),
    )
    .await;
    let untitled = as_alice(&server)
        .post(&format!("{REPO}/issues"), &json!({"body": "no title"}))
        .await;
    let labels = as_alice(&server).get(&format!("{REPO}/labels")).await;
    let all = deliveries(&server).await;
    // Then
    assert_eq!(by_writer["title"], json!("7"));
    let names: Vec<&Value> = by_writer["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| &l["name"])
        .collect();
    assert_eq!(names, [&json!("bug"), &json!("agent")]);
    assert_eq!(by_writer["assignee"]["login"], json!(ALICE));
    assert_eq!(by_writer["author_association"], json!("MEMBER"));
    assert_eq!(by_reader["labels"], json!([]));
    assert_eq!(by_reader["assignees"], json!([]));
    assert_eq!(untitled.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(untitled.json()["errors"][0]["field"], json!("title"));
    assert_eq!(untitled.json()["errors"][0]["code"], json!("missing_field"));
    let labels = valid("issues/list-labels-for-repo", &labels);
    assert_eq!(labels[0]["color"], json!("ededed"));

    let opened = of(&all, "issues", "opened");
    assert_eq!(opened.len(), 2);
    assert_eq!(octokit(opened[0])["issue"]["number"], by_writer["number"]);
    let labeled = of(&all, "issues", "labeled");
    assert_eq!(labeled.len(), 2);
    assert_eq!(octokit(labeled[1])["label"]["name"], json!("agent"));
    let assigned = of(&all, "issues", "assigned");
    assert_eq!(octokit(assigned[0])["assignee"]["login"], json!(ALICE));
}

#[tokio::test]
async fn editing_an_issue_sends_edited_closed_and_reopened_and_needs_triage_for_others() {
    // Given
    let server = start().await;
    let alice = as_alice(&server);
    let issue = open_issue(&alice, &json!({"title": "old", "body": "first"})).await;
    let path = format!("{REPO}/issues/{}", issue["number"]);
    // When
    let by_stranger = client(&server, BOB_TOKEN)
        .call_json(Method::PATCH, &path, &json!({"title": "bob's"}))
        .await;
    let edited = alice
        .call_json(Method::PATCH, &path, &json!({"title": "new", "body": null}))
        .await;
    let closed = alice
        .call_json(
            Method::PATCH,
            &path,
            &json!({"state": "closed", "state_reason": "not_planned"}),
        )
        .await;
    let reopened = alice
        .call_json(Method::PATCH, &path, &json!({"state": "open"}))
        .await;
    let bad_state = alice
        .call_json(Method::PATCH, &path, &json!({"state": "gone"}))
        .await;
    let got = alice.get(&path).await;
    let all = deliveries(&server).await;
    // Then
    assert_eq!(by_stranger.status, StatusCode::FORBIDDEN);
    assert_eq!(edited.status, StatusCode::OK);
    assert_eq!(edited.json()["body"], Value::Null);
    assert_eq!(closed.json()["state_reason"], json!("not_planned"));
    assert_eq!(closed.json()["closed_by"]["login"], json!(ALICE));
    assert_eq!(reopened.json()["state"], json!("open"));
    assert_eq!(reopened.json()["closed_at"], Value::Null);
    assert_eq!(bad_state.status, StatusCode::UNPROCESSABLE_ENTITY);
    let got = valid("issues/get", &got);
    assert_eq!(got["title"], json!("new"));

    let edit = octokit(of(&all, "issues", "edited")[0]);
    assert_eq!(edit["changes"]["title"]["from"], json!("old"));
    assert_eq!(edit["changes"]["body"]["from"], json!("first"));
    assert_eq!(
        octokit(of(&all, "issues", "closed")[0])["issue"]["state"],
        json!("closed")
    );
    assert_eq!(of(&all, "issues", "reopened").len(), 1);
}

#[tokio::test]
async fn pull_request_state_through_issues_has_no_reason_and_a_merged_one_stays_closed() {
    // Given an open pull request and a merged one
    let server = start().await;
    let alice = as_alice(&server);
    let open = pull_request(&server, "open", "", IssueState::Open);
    let merged = pull_request(&server, "merged", "", IssueState::Closed);
    must(
        server.server.services().store.update(|tx| {
            let mut issue = Issue::clone(
                tx.table::<Issues>()
                    .by_number(PUBLIC_REPO_ID, merged)
                    .unwrap(),
            );
            issue.pull.as_mut().unwrap().merged_at = Some(Timestamp::SEED);
            tx.table_mut::<Issues>().replace(Arc::new(issue))?;
            Ok(())
        }),
        "merge the pull request",
    );
    let path = |n: i64| format!("{REPO}/issues/{n}");
    // When the open one is closed and reopened, and the merged one is reopened
    let closed = alice
        .call_json(
            Method::PATCH,
            &path(open),
            &json!({"state": "closed", "state_reason": "not_planned"}),
        )
        .await;
    let reopened = alice
        .call_json(Method::PATCH, &path(open), &json!({"state": "open"}))
        .await;
    let refused = alice
        .call_json(Method::PATCH, &path(merged), &json!({"state": "open"}))
        .await;
    let after = alice.get(&path(merged)).await;
    let all = deliveries(&server).await;
    // Then a pull request has no state reason, as on GitHub
    assert_eq!(closed.status, StatusCode::OK);
    assert_eq!(closed.json()["state"], json!("closed"));
    assert_eq!(closed.json()["state_reason"], Value::Null);
    assert_eq!(reopened.json()["state"], json!("open"));
    assert_eq!(reopened.json()["state_reason"], Value::Null);
    // And the merged one is refused as the pulls endpoint refuses it, and stays closed
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    let error = &refused.json()["errors"][0];
    assert_eq!(
        (&error["resource"], &error["field"], &error["code"]),
        (&json!("PullRequest"), &json!("state"), &json!("custom"))
    );
    let after = valid("issues/get", &after);
    assert_eq!(after["state"], json!("closed"));
    assert_eq!(after["closed_at"], json!(Timestamp::SEED));
    assert_eq!(of(&all, "pull_request", "reopened").len(), 1);
}

#[tokio::test]
async fn pull_request_label_changes_send_webhooks_and_list_who_labeled_it() {
    // Given
    let server = start().await;
    let pr = pull_request(&server, "change", "", IssueState::Open);
    let path = format!("{REPO}/issues/{pr}/labels");
    // When
    let added = as_alice(&server)
        .post(&path, &json!(["agent:review"]))
        .await;
    let again = as_alice(&server)
        .post(
            &path,
            &json!({"labels": [{"name": "AGENT:review"}, "second"]}),
        )
        .await;
    let by_reader = client(&server, BOB_TOKEN).post(&path, &json!(["x"])).await;
    let removed = as_alice(&server)
        .call_json(
            Method::PATCH,
            &format!("{REPO}/issues/{pr}"),
            &json!({"labels": ["second"]}),
        )
        .await;
    assert_eq!(removed.status, StatusCode::OK);
    let events = as_alice(&server)
        .get(&format!("{REPO}/issues/{pr}/events?per_page=100"))
        .await;
    let all = deliveries(&server).await;
    // Then
    assert_eq!(added.status, StatusCode::OK);
    valid("issues/add-labels", &added);
    let names: Vec<Value> = again
        .json()
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["name"].clone())
        .collect();
    assert_eq!(names, [json!("agent:review"), json!("second")]);
    assert_eq!(by_reader.status, StatusCode::FORBIDDEN);
    let events = valid("issues/list-events", &events);
    let labeled: Vec<&Value> = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["event"] == json!("labeled"))
        .collect();
    assert_eq!(labeled.len(), 2);
    assert_eq!(labeled[0]["label"]["name"], json!("agent:review"));
    assert_eq!(labeled[0]["actor"]["login"], json!(ALICE));
    assert_eq!(labeled[0]["actor"]["type"], json!("User"));

    let sent = of(&all, "pull_request", "labeled");
    assert_eq!(sent.len(), 2);
    let p = octokit(sent[0]);
    assert_eq!(p["number"], json!(pr));
    assert_eq!(p["label"]["name"], json!("agent:review"));
    assert_eq!(
        p["pull_request"]["labels"][0]["name"],
        json!("agent:review")
    );
    let unlabeled = of(&all, "pull_request", "unlabeled");
    assert_eq!(unlabeled.len(), 1);
    let p = octokit(unlabeled[0]);
    assert_eq!(p["number"], pr);
    assert_eq!(p["label"]["name"], "agent:review");
    assert_eq!(p["pull_request"]["labels"][0]["name"], "second");
    assert!(of(&all, "issues", "labeled").is_empty());
}

#[tokio::test]
async fn the_events_list_names_who_closed_a_pull_request_another_track_closed() {
    // Given
    let server = start().await;
    let pr = pull_request(&server, "closed elsewhere", "", IssueState::Closed);
    // When
    let events = as_alice(&server)
        .get(&format!("{REPO}/issues/{pr}/events?per_page=100"))
        .await;
    // Then
    let events = events.json();
    let closed: Vec<&Value> = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["event"] == json!("closed"))
        .collect();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0]["actor"]["login"], json!(ALICE));
}

#[tokio::test]
async fn comments_page_from_the_newest_and_filter_by_since() {
    // Given
    let server = start().await;
    let alice = as_alice(&server);
    let issue = open_issue(&alice, &json!({"title": "t"})).await;
    let number = issue["number"].as_i64().unwrap();
    let mut ids = Vec::new();
    for n in 0..5 {
        server.clock.advance(60);
        ids.push(comment(&alice, number, &format!("c{n}")).await["id"].clone());
    }
    let since = Timestamp::SEED
        .plus_seconds(180)
        .to_string()
        .replace(':', "%3A");
    let path = format!("{REPO}/issues/{number}/comments");
    // When
    let first = alice.get(&format!("{path}?per_page=2")).await;
    let last = alice.get(&format!("{path}?page=3&per_page=2")).await;
    let recent = alice
        .get(&format!("{path}?per_page=100&since={since}"))
        .await;
    let repo_wide = alice
        .get(&format!(
            "{REPO}/issues/comments?sort=created&direction=desc&per_page=2"
        ))
        .await;
    let bad_since = alice.get(&format!("{path}?since=yesterday")).await;
    // Then
    let first_page = valid("issues/list-comments", &first);
    assert_eq!(first_page.as_array().unwrap().len(), 2);
    assert!(first.header("link").contains("page=3>; rel=\"last\""));
    assert_eq!(last.json()[0]["id"], ids[4]);
    let recent: Vec<Value> = recent
        .json()
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].clone())
        .collect();
    assert_eq!(recent, ids[2..]);
    let repo_wide = valid("issues/list-comments-for-repo", &repo_wide);
    assert_eq!(repo_wide[0]["id"], ids[4]);
    assert_eq!(repo_wide[1]["id"], ids[3]);
    assert_eq!(bad_since.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn a_comment_is_read_edited_by_its_author_only_and_deleted() {
    // Given
    let server = start().await;
    let alice = as_alice(&server);
    let issue = open_issue(&alice, &json!({"title": "t"})).await;
    let said = comment(
        &client(&server, CAROL_TOKEN),
        issue["number"].as_i64().unwrap(),
        "carol's",
    )
    .await;
    let path = format!("{REPO}/issues/comments/{}", said["id"]);
    // When
    let by_other = client(&server, BOB_TOKEN)
        .call_json(Method::PATCH, &path, &json!({"body": "bob's"}))
        .await;
    let empty = client(&server, CAROL_TOKEN)
        .call_json(Method::PATCH, &path, &json!({}))
        .await;
    server.clock.advance(30);
    let edited = client(&server, CAROL_TOKEN)
        .call_json(Method::PATCH, &path, &json!({"body": "edited"}))
        .await;
    let got = alice.get(&path).await;
    let elsewhere = alice
        .get(&format!(
            "repos/acme/private/issues/comments/{}",
            said["id"]
        ))
        .await;
    let deleted = alice.call(Method::DELETE, &path).await;
    let gone = alice.get(&path).await;
    let all = deliveries(&server).await;
    // Then
    assert_eq!(by_other.status, StatusCode::FORBIDDEN);
    assert_eq!(empty.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(edited.status, StatusCode::OK);
    let got = valid("issues/get-comment", &got);
    assert_eq!(got["body"], json!("edited"));
    assert_ne!(got["updated_at"], got["created_at"]);
    assert_eq!(elsewhere.status, StatusCode::NOT_FOUND);
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    let edit = octokit(of(&all, "issue_comment", "edited")[0]);
    assert_eq!(edit["changes"]["body"]["from"], json!("carol's"));
    assert_eq!(
        octokit(of(&all, "issue_comment", "deleted")[0])["comment"]["id"],
        said["id"]
    );
}

#[tokio::test]
async fn labels_are_created_and_edited_by_writers_with_github_validation() {
    // Given
    let server = start().await;
    let alice = as_alice(&server);
    let body = json!({"name": "agent:review", "color": "B60205", "description": "managed"});
    // When
    let made = alice.post(&format!("{REPO}/labels"), &body).await;
    let twice = alice
        .post(
            &format!("{REPO}/labels"),
            &json!({"name": "AGENT:REVIEW", "color": "ffffff"}),
        )
        .await;
    let bad_color = alice
        .post(
            &format!("{REPO}/labels"),
            &json!({"name": "x", "color": "#ffffff"}),
        )
        .await;
    let by_reader = client(&server, BOB_TOKEN)
        .post(&format!("{REPO}/labels"), &json!({"name": "y"}))
        .await;
    let updated = alice
        .call_json(
            Method::PATCH,
            &format!("{REPO}/labels/agent:review"),
            &json!({"color": "0e8a16", "description": "kept"}),
        )
        .await;
    let missing = alice
        .call_json(
            Method::PATCH,
            &format!("{REPO}/labels/nope"),
            &json!({"color": "0e8a16"}),
        )
        .await;
    // Then
    assert_eq!(made.status, StatusCode::CREATED);
    valid("issues/create-label", &made);
    assert_eq!(twice.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(twice.json()["errors"][0]["code"], json!("already_exists"));
    assert_eq!(bad_color.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(by_reader.status, StatusCode::FORBIDDEN);
    let updated = valid("issues/update-label", &updated);
    assert_eq!(updated["color"], json!("0e8a16"));
    assert_eq!(updated["description"], json!("kept"));
    assert_eq!(updated["name"], json!("agent:review"));
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

const ORIGINAL_COMMENT_EDIT: &str =
    include_str!("../../../docs/inventory/graphql/originalCommentEdit.graphql");
const LAST_EDITS: &str = include_str!("../../../docs/inventory/graphql/lastEdits.graphql");

#[tokio::test]
async fn the_comment_documents_read_an_issue_comment_and_hide_a_private_one() {
    // Given
    let server = start().await;
    let pr = pull_request(&server, "change", "", IssueState::Open);
    let said = comment(&client(&server, APP_TOKEN), pr, "by the app").await;
    server.clock.advance(60);
    as_alice(&server)
        .call_json(
            Method::PATCH,
            &format!("{REPO}/issues/comments/{}", said["id"]),
            &json!({"body": "edited"}),
        )
        .await;
    let private_issue = open_issue_in_private(&server);
    let alice = Graphql::new(as_alice(&server));
    let bob = Graphql::new(client(&server, BOB_TOKEN));
    // When
    let original = alice
        .query(ORIGINAL_COMMENT_EDIT, &json!({"id": said["node_id"]}))
        .await;
    let edits = bob
        .query(
            LAST_EDITS,
            &json!({"ids": [said["node_id"], private_issue]}),
        )
        .await;
    // Then
    let original = original.json();
    assert_eq!(original.get("errors"), None, "{original}");
    let node = &original["data"]["node"];
    assert_eq!(node["__typename"], json!("IssueComment"));
    assert_eq!(node["databaseId"], said["id"]);
    assert_eq!(node["body"], json!("edited"));
    assert_eq!(node["createdAt"], json!("2026-01-01T00:00:00Z"));
    assert_eq!(node["lastEditedAt"], json!("2026-01-01T00:01:00Z"));
    assert_eq!(node["author"]["__typename"], json!("Bot"));
    assert_eq!(node["author"]["login"], json!(format!("{APP_SLUG}[bot]")));
    assert_eq!(node["issue"]["number"], json!(pr));
    assert_eq!(node["pullRequest"]["number"], json!(pr));
    assert_eq!(
        node["pullRequest"]["repository"]["nameWithOwner"],
        json!("acme/public")
    );

    let edits = edits.json();
    assert_eq!(edits["data"]["nodes"][0]["id"], said["node_id"]);
    assert_eq!(
        edits["data"]["nodes"][0]["lastEditedAt"],
        json!("2026-01-01T00:01:00Z")
    );
    assert_eq!(edits["data"]["nodes"][1], Value::Null);
    assert_eq!(edits["errors"][0]["type"], json!("NOT_FOUND"));
    assert_eq!(edits["errors"][0]["path"], json!(["nodes", 1]));
}

fn open_issue_in_private(server: &TestServer) -> String {
    let store = &server.server.services().store;
    must(
        store.update(|tx| {
            let id = tx.next_id();
            let repo = hubdouble_testkit::fixture::PRIVATE_REPO_ID;
            let number = tx.table_mut::<Counters>().next_number(repo);
            let node = node_id::entity(node_id::ISSUE, id);
            tx.table_mut::<Issues>().insert(Issue {
                id,
                node_id: node.clone(),
                repo_id: repo,
                number,
                title: "private".to_owned(),
                body: None,
                user_id: ALICE_ID,
                state: IssueState::Open,
                state_reason: None,
                locked: false,
                active_lock_reason: None,
                label_ids: Vec::new(),
                assignee_ids: Vec::new(),
                closed_by: None,
                created_at: Timestamp::SEED,
                updated_at: Timestamp::SEED,
                closed_at: None,
                last_edited_at: None,
                pull: None,
            })?;
            Ok(node)
        }),
        "add a private issue",
    )
}

const IMPLEMENT_ISSUE: &str =
    include_str!("../../../docs/inventory/graphql/implementIssue.graphql");

fn implement_issue(aliases: usize) -> String {
    let templates = (0..aliases).fold(String::new(), |mut s, n| {
        let _ = writeln!(
            s,
            "t{n}: object(expression: \"HEAD:t{n}.md\") {{ ... on Blob {{ text isTruncated }} }}"
        );
        s
    });
    let (Some(start), Some(end)) = (IMPLEMENT_ISSUE.find("t0:"), IMPLEMENT_ISSUE.find("issue("))
    else {
        panic!("implementIssue has no template aliases before its issue field");
    };
    format!(
        "{}{templates}{}",
        &IMPLEMENT_ISSUE[..start],
        &IMPLEMENT_ISSUE[end..]
    )
}

#[tokio::test]
async fn implement_issue_reads_the_issue_and_the_open_pull_requests_that_close_it() {
    // Given
    let server = start().await;
    let issue = open_issue(
        &as_alice(&server),
        &json!({"title": "bug", "body": "it breaks"}),
    )
    .await;
    let n = issue["number"].as_i64().unwrap();
    let closing = pull_request(&server, "fix", &format!("Fixes #{n}"), IssueState::Open);
    pull_request(
        &server,
        "old fix",
        &format!("closes #{n}"),
        IssueState::Closed,
    );
    pull_request(&server, "unrelated", &format!("see #{n}"), IssueState::Open);
    let alice = Graphql::new(as_alice(&server));
    let vars = |number| json!({"owner": ORG, "name": PUBLIC_REPO, "number": number});
    for document in [
        IMPLEMENT_ISSUE.to_owned(),
        implement_issue(1),
        implement_issue(100),
    ] {
        // When
        let found = alice.query(&document, &vars(n)).await.json();
        let a_pull = alice.query(&document, &vars(closing)).await.json();
        // Then
        let issue = &found["data"]["repository"]["issue"];
        assert_eq!(issue["state"], json!("OPEN"));
        assert_eq!(issue["title"], json!("bug"));
        assert_eq!(issue["body"], json!("it breaks"));
        assert_eq!(
            issue["closedByPullRequestsReferences"]["nodes"],
            json!([{"number": closing, "state": "OPEN", "headRefName": "feature",
                    "headRepository": {"nameWithOwner": "acme/public"}}])
        );
        let errors = found["errors"].as_array().cloned().unwrap_or_default();
        assert!(
            errors
                .iter()
                .all(|e| e["type"] == json!("HUBDOUBLE_NOT_IMPLEMENTED")
                    && e["path"][1] != json!("issue")),
            "only other tracks' fields may fail: {errors:?}"
        );
        assert_eq!(a_pull["data"]["repository"]["issue"], Value::Null);
        assert!(
            a_pull["errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["type"] == json!("NOT_FOUND")
                    && e["path"] == json!(["repository", "issue"]))
        );
    }
}

#[tokio::test]
async fn implement_issue_filters_manual_and_cross_repository_keyword_links() {
    // Given
    let server = start().await;
    let issue = open_issue(&as_alice(&server), &json!({"title": "bug"})).await;
    let n = issue["number"].as_i64().unwrap();
    let keyword = pull_request(&server, "fix", &format!("Fixes #{n}"), IssueState::Open);
    let by_hand = pull_request(&server, "wip", "work in progress", IssueState::Open);
    let closed_by_hand = pull_request(&server, "old", "", IssueState::Closed);
    let cross_repo = |body: &str, branch: &str| {
        pull_request_on(
            &server,
            "external fix",
            body,
            IssueState::Open,
            (PRIVATE_REPO_ID, branch),
        )
    };
    let qualified = format!("Fixes ACME/Public#{n}");
    let external = cross_repo(&qualified, "main");
    cross_repo(&qualified, "develop");
    cross_repo(&format!("Fixes #{n}"), "main");
    for pull in [by_hand, closed_by_hand] {
        let link = json!({"repository": format!("{ORG}/{PUBLIC_REPO}"),
                          "issue_number": n, "pull_number": pull});
        let answer = server.admin(Method::POST, "issue_link", Some(&link)).await;
        assert_eq!(answer.status, StatusCode::CREATED);
    }
    let alice = Graphql::new(as_alice(&server));
    let bob = Graphql::new(client(&server, BOB_TOKEN));
    let vars = json!({"owner": ORG, "name": PUBLIC_REPO, "number": n});
    let numbers = |answer: &Value| -> Vec<i64> {
        answer["data"]["repository"]["issue"]["closedByPullRequestsReferences"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["number"].as_i64().unwrap())
            .collect()
    };
    let only = |argument: &str| {
        format!(
            "query($owner: String!, $name: String!, $number: Int!) {{
               repository(owner: $owner, name: $name) {{ issue(number: $number) {{
                 closedByPullRequestsReferences(first: 20, {argument}: true) {{ nodes {{ number }} }}
               }} }} }}"
        )
    };
    // When
    let found = alice.query(IMPLEMENT_ISSUE, &vars).await.json();
    let user_linked = alice.query(&only("userLinkedOnly"), &vars).await.json();
    let keyword_only = alice.query(&only("excludeUserLinked"), &vars).await.json();
    let visible_to_bob = bob.query(IMPLEMENT_ISSUE, &vars).await.json();
    // Then
    assert_eq!(numbers(&found), [keyword, by_hand, external]);
    assert_eq!(
        found["data"]["repository"]["issue"]["closedByPullRequestsReferences"]["nodes"][1],
        json!({"number": by_hand, "state": "OPEN", "headRefName": "feature",
               "headRepository": {"nameWithOwner": "acme/public"}})
    );
    assert_eq!(numbers(&user_linked), [by_hand]);
    assert_eq!(numbers(&keyword_only), [keyword, external]);
    assert_eq!(numbers(&visible_to_bob), [keyword, by_hand]);
}

use http::{Method, StatusCode};
use hubdouble_testkit::fixture::{self, ALICE_TOKEN, BOB_TOKEN, CAROL_TOKEN};
use hubdouble_testkit::{Answer, Delivery, GoGithub, Graphql, TestServer};
use serde_json::{Value, json};

const REPO: &str = "repos/acme/public";
const INSTALLATION_TOKEN: &str = "ghs_write_guards_issues";

async fn server(archived: bool) -> TestServer {
    let mut builder = TestServer::builder().frame(hubdouble_api::register_all);
    for mut entry in fixture::seed() {
        if entry["kind"] == "repository" && entry["name"] == fixture::PUBLIC_REPO {
            entry["archived"] = json!(archived);
            entry["collaborators"] = json!([{"login": "bob", "permission": "write"}]);
        }
        builder = builder.entry(entry);
    }
    builder
        .entry(
            json!({"kind": "pull_request", "repository": "acme/public", "user": "alice",
                      "title": "feature", "head": "feature", "base": "main"}),
        )
        .start()
        .await
}

fn client(server: &TestServer, token: &str) -> GoGithub {
    GoGithub::enterprise(server).with_token(token)
}

fn installation(server: &TestServer) -> GoGithub {
    let services = server.server.services();
    services
        .store
        .update_quiet(|tx| {
            let id = tx.next_id();
            tx.table_mut::<hubdouble_store::Tokens>()
                .insert(hubdouble_model::Token {
                    id,
                    token: INSTALLATION_TOKEN.into(),
                    kind: hubdouble_model::TokenKind::Installation,
                    app_id: Some(fixture::APP_ID),
                    installation_id: Some(fixture::INSTALLATION_ID),
                    user_id: None,
                    repo_ids: None,
                    permissions: [("issues".into(), hubdouble_model::Access::Write)].into(),
                    scopes: vec![],
                    expires_at: None,
                    revoked: false,
                    owner_seeded: false,
                    created_at: hubdouble_model::Clock::now(&*server.clock),
                })?;
            Ok(())
        })
        .unwrap_or_else(|e| panic!("installation token: {e:?}"));
    client(server, INSTALLATION_TOKEN)
}

async fn deliveries(server: &TestServer) -> Vec<Delivery> {
    let drained = server.admin(Method::POST, "hooks/drain", None).await;
    assert_eq!(drained.status, StatusCode::NO_CONTENT);
    server.receiver.received().await
}

async fn tip(client: &GoGithub, branch: &str) -> String {
    let answer = client.get(&format!("{REPO}/git/ref/heads/{branch}")).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
    answer.json()["object"]["sha"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn assert_refused(answer: &Answer, code: StatusCode, docs: &str, step: &str) {
    let body = answer.json();
    assert_eq!(answer.status, code, "{step}: {body}");
    assert_eq!(body["status"], code.as_u16().to_string(), "{step}");
    assert_eq!(body["documentation_url"], docs, "{step}");
}

#[tokio::test]
async fn an_archived_repository_refuses_every_write_and_still_answers_reads() {
    // Given an archived repository
    let server = server(true).await;
    let alice = client(&server, ALICE_TOKEN);
    let main = tip(&alice, "main").await;
    let feature = tip(&alice, "feature").await;
    deliveries(&server).await;
    // When its admin writes through REST
    for (method, path, body, docs) in [
        (
            Method::POST,
            "issues",
            json!({"title": "bug"}),
            "https://docs.github.com/rest/issues/issues#create-an-issue",
        ),
        (
            Method::POST,
            "issues/1/comments",
            json!({"body": "hi"}),
            "https://docs.github.com/rest/issues/comments#create-an-issue-comment",
        ),
        (
            Method::PATCH,
            "git/refs/heads/main",
            json!({"sha": feature, "force": true}),
            "https://docs.github.com/rest/git/refs#update-a-reference",
        ),
        (
            Method::PUT,
            "pulls/1/merge",
            json!({}),
            "https://docs.github.com/rest/pulls/pulls#merge-a-pull-request",
        ),
    ] {
        let answer = alice
            .call_json(method.clone(), &format!("{REPO}/{path}"), &body)
            .await;
        // Then each write is refused with 403 and the operation's docs
        assert_refused(
            &answer,
            StatusCode::FORBIDDEN,
            docs,
            &format!("{method} {path}"),
        );
    }
    // When it writes through GraphQL
    let answer = Graphql::new(alice.clone())
        .query(
            "mutation($input: CreateCommitOnBranchInput!) {createCommitOnBranch(input: $input) {commit {oid}}}",
            &json!({"input": {"branch": {"repositoryNameWithOwner": "acme/public", "branchName": "main"},
                "expectedHeadOid": main, "message": {"headline": "direct"},
                "fileChanges": {"additions": [{"path": "x.txt", "contents": "eAo="}]}}}),
        )
        .await
        .json();
    // Then the mutation fails as unprocessable
    assert_eq!(
        answer["data"]["createCommitOnBranch"],
        Value::Null,
        "{answer}"
    );
    assert_eq!(answer["errors"][0]["type"], "UNPROCESSABLE", "{answer}");
    // Then nothing changed and nothing was sent
    assert_eq!(tip(&alice, "main").await, main);
    let issues = alice.get(&format!("{REPO}/issues?state=all")).await.json();
    assert_eq!(issues.as_array().map(Vec::len), Some(1), "{issues}");
    assert_eq!(deliveries(&server).await.len(), 0);
    // Then reads still answer, and the repository says it is archived
    let repo = alice.get(REPO).await;
    assert_eq!(repo.status, StatusCode::OK);
    assert_eq!(repo.json()["archived"], true);
    // Then the write that unarchives it is not refused as read-only
    let unarchive = alice
        .call_json(Method::PATCH, REPO, &json!({"archived": false}))
        .await;
    assert_ne!(
        unarchive.status,
        StatusCode::FORBIDDEN,
        "{}",
        unarchive.json()
    );
}

const LOCKED_DOCS: &str = "https://docs.github.com/articles/locking-conversations/";

async fn comment(client: &GoGithub, number: u64) -> Answer {
    client
        .post(
            &format!("{REPO}/issues/{number}/comments"),
            &json!({"body": "hello"}),
        )
        .await
}

async fn comment_count(client: &GoGithub, number: u64) -> usize {
    let all = client
        .get(&format!("{REPO}/issues/{number}/comments"))
        .await
        .json();
    all.as_array().map_or(0, Vec::len)
}

#[tokio::test]
async fn a_locked_conversation_takes_comments_only_from_writers() {
    // Given an issue that alice locked as too heated
    let server = server(false).await;
    let alice = client(&server, ALICE_TOKEN);
    let opened = alice
        .post(&format!("{REPO}/issues"), &json!({"title": "bug"}))
        .await;
    let number = opened.json()["number"].as_u64().unwrap_or_default();
    let lock = alice
        .call_json(
            Method::PUT,
            &format!("{REPO}/issues/{number}/lock"),
            &json!({"lock_reason": "too heated"}),
        )
        .await;
    assert_eq!(lock.status, StatusCode::NO_CONTENT, "{}", lock.json());
    let issue = alice.get(&format!("{REPO}/issues/{number}")).await.json();
    assert_eq!(issue["locked"], true);
    assert_eq!(issue["active_lock_reason"], "too heated");
    let locked: Vec<Value> = deliveries(&server)
        .await
        .iter()
        .filter(|d| d.header("x-github-event") == "issues")
        .map(Delivery::json)
        .filter(|p| p["action"] == "locked")
        .collect();
    assert_eq!(locked.len(), 1);
    assert_eq!(locked[0]["issue"]["locked"], true);
    // When carol, who can only read, and an installation with issues write comment
    let carol = comment(&client(&server, CAROL_TOKEN), number).await;
    let app = comment(&installation(&server), number).await;
    // Then both are refused with 403 and no comment is added
    assert_refused(&carol, StatusCode::FORBIDDEN, LOCKED_DOCS, "carol");
    assert_refused(&app, StatusCode::FORBIDDEN, LOCKED_DOCS, "installation");
    assert_eq!(comment_count(&alice, number).await, 0);
    // When bob, a collaborator with write access, and alice, an admin, comment
    let bob = comment(&client(&server, BOB_TOKEN), number).await;
    let admin = comment(&alice, number).await;
    // Then both comments are added
    assert_eq!(bob.status, StatusCode::CREATED, "{}", bob.json());
    assert_eq!(admin.status, StatusCode::CREATED, "{}", admin.json());
    // When alice unlocks it, carol can comment again
    let unlock = alice
        .call_json(
            Method::DELETE,
            &format!("{REPO}/issues/{number}/lock"),
            &json!({}),
        )
        .await;
    assert_eq!(unlock.status, StatusCode::NO_CONTENT);
    let carol = comment(&client(&server, CAROL_TOKEN), number).await;
    assert_eq!(carol.status, StatusCode::CREATED, "{}", carol.json());
}

#[tokio::test]
async fn a_locked_pull_request_refuses_reviews_and_review_comments_from_readers() {
    // Given pull request 1, locked by alice
    let server = server(false).await;
    let alice = client(&server, ALICE_TOKEN);
    let lock = alice
        .call_json(Method::PUT, &format!("{REPO}/issues/1/lock"), &json!({}))
        .await;
    assert_eq!(lock.status, StatusCode::NO_CONTENT, "{}", lock.json());
    let head = tip(&alice, "feature").await;
    let carol = client(&server, CAROL_TOKEN);
    // When carol reviews with a body, comments on a line, and adds a thread through GraphQL
    let review = carol
        .post(
            &format!("{REPO}/pulls/1/reviews"),
            &json!({"event": "COMMENT", "body": "looks off"}),
        )
        .await;
    let line = carol
        .post(
            &format!("{REPO}/pulls/1/comments"),
            &json!({"body": "here", "commit_id": head, "path": "src/feature.txt", "line": 1,
                    "side": "RIGHT"}),
        )
        .await;
    let pull = alice.get(&format!("{REPO}/pulls/1")).await.json();
    let thread = Graphql::new(carol.clone())
        .query(
            "mutation($input: AddPullRequestReviewThreadInput!) {addPullRequestReviewThread(input: $input) {thread {id}}}",
            &json!({"input": {"pullRequestId": pull["node_id"], "body": "here",
                    "path": "src/feature.txt", "line": 1, "side": "RIGHT"}}),
        )
        .await
        .json();
    // Then each is refused, and the pull request has no review
    assert_refused(&review, StatusCode::FORBIDDEN, LOCKED_DOCS, "review");
    assert_refused(&line, StatusCode::FORBIDDEN, LOCKED_DOCS, "line comment");
    assert_eq!(thread["errors"][0]["type"], "FORBIDDEN", "{thread}");
    let reviews = alice.get(&format!("{REPO}/pulls/1/reviews")).await.json();
    assert_eq!(reviews, json!([]));
    // When bob, who has write access, reviews with a body
    let bob = client(&server, BOB_TOKEN)
        .post(
            &format!("{REPO}/pulls/1/reviews"),
            &json!({"event": "COMMENT", "body": "fine"}),
        )
        .await;
    // Then the review is added
    assert_eq!(bob.status, StatusCode::OK, "{}", bob.json());
}

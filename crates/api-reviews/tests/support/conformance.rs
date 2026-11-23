use http::{Method, StatusCode};
use hubdouble_admin::Registrar;
use hubdouble_git::{CommitNode, EntryMode, FileChange, Signature};
use hubdouble_gql::{Obj, Resolved, Target};
use hubdouble_model::{
    Access, Account, ApiError, Issue, IssueState, Pull, PullRef, Repo, RepoRole, Team, TeamPrivacy,
    TeamRepo, Timestamp, Token, TokenKind, node_id,
};
use hubdouble_store::{Accounts, Issues, Teams, Tokens};
use hubdouble_testkit::{Answer, GoGithub, Graphql, TestServer, fixture, standins, validate};
use serde_json::{Value, json};

pub(super) const PULL: &str = "repos/acme/public/pulls/1";
pub(super) const COMMENTS: &str = "repos/acme/public/pulls/comments";
pub(super) const TOKEN: &str = "ghs_reviews_conformance";
const NOW: Timestamp = Timestamp::SEED;

pub(super) async fn fixture_server() -> TestServer {
    let mut builder = TestServer::builder()
        .register(hubdouble_api_reviews::register)
        .register(standins::query_repository)
        .register(standins::repository_pull_request)
        .register(review_parents);
    for mut entry in fixture::seed() {
        if entry["kind"] == "app" {
            entry["events"] = json!([
                "pull_request",
                "pull_request_review",
                "pull_request_review_comment",
                "pull_request_review_thread",
            ]);
        }
        if entry["name"] == fixture::PUBLIC_REPO && entry["kind"] == "repository" {
            entry["commits"][0]["files"]["src/feature.txt"] = json!("first\nmiddle\nlast\n");
        }
        builder = builder.entry(entry);
    }
    let server = builder.start().await;
    let services = server.server.services();
    services
        .store
        .update_quiet(|tx| {
            let git = &services.git;
            let repo = fixture::PUBLIC_REPO_ID;
            let base = git.resolve_ref(tx, repo, "refs/heads/main")?;
            let head = git.resolve_ref(tx, repo, "refs/heads/feature")?;
            let issue_id = tx.next_id();
            let pull_id = tx.next_id();
            tx.table_mut::<Issues>().insert(pull_issue(
                issue_id,
                pull_id,
                base.to_string(),
                head.to_string(),
            ))?;
            insert_team_and_token(tx)?;
            Ok(())
        })
        .unwrap_or_else(|error| panic!("set up the review fixture: {error}"));
    server
}

pub(super) fn head_sha(server: &TestServer) -> String {
    let services = server.server.services();
    services
        .store
        .view(|tx| {
            let head =
                services
                    .git
                    .resolve_ref(tx, fixture::PUBLIC_REPO_ID, "refs/heads/feature")?;
            Ok(head.to_string())
        })
        .unwrap_or_else(|error| panic!("read the fixture head: {error}"))
}

fn pull_issue(issue_id: i64, pull_id: i64, base: String, head: String) -> Issue {
    let repo = fixture::PUBLIC_REPO_ID;
    Issue {
        id: issue_id,
        node_id: node_id::entity(node_id::ISSUE, issue_id),
        repo_id: repo,
        number: 1,
        title: "Add a feature".into(),
        body: None,
        user_id: fixture::BOB_ID,
        state: IssueState::Open,
        state_reason: None,
        locked: false,
        active_lock_reason: None,
        label_ids: vec![],
        assignee_ids: vec![],
        closed_by: None,
        created_at: NOW,
        updated_at: NOW,
        closed_at: None,
        last_edited_at: None,
        pull: Some(Pull {
            id: pull_id,
            node_id: node_id::entity(node_id::PULL_REQUEST, pull_id),
            head: PullRef {
                repo_id: Some(repo),
                ref_name: "feature".into(),
                sha: head,
            },
            base: PullRef {
                repo_id: Some(repo),
                ref_name: "main".into(),
                sha: base,
            },
            draft: false,
            merged_at: None,
            merge_commit_sha: None,
            merged_by: None,
            mergeable: Some(true),
            maintainer_can_modify: true,
            requested_reviewers: vec![],
            requested_teams: vec![],
        }),
    }
}

pub(super) fn update_feature(server: &TestServer, text: &str) {
    let services = server.server.services();
    services
        .store
        .update(|tx| {
            let git = &services.git;
            let repo = fixture::PUBLIC_REPO_ID;
            let head = git.resolve_ref(tx, repo, "refs/heads/feature")?;
            let signature = Signature::seed();
            git.commit(
                tx,
                repo,
                fixture::ALICE_ID,
                "feature",
                Some(head),
                &[FileChange::Write {
                    path: "src/feature.txt".to_owned(),
                    data: text.as_bytes().into(),
                    mode: EntryMode::File,
                }],
                "Change the feature",
                &signature,
                &signature,
                false,
            )?;
            Ok(())
        })
        .unwrap_or_else(|error| panic!("update the fixture branch: {error}"));
}

fn insert_team_and_token(tx: &mut hubdouble_store::TxMut<'_>) -> Result<(), ApiError> {
    let team_id = tx.next_id();
    tx.table_mut::<Teams>().insert(Team {
        id: team_id,
        node_id: node_id::entity(node_id::TEAM, team_id),
        org_id: fixture::ORG_ID,
        name: "Review Team".into(),
        slug: "review-team".into(),
        description: Some("Reviewers".into()),
        privacy: TeamPrivacy::Closed,
        notifications_enabled: true,
        parent_id: None,
        members: vec![],
        repos: vec![TeamRepo {
            repo_id: fixture::PUBLIC_REPO_ID,
            role: RepoRole::Write,
        }],
        created_at: NOW,
        updated_at: NOW,
    })?;
    let id = tx.next_id();
    tx.table_mut::<Tokens>().insert(Token {
        id,
        token: TOKEN.into(),
        kind: TokenKind::Installation,
        app_id: Some(fixture::APP_ID),
        installation_id: Some(fixture::INSTALLATION_ID),
        user_id: None,
        repo_ids: None,
        permissions: [("metadata".into(), Access::Read)].into(),
        scopes: vec![],
        expires_at: None,
        revoked: false,
        owner_seeded: false,
        created_at: NOW,
    })?;
    Ok(())
}

fn review_parents(r: &mut Registrar<'_>) {
    r.gql.object::<Issue>("PullRequest");
    r.gql.object::<Repo>("Repository");
    r.gql.object::<Account>("User");
    r.gql.object_resolved::<CommitNode>("Commit");
    r.gql.field("Commit", "oid", |_, p, _| {
        Ok(Resolved::Leaf(json!(p.get::<CommitNode>()?.id)))
    });
    r.gql.field("User", "databaseId", |_, p, _| {
        Ok(Resolved::Leaf(json!(p.get::<Account>()?.id)))
    });
    r.gql.field("PullRequest", "repository", |ctx, p, _| {
        ctx.load(Target::RepoId(p.get::<Issue>()?.repo_id))
    });
    r.gql.field("Repository", "nameWithOwner", |ctx, p, _| {
        let r = p.get::<Repo>()?;
        let owner = ctx
            .tx
            .table::<Accounts>()
            .get(r.owner_id)
            .ok_or_else(ApiError::not_found)?;
        Ok(Resolved::Leaf(json!(format!("{}/{}", owner.login, r.name))))
    });
    r.gql.field("PullRequest", "baseRepository", |ctx, p, _| {
        branch(p, false)?
            .repo_id
            .map_or(Ok(Resolved::Null), |id| ctx.load(Target::RepoId(id)))
    });
    r.gql.field("PullRequest", "headRepository", |ctx, p, _| {
        branch(p, true)?
            .repo_id
            .map_or(Ok(Resolved::Null), |id| ctx.load(Target::RepoId(id)))
    });
    r.gql.field("PullRequest", "baseRefName", |_, p, _| {
        Ok(Resolved::Leaf(json!(branch(p, false)?.ref_name)))
    });
    r.gql.field("PullRequest", "baseRefOid", |_, p, _| {
        Ok(Resolved::Leaf(json!(branch(p, false)?.sha)))
    });
    r.gql.field("PullRequest", "headRefOid", |_, p, _| {
        Ok(Resolved::Leaf(json!(branch(p, true)?.sha)))
    });
    r.gql
        .field("PullRequest", "baseRef", |_, _, _| Ok(Resolved::Null));
    r.gql.field("PullRequest", "mergeable", |_, _, _| {
        Ok(Resolved::Leaf(json!("MERGEABLE")))
    });
}

fn branch(p: &Obj, head: bool) -> Result<&PullRef, hubdouble_gql::GqlError> {
    let pull = p
        .get::<Issue>()?
        .pull
        .as_ref()
        .ok_or_else(ApiError::not_found)?;
    Ok(if head { &pull.head } else { &pull.base })
}

pub(super) fn check(answer: &Answer, status: u16, operation: &str) -> Value {
    assert_eq!(
        answer.status.as_u16(),
        status,
        "{operation}: {:?}",
        answer.body
    );
    let body = answer.json();
    validate::openapi_response(operation, status, &body)
        .unwrap_or_else(|e| panic!("{operation}: {e:?}"));
    body
}

pub(super) fn field_id(body: &Value, key: &str) -> i64 {
    body[key]
        .as_i64()
        .unwrap_or_else(|| panic!("expected an integer in {key}: {body}"))
}

pub(super) async fn graphql(api: &Graphql, query: &str, vars: &Value) -> Value {
    let result = api.query(query, vars).await;
    assert_eq!(result.status, StatusCode::OK);
    let body = result.json();
    assert!(body["errors"].is_null(), "{}", body["errors"]);
    body["data"].clone()
}

pub(super) async fn drain(server: &TestServer) -> Vec<hubdouble_testkit::Delivery> {
    let drained = server.admin(Method::POST, "hooks/drain", None).await;
    assert!(drained.status.is_success());
    server.receiver.received().await
}

pub(super) fn webhook(
    deliveries: &[hubdouble_testkit::Delivery],
    event: &str,
    action: &str,
) -> Value {
    let found = deliveries
        .iter()
        .find(|d| d.header("x-github-event") == event && d.json()["action"] == action)
        .unwrap_or_else(|| panic!("missing {event}.{action}"));
    assert!(found.signed_with(fixture::WEBHOOK_SECRET));
    assert_ne!(found.header("x-github-delivery"), "");
    let payload = found.json();
    validate::webhook_payload(event, Some(action), &payload)
        .unwrap_or_else(|e| panic!("{event}.{action}: {e:?}"));
    payload
}

pub(super) async fn api_call(
    client: &GoGithub,
    method: Method,
    path: &str,
    body: Option<&Value>,
    expected: (&str, u16),
) -> Value {
    let response = match body {
        Some(body) => client.call_json(method, path, body).await,
        None => client.call(method, path).await,
    };
    check(&response, expected.1, expected.0)
}

pub(super) async fn assert_reactions(client: &GoGithub, comment: &str) {
    let reactions = format!("{comment}/reactions");
    let eyes = json!({"content": "eyes"});
    let operation = "reactions/create-for-pull-request-review-comment";
    let added = api_call(
        client,
        Method::POST,
        &reactions,
        Some(&eyes),
        (operation, 201),
    )
    .await;
    let repeat = api_call(
        client,
        Method::POST,
        &reactions,
        Some(&eyes),
        (operation, 200),
    )
    .await;
    assert_eq!(repeat["id"], added["id"]);
    let heart = json!({"content": "heart"});
    api_call(
        client,
        Method::POST,
        &reactions,
        Some(&heart),
        (operation, 201),
    )
    .await;
    let filtered = format!("{reactions}?content=eyes&per_page=100&page=1");
    assert_eq!(
        api_call(
            client,
            Method::GET,
            &filtered,
            None,
            ("reactions/list-for-pull-request-review-comment", 200)
        )
        .await,
        json!([added])
    );
    assert_eq!(
        api_call(
            client,
            Method::GET,
            comment,
            None,
            ("pulls/get-review-comment", 200)
        )
        .await["reactions"]["eyes"],
        1
    );
    assert_eq!(
        client
            .post(&reactions, &json!({"content": "invalid"}))
            .await
            .status
            .as_u16(),
        422
    );
}

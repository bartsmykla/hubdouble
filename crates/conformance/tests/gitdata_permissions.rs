use std::collections::BTreeMap;

use http::{Method, StatusCode};
use hubdouble_model::{Access, ApiError, Clock, Token, TokenKind};
use hubdouble_store::{Installations, Tokens};
use hubdouble_testkit::{Answer, GoGithub, Graphql, TestServer, fixture, validate};
use serde_json::{Value, json};

const DOC: &str = include_str!("../../../docs/inventory/graphql/createCommit.graphql");
const READ_ONLY: &str = "ghs_contents_read";
const CONTENTS: &str = "ghs_contents_only";
const WORKFLOWS: &str = "ghs_contents_and_workflows";
const GIT: &str = "repos/acme/public/git";

async fn server_with_tokens() -> TestServer {
    let server = TestServer::builder()
        .frame(hubdouble_api::register_all)
        .fixture()
        .start()
        .await;
    let services = server.server.services();
    services
        .store
        .update_quiet(|tx| {
            let mut installation = (**tx
                .table::<Installations>()
                .get(fixture::INSTALLATION_ID)
                .ok_or_else(ApiError::not_found)?)
            .clone();
            installation
                .permissions
                .insert("contents".into(), Access::Write);
            installation
                .permissions
                .insert("workflows".into(), Access::Write);
            tx.table_mut::<Installations>().replace(installation)?;
            for (token, contents, workflow) in [
                (READ_ONLY, Access::Read, false),
                (CONTENTS, Access::Write, false),
                (WORKFLOWS, Access::Write, true),
            ] {
                let mut permissions = BTreeMap::from([("contents".into(), contents)]);
                if workflow {
                    permissions.insert("workflows".into(), Access::Write);
                }
                let id = tx.next_id();
                tx.table_mut::<Tokens>().insert(Token {
                    id,
                    token: token.into(),
                    kind: TokenKind::Installation,
                    app_id: Some(fixture::APP_ID),
                    installation_id: Some(fixture::INSTALLATION_ID),
                    user_id: None,
                    repo_ids: Some(vec![fixture::PUBLIC_REPO_ID]),
                    permissions,
                    scopes: vec![],
                    expires_at: None,
                    revoked: false,
                    owner_seeded: false,
                    created_at: server.clock.now(),
                })?;
            }
            Ok(())
        })
        .unwrap_or_else(|error| panic!("install test tokens: {error}"));
    server
}

#[tokio::test]
async fn workflow_changes_require_a_separate_installation_grant() {
    let server = server_with_tokens().await;
    let main = branch_sha(&GoGithub::new(&server).with_token(CONTENTS), "main").await;
    let input = json!({"input": {
        "branch": {"repositoryNameWithOwner": "acme/public", "branchName": "main"},
        "expectedHeadOid": main,
        "message": {"headline": "Add a workflow"},
        "fileChanges": {"additions": [{"path": ".github/workflows/check.yml", "contents": "e30K"}]}
    }});

    let denied = Graphql::new(GoGithub::new(&server).with_token(CONTENTS))
        .query(DOC, &input)
        .await
        .json();
    assert_eq!(denied["errors"][0]["type"], "FORBIDDEN");

    let allowed = Graphql::new(GoGithub::new(&server).with_token(WORKFLOWS))
        .query(DOC, &input)
        .await
        .json();
    assert!(allowed.get("errors").is_none(), "{allowed}");
    let oid = &allowed["data"]["createCommitOnBranch"]["commit"]["oid"];
    assert!(oid.is_string());
    assert_ne!(*oid, json!(main));
}

#[tokio::test]
async fn rest_ref_creation_rejects_a_workflow_change_without_the_grant() {
    let server = server_with_tokens().await;
    let client = GoGithub::new(&server).with_token(CONTENTS);
    let main = branch_sha(&client, "main").await;
    let sha = detached_commit(&client, &main, ".github/workflows/check.yml").await;
    let body = json!({"ref": "refs/heads/topic", "sha": sha});

    let denied = client.post(&format!("{GIT}/refs"), &body).await;
    checked("git/create-ref", StatusCode::UNPROCESSABLE_ENTITY, &denied);
    assert_eq!(
        client.get(&format!("{GIT}/ref/heads/topic")).await.status,
        StatusCode::NOT_FOUND
    );

    let writer = GoGithub::new(&server).with_token(WORKFLOWS);
    let allowed = writer.post(&format!("{GIT}/refs"), &body).await;
    checked("git/create-ref", StatusCode::CREATED, &allowed);
    assert_eq!(branch_sha(&client, "topic").await, sha);
    let plain = detached_commit(&client, &main, "notes.md").await;
    let allowed = client
        .post(
            &format!("{GIT}/refs"),
            &json!({"ref": "refs/heads/plain", "sha": plain}),
        )
        .await;
    checked("git/create-ref", StatusCode::CREATED, &allowed);
}

#[tokio::test]
async fn rest_ref_update_rejects_a_workflow_change_without_the_grant() {
    let server = server_with_tokens().await;
    let client = GoGithub::new(&server).with_token(CONTENTS);
    let main = branch_sha(&client, "main").await;
    let path = format!("{GIT}/refs/heads%2Fmain");
    let sha = detached_commit(&client, &main, ".github/workflows/check.yml").await;

    let denied = client
        .call_json(Method::PATCH, &path, &json!({"sha": sha}))
        .await;
    checked("git/update-ref", StatusCode::UNPROCESSABLE_ENTITY, &denied);
    assert_eq!(branch_sha(&client, "main").await, main);

    let hidden = client
        .call_json(
            Method::PATCH,
            "repos/acme/private/git/refs/heads%2Fmain",
            &json!({"sha": sha}),
        )
        .await;
    assert_eq!(hidden.status, StatusCode::NOT_FOUND);
    let read_only = GoGithub::new(&server).with_token(READ_ONLY);
    let denied = read_only
        .call_json(Method::PATCH, &path, &json!({"sha": sha}))
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert_eq!(
        denied.header("x-accepted-github-permissions"),
        "contents=write"
    );

    let writer = GoGithub::new(&server).with_token(WORKFLOWS);
    let invalid = writer
        .call_json(Method::PATCH, &path, &json!({"sha": "bad"}))
        .await;
    let invalid = checked("git/update-ref", StatusCode::UNPROCESSABLE_ENTITY, &invalid);
    assert_eq!(invalid["errors"][0]["field"], "sha");
    let allowed = writer
        .call_json(Method::PATCH, &path, &json!({"sha": sha}))
        .await;
    checked("git/update-ref", StatusCode::OK, &allowed);
    assert_eq!(branch_sha(&client, "main").await, sha);

    let plain = detached_commit(&client, &sha, "notes.md").await;
    let allowed = client
        .call_json(Method::PATCH, &path, &json!({"sha": plain}))
        .await;
    checked("git/update-ref", StatusCode::OK, &allowed);
    assert_eq!(branch_sha(&client, "main").await, plain);

    let denied = client
        .call_json(Method::PATCH, &path, &json!({"sha": main, "force": true}))
        .await;
    checked("git/update-ref", StatusCode::UNPROCESSABLE_ENTITY, &denied);
    assert_eq!(branch_sha(&client, "main").await, plain);
}

async fn branch_sha(client: &GoGithub, branch: &str) -> String {
    let answer = client.get(&format!("{GIT}/ref/heads/{branch}")).await;
    checked("git/get-ref", StatusCode::OK, &answer)["object"]["sha"]
        .as_str()
        .unwrap_or_else(|| panic!("{branch}: missing ref SHA"))
        .to_owned()
}

async fn detached_commit(client: &GoGithub, parent: &str, path: &str) -> String {
    let parent_commit = client.get(&format!("{GIT}/commits/{parent}")).await;
    let base = checked("git/get-commit", StatusCode::OK, &parent_commit);
    let tree = client.post(&format!("{GIT}/trees"), &json!({
        "base_tree": base["tree"]["sha"],
        "tree": [{"path": path, "mode": "100644", "type": "blob", "content": "name: Check\n"}]
    })).await;
    let tree = checked("git/create-tree", StatusCode::CREATED, &tree);
    let commit = client
        .post(
            &format!("{GIT}/commits"),
            &json!({
                "message": "Write a file", "tree": tree["sha"], "parents": [parent]
            }),
        )
        .await;
    checked("git/create-commit", StatusCode::CREATED, &commit)["sha"]
        .as_str()
        .unwrap_or_else(|| panic!("{path}: missing commit SHA"))
        .to_owned()
}

fn checked(operation: &str, status: StatusCode, answer: &Answer) -> Value {
    assert_eq!(answer.status, status, "{operation}");
    let body = answer.json();
    validate::openapi_response(operation, status.as_u16(), &body)
        .unwrap_or_else(|errors| panic!("{operation}: {errors:?}"));
    body
}

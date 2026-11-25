use std::collections::BTreeMap;

use http::{Method, StatusCode};
use hubdouble_model::{Access, ApiError, Clock, Token, TokenKind};
use hubdouble_store::{Installations, Tokens};
use hubdouble_testkit::{Answer, GoGithub, Graphql, TestServer, fixture, validate};
use serde_json::{Value, json};

const DOC: &str = include_str!("../../../docs/inventory/graphql/createCommit.graphql");
const UPDATE_REFS: &str = include_str!("../../../docs/inventory/graphql/updateRefs.graphql");
const ISSUES_ONLY: &str = "ghs_issues_read_only";
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
            installation.repo_ids.push(fixture::PRIVATE_REPO_ID);
            tx.table_mut::<Installations>().replace(installation)?;
            let id = tx.next_id();
            tx.table_mut::<Tokens>().insert(Token {
                id,
                token: ISSUES_ONLY.into(),
                kind: TokenKind::Installation,
                app_id: Some(fixture::APP_ID),
                installation_id: Some(fixture::INSTALLATION_ID),
                user_id: None,
                repo_ids: None,
                permissions: BTreeMap::from([("issues".into(), Access::Read)]),
                scopes: vec![],
                expires_at: None,
                revoked: false,
                owner_seeded: false,
                created_at: server.clock.now(),
            })?;
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

#[tokio::test]
async fn update_refs_rejects_a_workflow_change_without_the_grant() {
    // Given
    let server = server_with_tokens().await;
    let client = GoGithub::new(&server).with_token(CONTENTS);
    let main = branch_sha(&client, "main").await;
    let sha = detached_commit(&client, &main, ".github/workflows/check.yml").await;
    let repo = client.get("repos/acme/public").await.json();
    let input = json!({"input": {"repositoryId": repo["node_id"], "refUpdates": [
        {"name": "refs/heads/main", "beforeOid": main, "afterOid": sha, "force": false}
    ]}});
    // When
    let denied = Graphql::new(GoGithub::new(&server).with_token(CONTENTS))
        .query(UPDATE_REFS, &input)
        .await
        .json();
    let main_after_denial = branch_sha(&client, "main").await;
    let allowed = Graphql::new(GoGithub::new(&server).with_token(WORKFLOWS))
        .query(UPDATE_REFS, &input)
        .await
        .json();
    // Then
    assert_eq!(denied["data"]["updateRefs"], Value::Null, "{denied}");
    assert_eq!(denied["errors"][0]["type"], "UNPROCESSABLE");
    assert_eq!(main_after_denial, main);
    assert!(allowed.get("errors").is_none(), "{allowed}");
    assert_eq!(branch_sha(&client, "main").await, sha);
}

#[tokio::test]
async fn graphql_git_fields_need_the_contents_grant_on_a_private_repository() {
    // Given
    let server = server_with_tokens().await;
    let document = "query($name: String!) { repository(owner: \"acme\", name: $name) { name \
        object(expression: \"HEAD:\") { oid } ref(qualifiedName: \"refs/heads/main\") { name } \
        defaultBranchRef { name } refs(refPrefix: \"refs/heads/\", first: 10) { totalCount } } }";
    let variables = json!({"name": fixture::PRIVATE_REPO});
    // When
    let without = Graphql::new(GoGithub::new(&server).with_token(ISSUES_ONLY))
        .query(document, &variables)
        .await
        .json();
    // Then
    assert_eq!(without["data"]["repository"]["name"], fixture::PRIVATE_REPO);
    let mut denied: Vec<&str> = without["errors"]
        .as_array()
        .unwrap_or_else(|| panic!("no errors: {without}"))
        .iter()
        .inspect(|e| assert_eq!(e["type"], "FORBIDDEN", "{e}"))
        .filter_map(|e| e["path"][1].as_str())
        .collect();
    denied.sort_unstable();
    assert_eq!(denied, ["defaultBranchRef", "object", "ref", "refs"]);
    for field in ["object", "ref", "defaultBranchRef", "refs"] {
        assert_eq!(without["data"]["repository"][field], Value::Null, "{field}");
    }
}

async fn workflow_branch(server: &TestServer) -> String {
    let writer = GoGithub::new(server).with_token(WORKFLOWS);
    let main = branch_sha(&writer, "main").await;
    let sha = detached_commit(&writer, &main, ".github/workflows/check.yml").await;
    let created = writer
        .post(
            &format!("{GIT}/refs"),
            &json!({"ref": "refs/heads/wf", "sha": sha}),
        )
        .await;
    checked("git/create-ref", StatusCode::CREATED, &created);
    main
}

#[tokio::test]
async fn merges_that_bring_in_a_workflow_change_need_the_workflows_grant() {
    // Given a wf branch that adds a workflow file to main
    let server = server_with_tokens().await;
    let main = workflow_branch(&server).await;
    let contents = GoGithub::new(&server).with_token(CONTENTS);
    let merge = json!({"base": "main", "head": "wf"});
    // When an installation without workflows merges it through POST /merges
    let denied = contents.post("repos/acme/public/merges", &merge).await;
    // Then GitHub refuses with 403 and main stays
    let body = checked("repos/merge", StatusCode::FORBIDDEN, &denied);
    assert_eq!(
        body["documentation_url"],
        "https://docs.github.com/rest/branches/branches#merge-a-branch"
    );
    assert_eq!(branch_sha(&contents, "main").await, main);
    // When alice opens a pull request of wf and the same installation merges it
    let alice = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    let opened = alice
        .post(
            "repos/acme/public/pulls",
            &json!({"title": "workflow", "head": "wf", "base": "main"}),
        )
        .await;
    let number = checked("pulls/create", StatusCode::CREATED, &opened)["number"].clone();
    let path = format!("repos/acme/public/pulls/{number}/merge");
    let denied = contents.call_json(Method::PUT, &path, &json!({})).await;
    // Then that merge is refused with 403 too, and main stays
    let body = checked("pulls/merge", StatusCode::FORBIDDEN, &denied);
    assert_eq!(
        body["documentation_url"],
        "https://docs.github.com/rest/pulls/pulls#merge-a-pull-request"
    );
    assert_eq!(branch_sha(&contents, "main").await, main);
    // When an installation with workflows merges it
    let allowed = GoGithub::new(&server)
        .with_token(WORKFLOWS)
        .call_json(Method::PUT, &path, &json!({}))
        .await;
    // Then the merge goes through
    checked("pulls/merge", StatusCode::OK, &allowed);
    assert_ne!(branch_sha(&contents, "main").await, main);
}

#[tokio::test]
async fn a_classic_token_without_the_workflow_scope_cannot_change_a_workflow() {
    // Given alice's classic tokens with and without the workflow scope
    let plain = format!("ghp_{:0<36}", "c1a55");
    let scoped = format!("ghp_{:0<36}", "c1a55f");
    let server = TestServer::builder()
        .frame(hubdouble_api::register_all)
        .fixture()
        .entry(json!({"kind": "token", "user": "alice", "token": plain, "scopes": ["repo"]}))
        .entry(json!({"kind": "token", "user": "alice", "token": scoped,
                      "scopes": ["repo", "workflow"]}))
        .start()
        .await;
    let client = GoGithub::new(&server).with_token(&plain);
    let main = branch_sha(&client, "main").await;
    let sha = detached_commit(&client, &main, ".github/workflows/check.yml").await;
    let path = format!("{GIT}/refs/heads%2Fmain");
    // When the token without the scope moves main to the workflow change
    let denied = client
        .call_json(Method::PATCH, &path, &json!({"sha": sha}))
        .await;
    // Then GitHub refuses it and main stays
    checked("git/update-ref", StatusCode::UNPROCESSABLE_ENTITY, &denied);
    assert_eq!(branch_sha(&client, "main").await, main);
    // When the token with the scope does the same
    let allowed = GoGithub::new(&server)
        .with_token(&scoped)
        .call_json(Method::PATCH, &path, &json!({"sha": sha}))
        .await;
    // Then main moves
    checked("git/update-ref", StatusCode::OK, &allowed);
    assert_eq!(branch_sha(&client, "main").await, sha);
}

#[tokio::test]
async fn a_workflow_file_that_another_branch_holds_needs_no_workflow_scope() {
    // Given alice's classic tokens with and without the workflow scope, ci and wf branches,
    // written with the scope, that hold .github/workflows/check.yml and wf.yml, and a main
    // one plain commit ahead
    let plain = format!("ghp_{:0<36}", "c1a55");
    let scoped = format!("ghp_{:0<36}", "c1a55f");
    let server = TestServer::builder()
        .frame(hubdouble_api::register_all)
        .fixture()
        .entry(json!({"kind": "token", "user": "alice", "token": plain, "scopes": ["repo"]}))
        .entry(json!({"kind": "token", "user": "alice", "token": scoped,
                      "scopes": ["repo", "workflow"]}))
        .start()
        .await;
    let client = GoGithub::new(&server).with_token(&plain);
    let writer = GoGithub::new(&server).with_token(&scoped);
    let main = branch_sha(&client, "main").await;
    for (branch, file) in [("ci", "check.yml"), ("wf", "wf.yml")] {
        let sha = detached_commit(&client, &main, &format!(".github/workflows/{file}")).await;
        let made = writer
            .post(
                &format!("{GIT}/refs"),
                &json!({"ref": format!("refs/heads/{branch}"), "sha": sha}),
            )
            .await;
        checked("git/create-ref", StatusCode::CREATED, &made);
    }
    let path = format!("{GIT}/refs/heads%2Fmain");
    let plain = detached_commit(&client, &main, "notes.md").await;
    let moved = client
        .call_json(Method::PATCH, &path, &json!({"sha": plain}))
        .await;
    checked("git/update-ref", StatusCode::OK, &moved);

    // When a token without the grant adds the same contents at another workflow path
    let elsewhere = detached_commit(&client, &plain, ".github/workflows/other.yml").await;
    let denied = client
        .call_json(Method::PATCH, &path, &json!({"sha": elsewhere}))
        .await;
    // Then GitHub refuses it, as no branch holds that path
    checked("git/update-ref", StatusCode::UNPROCESSABLE_ENTITY, &denied);
    assert_eq!(branch_sha(&client, "main").await, plain);

    // When it adds the file that ci holds, at the same path and with the same contents
    let same = detached_commit(&client, &plain, ".github/workflows/check.yml").await;
    let allowed = client
        .call_json(Method::PATCH, &path, &json!({"sha": same}))
        .await;
    // Then main moves, as the OAuth scopes docs allow
    checked("git/update-ref", StatusCode::OK, &allowed);
    assert_eq!(branch_sha(&client, "main").await, same);

    // When the same token merges wf, which holds its workflow file, into main
    let merged = client
        .post(
            "repos/acme/public/merges",
            &json!({"base": "main", "head": "wf"}),
        )
        .await;
    // Then the merge goes through
    checked("repos/merge", StatusCode::CREATED, &merged);
}

fn classic_tokens() -> (String, String) {
    (format!("ghp_{:0<36}", "c1a55"), format!("ghp_{:0<36}", "c1a55f"))
}

async fn server_with_classic_tokens(plain: &str, scoped: &str) -> TestServer {
    TestServer::builder()
        .frame(hubdouble_api::register_all)
        .fixture()
        .entry(json!({"kind": "token", "user": "alice", "token": plain, "scopes": ["repo"]}))
        .entry(json!({"kind": "token", "user": "alice", "token": scoped,
                      "scopes": ["repo", "workflow"]}))
        .start()
        .await
}

#[tokio::test]
async fn a_merge_without_the_workflow_scope_cannot_make_a_new_workflow_file() {
    // Given branches a and b from main, written with the workflow scope, that each change
    // another line of .github/workflows/x.yml
    let (plain, scoped) = classic_tokens();
    let server = server_with_classic_tokens(&plain, &scoped).await;
    let client = GoGithub::new(&server).with_token(&plain);
    let writer = GoGithub::new(&server).with_token(&scoped);
    let main = branch_sha(&writer, "main").await;
    let file = ".github/workflows/x.yml";
    let lines = ["name: X", "on: push", "jobs: {}", "env: {}", "permissions: {}"];
    let text = |first: &str, last: &str| {
        format!("{first}\n{}\n{last}\n", lines[1..4].join("\n"))
    };
    let shared = commit_file(&writer, &main, file, &text(lines[0], lines[4])).await;
    let moved = writer
        .call_json(
            Method::PATCH,
            &format!("{GIT}/refs/heads%2Fmain"),
            &json!({"sha": shared}),
        )
        .await;
    checked("git/update-ref", StatusCode::OK, &moved);
    for (branch, content) in [
        ("a", text("name: A", lines[4])),
        ("b", text(lines[0], "permissions: {contents: read}")),
    ] {
        let sha = commit_file(&writer, &shared, file, &content).await;
        let made = writer
            .post(
                &format!("{GIT}/refs"),
                &json!({"ref": format!("refs/heads/{branch}"), "sha": sha}),
            )
            .await;
        checked("git/create-ref", StatusCode::CREATED, &made);
    }
    let a = branch_sha(&client, "a").await;
    // When a token without the scope merges b into a, which writes a file no branch holds
    let denied = client
        .post("repos/acme/public/merges", &json!({"base": "a", "head": "b"}))
        .await;
    // Then GitHub refuses it with 403 and a stays
    checked("repos/merge", StatusCode::FORBIDDEN, &denied);
    assert_eq!(branch_sha(&client, "a").await, a);
    // When a pull request of b into a is merged by the same token
    let opened = client
        .post(
            "repos/acme/public/pulls",
            &json!({"title": "x", "head": "b", "base": "a"}),
        )
        .await;
    let number = checked("pulls/create", StatusCode::CREATED, &opened)["number"].clone();
    let merge = format!("repos/acme/public/pulls/{number}/merge");
    let denied = client.call_json(Method::PUT, &merge, &json!({})).await;
    // Then it is refused with 403 too, and a stays
    checked("pulls/merge", StatusCode::FORBIDDEN, &denied);
    assert_eq!(branch_sha(&client, "a").await, a);
}

#[tokio::test]
async fn create_commit_on_branch_without_the_workflow_scope_may_add_a_held_workflow_file() {
    // Given a ci branch, written with the workflow scope, that holds
    // .github/workflows/check.yml
    let (plain, scoped) = classic_tokens();
    let server = server_with_classic_tokens(&plain, &scoped).await;
    let client = GoGithub::new(&server).with_token(&plain);
    let writer = GoGithub::new(&server).with_token(&scoped);
    let main = branch_sha(&writer, "main").await;
    let ci = detached_commit(&writer, &main, ".github/workflows/check.yml").await;
    let made = writer
        .post(
            &format!("{GIT}/refs"),
            &json!({"ref": "refs/heads/ci", "sha": ci}),
        )
        .await;
    checked("git/create-ref", StatusCode::CREATED, &made);
    let input = |head: &str, contents: &str| {
        json!({"input": {
            "branch": {"repositoryNameWithOwner": "acme/public", "branchName": "main"},
            "expectedHeadOid": head,
            "message": {"headline": "Add a workflow"},
            "fileChanges": {"additions": [
                {"path": ".github/workflows/check.yml", "contents": contents}
            ]}
        }})
    };
    // When a token without the scope commits other contents at that path on main
    let denied = Graphql::new(client.clone())
        .query(DOC, &input(&main, "e30K"))
        .await
        .json();
    // Then GitHub refuses it and main stays
    assert_eq!(denied["errors"][0]["type"], "FORBIDDEN", "{denied}");
    assert_eq!(branch_sha(&client, "main").await, main);
    // When it commits the contents that ci holds there
    let allowed = Graphql::new(client.clone())
        .query(DOC, &input(&main, "bmFtZTogQ2hlY2sK"))
        .await
        .json();
    // Then main moves, as the OAuth scopes docs allow
    assert!(allowed.get("errors").is_none(), "{allowed}");
    assert_ne!(branch_sha(&client, "main").await, main);
}

async fn branch_sha(client: &GoGithub, branch: &str) -> String {
    let answer = client.get(&format!("{GIT}/ref/heads/{branch}")).await;
    checked("git/get-ref", StatusCode::OK, &answer)["object"]["sha"]
        .as_str()
        .unwrap_or_else(|| panic!("{branch}: missing ref SHA"))
        .to_owned()
}

async fn detached_commit(client: &GoGithub, parent: &str, path: &str) -> String {
    commit_file(client, parent, path, "name: Check\n").await
}

async fn commit_file(client: &GoGithub, parent: &str, path: &str, content: &str) -> String {
    let parent_commit = client.get(&format!("{GIT}/commits/{parent}")).await;
    let base = checked("git/get-commit", StatusCode::OK, &parent_commit);
    let tree = client.post(&format!("{GIT}/trees"), &json!({
        "base_tree": base["tree"]["sha"],
        "tree": [{"path": path, "mode": "100644", "type": "blob", "content": content}]
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

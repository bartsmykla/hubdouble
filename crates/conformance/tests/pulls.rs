use std::{collections::BTreeMap, sync::Arc};

use http::{Method, StatusCode};
use hubdouble_admin::Registrar;
use hubdouble_git::{CommitNode, EntryMode, FileChange, RefNode, Signature};
use hubdouble_gql::{Args, Ctx, ErrorType, GqlError, Obj, Resolved};
use hubdouble_model::{
    Access, ApiError, Clock, Repo, Review, ReviewState, Token, TokenKind, node_id,
};
use hubdouble_store::{Issues, Reviews, Tokens};
use hubdouble_testkit::{
    Answer, Builder, GoGithub, Graphql, TestServer, fixture, standins, validate,
};
use serde_json::{Value, json};

const PULLS: &str = "repos/acme/public/pulls";
const NUM: &str = "repos/acme/public/pulls/1";
const INSTALLATION_TOKEN: &str = "ghs_fixture_pulls_without_a_grant";
const CONTENTS_TOKEN: &str = "ghs_fixture_pulls_with_contents";
const READY: &str = include_str!("../../../docs/inventory/graphql/ready.graphql");
const PULL_OF_NODE: &str = include_str!("../../../docs/inventory/graphql/pullOfNode.graphql");

fn fixture() -> Builder {
    TestServer::builder()
        .fixture()
        .register(hubdouble_api_pulls::register)
}

fn writer(server: &TestServer) -> GoGithub {
    GoGithub::enterprise(server).with_token(fixture::ALICE_TOKEN)
}

fn seeded(base: &str, draft: bool) -> Value {
    json!({"kind": "pull_request", "repository": "acme/public", "user": "alice",
        "title": format!("Feature against {base}"), "body": "original", "head": "feature", "base": base, "draft": draft})
}

fn checked(operation: &str, status: StatusCode, answer: &Answer) -> Value {
    assert_eq!(answer.status, status, "{operation}: {:?}", answer.json());
    answer.json()
}

async fn edit(client: &GoGithub, path: &str, body: Value) -> Value {
    let answer = client.call_json(Method::PATCH, path, &body).await;
    checked("pulls/update", StatusCode::OK, &answer)
}

async fn get(client: &GoGithub, path: &str, op: &str) -> Value {
    checked(op, StatusCode::OK, &client.get(path).await)
}

#[tokio::test]
async fn only_the_author_or_a_writer_edits_a_pull_request() {
    // Given PR 1 by alice and PR 2 by carol, who has no role on acme/public
    let mut by_carol = seeded("develop", false);
    by_carol["user"] = json!(fixture::CAROL);
    let server = fixture()
        .entry(seeded("main", false))
        .entry(by_carol)
        .start()
        .await;
    let carol = GoGithub::enterprise(&server).with_token(fixture::CAROL_TOKEN);
    // When
    let edit_other = carol
        .call_json(Method::PATCH, NUM, &json!({"title": "Taken over"}))
        .await;
    let edit_own = carol
        .call_json(
            Method::PATCH,
            "repos/acme/public/pulls/2",
            &json!({"title": "Mine"}),
        )
        .await;
    // Then
    assert_eq!(edit_other.status, StatusCode::FORBIDDEN);
    assert_eq!(
        get(&carol, NUM, "pulls/get").await["title"],
        "Feature against main"
    );
    assert_eq!(
        checked("pulls/update", StatusCode::OK, &edit_own)["title"],
        "Mine"
    );
    assert_eq!(
        edit(
            &writer(&server),
            "repos/acme/public/pulls/2",
            json!({"title": "Edited by a writer"})
        )
        .await["title"],
        "Edited by a writer"
    );
}

#[tokio::test]
async fn list_filters_pages_and_sorts_before_render() {
    // Given go-github v93 github/pulls.go:185, with the options from internal/versioning/change_projects.go:873-877.
    let server = fixture()
        .entry(seeded("main", false))
        .entry(seeded("develop", false))
        .start()
        .await;
    let client = writer(&server);
    server.clock.advance(60);
    edit(
        &client,
        NUM,
        json!({"title": "Changed", "body": "new text"}),
    )
    .await;
    let path =
        format!("{PULLS}?head=Acme%3Afeature&state=all&sort=updated&direction=desc&per_page=1");
    let first = client.get(&path).await;
    let second = client.get(&format!("{path}&page=2")).await;
    let pulls = checked("pulls/list", StatusCode::OK, &first);
    assert_eq!(pulls[0]["number"], 1);
    assert_eq!(pulls[0]["title"], "Changed");
    assert!(first.header("link").contains("rel=\"next\""));
    assert!(first.header("link").contains("head=Acme:feature"));
    assert_eq!(
        checked("pulls/list", StatusCode::OK, &second)[0]["number"],
        2
    );
    assert!(second.header("link").contains("rel=\"prev\""));
    let answer = client
        .get(&format!(
            "{PULLS}?state=open&base=develop&head=acme%3Afeature&per_page=100"
        ))
        .await;
    assert_eq!(answer.json().as_array().unwrap().len(), 1);
    assert_eq!(answer.json()[0]["base"]["ref"], "develop");
    edit(&client, NUM, json!({"state": "closed"})).await;
    assert_eq!(client.get(PULLS).await.json().as_array().unwrap().len(), 1);
    assert_eq!(
        client
            .get(&format!("{PULLS}?state=closed&base=main"))
            .await
            .json()[0]["number"],
        1
    );
}

#[tokio::test]
async fn pull_list_ignores_a_head_without_an_owner_separator() {
    let server = fixture().entry(seeded("main", false)).start().await;
    let client = writer(&server);
    let answer = client.get(&format!("{PULLS}?head=trunk")).await;
    let pulls = checked("pulls/list", StatusCode::OK, &answer);
    assert_eq!(pulls[0]["head"]["ref"], "feature");
}

#[tokio::test]
async fn pull_list_uses_open_for_an_unknown_state() {
    let server = fixture()
        .entry(seeded("main", false))
        .entry(seeded("develop", false))
        .start()
        .await;
    let client = writer(&server);
    edit(&client, NUM, json!({"state": "closed"})).await;
    let answer = client.get(&format!("{PULLS}?state=bogus")).await;
    let pulls = checked("pulls/list", StatusCode::OK, &answer);
    assert_eq!(pulls.as_array().unwrap().len(), 1);
    assert_eq!(pulls[0]["number"], 2);
}

#[tokio::test]
async fn pull_list_sorts_oldest_first_for_an_unknown_sort() {
    let server = fixture()
        .entry(seeded("main", false))
        .entry(seeded("develop", false))
        .start()
        .await;
    let client = writer(&server);
    let answer = client.get(&format!("{PULLS}?sort=bogus")).await;
    let pulls = checked("pulls/list", StatusCode::OK, &answer);
    assert_eq!(pulls[0]["number"], 1);
    assert_eq!(pulls[1]["number"], 2);
}

#[tokio::test]
async fn pull_list_sorts_newest_first_for_an_unknown_direction() {
    let server = fixture()
        .entry(seeded("main", false))
        .entry(seeded("develop", false))
        .start()
        .await;
    let client = writer(&server);
    for direction in ["bogus", "ASC", ""] {
        let answer = client
            .get(&format!("{PULLS}?sort=created&direction={direction}"))
            .await;
        let pulls = checked("pulls/list", StatusCode::OK, &answer);
        assert_eq!(pulls[0]["number"], 2);
        assert_eq!(pulls[1]["number"], 1);
    }
}

#[tokio::test]
async fn fork_commits_and_files_preserve_head_and_base_identity() {
    // Given go-github v93 github/pulls.go:216,243,313,428,454.
    let server = fixture().entry(json!({
        "kind": "repository", "id": 3100, "owner": "bob", "name": "public", "fork_of": "acme/public",
        "commits": [{"branch": "fork-work", "message": "first change", "files": {"fork a.txt": "first\n"}},
                    {"branch": "fork-work", "message": "second change", "files": {"fork b.txt": "second\n"},
                     "author": {"name": "Alice", "email": "alice@example.com"}}]
    })).start().await;
    let client = writer(&server);
    let input = json!({"title": "Fork work", "body": "changes", "head": "bob:fork-work", "base": "main", "draft": true});
    let created = checked(
        "pulls/create",
        StatusCode::CREATED,
        &client.post(PULLS, &input).await,
    );
    let pull = get(&client, NUM, "pulls/get").await;
    let first_files = client.get(&format!("{NUM}/files?per_page=1")).await;
    let files = checked("pulls/list-files", StatusCode::OK, &first_files);
    let second = get(
        &client,
        &format!("{NUM}/files?page=2&per_page=1"),
        "pulls/list-files",
    )
    .await;
    let commits = get(
        &client,
        &format!("{NUM}/commits?per_page=1&page=2"),
        "pulls/list-commits",
    )
    .await;
    let sha = pull["head"]["sha"].as_str().unwrap();
    let associated = get(
        &client,
        &format!("repos/acme/public/commits/{sha}/pulls?per_page=100"),
        "repos/list-pull-requests-associated-with-commit",
    )
    .await;
    assert_eq!(created["url"], format!("{}/api/v3/{NUM}", server.base));
    assert_eq!(pull["base"]["repo"]["full_name"], "acme/public");
    assert_eq!(pull["head"]["repo"]["full_name"], "bob/public");
    assert_eq!(pull["draft"], true);
    assert_eq!(pull["commits"], 2);
    assert_eq!(pull["changed_files"], 2);
    assert_eq!(files[0]["filename"], "fork a.txt");
    assert!(files[0]["patch"].as_str().unwrap().contains("+first"));
    assert!(first_files.header("link").contains("rel=\"next\""));
    assert_eq!(second[0]["filename"], "fork b.txt");
    let contents_url = second[0]["contents_url"].as_str().unwrap();
    assert!(contents_url.contains("fork%20b.txt"));
    assert_eq!(commits[0]["sha"], sha);
    assert_eq!(commits[0]["commit"]["message"], "second change");
    assert_eq!(commits[0]["author"]["login"], "alice");
    assert_eq!(associated[0]["number"], 1);
    for invalid in [
        input,
        json!({"title": "Invalid", "head": "absent", "base": "main"}),
    ] {
        assert_eq!(
            client.post(PULLS, &invalid).await.status,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
}

#[tokio::test]
async fn access_checks_precede_all_pull_handlers() {
    // Given an installation token without pull-request access and a private repository outside its selection.
    let server = fixture().entry(seeded("main", false)).start().await;
    let store = &server.server.services().store;
    store
        .update(|tx| {
            for (token, permissions) in [
                (
                    INSTALLATION_TOKEN,
                    BTreeMap::from([("metadata".into(), Access::Read)]),
                ),
                (
                    CONTENTS_TOKEN,
                    BTreeMap::from([("contents".into(), Access::Read)]),
                ),
            ] {
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
        .unwrap();
    let client = GoGithub::new(&server).with_token(INSTALLATION_TOKEN);
    for (method, path, grant) in [
        (Method::GET, "pulls", "pull_requests=read"),
        (Method::POST, "pulls", "pull_requests=write"),
        (Method::GET, "pulls/1", "pull_requests=read; contents=read"),
        (Method::PATCH, "pulls/1", "pull_requests=write"),
        (Method::GET, "pulls/1/files", "pull_requests=read"),
        (Method::GET, "pulls/1/commits", "pull_requests=read"),
        (Method::GET, "commits/feature/pulls", "pull_requests=read"),
        (Method::PUT, "pulls/1/merge", "contents=write"),
    ] {
        let denied = client
            .call(method.clone(), &format!("repos/acme/public/{path}"))
            .await;
        let hidden = client
            .call(method, &format!("repos/acme/private/{path}"))
            .await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN, "{path}");
        assert_eq!(denied.header("x-accepted-github-permissions"), grant);
        assert_eq!(hidden.status, StatusCode::NOT_FOUND, "{path}");
    }
    let contents = GoGithub::new(&server).with_token(CONTENTS_TOKEN);
    let answer = get(&contents, NUM, "pulls/get").await;
    assert_eq!(answer["number"], 1);
    assert_eq!(contents.get(PULLS).await.status, StatusCode::FORBIDDEN);
}

async fn hooks(server: &TestServer, actions: &[&str]) -> Vec<Value> {
    assert_eq!(
        server.admin(Method::POST, "hooks/drain", None).await.status,
        StatusCode::NO_CONTENT
    );
    let mut seen = vec![];
    for delivery in server.receiver.received().await {
        if delivery.header("x-github-event") == "pull_request" {
            let payload = delivery.json();
            let action = payload["action"]
                .as_str()
                .unwrap_or_else(|| panic!("missing action"));
            assert!(delivery.signed_with(fixture::WEBHOOK_SECRET));
            validate::webhook_payload("pull_request", Some(action), &payload)
                .unwrap_or_else(|errors| panic!("{action}: {errors:?}"));
            assert_eq!(payload["installation"]["id"], fixture::INSTALLATION_ID);
            assert_eq!(payload["sender"]["login"], "alice");
            seen.push(payload);
        }
    }
    assert_eq!(seen.len(), actions.len());
    actions
        .iter()
        .map(|action| {
            let Some(i) = seen.iter().position(|p| p["action"] == *action) else {
                panic!("missing {action}")
            };
            seen.swap_remove(i)
        })
        .collect()
}

#[tokio::test]
async fn rest_merge_validates_options_and_preserves_history() {
    // Given go-github v93 github/pulls.go:554, with the SHA guard and merge method.
    let server = fixture()
        .entry(seeded("main", false))
        .register(stand_in_git)
        .start()
        .await;
    let client = writer(&server);
    let path = format!("{NUM}/merge");
    let before = client.get(NUM).await.json();
    // When
    let unsupported = client
        .call_json(Method::PUT, &path, &json!({"merge_method": "squash"}))
        .await;
    let stale = client
        .call_json(Method::PUT, &path, &json!({"sha": before["base"]["sha"]}))
        .await;
    server.clock.advance(120);
    let merged = checked("pulls/merge", StatusCode::OK, &client.call_json(Method::PUT, &path, &json!({
        "sha": before["head"]["sha"], "merge_method": "merge", "commit_title": "Join feature", "commit_message": "A second paragraph"
    })).await);
    // Then
    assert_eq!(unsupported.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(stale.status, StatusCode::CONFLICT);
    assert_eq!(merged["merged"], true);
    let pull = get(&client, NUM, "pulls/get").await;
    assert_eq!(pull["state"], "closed");
    assert_eq!(pull["merged"], true);
    assert_eq!(pull["merged_at"], json!(server.clock.now()));
    assert_eq!(pull["merge_commit_sha"], merged["sha"]);
    assert_eq!(pull["merged_by"]["login"], "alice");
    let reopen = client
        .call_json(Method::PATCH, NUM, &json!({"state": "open"}))
        .await;
    assert_eq!(reopen.status, StatusCode::UNPROCESSABLE_ENTITY);
    let error = &reopen.json()["errors"][0];
    assert_eq!(
        (&error["resource"], &error["field"], &error["code"]),
        (&json!("PullRequest"), &json!("state"), &json!("custom"))
    );
    let graph = Graphql::new(client.clone())
        .query(
            "query($id:ID!) {node(id:$id) {... on PullRequest {state mergedAt mergeCommit {oid}}}}",
            &json!({"id": pull["node_id"]}),
        )
        .await
        .json();
    assert_eq!(
        graph["data"]["node"],
        json!({
            "state": "MERGED", "mergedAt": pull["merged_at"], "mergeCommit": {"oid": merged["sha"]},
        })
    );
    assert_eq!(
        client.get(&format!("{NUM}/commits")).await.json()[0]["sha"],
        before["head"]["sha"]
    );
    let payloads = hooks(&server, &["closed"]).await;
    assert_eq!(payloads[0]["pull_request"]["merged"], true);
    let sha = before["head"]["sha"].as_str().unwrap();
    assert_eq!(
        client
            .get(&format!("repos/acme/public/commits/{sha}/pulls"))
            .await
            .json()[0]["number"],
        1
    );
}

#[tokio::test]
async fn ref_updates_synchronize_and_merge_in_the_same_store_transaction() {
    // Given a pull request with a head on a fork.
    let server = fixture().entry(json!({
        "kind": "repository", "id": 3100, "owner": "bob", "name": "public", "fork_of": "acme/public",
        "commits": [{"branch": "fork-work", "message": "head", "files": {"fork.txt": "head\n"}}]
    })).start().await;
    let client = writer(&server);
    let input = json!({"head":"bob:fork-work", "base":"main", "title":"Fork head"});
    let created = client.post(PULLS, &input).await;
    assert_eq!(created.status, StatusCode::CREATED);
    let created = created.json();
    let old = created["head"]["sha"].as_str().unwrap().parse().unwrap();
    let services = server.server.services();
    server.clock.advance(2);
    let sig = Signature::seed();
    // When
    let new = services
        .store
        .update(|tx| {
            Ok(services.git.commit(
                tx,
                3100,
                fixture::ALICE_ID,
                "fork-work",
                Some(old),
                &[FileChange::Write {
                    path: "other.txt".into(),
                    data: Arc::from(&b"more\n"[..]),
                    mode: EntryMode::File,
                }],
                "update fork",
                &sig,
                &sig,
                false,
            )?)
        })
        .unwrap();
    let open = client.get(NUM).await.json();
    let merge = services
        .store
        .update(|tx| {
            Ok(services.git.merge(
                tx,
                fixture::PUBLIC_REPO_ID,
                fixture::ALICE_ID,
                "main",
                &new.to_string(),
                "merge fork",
            )?)
        })
        .unwrap();
    // Then
    assert_eq!(open["head"]["sha"], json!(new));
    assert_eq!(open["mergeable"], true);
    let closed = client.get(NUM).await.json();
    assert_eq!(closed["base"]["sha"], json!(merge.commit));
    assert_eq!(closed["merged"], true);
    let payloads = hooks(&server, &["opened", "synchronize", "closed"]).await;
    assert_eq!(payloads[1]["before"], json!(old));
    assert_eq!(payloads[1]["after"], json!(new));
    assert_eq!(payloads[1]["pull_request"]["head"]["sha"], json!(new));
}

fn stand_in_repo(r: &mut Registrar<'_>) {
    r.gql.object::<Repo>("Repository");
    r.gql.field("Repository", "nameWithOwner", |cx, obj, _| {
        let repo = obj.get::<Repo>()?;
        let owner = cx
            .tx
            .table::<hubdouble_store::Accounts>()
            .get(repo.owner_id)
            .ok_or_else(|| GqlError::new(ErrorType::NotFound, "No owner"))?;
        Ok(Resolved::Leaf(json!(format!(
            "{}/{}",
            owner.login, repo.name
        ))))
    });
}

fn stand_in_git(r: &mut Registrar<'_>) {
    r.gql.object_resolved::<CommitNode>("Commit");
    r.gql.object_resolved::<RefNode>("Ref");
    r.gql.field("Commit", "oid", |_, obj, _| {
        Ok(Resolved::Leaf(json!(obj.get::<CommitNode>()?.id)))
    });
    r.gql.field("Repository", "ref", resolve_ref);
}

fn resolve_ref(cx: &Ctx<'_>, obj: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let repo_id = obj.get::<Repo>()?.id;
    let name = args
        .get::<String>("qualifiedName")?
        .ok_or_else(ApiError::not_found)?;
    let target = cx
        .git
        .resolve_ref(cx.tx, repo_id, &name)
        .map_err(hubdouble_model::ApiError::from)?;
    Ok(Resolved::Object {
        type_name: "Ref",
        value: Arc::new(RefNode {
            repo_id,
            name,
            prefix: "refs/heads/".to_owned(),
            target,
        }),
    })
}

fn stand_in_ref_target(r: &mut Registrar<'_>) {
    r.gql.field("Ref", "name", |_, obj, _| {
        let r = obj.get::<RefNode>()?;
        Ok(Resolved::Leaf(json!(r.name.strip_prefix(&r.prefix))))
    });
    r.gql.field("Ref", "target", |cx, obj, _| {
        let r = obj.get::<RefNode>()?;
        let object = cx
            .git
            .object(cx.tx, r.repo_id, r.target)
            .map_err(ApiError::from)?;
        let hubdouble_git::ObjectData::Commit(commit) = object.data else {
            return Err(GqlError::new(ErrorType::Internal, "not a commit"));
        };
        Ok(Resolved::Object {
            type_name: "Commit",
            value: Arc::new(CommitNode {
                repo_id: r.repo_id,
                id: r.target,
                commit,
            }),
        })
    });
}

#[tokio::test]
async fn base_ref_follows_the_base_branch_tip_and_is_null_once_the_branch_is_gone() {
    // Given pull request 1 into main and pull request 2 into develop
    let server = fixture()
        .entry(seeded("main", false))
        .entry(seeded("develop", false))
        .register(standins::query_repository)
        .register(stand_in_repo)
        .register(stand_in_git)
        .register(stand_in_ref_target)
        .start()
        .await;
    let services = server.server.services();
    let old = services
        .store
        .view(|tx| {
            Ok(services
                .git
                .resolve_ref(tx, fixture::PUBLIC_REPO_ID, "refs/heads/main")?)
        })
        .unwrap();
    let sig = Signature::seed();
    // When main moves and develop is deleted
    let tip = services
        .store
        .update(|tx| {
            let tip = services.git.commit(
                tx,
                fixture::PUBLIC_REPO_ID,
                fixture::ALICE_ID,
                "main",
                Some(old),
                &[FileChange::Write {
                    path: "later.txt".into(),
                    data: Arc::from(&b"later\n"[..]),
                    mode: EntryMode::File,
                }],
                "move main",
                &sig,
                &sig,
                false,
            )?;
            let develop =
                services
                    .git
                    .resolve_ref(tx, fixture::PUBLIC_REPO_ID, "refs/heads/develop")?;
            services.git.update_ref(
                tx,
                fixture::PUBLIC_REPO_ID,
                fixture::ALICE_ID,
                "refs/heads/develop",
                Some(develop),
                None,
                false,
            )?;
            Ok(tip)
        })
        .unwrap();
    let answer = Graphql::new(writer(&server))
        .query(
            r#"{ repository(owner:"acme", name:"public") {
                a: pullRequest(number:1) { baseRef { name target { oid } } }
                b: pullRequest(number:2) { baseRef { name } } }}"#,
            &json!({}),
        )
        .await
        .json();
    // Then
    assert!(answer.get("errors").is_none(), "{answer}");
    let repo = &answer["data"]["repository"];
    assert_ne!(tip, old);
    assert_eq!(
        repo["a"]["baseRef"],
        json!({"name": "main", "target": {"oid": tip}})
    );
    assert_eq!(repo["b"]["baseRef"], Value::Null);
}

fn approve(server: &TestServer, sha: &str) -> Result<(), ApiError> {
    let now = server.clock.now();
    let store = &server.server.services().store;
    store.update(|tx| {
        let issues = tx.table::<Issues>();
        let issue = issues
            .by_number(fixture::PUBLIC_REPO_ID, 1)
            .ok_or_else(ApiError::not_found)?;
        let issue_id = issue.id;
        let id = tx.next_id();
        tx.table_mut::<Reviews>().insert(Review {
            id,
            node_id: node_id::entity(node_id::REVIEW, id),
            repo_id: fixture::PUBLIC_REPO_ID,
            issue_id,
            user_id: fixture::BOB_ID,
            state: ReviewState::Approved,
            body: String::new(),
            commit_id: Some(sha.to_owned()),
            submitted_at: Some(now),
            created_at: now,
            last_edited_at: None,
        })?;
        Ok(())
    })
}

#[tokio::test]
async fn graphql_links_nodes_refs_commits_reviews_and_draft_transitions() {
    // Given an open draft and a closed pull request with the same head and a different base.
    let server = fixture()
        .entry(seeded("main", true))
        .entry(seeded("develop", false))
        .register(standins::query_repository)
        .register(stand_in_repo)
        .register(stand_in_git)
        .start()
        .await;
    let client = writer(&server);
    let draft = client.get(NUM).await.json();
    let id = json!({"id": draft["node_id"]});
    let graph = Graphql::new(client.clone());
    let other = Graphql::new(GoGithub::new(&server).with_token(fixture::BOB_TOKEN));
    // When
    let denied = other.query(READY, &id).await.json();
    assert_eq!(denied["errors"][0]["type"], "FORBIDDEN");
    assert_eq!(client.get(NUM).await.json()["draft"], true);
    let found = graph.query(PULL_OF_NODE, &id).await.json();
    let expected_node = json!({"number":1,"repository":{"nameWithOwner":"acme/public"}});
    assert_eq!(found["data"]["node"], expected_node);
    let ready = graph.query(READY, &id).await.json();
    assert_eq!(
        ready["data"]["markPullRequestReadyForReview"]["pullRequest"]["isDraft"],
        false
    );
    let again = graph.query("mutation($id: ID!) {convertPullRequestToDraft(input: {pullRequestId: $id}) {pullRequest {isDraft}}}", &id).await.json();
    assert!(again.get("errors").is_none(), "{again}");
    let second = format!("{PULLS}/2");
    edit(&client, &second, json!({"state":"closed"})).await;
    approve(&server, draft["head"]["sha"].as_str().unwrap()).unwrap();
    let queried = graph.query(r#"{ repository(owner:"acme", name:"public") {
        pullRequest(number:1) { state mergeable baseRefName baseRefOid headRefOid headRefName
            baseRepository {nameWithOwner} headRepository {nameWithOwner}
            title body url updatedAt mergedAt mergeCommit {oid} reviewDecision commits(last:1) {nodes {commit {oid}}} }
        all: ref(qualifiedName:"refs/heads/feature") { associatedPullRequests(first:2) {nodes {number reviewDecision}} }
        open: ref(qualifiedName:"refs/heads/feature") { associatedPullRequests(first:1, states:[OPEN], orderBy:{field:UPDATED_AT,direction:DESC}) {nodes {number url}} }
    }}"#, &json!({})).await.json();
    // Then
    assert!(queried.get("errors").is_none(), "{queried}");
    let repo = &queried["data"]["repository"];
    let pull = &repo["pullRequest"];
    let expected = json!({"state": "OPEN", "mergeable": "MERGEABLE", "baseRefName": "main",
        "baseRefOid": draft["base"]["sha"], "headRefOid": draft["head"]["sha"], "headRefName": "feature",
        "baseRepository": {"nameWithOwner": "acme/public"}, "headRepository": {"nameWithOwner": "acme/public"},
        "title": "Feature against main", "body": "original", "url": draft["html_url"],
        "updatedAt": draft["updated_at"], "mergedAt": null, "mergeCommit": null, "reviewDecision": null,
        "commits": {"nodes": [{"commit": {"oid": draft["head"]["sha"]}}]}});
    assert_eq!(*pull, expected);
    assert_eq!(
        repo["open"]["associatedPullRequests"]["nodes"][0]["number"],
        1
    );
    assert_eq!(
        repo["all"]["associatedPullRequests"]["nodes"][0]["reviewDecision"],
        Value::Null
    );
    edit(
        &client,
        &second,
        json!({"state":"open", "body": "reopened"}),
    )
    .await;
    let payloads = hooks(
        &server,
        &[
            "ready_for_review",
            "converted_to_draft",
            "closed",
            "edited",
            "reopened",
        ],
    )
    .await;
    assert_eq!(payloads[3]["changes"]["body"]["from"], "original");
}

#[tokio::test]
async fn a_merge_into_the_default_branch_closes_the_issues_the_pull_request_links() {
    // Given PR 1 into main and PR 2 into develop, issue 3 named by PR 1's closing keyword, issue 4 linked to PR 1 by hand, and issue 5 named by PR 2's closing keyword
    let server = TestServer::builder()
        .fixture()
        .frame(hubdouble_api::register_all)
        .entry(seeded("main", false))
        .entry(seeded("develop", false))
        .start()
        .await;
    let client = writer(&server);
    for title in ["keyword", "by hand", "other branch"] {
        let opened = client
            .post("repos/acme/public/issues", &json!({"title": title}))
            .await;
        checked("issues/create", StatusCode::CREATED, &opened);
    }
    edit(&client, NUM, json!({"body": "Fixes #3"})).await;
    edit(
        &client,
        "repos/acme/public/pulls/2",
        json!({"body": "Fixes #5"}),
    )
    .await;
    let link = json!({"repository": "acme/public", "issue_number": 4, "pull_number": 1});
    let linked = server.admin(Method::POST, "issue_link", Some(&link)).await;
    assert_eq!(linked.status, StatusCode::CREATED);
    server.admin(Method::POST, "hooks/drain", None).await;
    server.receiver.received().await;
    server.clock.advance(60);
    // When both pull requests merge
    for number in [2, 1] {
        let path = format!("{PULLS}/{number}/merge");
        let merged = client
            .call_json(Method::PUT, &path, &json!({"merge_method": "merge"}))
            .await;
        checked("pulls/merge", StatusCode::OK, &merged);
    }
    // Then the issues PR 1 links are closed as completed by the merger, and issue 5 stays open
    for number in [3, 4] {
        let issue = get(
            &client,
            &format!("repos/acme/public/issues/{number}"),
            "issues/get",
        )
        .await;
        assert_eq!(issue["state"], "closed");
        assert_eq!(issue["state_reason"], "completed");
        assert_eq!(issue["closed_at"], json!(server.clock.now()));
        assert_eq!(issue["closed_by"]["login"], "alice");
    }
    let other = get(&client, "repos/acme/public/issues/5", "issues/get").await;
    assert_eq!(other["state"], "open");
    // Then each closed issue sends issues.closed
    assert_eq!(
        server.admin(Method::POST, "hooks/drain", None).await.status,
        StatusCode::NO_CONTENT
    );
    let mut closed: Vec<_> = server
        .receiver
        .received()
        .await
        .into_iter()
        .filter(|d| d.header("x-github-event") == "issues")
        .map(|d| {
            let payload = d.json();
            validate::webhook_payload("issues", Some("closed"), &payload)
                .unwrap_or_else(|errors| panic!("issues.closed: {errors:?}"));
            (
                payload["action"].clone(),
                payload["issue"]["number"].clone(),
            )
        })
        .collect();
    closed.sort_by_key(|(_, number)| number.as_i64());
    assert_eq!(
        closed,
        [(json!("closed"), json!(3)), (json!("closed"), json!(4))]
    );
}

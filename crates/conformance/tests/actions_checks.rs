#![expect(
    clippy::unwrap_used,
    reason = "tests fail immediately when an answer and its contract differ"
)]

use std::collections::BTreeSet;
use std::sync::Arc;

use http::{Method, StatusCode};
use hubdouble_admin::Registrar;
use hubdouble_git::{
    Commit, CommitNode, EntryMode, FileChange, ObjectData, ObjectKind, Signature, Tag,
};
use hubdouble_model::{
    Access, ApiError, Config, Issue, IssueState, ManualClock, Pull, PullRef, Repo, Timestamp,
    Token, TokenKind, node_id,
};
use hubdouble_server::{Registries, Server, ServerError};
use hubdouble_store::{Apps, Counters, Issues, Sequence, Tokens};
use hubdouble_testkit::fixture::{
    self, ALICE_ID, APP_ID, APP_PRIVATE_KEY, INSTALLATION_ID, PUBLIC_REPO_ID,
};
use hubdouble_testkit::{Answer, Delivery, GoGithub, Graphql, TestServer, standins, validate};
use serde_json::{Value, json};

const REPO: &str = "repos/acme/public";
const TOKEN: &str = "ghs_actions_checks_fixture_app";
const OTHER_TOKEN: &str = "ghs_actions_checks_other_app";
const OTHER_APP_ID: i64 = 4002;
const OTHER_INSTALLATION_ID: i64 = 5002;
const ACTIONS_APP_ID: i64 = 15368;
const DAY: i64 = 24 * 60 * 60;
const RERUN_DOCS: &str = "https://docs.github.com/rest/actions/workflow-runs#re-run-a-workflow";
const ROLLUP: &str = "query($owner: String!, $name: String!, $oid: GitObjectID!) {
  repository(owner: $owner, name: $name) {
    object(oid: $oid) { ... on Commit { statusCheckRollup { state } } }
  }
}";

fn parents(r: &mut Registrar<'_>) {
    r.gql.object::<Repo>("Repository");
    r.gql.object_resolved::<CommitNode>("Commit");
}

fn token(value: &str, (app, installation): (i64, i64)) -> Token {
    Token {
        id: 0,
        token: value.to_owned(),
        kind: TokenKind::Installation,
        app_id: Some(app),
        installation_id: Some(installation),
        user_id: None,
        repo_ids: Some(vec![PUBLIC_REPO_ID]),
        permissions: [
            ("actions", Access::Write),
            ("checks", Access::Write),
            ("metadata", Access::Read),
        ]
        .into_iter()
        .map(|(name, access)| (name.to_owned(), access))
        .collect(),
        scopes: Vec::new(),
        expires_at: None,
        revoked: false,
        owner_seeded: true,
        created_at: Timestamp::SEED,
    }
}

async fn start(auto_complete: &str) -> TestServer {
    start_with(auto_complete, []).await
}

async fn start_with(auto_complete: &str, extra: impl IntoIterator<Item = Value>) -> TestServer {
    let mut builder = TestServer::builder()
        .register(hubdouble_api_checks::register)
        .register(hubdouble_api_actions::register)
        .register(hubdouble_api_users::register)
        .register(standins::query_repository)
        .register(standins::repository_object)
        .register(parents);
    let grant = json!({"actions": "write", "checks": "write", "contents": "read",
        "metadata": "read"});
    for mut entry in fixture::seed() {
        match entry["kind"].as_str() {
            Some("repository") => {
                entry["files"][".github/workflows/chain.yml"] = json!(concat!(
                    "name: Chain\non: workflow_dispatch\njobs:\n  build: {}\n  lint: {}\n",
                    "  test: {needs: build}\n  deploy: {needs: [lint, test]}\n",
                ));
            }
            Some("app") => {
                entry["permissions"] = grant.clone();
                entry["events"] = json!(["check_run", "check_suite", "workflow_run"]);
            }
            Some("installation") => entry["permissions"] = grant.clone(),
            _ => {}
        }
        builder = builder.entry(entry);
    }
    for entry in extra {
        builder = builder.entry(entry);
    }
    let server = builder
        .entry(
            json!({"kind": "app", "id": OTHER_APP_ID, "slug": "other-checks",
            "name": "Other Checks", "owner": fixture::ORG, "public_key": fixture::APP_PUBLIC_KEY,
            "permissions": {"checks": "write", "actions": "write", "metadata": "read"},
            "events": []}),
        )
        .entry(
            json!({"kind": "installation", "id": OTHER_INSTALLATION_ID, "app": "other-checks",
            "account": fixture::ORG, "repositories": [fixture::PUBLIC_REPO],
            "permissions": {"checks": "write", "actions": "write", "metadata": "read"}}),
        )
        .entry(json!({"kind": "actions", "auto_complete": auto_complete,
            "oidc_signing_key": APP_PRIVATE_KEY}))
        .start()
        .await;
    server
        .server
        .services()
        .store
        .update_quiet(|tx| {
            for (value, owner) in [
                (TOKEN, (APP_ID, INSTALLATION_ID)),
                (OTHER_TOKEN, (OTHER_APP_ID, OTHER_INSTALLATION_ID)),
            ] {
                let row = Token {
                    id: tx.next_id(),
                    ..token(value, owner)
                };
                tx.table_mut::<Tokens>().insert(row)?;
            }
            Ok(())
        })
        .unwrap();
    server
}

fn app(server: &TestServer) -> GoGithub {
    GoGithub::enterprise(server).with_token(TOKEN)
}

fn ok(answer: &Answer, status: StatusCode) -> Value {
    assert_eq!(answer.status, status, "{:?}", answer.json());
    answer.json()
}

fn push_feature(server: &TestServer) -> (String, String) {
    let services = server.server.services();
    services
        .store
        .update(|tx| {
            let git = &services.git;
            let old = git.resolve_ref(tx, PUBLIC_REPO_ID, "refs/heads/feature")?;
            let signature = Signature::seed();
            let new = git.commit(
                tx,
                PUBLIC_REPO_ID,
                ALICE_ID,
                "feature",
                Some(old),
                &[FileChange::Write {
                    path: "src/feature.txt".to_owned(),
                    data: b"changed\n".as_slice().into(),
                    mode: EntryMode::File,
                }],
                "Change the feature",
                &signature,
                &signature,
                false,
            )?;
            Ok::<_, ApiError>((old.to_string(), new.to_string()))
        })
        .unwrap()
}

fn tag_new_commit(server: &TestServer, tag: &str) -> String {
    tag_new_commit_as(server, tag, false)
}

fn tag_new_commit_as(server: &TestServer, tag: &str, annotated: bool) -> String {
    let services = server.server.services();
    services
        .store
        .update(|tx| {
            let git = &services.git;
            let parent = git.resolve_ref(tx, PUBLIC_REPO_ID, "refs/heads/feature")?;
            let ObjectData::Commit(commit) = git.object(tx, PUBLIC_REPO_ID, parent)?.data else {
                return Err(ApiError::not_found());
            };
            let head = git.write_commit(
                tx,
                PUBLIC_REPO_ID,
                &Commit {
                    tree: commit.tree,
                    parents: vec![parent],
                    author: Signature::seed(),
                    committer: Signature::seed(),
                    message: "Release".to_owned(),
                    gpg_signature: None,
                },
            )?;
            let target = if annotated {
                git.write_tag(
                    tx,
                    PUBLIC_REPO_ID,
                    &Tag {
                        target: head,
                        target_kind: ObjectKind::Commit,
                        name: tag.trim_start_matches("refs/tags/").to_owned(),
                        tagger: Some(Signature::seed()),
                        message: "Release\n".to_owned(),
                        gpg_signature: None,
                    },
                )?
            } else {
                head
            };
            git.update_ref(tx, PUBLIC_REPO_ID, ALICE_ID, tag, None, Some(target), false)?;
            Ok(head.to_string())
        })
        .unwrap()
}

fn move_branch(server: &TestServer, branch: &str, sha: &str) {
    let services = server.server.services();
    services
        .store
        .update(|tx| {
            let git = &services.git;
            let name = format!("refs/heads/{branch}");
            let old = git.resolve_ref(tx, PUBLIC_REPO_ID, &name)?;
            let new = sha.parse().map_err(|_| ApiError::not_found())?;
            git.update_ref(
                tx,
                PUBLIC_REPO_ID,
                ALICE_ID,
                &name,
                Some(old),
                Some(new),
                true,
            )?;
            Ok::<_, ApiError>(())
        })
        .unwrap();
}

async fn suite_of_run(client: &GoGithub, sha: &str, run_id: i64) -> Value {
    let suite = run(client, run_id).await["check_suite_id"].clone();
    let path = format!("{REPO}/commits/{sha}/check-suites?app_id={ACTIONS_APP_ID}&per_page=100");
    ok(&client.get(&path).await, StatusCode::OK)["check_suites"]
        .as_array()
        .unwrap()
        .iter()
        .find(|listed| listed["id"] == suite)
        .unwrap()
        .clone()
}

fn open_pull_request(server: &TestServer, sha: &str) -> i64 {
    server
        .server
        .services()
        .store
        .update(|tx| {
            let id = tx.next_id();
            let pull_id = tx.next_id();
            let number = tx.table_mut::<Counters>().next_number(PUBLIC_REPO_ID);
            let side = |name: &str| PullRef {
                repo_id: Some(PUBLIC_REPO_ID),
                ref_name: name.to_owned(),
                sha: sha.to_owned(),
            };
            tx.table_mut::<Issues>().insert(Issue {
                id,
                node_id: node_id::entity(node_id::ISSUE, id),
                repo_id: PUBLIC_REPO_ID,
                number,
                title: "feature".to_owned(),
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
                pull: Some(Pull {
                    id: pull_id,
                    node_id: node_id::entity(node_id::PULL_REQUEST, pull_id),
                    head: side("feature"),
                    base: side("main"),
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
        })
        .unwrap()
}

async fn dispatch(client: &GoGithub, git_ref: &str) -> i64 {
    let answer = client
        .post(
            &format!("{REPO}/actions/workflows/chain.yml/dispatches"),
            &json!({"ref": git_ref, "return_run_details": true}),
        )
        .await;
    ok(&answer, StatusCode::OK)["workflow_run_id"]
        .as_i64()
        .unwrap()
}

async fn run(client: &GoGithub, id: i64) -> Value {
    ok(
        &client.get(&format!("{REPO}/actions/runs/{id}")).await,
        StatusCode::OK,
    )
}

async fn jobs(client: &GoGithub, run: i64, filter: &str) -> Vec<Value> {
    let path = format!("{REPO}/actions/runs/{run}/jobs?filter={filter}&per_page=100");
    ok(&client.get(&path).await, StatusCode::OK)["jobs"]
        .as_array()
        .unwrap()
        .clone()
}

fn named<'j>(jobs: &'j [Value], name: &str) -> &'j Value {
    jobs.iter().find(|job| job["name"] == name).unwrap()
}

async fn finish(server: &TestServer, client: &GoGithub, run: i64, conclusion: &str) {
    let jobs: Vec<_> = jobs(client, run, "latest")
        .await
        .into_iter()
        .filter(|job| job["status"] != "completed")
        .map(|job| {
            let conclusion = match job["name"].as_str().unwrap() {
                "build" => conclusion,
                "lint" => "success",
                _ => "skipped",
            };
            json!({"id": job["id"], "conclusion": conclusion})
        })
        .collect();
    let body = json!({"conclusion": conclusion, "jobs": jobs});
    let answer = server
        .admin(Method::POST, &format!("actions/runs/{run}"), Some(&body))
        .await;
    ok(&answer, StatusCode::OK);
}

async fn check_runs(client: &GoGithub, sha: &str, query: &str) -> Value {
    let path = format!("{REPO}/commits/{sha}/check-runs?{query}");
    ok(&client.get(&path).await, StatusCode::OK)
}

async fn actions_suite(client: &GoGithub, sha: &str) -> Value {
    let path = format!("{REPO}/commits/{sha}/check-suites?app_id={ACTIONS_APP_ID}");
    let listed = ok(&client.get(&path).await, StatusCode::OK);
    assert_eq!(listed["total_count"], 1);
    listed["check_suites"][0].clone()
}

async fn rerun(client: &GoGithub, path: &str) -> Answer {
    client.call(Method::POST, &format!("{REPO}/{path}")).await
}

async fn drain(server: &TestServer) -> Vec<Delivery> {
    let drained = server.admin(Method::POST, "hooks/drain", None).await;
    assert_eq!(drained.status, StatusCode::NO_CONTENT);
    server.receiver.received().await
}

fn events(all: &[Delivery], event: &str, action: &str) -> Vec<Value> {
    all.iter()
        .filter(|d| d.header("x-github-event") == event)
        .map(Delivery::json)
        .filter(|payload| payload["action"] == action)
        .inspect(|payload| {
            validate::webhook_payload(event, Some(action), payload)
                .unwrap_or_else(|e| panic!("{event}.{action}: {e:?}"));
        })
        .collect()
}

#[tokio::test]
async fn a_workflow_job_is_its_check_run_in_the_github_actions_suite_of_its_run() {
    // Given a pushed head with an open pull request and a run on it whose build job failed with one annotation, as hashicorp/terraform run 44826626663 shows
    let server = start("none").await;
    let client = app(&server);
    let (before, after) = push_feature(&server);
    let number = open_pull_request(&server, &after);
    let id = dispatch(&client, "feature").await;
    let suite = run(&client, id).await["check_suite_id"].as_i64().unwrap();
    let annotation = json!({"path": "src/feature.txt", "start_line": 1, "end_line": 1,
        "annotation_level": "failure", "message": "the build broke"});
    let build = named(&jobs(&client, id, "latest").await, "build")["id"].clone();
    let body = json!({"conclusion": "failure", "jobs": [
        {"id": build, "conclusion": "failure", "annotations": [annotation]}]});
    let answer = server
        .admin(Method::POST, &format!("actions/runs/{id}"), Some(&body))
        .await;
    ok(&answer, StatusCode::OK);
    let job = named(&jobs(&client, id, "latest").await, "build").clone();

    // When the job is read as a check run
    let check = ok(
        &client.get(&format!("{REPO}/check-runs/{build}")).await,
        StatusCode::OK,
    );

    // Then it has the job's identity, state and links, and GitHub Actions as its App
    assert_eq!(check["id"], job["id"]);
    assert_eq!(check["node_id"], job["node_id"]);
    assert_eq!(check["url"], job["check_run_url"]);
    assert_eq!(check["name"], "build");
    assert_eq!(check["head_sha"], after);
    assert_eq!(check["status"], "completed");
    assert_eq!(check["conclusion"], "failure");
    assert_eq!(check["started_at"], job["started_at"]);
    assert_eq!(check["completed_at"], job["completed_at"]);
    assert_eq!(check["details_url"], job["html_url"]);
    assert_eq!(check["html_url"], job["html_url"]);
    let external = check["external_id"].as_str().unwrap();
    assert_eq!((external.len(), external.matches('-').count()), (36, 4));
    assert_eq!(check["output"]["title"], Value::Null);
    assert_eq!(check["output"]["summary"], Value::Null);
    assert_eq!(check["output"]["text"], Value::Null);
    assert_eq!(check["output"]["annotations_count"], 1);
    assert_eq!(check["check_suite"], json!({"id": suite}));
    assert_eq!(check["app"]["id"], ACTIONS_APP_ID);
    assert_eq!(check["app"]["slug"], "github-actions");
    assert_eq!(check["app"]["name"], "GitHub Actions");
    assert_eq!(check["app"]["owner"]["login"], "github");
    assert_eq!(check["app"]["owner"]["id"], 9919);
    assert_eq!(check["pull_requests"][0]["number"], number);
    // And its annotation is listed
    let listed = ok(
        &client
            .get(&format!("{REPO}/check-runs/{build}/annotations"))
            .await,
        StatusCode::OK,
    );
    assert_eq!(listed[0]["path"], "src/feature.txt");
    assert_eq!(listed[0]["annotation_level"], "failure");
    // And the run's check suite is GitHub Actions' suite on the pushed head
    let suite_view = actions_suite(&client, &after).await;
    assert_eq!(suite_view["id"], suite);
    assert_eq!(suite_view["app"]["slug"], "github-actions");
    assert_eq!(suite_view["head_branch"], "feature");
    assert_eq!(suite_view["head_sha"], after);
    assert_eq!(suite_view["before"], before);
    assert_eq!(suite_view["after"], after);
    assert_eq!(suite_view["status"], "completed");
    assert_eq!(suite_view["conclusion"], "failure");
    assert_eq!(suite_view["latest_check_runs_count"], 4);
    assert_eq!(suite_view["rerequestable"], true);
    assert_eq!(suite_view["runs_rerequestable"], false);
    assert_eq!(suite_view["pull_requests"][0]["number"], number);
    assert_eq!(
        suite_view["check_runs_url"],
        format!(
            "{}/check-suites/{suite}/check-runs",
            suite_view["repository"]["url"].as_str().unwrap()
        )
    );
    // And the check name finds that suite among every App's suites on the head
    let by_name = ok(
        &client
            .get(&format!(
                "{REPO}/commits/{after}/check-suites?check_name=build"
            ))
            .await,
        StatusCode::OK,
    );
    assert_eq!(by_name["total_count"], 1);
    assert_eq!(by_name["check_suites"][0]["id"], suite);
}

#[tokio::test]
async fn check_runs_for_a_ref_count_attempts_as_github_filters_them() {
    // Given a run of four jobs whose failed build was re-run with the jobs that need it, which gives attempt 2 four new check runs in the same suite
    let server = start("none").await;
    let client = app(&server);
    let id = dispatch(&client, "main").await;
    let first = run(&client, id).await;
    let sha = first["head_sha"].as_str().unwrap().to_owned();
    finish(&server, &client, id, "failure").await;
    ok(
        &rerun(&client, &format!("actions/runs/{id}/rerun-failed-jobs")).await,
        StatusCode::CREATED,
    );
    assert_eq!(
        run(&client, id).await["check_suite_id"],
        first["check_suite_id"]
    );

    // When the head's check runs are listed with the client's filters
    let all = check_runs(&client, &sha, "filter=all&per_page=100").await;
    let latest = check_runs(&client, &sha, "filter=latest&per_page=100").await;
    let default = check_runs(&client, &sha, "per_page=100").await;
    let build = check_runs(&client, &sha, "check_name=build&filter=all").await;
    let queued = check_runs(&client, &sha, "status=queued").await;
    let actions = check_runs(
        &client,
        &sha,
        &format!("app_id={ACTIONS_APP_ID}&filter=all"),
    )
    .await;
    let other = check_runs(&client, &sha, &format!("app_id={APP_ID}&filter=all")).await;
    let paged = check_runs(&client, &sha, "filter=all&per_page=3&page=3").await;

    // Then every attempt counts with filter=all and only the newest per name otherwise
    assert_eq!(all["total_count"], 8);
    assert_eq!(latest["total_count"], 4);
    assert_eq!(default["total_count"], 4);
    let newest: BTreeSet<_> = jobs(&client, id, "latest")
        .await
        .iter()
        .map(|job| job["id"].as_i64().unwrap())
        .collect();
    let listed: BTreeSet<_> = latest["check_runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|check| check["id"].as_i64().unwrap())
        .collect();
    assert_eq!(listed, newest);
    // And the name, status and App filters and paging apply to the Actions check runs
    assert_eq!(build["total_count"], 2);
    assert!(build["check_runs"].as_array().unwrap().iter().all(|check| {
        check["name"] == "build" && check["check_suite"]["id"] == first["check_suite_id"]
    }));
    assert_eq!(queued["total_count"], 3);
    assert_eq!(actions["total_count"], 8);
    assert_eq!(other["total_count"], 0);
    assert_eq!(paged["total_count"], 8);
    assert_eq!(paged["check_runs"].as_array().unwrap().len(), 2);
    // And the suite follows the queued attempt
    let suite = actions_suite(&client, &sha).await;
    assert_eq!(
        (suite["status"].clone(), suite["conclusion"].clone()),
        (json!("queued"), Value::Null)
    );
    assert_eq!(suite["latest_check_runs_count"], 4);
}

#[tokio::test]
async fn every_suite_on_a_head_that_a_new_tag_brought_in_has_zeros_as_before() {
    // Given a commit that a new tag brought in, and the feature branch moved to it later, as every suite on goreleaser/goreleaser commit a6edafa, tag v2.20.3, shows
    let server = start("none").await;
    let client = app(&server);
    let head = tag_new_commit(&server, "refs/tags/v9.9.9");
    let on_tag = dispatch(&client, "v9.9.9").await;
    move_branch(&server, "feature", &head);
    let on_branch = dispatch(&client, "feature").await;

    for id in [on_tag, on_branch] {
        // When the suite of each run is read
        let suite = suite_of_run(&client, &head, id).await;

        // Then it has zeros as before and the head as after
        assert_eq!(suite["before"], "0000000000000000000000000000000000000000");
        assert_eq!(suite["after"], head);
    }
}

#[tokio::test]
async fn a_suite_on_a_commit_an_annotated_tag_brought_in_has_zeros_and_the_commit() {
    // Given a commit that a new annotated tag object brought in, and the feature branch moved to it later, as goreleaser/goreleaser tag v2.20.3 (tag object 2f30744) and suite 120915285440 show
    let server = start("none").await;
    let client = app(&server);
    let head = tag_new_commit_as(&server, "refs/tags/v9.9.9", true);
    move_branch(&server, "feature", &head);

    // When a run on the branch opens its suite
    let id = dispatch(&client, "feature").await;
    let suite = suite_of_run(&client, &head, id).await;

    // Then the suite has zeros as before and the commit as after
    assert_eq!(suite["head_sha"], head);
    assert_eq!(suite["before"], "0000000000000000000000000000000000000000");
    assert_eq!(suite["after"], head);
}

#[tokio::test]
async fn a_ref_outside_branches_and_tags_is_not_the_first_push_of_a_commit() {
    // Given a new commit that a ref outside refs/heads and refs/tags points at first, and the feature branch moved to it later
    let server = start("none").await;
    let client = app(&server);
    let services = server.server.services();
    let before = services
        .store
        .view(|tx| {
            Ok(services
                .git
                .resolve_ref(tx, PUBLIC_REPO_ID, "refs/heads/feature")?)
        })
        .unwrap()
        .to_string();
    let head = tag_new_commit(&server, "refs/notes/release");
    move_branch(&server, "feature", &head);

    // When a run on the branch opens its suite
    let id = dispatch(&client, "feature").await;
    let suite = suite_of_run(&client, &head, id).await;

    // Then the suite has the branch move as its push
    assert_eq!(suite["before"], before);
    assert_eq!(suite["after"], head);
}

#[tokio::test]
async fn a_suite_takes_before_from_the_first_push_of_its_head_after_that_branch_moved() {
    // Given a head first pushed onto feature, feature moved on, and main pushed to the head later, as the suites on cli/cli commit 233b470 made days after its push show
    let server = start("none").await;
    let client = app(&server);
    let (before, head) = push_feature(&server);
    push_feature(&server);
    move_branch(&server, "main", &head);

    // When a run on main opens its suite
    let id = dispatch(&client, "main").await;
    let suite = suite_of_run(&client, &head, id).await;

    // Then the suite has the before and after of the head's first push
    assert_eq!(suite["head_branch"], "main");
    assert_eq!(suite["before"], before);
    assert_eq!(suite["after"], head);
}

#[tokio::test]
async fn another_app_cannot_update_an_actions_check_run() {
    // Given a queued job and two Apps with checks write on the repository
    let server = start("none").await;
    let id = dispatch(&app(&server), "main").await;
    let job = jobs(&app(&server), id, "latest").await[0]["id"].clone();
    let path = format!("{REPO}/check-runs/{job}");

    for token in [TOKEN, OTHER_TOKEN] {
        // When the App patches the job's check run
        let client = GoGithub::enterprise(&server).with_token(token);
        let answer = client
            .call_json(
                Method::PATCH,
                &path,
                &json!({"status": "completed", "conclusion": "success"}),
            )
            .await;

        // Then GitHub's refusal of a check run another App created comes back
        assert_eq!(answer.status, StatusCode::FORBIDDEN);
        let body = answer.json();
        assert_eq!(body["status"], "403");
        assert_eq!(
            body["documentation_url"],
            "https://docs.github.com/rest/checks/runs#update-a-check-run"
        );
    }
    // And the check run and its job stay queued
    let check = ok(&app(&server).get(&path).await, StatusCode::OK);
    assert_eq!(check["status"], "queued");
    assert_eq!(
        jobs(&app(&server), id, "latest").await[0]["status"],
        "queued"
    );
}

fn seed_error(app: &Value) -> ApiError {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("seed.json");
    let seed = json!([{"kind": "org", "login": fixture::ORG}, app]);
    std::fs::write(&path, seed.to_string()).unwrap();
    let config = Config {
        seed: Some(path),
        ..Config::default()
    };
    let clock = Arc::new(ManualClock::new(Timestamp::SEED));
    let registries = Registries::new(hubdouble_admin::register);
    match Server::start(config, clock, &|_| None, registries) {
        Err(ServerError::Seed(error)) => error,
        Err(other) => panic!("{other}"),
        Ok(_) => panic!("the seed loaded"),
    }
}

#[tokio::test]
async fn no_local_app_takes_the_id_or_slug_of_github_actions() {
    // Given Apps that claim the GitHub Actions App's ID or slug
    let by_id = json!({"kind": "app", "id": ACTIONS_APP_ID, "slug": "impostor",
        "owner": fixture::ORG, "public_key": fixture::APP_PUBLIC_KEY});
    let by_slug = json!({"kind": "app", "slug": "github-actions", "owner": fixture::ORG,
        "public_key": fixture::APP_PUBLIC_KEY});

    for claim in [&by_id, &by_slug] {
        // When a seed holds one
        let error = seed_error(claim);

        // Then the seed does not load
        assert_eq!(error.status, 422);
    }

    // Given a running server whose ID sequence hands out the GitHub Actions App's ID next
    let server = start("none").await;
    server
        .server
        .services()
        .store
        .update_quiet(|tx| {
            tx.table_mut::<Counters>()
                .bump(Sequence::Id, ACTIONS_APP_ID - 1);
            Ok::<_, ApiError>(())
        })
        .unwrap();

    // When the admin API creates an App with that slug, and one with no ID
    let mut admin_slug = by_slug.clone();
    admin_slug.as_object_mut().unwrap().remove("kind");
    let refused = server.admin(Method::POST, "app", Some(&admin_slug)).await;
    let fresh = json!({"slug": "fresh", "owner": fixture::ORG,
        "public_key": fixture::APP_PUBLIC_KEY});
    let made = server.admin(Method::POST, "app", Some(&fresh)).await;

    // Then the slug is refused and the new App gets another ID
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(made.status, StatusCode::CREATED);
    let id = server
        .server
        .services()
        .store
        .view(|tx| Ok(tx.table::<Apps>().by_slug("fresh").unwrap().id))
        .unwrap();
    assert_ne!(id, ACTIONS_APP_ID);
}

#[tokio::test]
async fn status_check_rollup_follows_the_jobs_of_a_run() {
    // Given a run on a pushed head that has no other check runs or statuses
    let server = start("none").await;
    let client = app(&server);
    let (_, head) = push_feature(&server);
    let graphql = Graphql::new(app(&server));
    let rollup = || async {
        let answer = graphql
            .query(
                ROLLUP,
                &json!({"owner": "acme", "name": "public", "oid": head}),
            )
            .await;
        let body = ok(&answer, StatusCode::OK);
        assert_eq!(body["errors"], Value::Null, "{body}");
        body["data"]["repository"]["object"]["statusCheckRollup"]["state"].clone()
    };
    let id = dispatch(&client, "feature").await;

    // When the run is queued and then fails
    let queued = rollup().await;
    finish(&server, &client, id, "failure").await;
    let failed = rollup().await;

    // Then the rollup is pending while its jobs run and fails with the failed job
    assert_eq!(queued, "PENDING");
    assert_eq!(failed, "FAILURE");
}

#[tokio::test]
async fn a_seeded_auto_complete_policy_completes_the_check_runs_of_a_run() {
    // Given the seed's Actions policy that completes every run with success
    let server = start("success").await;
    let client = app(&server);

    // When a run is dispatched
    let id = dispatch(&client, "main").await;
    let sha = run(&client, id).await["head_sha"]
        .as_str()
        .unwrap()
        .to_owned();

    // Then each job's check run and the run's suite are completed with success
    let checks = check_runs(&client, &sha, "filter=all").await;
    assert_eq!(checks["total_count"], 4);
    assert!(
        checks["check_runs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|check| { check["status"] == "completed" && check["conclusion"] == "success" })
    );
    let suite = actions_suite(&client, &sha).await;
    assert_eq!(
        (suite["status"].clone(), suite["conclusion"].clone()),
        (json!("completed"), json!("success"))
    );
}

#[tokio::test]
async fn apps_get_created_and_completed_events_for_actions_checks() {
    // Given an App subscribed to check_run and check_suite on the repository
    let server = start("none").await;
    let client = app(&server);
    let id = dispatch(&client, "main").await;
    let suite = run(&client, id).await["check_suite_id"].clone();
    let queued = drain(&server).await;

    // When the run's jobs complete
    finish(&server, &client, id, "failure").await;
    let done = drain(&server).await;

    // Then each new job's check run was announced, and its completion and the suite's sent
    let created = events(&queued, "check_run", "created");
    assert_eq!(created.len(), 4);
    for payload in &created {
        assert_eq!(payload["check_run"]["app"]["id"], ACTIONS_APP_ID);
        assert_eq!(payload["check_run"]["check_suite"]["id"], suite);
        assert_eq!(payload["check_run"]["status"], "queued");
    }
    let completed = events(&done, "check_run", "completed");
    assert_eq!(completed.len(), 4);
    let suites = events(&done, "check_suite", "completed");
    assert_eq!(suites.len(), 1);
    assert_eq!(suites[0]["check_suite"]["id"], suite);
    assert_eq!(suites[0]["check_suite"]["conclusion"], "failure");
    assert_eq!(suites[0]["check_suite"]["app"]["slug"], "github-actions");
    // And nothing meant for the suite's own App reached this App
    assert!(!queued.iter().chain(&done).any(|delivery| {
        let event = delivery.header("x-github-event");
        let action = delivery.json()["action"].clone();
        (event == "check_run" || event == "check_suite")
            && (action == "requested" || action == "rerequested" || action == "requested_action")
    }));
}

#[tokio::test]
async fn a_rerun_of_a_run_created_over_30_days_ago_is_refused() {
    // Given a failed run created at the start, re-run on day 20
    let server = start("failure").await;
    let client = app(&server);
    let id = dispatch(&client, "main").await;
    server.clock.advance(20 * DAY);
    ok(
        &rerun(&client, &format!("actions/runs/{id}/rerun")).await,
        StatusCode::CREATED,
    );

    // When it is re-run exactly 30 days after the first attempt
    server.clock.advance(10 * DAY);
    let at_limit = rerun(&client, &format!("actions/runs/{id}/rerun-failed-jobs")).await;

    // Then that re-run still starts
    ok(&at_limit, StatusCode::CREATED);

    // And one second later every re-run route answers GitHub's 403
    server.clock.advance(1);
    let job = jobs(&client, id, "latest").await[0]["id"].clone();
    for path in [
        format!("actions/runs/{id}/rerun"),
        format!("actions/runs/{id}/rerun-failed-jobs"),
        format!("actions/jobs/{job}/rerun"),
    ] {
        let refused = rerun(&client, &path).await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{path}");
        let body = refused.json();
        assert_eq!(body["status"], "403");
        assert_eq!(body["documentation_url"], RERUN_DOCS);
    }
    assert_eq!(run(&client, id).await["run_attempt"], 3);
}

#[tokio::test]
async fn the_51st_rerun_gives_a_failed_attempt_without_jobs() {
    // Given a run re-run 50 times through all three re-run routes
    let server = start("failure").await;
    let client = app(&server);
    let id = dispatch(&client, "main").await;
    for attempt in 1..=50 {
        let path = match attempt % 3 {
            0 => format!("actions/runs/{id}/rerun"),
            1 => format!("actions/runs/{id}/rerun-failed-jobs"),
            _ => {
                let job = named(&jobs(&client, id, "latest").await, "build")["id"].clone();
                format!("actions/jobs/{job}/rerun")
            }
        };
        ok(&rerun(&client, &path).await, StatusCode::CREATED);
    }
    let fifty = run(&client, id).await;
    assert_eq!(fifty["run_attempt"], 51);
    let sha = fifty["head_sha"].as_str().unwrap().to_owned();
    let before = check_runs(&client, &sha, "filter=all").await["total_count"].clone();

    // When it is re-run once more
    let answer = rerun(&client, &format!("actions/runs/{id}/rerun")).await;

    // Then the call succeeds and gives a failed attempt 52 with no jobs
    ok(&answer, StatusCode::CREATED);
    let limited = run(&client, id).await;
    assert_eq!(limited["run_attempt"], 52);
    assert_eq!(limited["status"], "completed");
    assert_eq!(limited["conclusion"], "failure");
    let attempt = format!("{REPO}/actions/runs/{id}/attempts/52/jobs");
    assert_eq!(
        ok(&client.get(&attempt).await, StatusCode::OK)["total_count"],
        0
    );
    // And the run's suite fails with no new check run
    let suite = actions_suite(&client, &sha).await;
    assert_eq!(
        (suite["status"].clone(), suite["conclusion"].clone()),
        (json!("completed"), json!("failure"))
    );
    assert_eq!(
        check_runs(&client, &sha, "filter=all").await["total_count"],
        before
    );
}

#[tokio::test]
async fn a_full_rerun_queues_every_job_and_a_job_rerun_queues_it_and_its_dependents() {
    // Given a completed run whose build failed
    let server = start("none").await;
    let client = app(&server);
    let id = dispatch(&client, "main").await;
    finish(&server, &client, id, "failure").await;
    let state = |jobs: &[Value]| -> BTreeSet<(String, String)> {
        jobs.iter()
            .map(|job| {
                (
                    job["name"].as_str().unwrap().to_owned(),
                    job["status"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };
    let pair = |name: &str, status: &str| (name.to_owned(), status.to_owned());

    // When the whole run is re-run
    ok(
        &rerun(&client, &format!("actions/runs/{id}/rerun")).await,
        StatusCode::CREATED,
    );

    // Then every job is queued in attempt 2
    let second = jobs(&client, id, "latest").await;
    assert!(second.iter().all(|job| job["run_attempt"] == 2));
    assert_eq!(
        state(&second),
        BTreeSet::from(["build", "deploy", "lint", "test"].map(|name| pair(name, "queued")))
    );

    // When attempt 2 passes and only its lint job is re-run
    finish(&server, &client, id, "success").await;
    let lint = named(&jobs(&client, id, "latest").await, "lint")["id"].clone();
    ok(
        &rerun(&client, &format!("actions/jobs/{lint}/rerun")).await,
        StatusCode::CREATED,
    );

    // Then lint and the deploy that needs it are queued and the rest are carried as finished
    let third = jobs(&client, id, "latest").await;
    assert!(third.iter().all(|job| job["run_attempt"] == 3));
    assert_eq!(
        state(&third),
        BTreeSet::from([
            pair("build", "completed"),
            pair("deploy", "queued"),
            pair("lint", "queued"),
            pair("test", "completed"),
        ])
    );
}

#[tokio::test]
async fn the_github_actions_app_names_the_seeded_account_that_holds_the_github_login() {
    // Given a seed whose own organization holds the login github, and a run with a job
    let server = start_with(
        "none",
        [json!({"kind": "org", "id": 7777, "login": "github", "name": "Seeded"})],
    )
    .await;
    let client = app(&server);
    let (_, after) = push_feature(&server);
    let id = dispatch(&client, "feature").await;
    let build = named(&jobs(&client, id, "latest").await, "build")["id"].clone();

    // When the job's check run and the account with that login are read
    let check = ok(
        &client.get(&format!("{REPO}/check-runs/{build}")).await,
        StatusCode::OK,
    );
    let account = ok(&client.get("users/github").await, StatusCode::OK);

    // Then the App's owner is that account
    assert_eq!(check["head_sha"], after);
    assert_eq!(check["app"]["owner"]["id"], account["id"]);
    assert_eq!(check["app"]["owner"]["id"], 7777);
    assert_eq!(check["app"]["owner"]["login"], "github");
}

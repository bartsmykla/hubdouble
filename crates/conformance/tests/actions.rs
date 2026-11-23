#![expect(
    clippy::unwrap_used,
    reason = "tests fail immediately when an answer and its contract differ"
)]

#[path = "../../api-actions/tests/support/oidc.rs"]
mod oidc;
#[path = "../../api-actions/tests/support/conformance.rs"]
mod support;

use std::collections::BTreeSet;

use http::header::{self, HeaderValue};
use http::{HeaderMap, Method, StatusCode};
use hubdouble_model::{Issue, IssueState, Pull, PullRef, Timestamp, node_id};
use hubdouble_store::{Counters, Issues, WorkflowRuns};
use hubdouble_testkit::fixture::{
    ALICE_ID, ALICE_TOKEN, BOB_TOKEN, PUBLIC_REPO_ID, WEBHOOK_SECRET,
};
use hubdouble_testkit::validate::webhook_payload;
use hubdouble_testkit::{GoGithub, TestServer};
use serde_json::json;

use support::{
    ACTIONS, LIMITED, PRIVATE_ACTIONS, WORKFLOW, WRITER, client, dispatch, dispatch_workflow,
    drain, server, valid,
};

#[tokio::test]
async fn dispatch_retry_and_validation_are_atomic_and_use_stable_workflow_ids() {
    let server = server("none").await;
    let client = GoGithub::enterprise(&server).with_token(WRITER);
    let path = format!("{ACTIONS}/workflows/agent.yml");
    let workflow = valid("get-workflow", &client.get(&path).await);
    let workflow_id = workflow["id"].as_i64().unwrap();
    assert_eq!(workflow["path"], WORKFLOW);
    assert_eq!(
        workflow,
        valid(
            "get-workflow",
            &client
                .get(&format!("{ACTIONS}/workflows/{workflow_id}"))
                .await
        )
    );

    let rejected = client
        .post(
            &format!("{path}/dispatches"),
            &json!({
                "ref":"main", "inputs":{"z-input":"x","a-input":"x"}, "return_run_details":true
            }),
        )
        .await;
    assert_eq!(rejected.status, StatusCode::UNPROCESSABLE_ENTITY);
    let rejected = rejected.json();
    assert_eq!(rejected["errors"][0]["field"], "inputs");
    let message = rejected["message"].as_str().unwrap();
    assert!(message.find("\"a-input\"").unwrap() < message.find("\"z-input\"").unwrap());
    for (workflow, git_ref) in [
        ("ci.yml", "main"),
        ("input.yml", "main"),
        ("agent.yml", "missing"),
    ] {
        let response = client
            .post(
                &format!("{ACTIONS}/workflows/{workflow}/dispatches"),
                &json!({"ref":git_ref}),
            )
            .await;
        assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(response.json()["errors"][0]["field"].is_string());
    }
    let empty = valid(
        "list-workflow-runs-for-repo",
        &client.get(&format!("{ACTIONS}/runs")).await,
    );
    assert_eq!(empty, json!({"workflow_runs": [], "total_count": 0}));
    drain(&server).await;
    assert!(server.receiver.received().await.is_empty());

    let detail = dispatch(&client, "main").await;
    let id = detail["workflow_run_id"].as_i64().unwrap();
    assert_eq!(
        detail["run_url"],
        format!("{}/api/v3/{ACTIONS}/runs/{id}", server.base)
    );
    assert_eq!(
        detail["html_url"],
        format!("{}/acme/public/actions/runs/{id}", server.base)
    );
    let listed = valid(
        "list-workflow-runs",
        &client
            .get(&format!("{ACTIONS}/workflows/{workflow_id}/runs"))
            .await,
    );
    assert_eq!(listed["total_count"], 1);
    assert_eq!(listed["workflow_runs"][0]["id"], id);
    assert_eq!(listed["workflow_runs"][0]["status"], "queued");
    assert_eq!(listed["workflow_runs"][0]["workflow_id"], workflow_id);

    let empty = client
        .post(
            &format!("{ACTIONS}/workflows/{workflow_id}/dispatches"),
            &json!({"ref":"main"}),
        )
        .await;
    assert_eq!(empty.status, StatusCode::NO_CONTENT);
    assert!(empty.body.is_empty());
    let reset = server.admin(Method::POST, "reset", None).await;
    assert_eq!(reset.status, StatusCode::NO_CONTENT);
    let next = dispatch(&client, "main").await;
    assert!(next["workflow_run_id"].as_i64().unwrap() > id + 1);
    let workflow = valid("get-workflow", &client.get(&path).await);
    assert_eq!(workflow["id"], workflow_id);
}

#[tokio::test]
async fn workflow_run_filters_apply_together_before_pagination() {
    let server = server("none").await;
    let client = client(&server);
    let old = dispatch(&client, "main").await;
    server.clock.advance(60);
    let wanted = dispatch(&client, "main").await["workflow_run_id"]
        .as_i64()
        .unwrap();
    let also = dispatch(&client, "main").await["workflow_run_id"]
        .as_i64()
        .unwrap();
    let other_event = dispatch(&client, "main").await["workflow_run_id"]
        .as_i64()
        .unwrap();
    let completed = dispatch(&client, "main").await["workflow_run_id"]
        .as_i64()
        .unwrap();
    let other_branch = dispatch(&client, "develop").await["workflow_run_id"]
        .as_i64()
        .unwrap();
    let other_head = dispatch(&client, "feature").await["workflow_run_id"]
        .as_i64()
        .unwrap();
    server
        .server
        .services()
        .store
        .update(|tx| {
            let get = |id| tx.table::<WorkflowRuns>().get(id).unwrap().as_ref().clone();
            let mut event = get(other_event);
            let mut branch = get(other_branch);
            let mut head = get(other_head);
            event.event = "push".to_owned();
            branch.head_sha.clone_from(&get(wanted).head_sha);
            head.head_branch = Some("main".to_owned());
            let mut table = tx.table_mut::<WorkflowRuns>();
            for row in [event, branch, head] {
                table.replace(row)?;
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(
        server
            .admin(
                Method::POST,
                &format!("actions/runs/{completed}"),
                Some(&json!({"conclusion":"success"}))
            )
            .await
            .status,
        StatusCode::OK
    );
    let run = valid(
        "get-workflow-run",
        &client.get(&format!("{ACTIONS}/runs/{wanted}")).await,
    );
    let sha = run["head_sha"].as_str().unwrap();
    let filters = format!(
        "event=workflow_dispatch&created=%3E%3D2026-01-01T00%3A01%3A00Z&status=queued&branch=main&head_sha={sha}&per_page=1"
    );
    for (base, op) in [
        (format!("{ACTIONS}/runs"), "list-workflow-runs-for-repo"),
        (
            format!("{ACTIONS}/workflows/agent.yml/runs"),
            "list-workflow-runs",
        ),
    ] {
        let first = client.get(&format!("{base}?{filters}")).await;
        let one = valid(op, &first);
        assert_eq!(one["total_count"], 2);
        assert_eq!(one["workflow_runs"][0]["id"], also);
        assert!(first.header("link").contains("page=2>; rel=\"next\""));
        let two = valid(op, &client.get(&format!("{base}?{filters}&page=2")).await);
        assert_eq!(two["workflow_runs"][0]["id"], wanted);
        assert_ne!(two["workflow_runs"][0]["id"], old["workflow_run_id"]);
        for field in [
            "path",
            "check_suite_id",
            "run_attempt",
            "status",
            "run_started_at",
            "updated_at",
        ] {
            assert!(!two["workflow_runs"][0][field].is_null(), "{field}");
        }
    }
}

fn open_pull_request(server: &TestServer, head: &str) -> (i64, String) {
    let services = server.server.services();
    services
        .store
        .update(|tx| {
            let sha = services
                .git
                .resolve_ref(tx, PUBLIC_REPO_ID, &format!("refs/heads/{head}"))
                .unwrap()
                .to_string();
            let id = tx.next_id();
            let pull_id = tx.next_id();
            let number = tx.table_mut::<Counters>().next_number(PUBLIC_REPO_ID);
            let side = |name: &str| PullRef {
                repo_id: Some(PUBLIC_REPO_ID),
                ref_name: name.to_owned(),
                sha: sha.clone(),
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
                    head: side(head),
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
            Ok((number, sha))
        })
        .unwrap()
}

#[tokio::test]
async fn workflow_runs_leave_out_pull_requests_when_client_excludes_them() {
    // Given a run on the head of an open pull request
    let server = server("none").await;
    let client = client(&server);
    let (number, sha) = open_pull_request(&server, "feature");
    let run = dispatch(&client, "feature").await["workflow_run_id"]
        .as_i64()
        .unwrap();
    let list =
        |exclude: &str| format!("{ACTIONS}/runs?{exclude}head_sha={sha}&page=1&per_page=100");

    // When the runs of that head are listed with and without exclude_pull_requests, as
    // client's ciSuites lists them (internal/dispatch/ci.go:200)
    let excluded = valid(
        "list-workflow-runs-for-repo",
        &client.get(&list("exclude_pull_requests=true&")).await,
    );
    let included = valid("list-workflow-runs-for-repo", &client.get(&list("")).await);

    // Then only the answer without the option names the pull request
    assert_eq!(excluded["total_count"], 1);
    assert_eq!(excluded["workflow_runs"][0]["id"], run);
    assert_eq!(excluded["workflow_runs"][0]["pull_requests"], json!([]));
    assert_eq!(
        included["workflow_runs"][0]["pull_requests"][0]["number"],
        number
    );
}

#[tokio::test]
async fn changing_the_default_branch_refreshes_the_workflow_index() {
    let public = fixture_public_with_branch_workflows();
    let server = TestServer::builder()
        .frame(hubdouble_api::register_all)
        .entry(json!({"kind": "user", "login": "alice"}))
        .entry(public)
        .start()
        .await;
    let client = GoGithub::new(&server);
    let workflows = "repos/alice/workflows/actions/workflows";
    let common = format!("{workflows}/common.yml");
    let id = client.get(&common).await.json()["id"].clone();
    assert!(id.is_number());
    let edit = server
        .admin(
            Method::PATCH,
            "repos/alice/workflows",
            Some(&json!({"default_branch": "next"})),
        )
        .await;
    assert_eq!(edit.status, StatusCode::OK);
    let new = client.get(&format!("{workflows}/next.yml")).await;
    assert_eq!(new.status, StatusCode::OK);
    assert_eq!(new.json()["path"], ".github/workflows/next.yml");
    let removed = client.get(&format!("{workflows}/old.yml")).await;
    assert_eq!(removed.status, StatusCode::NOT_FOUND);
    let common = client.get(&common).await.json();
    assert_eq!(common["id"], id);
    assert_eq!(common["name"], "Updated");
}

fn fixture_public_with_branch_workflows() -> serde_json::Value {
    json!({
        "kind": "repository", "owner": "alice", "name": "workflows",
        "files": {
            ".github/workflows/common.yml": "name: Original\non: push\n",
            ".github/workflows/old.yml": "on: push\n"
        },
        "commits": [{"branch": "next", "message": "change workflows", "files": {
            ".github/workflows/common.yml": "name: Updated\non: push\n",
            ".github/workflows/old.yml": null,
            ".github/workflows/next.yml": "on: push\n"
        }}]
    })
}

#[tokio::test]
async fn workflow_run_list_returns_no_matches_for_an_invalid_created_filter() {
    let server = server("none").await;
    let client = client(&server);
    dispatch(&client, "main").await;
    let answer = client.get(&format!("{ACTIONS}/runs?created=bogus")).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(
        answer.json(),
        json!({"total_count": 0, "workflow_runs": []})
    );
}

#[tokio::test]
async fn lifecycle_preserves_attempts_and_serves_ranged_job_logs() {
    let server = server("none").await;
    let client = client(&server);
    let run_id = dispatch_workflow(&client, "multi.yml", "main").await["workflow_run_id"]
        .as_i64()
        .unwrap();
    let run_path = format!("{ACTIONS}/runs/{run_id}");
    let latest = format!("{run_path}/jobs?filter=latest&per_page=100");
    let jobs = valid("list-jobs-for-workflow-run", &client.get(&latest).await);
    assert_eq!(jobs["total_count"], 2);
    assert_eq!(jobs["jobs"][0]["name"], "fail");
    let job = jobs["jobs"][0]["id"].as_i64().unwrap();
    let passed = jobs["jobs"][1]["id"].as_i64().unwrap();
    let admin = format!("actions/runs/{run_id}");
    server.clock.advance(1);
    assert_eq!(
        server
            .admin(
                Method::POST,
                &admin,
                Some(&json!({"status":"in_progress","jobs":[{"id":job,"status":"in_progress"}]}))
            )
            .await
            .status,
        StatusCode::OK
    );
    server.clock.advance(3);
    let complete = server.admin(Method::POST, &admin, Some(&json!({
        "conclusion": "failure", "jobs": [
            {"id":job,"logs":"first\nsecond\n","steps":[{"number":1,"name":"Test","status":"completed","conclusion":"failure"}]},
            {"id":passed,"conclusion":"success"}
        ]
    }))).await;
    assert_eq!(complete.status, StatusCode::OK);
    let job_path = format!("{ACTIONS}/jobs/{job}");
    let current = valid("get-job-for-workflow-run", &client.get(&job_path).await);
    assert_eq!(current["conclusion"], "failure");
    assert_eq!(current["steps"][0]["number"], 1);
    log_range(&server, &format!("{job_path}/logs")).await;

    let retried = client
        .call(Method::POST, &format!("{run_path}/rerun-failed-jobs"))
        .await;
    assert_eq!(retried.status, StatusCode::CREATED);
    valid("re-run-workflow-failed-jobs", &retried);
    let run = valid("get-workflow-run", &client.get(&run_path).await);
    assert_eq!(run["run_attempt"], 2);
    assert_eq!(run["status"], "queued");
    let first = valid(
        "get-workflow-run-attempt",
        &client.get(&format!("{run_path}/attempts/1")).await,
    );
    assert_eq!(first["conclusion"], "failure");
    let first_jobs = valid(
        "list-jobs-for-workflow-run-attempt",
        &client
            .get(&format!("{run_path}/attempts/1/jobs?per_page=100"))
            .await,
    );
    assert_eq!(first_jobs["jobs"][0]["id"], job);
    assert_eq!(first_jobs["jobs"][1]["conclusion"], "success");
    let latest = valid("list-jobs-for-workflow-run", &client.get(&latest).await);
    assert_eq!(latest["total_count"], 2);
    assert_eq!(latest["jobs"][0]["name"], "fail");
    assert_ne!(latest["jobs"][0]["id"], job);
    assert_eq!(latest["jobs"][0]["status"], "queued");
    assert_carried(&first_jobs["jobs"][1], &latest["jobs"][1], &run);
    let second_jobs = client
        .get(&format!("{run_path}/attempts/2/jobs?per_page=100"))
        .await;
    assert_eq!(
        valid("list-jobs-for-workflow-run-attempt", &second_jobs),
        latest
    );
    let all = valid(
        "list-jobs-for-workflow-run",
        &client.get(&format!("{run_path}/jobs?filter=all")).await,
    );
    assert_eq!(all["total_count"], 4);
    assert_eq!(
        client
            .call(Method::POST, &format!("{run_path}/cancel"))
            .await
            .status,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        client
            .call(Method::POST, &format!("{run_path}/cancel"))
            .await
            .status,
        StatusCode::CONFLICT
    );
    let final_run = valid("get-workflow-run", &client.get(&run_path).await);
    assert_eq!(final_run["conclusion"], "cancelled");
    drain(&server).await;
    assert_lifecycle_webhooks(&server, &final_run).await;
}

fn assert_carried(
    before: &serde_json::Value,
    carried: &serde_json::Value,
    run: &serde_json::Value,
) {
    assert_ne!(carried["id"], before["id"]);
    assert_eq!(carried["run_attempt"], run["run_attempt"]);
    assert_eq!(carried["created_at"], run["run_started_at"]);
    for field in [
        "name",
        "status",
        "conclusion",
        "started_at",
        "completed_at",
        "steps",
    ] {
        assert_eq!(carried[field], before[field], "{field}");
    }
}

#[tokio::test]
async fn rerun_failed_jobs_queues_dependents_without_a_requested_run_event() {
    // Given a run whose build failed, whose lint passed, and whose dependents were skipped
    let server = server("none").await;
    let client = client(&server);
    let run_id = dispatch_workflow(&client, "chain.yml", "main").await["workflow_run_id"]
        .as_i64()
        .unwrap();
    let run_path = format!("{ACTIONS}/runs/{run_id}");
    let latest = format!("{run_path}/jobs?filter=latest&per_page=100");
    let jobs = valid("list-jobs-for-workflow-run", &client.get(&latest).await);
    let conclusions: Vec<_> = jobs["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|job| {
            let conclusion = match job["name"].as_str().unwrap() {
                "build" => "failure",
                "lint" => "success",
                _ => "skipped",
            };
            json!({"id": job["id"], "conclusion": conclusion})
        })
        .collect();
    let complete = server
        .admin(
            Method::POST,
            &format!("actions/runs/{run_id}"),
            Some(&json!({"conclusion": "failure", "jobs": conclusions})),
        )
        .await;
    assert_eq!(complete.status, StatusCode::OK);
    drain(&server).await;
    server.receiver.received().await;

    // When the failed jobs are re-run
    let retried = client
        .call(Method::POST, &format!("{run_path}/rerun-failed-jobs"))
        .await;
    assert_eq!(retried.status, StatusCode::CREATED);

    // Then the failed job and every job that needs it, directly or not, are queued again
    let latest = valid("list-jobs-for-workflow-run", &client.get(&latest).await);
    let states: BTreeSet<_> = latest["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|job| {
            assert_eq!(job["run_attempt"], 2);
            (
                job["name"].as_str().unwrap().to_owned(),
                job["status"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let state = |name: &str, status: &str| (name.to_owned(), status.to_owned());
    // And the job that passed is carried into the new attempt as it finished
    assert_eq!(
        states,
        BTreeSet::from([
            state("build", "queued"),
            state("deploy", "queued"),
            state("lint", "completed"),
            state("test", "queued"),
        ])
    );
    // And only the new jobs' queued events are sent, with no workflow_run requested
    drain(&server).await;
    let sent: Vec<_> = server
        .receiver
        .received()
        .await
        .iter()
        .map(|delivery| {
            let body = delivery.json();
            (
                delivery.header("x-github-event").to_owned(),
                body["action"].as_str().unwrap().to_owned(),
                body["workflow_job"]["run_attempt"].clone(),
            )
        })
        .collect();
    let queued = ("workflow_job".to_owned(), "queued".to_owned(), json!(2));
    assert_eq!(sent, [queued.clone(), queued.clone(), queued]);
}

async fn assert_lifecycle_webhooks(server: &TestServer, final_run: &serde_json::Value) {
    let mut completed = Vec::new();
    let mut events = BTreeSet::new();
    for delivery in server.receiver.received().await {
        assert!(delivery.signed_with(WEBHOOK_SECRET));
        let body = delivery.json();
        let event = delivery.header("x-github-event");
        let action = body["action"].as_str().unwrap();
        events.insert(format!("{event}/{action}"));
        if let Err(e) = webhook_payload(event, Some(action), &body) {
            panic!("{event}/{action}: {e:?}");
        }
        if event == "workflow_run" && action == "completed" {
            let run = &body["workflow_run"];
            assert_eq!(run["path"], ".github/workflows/multi.yml");
            assert_eq!(run["head_sha"], final_run["head_sha"]);
            assert_eq!(run["head_branch"], "main");
            assert_eq!(run["event"], "workflow_dispatch");
            assert_eq!(run["name"], "Multi");
            assert_eq!(run["html_url"], final_run["html_url"]);
            assert_eq!(run["pull_requests"], json!([]));
            completed.push((
                run["run_attempt"].as_i64().unwrap(),
                run["conclusion"].clone(),
            ));
        }
    }
    completed.sort_unstable_by_key(|(attempt, _)| *attempt);
    assert_eq!(completed, [(1, json!("failure")), (2, json!("cancelled"))]);
    assert_eq!(
        events,
        BTreeSet::from(
            [
                "workflow_job/completed",
                "workflow_job/in_progress",
                "workflow_job/queued",
                "workflow_run/completed",
                "workflow_run/in_progress",
                "workflow_run/requested",
            ]
            .map(str::to_owned)
        )
    );
}

async fn log_range(server: &TestServer, path: &str) {
    let logs = GoGithub::enterprise(server)
        .with_token(ALICE_TOKEN)
        .get(path)
        .await;
    assert_eq!(logs.status, StatusCode::FOUND);
    let path = logs.header("location").strip_prefix(&server.base).unwrap();
    let id = path.strip_prefix("/_blobs/").unwrap().as_bytes();
    assert_eq!(id.len(), 36);
    for (index, byte) in id.iter().enumerate() {
        if matches!(index, 8 | 13 | 18 | 23) {
            assert_eq!(*byte, b'-');
        } else {
            assert!(byte.is_ascii_hexdigit());
        }
    }
    assert_eq!(id[14], b'4');
    assert!(matches!(id[19], b'8' | b'9' | b'a' | b'b'));
    let root = GoGithub::new(server);
    let metadata = root.call(Method::HEAD, path).await;
    assert_eq!(metadata.status, StatusCode::OK);
    assert_eq!(metadata.header("content-length"), "13");
    let range = root
        .call_headers(
            Method::GET,
            path,
            HeaderMap::from_iter([(header::RANGE, HeaderValue::from_static("bytes=6-11"))]),
        )
        .await;
    assert_eq!(range.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(range.header("content-range"), "bytes 6-11/13");
    assert_eq!(&range.body[..], b"second");
}

#[tokio::test]
async fn repository_settings_and_actions_permissions_match_the_route() {
    let server = server("success").await;
    let client = client(&server);
    let writer = GoGithub::new(&server).with_token(WRITER);
    let limited = GoGithub::new(&server).with_token(LIMITED);
    let outsider = GoGithub::new(&server).with_token(BOB_TOKEN);
    let run = dispatch(&client, "main").await["workflow_run_id"]
        .as_i64()
        .unwrap();
    let path = format!("{ACTIONS}/runs/{run}");
    let automatic = valid("get-workflow-run", &writer.get(&path).await);
    assert_eq!(automatic["conclusion"], "success");
    let jobs = valid(
        "list-jobs-for-workflow-run",
        &writer.get(&format!("{path}/jobs")).await,
    );
    let job = jobs["jobs"][0]["id"].as_i64().unwrap();
    for suffix in [
        format!("jobs/{job}"),
        format!("jobs/{job}/logs"),
        "organization-secrets".into(),
        "organization-variables".into(),
        "runs".into(),
        format!("runs/{run}"),
        format!("runs/{run}/attempts/1"),
        format!("runs/{run}/attempts/1/jobs"),
        format!("runs/{run}/jobs"),
        "secrets/has_secret".into(),
        "variables/mode".into(),
        "workflows/agent.yml/runs".into(),
    ] {
        let denied = limited.get(&format!("{ACTIONS}/{suffix}")).await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN, "{suffix}");
        assert_ne!(denied.header("x-accepted-github-permissions"), "");
        assert_eq!(
            outsider
                .get(&format!("{PRIVATE_ACTIONS}/{suffix}"))
                .await
                .status,
            StatusCode::NOT_FOUND
        );
    }
    for suffix in [
        format!("runs/{run}/cancel"),
        format!("runs/{run}/rerun-failed-jobs"),
        "workflows/agent.yml/dispatches".into(),
    ] {
        let body = json!({"ref":"main"});
        assert_eq!(
            limited
                .post(&format!("{ACTIONS}/{suffix}"), &body)
                .await
                .status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            outsider
                .post(&format!("{PRIVATE_ACTIONS}/{suffix}"), &body)
                .await
                .status,
            StatusCode::NOT_FOUND
        );
    }
    let a = valid(
        "get-repo-secret",
        &client.get(&format!("{ACTIONS}/secrets/has_secret")).await,
    );
    assert_eq!(a["name"], "HAS_SECRET");
    assert!(a.get("value").is_none());
    let v = valid(
        "get-repo-variable",
        &client.get(&format!("{ACTIONS}/variables/mode")).await,
    );
    assert_eq!(v["value"], "local");
    let secrets = valid(
        "list-repo-organization-secrets",
        &client
            .get(&format!("{ACTIONS}/organization-secrets?per_page=100"))
            .await,
    );
    assert_eq!(secrets["total_count"], 1);
    assert_eq!(secrets["secrets"][0]["name"], "SHARED");
    let vars = valid(
        "list-repo-organization-variables",
        &client
            .get(&format!("{ACTIONS}/organization-variables?per_page=100"))
            .await,
    );
    assert_eq!(vars["total_count"], 1);
    assert_eq!(vars["variables"][0]["value"], "yes");
}

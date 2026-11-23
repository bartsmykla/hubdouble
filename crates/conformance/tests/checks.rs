use http::{Method, StatusCode};
use hubdouble_admin::Registrar;
use hubdouble_git::{CommitNode, EntryMode, FileChange, Signature};
use hubdouble_model::{Access, ApiError, Repo, Timestamp, Token, TokenKind};
use hubdouble_store::Tokens;
use hubdouble_testkit::{
    Answer, Delivery, GoGithub, Graphql, TestServer, fixture, standins, validate,
};
use serde_json::{Value, json};

const TOKEN: &str = "ghs_checks_conformance_installation";
const NARROW: &str = "ghs_checks_conformance_without_checks";
const RUNS: &str = "repos/acme/public/check-runs";
const OTHER_APP_ID: i64 = 4002;
const ROLLUP: &str = "query($owner: String!, $name: String!, $oid: GitObjectID!) {
  repository(owner: $owner, name: $name) {
    object(oid: $oid) { ... on Commit { statusCheckRollup { state } } }
  }
}";

fn entries() -> Vec<Value> {
    let mut entries = fixture::seed();
    for entry in &mut entries {
        if entry["kind"] == "app" {
            entry["events"] = json!(["check_run", "check_suite", "status"]);
            entry["permissions"]["statuses"] = json!("write");
        }
        if entry["kind"] == "installation" {
            entry["permissions"] = json!({"checks": "write", "statuses": "write",
                "contents": "read", "metadata": "read"});
        }
    }
    entries.push(
        json!({"kind": "app", "id": OTHER_APP_ID, "slug": "other-checks",
        "name": "Other Checks", "owner": fixture::ORG, "public_key": fixture::APP_PUBLIC_KEY,
        "permissions": {"checks": "write", "metadata": "read"}, "events": ["check_suite"]}),
    );
    entries.push(
        json!({"kind": "installation", "id": 5002, "app": "other-checks",
        "account": fixture::ORG, "repositories": [fixture::PUBLIC_REPO],
        "permissions": {"checks": "write", "metadata": "read"}}),
    );
    entries
}

fn parents(r: &mut Registrar<'_>) {
    r.gql.object::<Repo>("Repository");
    r.gql.object_resolved::<CommitNode>("Commit");
}

fn token(id: i64, value: &str, permissions: &[(&str, Access)]) -> Token {
    Token {
        id,
        token: value.into(),
        kind: TokenKind::Installation,
        app_id: Some(fixture::APP_ID),
        installation_id: Some(fixture::INSTALLATION_ID),
        user_id: None,
        repo_ids: None,
        permissions: permissions
            .iter()
            .map(|(p, a)| ((*p).to_owned(), *a))
            .collect(),
        scopes: vec![],
        expires_at: None,
        revoked: false,
        owner_seeded: false,
        created_at: Timestamp::SEED,
    }
}

async fn start(extra: Vec<Value>) -> TestServer {
    let mut builder = TestServer::builder()
        .register(hubdouble_api_checks::register)
        .register(standins::query_repository)
        .register(standins::repository_object)
        .register(parents);
    for entry in entries().into_iter().chain(extra) {
        builder = builder.entry(entry);
    }
    let server = builder.start().await;
    server
        .server
        .services()
        .store
        .update_quiet(|tx| {
            let id = tx.next_id();
            tx.table_mut::<Tokens>().insert(token(id, TOKEN, &[]))?;
            let id = tx.next_id();
            tx.table_mut::<Tokens>()
                .insert(token(id, NARROW, &[("metadata", Access::Read)]))?;
            Ok(())
        })
        .unwrap_or_else(|e| panic!("insert the check tokens: {e}"));
    server
}

fn app_client(server: &TestServer) -> GoGithub {
    GoGithub::enterprise(server).with_token(TOKEN)
}

fn sha(server: &TestServer, branch: &str) -> String {
    let services = server.server.services();
    services
        .store
        .view(|tx| {
            Ok(services
                .git
                .resolve_ref(tx, fixture::PUBLIC_REPO_ID, &format!("refs/heads/{branch}"))?
                .to_string())
        })
        .unwrap_or_else(|e| panic!("resolve {branch}: {e}"))
}

fn push_feature(server: &TestServer) -> (String, String) {
    let services = server.server.services();
    services
        .store
        .update(|tx| {
            let git = &services.git;
            let repo = fixture::PUBLIC_REPO_ID;
            let old = git.resolve_ref(tx, repo, "refs/heads/feature")?;
            let signature = Signature::seed();
            let new = git.commit(
                tx,
                repo,
                fixture::ALICE_ID,
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
        .unwrap_or_else(|e| panic!("push the feature branch: {e}"))
}

fn checked(answer: &Answer, status: u16, operation: &str) -> Value {
    assert_eq!(
        answer.status.as_u16(),
        status,
        "{operation}: {:?}",
        answer.json()
    );
    answer.json()
}

async fn drain(server: &TestServer) -> Vec<Delivery> {
    let drained = server.admin(Method::POST, "hooks/drain", None).await;
    assert!(drained.status.is_success());
    server.receiver.received().await
}

fn deliveries<'d>(
    all: &'d [Delivery],
    event: &'d str,
    action: Option<&'d str>,
) -> impl Iterator<Item = Value> + 'd {
    all.iter()
        .filter(move |d| d.header("x-github-event") == event)
        .map(|d| {
            assert!(d.signed_with(fixture::WEBHOOK_SECRET));
            d.json()
        })
        .filter(move |p| action.is_none_or(|a| p["action"] == a))
        .inspect(move |p| {
            validate::webhook_payload(event, action, p)
                .unwrap_or_else(|e| panic!("{event}.{action:?}: {e:?}"));
        })
}

fn webhook(all: &[Delivery], event: &str, action: Option<&str>) -> Value {
    deliveries(all, event, action)
        .next()
        .unwrap_or_else(|| panic!("no {event}.{action:?} delivery"))
}

fn annotations(count: usize) -> Vec<Value> {
    (0..count)
        .map(|i| {
            json!({"path": "src/feature.txt", "start_line": 1, "end_line": 1,
                "annotation_level": "notice", "title": format!("note {i}"), "message": "look here"})
        })
        .collect()
}

#[tokio::test]
async fn check_writes_follow_the_client_batches_and_keep_output() {
    // Given the client's internal/dispatch/checks.go:131-191, which posts the first batch of 50
    // annotations with the run facts and patches the next batches without them.
    let server = start(vec![]).await;
    let client = app_client(&server);
    let head = sha(&server, "feature");
    let actions =
        json!([{"label": "Re-run", "description": "Run the review again", "identifier": "rerun"}]);
    let first = json!({"name": "AGENT review", "head_sha": head, "status": "in_progress",
        "details_url": "https://agent.example/runs/1", "external_id": "job-1", "actions": actions,
        "output": {"title": "Review", "summary": "started", "annotations": annotations(50)}});
    // When
    let created = checked(&client.post(RUNS, &first).await, 201, "checks/create");
    let run = format!("{RUNS}/{}", created["id"]);
    let second = json!({"name": "AGENT review",
        "output": {"title": "Review", "summary": "started", "annotations": annotations(1)}});
    let patched = checked(
        &client.call_json(Method::PATCH, &run, &second).await,
        200,
        "checks/update",
    );
    let fix = json!([{"label": "Fix", "description": "Apply the fix", "identifier": "autofix"}]);
    let done = json!({"name": "AGENT review", "status": "completed", "conclusion": "success", "actions": fix,
        "output": {"title": "Reviewed", "summary": "done", "text": "kept"}});
    checked(
        &client.call_json(Method::PATCH, &run, &done).await,
        200,
        "checks/update",
    );
    let read = checked(&client.get(&run).await, 200, "checks/get");
    let notes = checked(
        &client.get(&format!("{run}/annotations?per_page=100")).await,
        200,
        "checks/list-annotations",
    );
    // Then
    assert_eq!(created["status"], "in_progress");
    assert_eq!(created["conclusion"], Value::Null);
    assert_eq!(created["head_sha"], head);
    assert_eq!(created["external_id"], "job-1");
    assert_eq!(created["details_url"], "https://agent.example/runs/1");
    assert_eq!(created["app"]["id"], fixture::APP_ID);
    assert_eq!(created["output"]["annotations_count"], 50);
    assert_eq!(patched["output"]["annotations_count"], 51);
    assert_eq!(patched["status"], "in_progress");
    assert_eq!(read["status"], "completed");
    assert_eq!(read["conclusion"], "success");
    assert_eq!(read["output"]["title"], "Reviewed");
    assert_eq!(read["output"]["text"], "kept");
    assert_eq!(read["output"]["annotations_count"], 51);
    let pressed = format!("check-runs/{}/requested-action", created["id"]);
    for (identifier, status) in [
        ("autofix", StatusCode::NO_CONTENT),
        ("rerun", StatusCode::UNPROCESSABLE_ENTITY),
    ] {
        let press = json!({"identifier": identifier, "sender": fixture::ALICE});
        let answer = server.admin(Method::POST, &pressed, Some(&press)).await;
        assert_eq!(answer.status, status, "{identifier}");
    }
    assert!(read["completed_at"].is_string());
    assert_eq!(read["check_suite"]["id"], created["check_suite"]["id"]);
    assert_eq!(notes.as_array().map(Vec::len), Some(51));
    assert_eq!(notes[50]["title"], "note 0");
    assert_eq!(notes[0]["annotation_level"], "notice");
}

#[tokio::test]
async fn check_writes_refuse_what_github_refuses() {
    // Given
    let server = start(vec![]).await;
    let client = app_client(&server);
    let head = sha(&server, "main");
    let output = json!({"title": "t", "summary": "s"});
    let refused = [
        json!({"name": "x", "head_sha": head, "output": {"title": "t", "summary": "s", "annotations": annotations(51)}}),
        json!({"name": "x", "head_sha": head, "status": "completed"}),
        json!({"name": "x", "head_sha": "0000000000000000000000000000000000000001"}),
        json!({"name": "x", "head_sha": "main"}),
        json!({"head_sha": head}),
        json!({"name": "x", "head_sha": head, "output": {"title": "t"}}),
        json!({"name": "x", "head_sha": head, "conclusion": "bogus"}),
        json!({"name": "x", "head_sha": head, "output": output,
            "actions": [{"label": "a label longer than twenty", "description": "d", "identifier": "i"}]}),
    ];
    for body in refused {
        // When
        let answer = client.post(RUNS, &body).await;
        // Then
        assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }
    let user = GoGithub::enterprise(&server).with_token(fixture::ALICE_TOKEN);
    let answer = user
        .post(RUNS, &json!({"name": "x", "head_sha": head}))
        .await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn check_runs_for_ref_honour_the_client_listing_options() {
    // Given runs from two Apps on main, seeded through the check_run kind
    let run = |app: &str, name: &str, state: Value| {
        let mut entry = json!({"kind": "check_run", "repository": "acme/public", "app": app,
            "name": name, "head_sha": "main"});
        if let (Some(entry), Some(state)) = (entry.as_object_mut(), state.as_object()) {
            entry.extend(state.clone());
        }
        entry
    };
    let failed = json!({"status": "completed", "conclusion": "failure"});
    let server = start(vec![
        run("fixture-app", "AGENT review", failed),
        run(
            "fixture-app",
            "AGENT review",
            json!({"status": "in_progress"}),
        ),
        run("fixture-app", "lint", json!({"conclusion": "success"})),
        run(
            "other-checks",
            "AGENT review",
            json!({"conclusion": "success"}),
        ),
    ])
    .await;
    let client = app_client(&server);
    let head = sha(&server, "main");
    let list = |query: &str| format!("repos/acme/public/commits/{head}/check-runs?{query}");
    // When the client's internal/dispatch/checks.go:94 looks for its newest review check
    let own = checked(
        &client
            .get(&list(&format!(
                "app_id={}&check_name=AGENT+review&filter=all&per_page=100",
                fixture::APP_ID
            )))
            .await,
        200,
        "checks/list-for-ref",
    );
    // And internal/dispatch/ci.go:221 reads the latest runs of one name
    let latest = checked(
        &client
            .get(&list("check_name=AGENT+review&filter=latest&per_page=100"))
            .await,
        200,
        "checks/list-for-ref",
    );
    let completed = checked(
        &client.get(&list("status=completed&per_page=100")).await,
        200,
        "checks/list-for-ref",
    );
    // And internal/dispatch/head.go:50 pages through every run
    let first = client.get(&list("per_page=1")).await;
    let second = client.get(&list("page=2&per_page=1")).await;
    let by_branch = client
        .get("repos/acme/public/commits/main/check-runs")
        .await;
    let unknown = client
        .get("repos/acme/public/commits/nothing-here/check-runs")
        .await;
    // Then
    assert_eq!(own["total_count"], 2);
    assert!(
        own["check_runs"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|r| r["app"]["id"] == fixture::APP_ID && r["name"] == "AGENT review")
    );
    assert_eq!(latest["total_count"], 2);
    let mine = latest["check_runs"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r["app"]["id"] == fixture::APP_ID)
        .unwrap_or_else(|| panic!("no fixture App run in {latest}"));
    assert_eq!(mine["status"], "in_progress");
    assert_eq!(completed["total_count"], 2);
    let first = checked(&first, 200, "checks/list-for-ref");
    assert_eq!(first["total_count"], 3);
    assert_eq!(first["check_runs"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        checked(&second, 200, "checks/list-for-ref")["check_runs"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        checked(&by_branch, 200, "checks/list-for-ref")["total_count"],
        3
    );
    assert_eq!(unknown.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn a_push_asks_each_checks_app_for_a_suite() {
    // Given
    let server = start(vec![]).await;
    let client = app_client(&server);
    // When
    let (old, new) = push_feature(&server);
    let all = drain(&server).await;
    let suites = |query: &str| format!("repos/acme/public/commits/{new}/check-suites?{query}");
    // And internal/dispatch/ci.go:105 counts the suites on the head
    let listed = checked(
        &client.get(&suites("per_page=1")).await,
        200,
        "checks/list-suites-for-ref",
    );
    let other = checked(
        &client.get(&suites(&format!("app_id={OTHER_APP_ID}"))).await,
        200,
        "checks/list-suites-for-ref",
    );
    // Then
    let requested: Vec<Value> = deliveries(&all, "check_suite", Some("requested")).collect();
    assert_eq!(requested.len(), 1);
    let suite = &requested[0];
    assert_eq!(suite["installation"]["id"], fixture::INSTALLATION_ID);
    assert_eq!(suite["check_suite"]["head_branch"], "feature");
    assert_eq!(suite["check_suite"]["head_sha"], new);
    assert_eq!(suite["check_suite"]["before"], old);
    assert_eq!(suite["check_suite"]["after"], new);
    assert_eq!(suite["check_suite"]["app"]["id"], fixture::APP_ID);
    assert_eq!(listed["total_count"], 2);
    assert_eq!(listed["check_suites"].as_array().map(Vec::len), Some(1));
    assert_eq!(other["total_count"], 1);
    assert_eq!(other["check_suites"][0]["app"]["id"], OTHER_APP_ID);
    assert_eq!(other["check_suites"][0]["status"], "queued");
}

#[tokio::test]
async fn check_run_events_reach_the_app_with_the_fields_the_client_reads() {
    // Given
    let server = start(vec![]).await;
    let client = app_client(&server);
    let head = sha(&server, "feature");
    let actions =
        json!([{"label": "Fix", "description": "Apply the fix", "identifier": "autofix"}]);
    let body = json!({"name": "AGENT review", "head_sha": head, "external_id": "job-7",
        "status": "completed", "conclusion": "failure", "actions": actions,
        "output": {"title": "Review", "summary": "found issues"}});
    let run = checked(&client.post(RUNS, &body).await, 201, "checks/create");
    let id = run["id"].as_i64().unwrap_or_default();
    let suite = run["check_suite"]["id"].as_i64().unwrap_or_default();
    // When a person presses the action, the run and its suite are rerequested
    let press = |identifier: &str| json!({"identifier": identifier, "sender": fixture::ALICE});
    let pressed = server
        .admin(
            Method::POST,
            &format!("check-runs/{id}/requested-action"),
            Some(&press("autofix")),
        )
        .await;
    let unknown = server
        .admin(
            Method::POST,
            &format!("check-runs/{id}/requested-action"),
            Some(&press("nothing")),
        )
        .await;
    let rerun = client
        .call(Method::POST, &format!("{RUNS}/{id}/rerequest"))
        .await;
    let rerun_suite = client
        .call(
            Method::POST,
            &format!("repos/acme/public/check-suites/{suite}/rerequest"),
        )
        .await;
    let suites = checked(
        &client
            .get(&format!("repos/acme/public/commits/{head}/check-suites"))
            .await,
        200,
        "checks/list-suites-for-ref",
    );
    let all = drain(&server).await;
    // Then
    assert_eq!(pressed.status, StatusCode::NO_CONTENT);
    assert_eq!(unknown.status, StatusCode::UNPROCESSABLE_ENTITY);
    checked(&rerun, 201, "checks/rerequest-run");
    checked(&rerun_suite, 201, "checks/rerequest-suite");
    assert_eq!(suites["check_suites"][0]["status"], "queued");
    let created = webhook(&all, "check_run", Some("created"));
    assert_eq!(created["check_run"]["external_id"], "job-7");
    assert_eq!(created["check_run"]["name"], "AGENT review");
    assert_eq!(created["check_run"]["head_sha"], head);
    assert_eq!(created["installation"]["id"], fixture::INSTALLATION_ID);
    let completed = webhook(&all, "check_run", Some("completed"));
    assert_eq!(completed["check_run"]["conclusion"], "failure");
    let finished = webhook(&all, "check_suite", Some("completed"));
    assert_eq!(finished["check_suite"]["conclusion"], "failure");
    let action = webhook(&all, "check_run", Some("requested_action"));
    assert_eq!(action["requested_action"]["identifier"], "autofix");
    assert_eq!(action["check_run"]["external_id"], "job-7");
    assert_eq!(action["check_run"]["name"], "AGENT review");
    assert_eq!(action["check_run"]["head_sha"], head);
    assert_eq!(action["sender"]["login"], fixture::ALICE);
    assert_eq!(
        webhook(&all, "check_run", Some("rerequested"))["check_run"]["id"],
        id
    );
    assert_eq!(
        webhook(&all, "check_suite", Some("rerequested"))["check_suite"]["id"],
        suite
    );
}

#[tokio::test]
async fn statuses_list_and_combine_as_the_client_reads_them() {
    // Given the client's internal/dispatch/ack.go:122-128 writing its run status
    let server = start(vec![]).await;
    let client = app_client(&server);
    let head = sha(&server, "main");
    let write = |state: &str, context: &str| {
        json!({"state": state, "context": context, "description": format!("{context} is {state}"),
            "target_url": "https://agent.example/runs/1"})
    };
    let path = format!("repos/acme/public/statuses/{head}");
    let first = checked(
        &client.post(&path, &write("pending", "agent/run")).await,
        201,
        "repos/create-commit-status",
    );
    for (state, context) in [("success", "agent/run"), ("error", "ci/lint")] {
        checked(
            &client.post(&path, &write(state, context)).await,
            201,
            "repos/create-commit-status",
        );
    }
    let commit = format!("repos/acme/public/commits/{head}");
    // When internal/dispatch/ci.go:252 pages through the statuses
    let page_one = client.get(&format!("{commit}/statuses?per_page=2")).await;
    let page_two = client
        .get(&format!("{commit}/statuses?page=2&per_page=2"))
        .await;
    // And internal/dispatch/head.go:76 reads the combined state
    let combined = checked(
        &client.get(&format!("{commit}/status?per_page=100")).await,
        200,
        "repos/get-combined-status-for-ref",
    );
    let narrow = client.get(&format!("{commit}/status?per_page=1")).await;
    let feature = sha(&server, "feature");
    let empty = checked(
        &client
            .get(&format!("repos/acme/public/commits/{feature}/status"))
            .await,
        200,
        "repos/get-combined-status-for-ref",
    );
    let all = drain(&server).await;
    // Then
    assert_eq!(first["state"], "pending");
    assert_eq!(first["context"], "agent/run");
    assert_eq!(first["creator"]["login"], "fixture-app[bot]");
    let listed = checked(&page_one, 200, "repos/list-commit-statuses-for-ref");
    assert_eq!(listed.as_array().map(Vec::len), Some(2));
    assert_eq!(listed[0]["context"], "ci/lint");
    assert!(page_one.header("link").contains("rel=\"next\""));
    assert_eq!(
        checked(&page_two, 200, "repos/list-commit-statuses-for-ref")[0]["state"],
        "pending"
    );
    assert_eq!(combined["state"], "failure");
    assert_eq!(combined["sha"], head);
    assert_eq!(combined["total_count"], 2);
    let contexts: Vec<(&str, &str)> = combined["statuses"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| Some((s["context"].as_str()?, s["state"].as_str()?)))
        .collect();
    assert_eq!(contexts, [("agent/run", "success"), ("ci/lint", "error")]);
    let narrow = checked(&narrow, 200, "repos/get-combined-status-for-ref");
    assert_eq!(narrow["statuses"].as_array().map(Vec::len), Some(1));
    assert_eq!(narrow["total_count"], 2);
    assert_eq!(empty["state"], "pending");
    assert_eq!(empty["statuses"], json!([]));
    let events: Vec<Value> = deliveries(&all, "status", None).collect();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["sha"], head);
    assert_eq!(events[0]["context"], "agent/run");
    assert_eq!(events[0]["state"], "pending");
    assert_eq!(events[0]["commit"]["sha"], head);
    let mut branches: Vec<&str> = events[0]["branches"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| b["name"].as_str())
        .collect();
    branches.sort_unstable();
    assert_eq!(branches, ["develop", "feature", "main"]);
    let feature_branch = events[0]["branches"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|b| b["name"] == "feature")
        .map(|b| b["commit"]["sha"].clone());
    assert_eq!(feature_branch, Some(json!(feature)));
    for body in [write("bogus", "agent/run"), json!({"context": "agent/run"})] {
        assert_eq!(
            client.post(&path, &body).await.status,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    let missing = client
        .post(
            "repos/acme/public/statuses/0000000000000000000000000000000000000001",
            &write("success", "agent/run"),
        )
        .await;
    assert_eq!(missing.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn status_check_rollup_reports_the_worst_latest_context() {
    // Given
    let server = start(vec![]).await;
    let client = app_client(&server);
    let api = Graphql::new(app_client(&server));
    let head = sha(&server, "main");
    let rollup = async || {
        let answer = api
            .query(
                ROLLUP,
                &json!({"owner": "acme", "name": "public", "oid": head}),
            )
            .await;
        assert_eq!(answer.status, StatusCode::OK);
        let body = answer.json();
        assert!(body["errors"].is_null(), "{}", body["errors"]);
        body["data"]["repository"]["object"]["statusCheckRollup"].clone()
    };
    let status = |state: &str| json!({"state": state, "context": "agent/run"});
    let statuses = format!("repos/acme/public/statuses/{head}");
    // When
    let nothing = rollup().await;
    let run = checked(
        &client
            .post(
                RUNS,
                &json!({"name": "build", "head_sha": head, "conclusion": "success"}),
            )
            .await,
        201,
        "checks/create",
    );
    client.post(&statuses, &status("success")).await;
    let success = rollup().await;
    client.post(&statuses, &status("pending")).await;
    let pending = rollup().await;
    client.post(&statuses, &status("success")).await;
    client
        .call_json(
            Method::PATCH,
            &format!("{RUNS}/{}", run["id"]),
            &json!({"conclusion": "timed_out"}),
        )
        .await;
    let failure = rollup().await;
    // Then
    assert_eq!(nothing, Value::Null);
    assert_eq!(success, json!({"state": "SUCCESS"}));
    assert_eq!(pending, json!({"state": "PENDING"}));
    assert_eq!(failure, json!({"state": "FAILURE"}));
}

#[tokio::test]
async fn every_operation_hides_unseen_repositories_and_needs_its_permission() {
    // Given
    let server = start(vec![]).await;
    let head = sha(&server, "main");
    let operations = [
        (Method::POST, "check-runs".to_owned()),
        (Method::GET, "check-runs/1".to_owned()),
        (Method::PATCH, "check-runs/1".to_owned()),
        (Method::GET, "check-runs/1/annotations".to_owned()),
        (Method::POST, "check-runs/1/rerequest".to_owned()),
        (Method::POST, "check-suites/1/rerequest".to_owned()),
        (Method::GET, "commits/main/check-runs".to_owned()),
        (Method::GET, "commits/main/check-suites".to_owned()),
        (Method::POST, format!("statuses/{head}")),
        (Method::GET, "commits/main/statuses".to_owned()),
        (Method::GET, "commits/main/status".to_owned()),
    ];
    let installed = app_client(&server);
    let narrow = GoGithub::enterprise(&server).with_token(NARROW);
    let body = json!({"name": "x", "head_sha": head, "state": "success"});
    for (method, path) in operations {
        // When
        let hidden = installed
            .call_json(method.clone(), &format!("repos/acme/private/{path}"), &body)
            .await;
        let denied = narrow
            .call_json(method.clone(), &format!("repos/acme/public/{path}"), &body)
            .await;
        // Then
        assert_eq!(hidden.status, StatusCode::NOT_FOUND, "{method} {path}");
        assert_eq!(denied.status, StatusCode::FORBIDDEN, "{method} {path}");
        assert_ne!(denied.header("x-accepted-github-permissions"), "");
    }
    let missing = installed.get("repos/acme/public/check-runs/999999").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    let reader = GoGithub::enterprise(&server).with_token(fixture::ALICE_TOKEN);
    let read = reader
        .get("repos/acme/public/commits/main/check-runs")
        .await;
    checked(&read, 200, "checks/list-for-ref");
}

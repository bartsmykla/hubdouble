#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails loudly on a broken example seed"
)]

use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use hubdouble_testkit::{Answer, GoGithub, Graphql, Receiver, TestServer, fixture, validate};
use serde::Deserialize;
use serde_json::{Value, json};

const SEED: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../examples/client-v5.yaml"
);
const CONFIG_MAP: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../deploy/kubernetes/configmap.yaml"
);
const USER: &str = "local-user";
const TARGET: &str = "repos/octo-org/e2e";
const RUNTIME: &str = "repos/octo-org/runner";
const RELEASE_TAGS: [&str; 2] = ["v5.0.6", "v5.1.0"];
const WEBHOOK_SECRET: &str = "example-seed-webhook";
const DELIVERY_PATH: &str = "app/hook/deliveries";
const TOKEN_PATH: &str = "app/installations/962416/access_tokens";
const JWT: &str = concat!(
    "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOjEwMDAwMDEsImlhdCI6MTc2NzIyNTU0MCwiZXhwIjoxNzY3MjI2MjA",
    "wfQ.Q-KsHyq-VLeatV0jO_pwlDnp0hHPh4q_kjxaLQfwyWbyFDNWd8OfYbsWWB79J6lL0B_KQ5SBYBK2Wi13cVg0d8zZ2ZGACaoO",
    "ZxcHZXIKohWrBTutsaJrTla4WER_e2SXa8DezHUXKpFhAyCq_H7by7CXzRoczYpIYre8aARCUvf61I3AYpticrL6VRgzkvOmNiuY",
    "c25sJBh2AzrrAmw-uj-G-9c-pevmRyaIJ3deHTLIu1cZ2zhURBmYLW0Yi8F4GyxoxczN7i_50TJ9wHKDRrO9ct6t6O_h5SAX2qU3",
    "ma6t_lHQU0jJ5APj6d_eS97HoAiNYEh5wLrXkaWGJe417w"
);

async fn example_server() -> (TestServer, Receiver) {
    let receiver = Receiver::start().await;
    let server = TestServer::builder()
        .frame(hubdouble_api::register_all)
        .seed_file(SEED)
        .env("HUBDOUBLE_APP_PUBLIC_KEY", fixture::APP_PUBLIC_KEY)
        .env("HUBDOUBLE_APP_WEBHOOK_URL", receiver.url())
        .env("HUBDOUBLE_APP_WEBHOOK_SECRET", WEBHOOK_SECRET)
        .env("HUBDOUBLE_USER_TOKEN", fixture::ALICE_TOKEN)
        .start()
        .await;
    (server, receiver)
}

fn json_body(answer: &Answer, operation: &str, status: StatusCode) -> Value {
    assert_eq!(answer.status, status, "{operation}: {:?}", answer.body);
    let body = answer.json();
    validate::openapi_response(operation, status.as_u16(), &body)
        .unwrap_or_else(|errors| panic!("{operation}: {errors:?}"));
    body
}

async fn get(client: &GoGithub, path: &str, operation: &str) -> Value {
    json_body(&client.get(path).await, operation, StatusCode::OK)
}

async fn install(server: &TestServer, app: &GoGithub, request: &Value) -> GoGithub {
    let token = json_body(
        &app.post(TOKEN_PATH, request).await,
        "apps/create-installation-access-token",
        StatusCode::CREATED,
    );
    GoGithub::new(server).with_token(token["token"].as_str().unwrap())
}

async fn file_at(client: &GoGithub, repo: &str, path: &str, sha: &str) -> String {
    let headers = HeaderMap::from_iter([(
        header::ACCEPT,
        HeaderValue::from_static("application/vnd.github.raw"),
    )]);
    let response = client
        .request(Method::GET, &format!("{repo}/contents/{path}?ref={sha}"))
        .headers(headers)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{repo}/{path}@{sha}");
    response.text().await.unwrap()
}

async fn main_sha(client: &GoGithub, repo: &str) -> String {
    let body = get(client, &format!("{repo}/git/ref/heads/main"), "git/get-ref").await;
    assert_eq!(body["object"]["type"], "commit");
    body["object"]["sha"].as_str().unwrap().to_owned()
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Catalog {
    releases: Vec<Release>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Release {
    tag: String,
    commit: String,
}

#[derive(Debug, PartialEq, Eq)]
struct Runtime {
    main: String,
    tags: Vec<String>,
    catalog: Catalog,
}

async fn runtime(client: &GoGithub) -> Runtime {
    let main = main_sha(client, RUNTIME).await;
    let catalog: Catalog =
        serde_json::from_str(&file_at(client, RUNTIME, ".github/releases.json", &main).await)
            .unwrap();
    let refs = get(
        client,
        &format!("{RUNTIME}/git/matching-refs/tags/v"),
        "git/list-matching-refs",
    )
    .await;
    let refs = refs.as_array().unwrap();
    assert_eq!(refs.len(), RELEASE_TAGS.len());
    assert_eq!(catalog.releases.len(), RELEASE_TAGS.len());
    let graphql = Graphql::new(client.clone());
    let mut tags = Vec::new();
    for (reference, tag) in refs.iter().zip(RELEASE_TAGS) {
        assert_eq!(reference["ref"], format!("refs/tags/{tag}"));
        assert_eq!(reference["object"]["type"], "tag");
        let oid = reference["object"]["sha"].as_str().unwrap();
        let commit = peel(&graphql, oid).await;
        assert!(catalog.releases.contains(&Release {
            tag: tag.to_owned(),
            commit: commit.clone(),
        }));
        assert_ne!(main, commit);
        tags.push(oid.to_owned());
    }
    assert_eq!(main_sha(client, RUNTIME).await, main);
    Runtime {
        main,
        tags,
        catalog,
    }
}

async fn peel(graphql: &Graphql, oid: &str) -> String {
    let reply = graphql
        .query(
            r"query($owner: String!, $name: String!, $oid: GitObjectID!) {
                repository(owner: $owner, name: $name) {
                    t0: object(oid: $oid) {
                        oid __typename ... on Tag { target { oid __typename } }
                    }
                }
            }",
            &json!({"owner": "octo-org", "name": "runner", "oid": oid}),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let body = reply.json();
    assert!(body.get("errors").is_none(), "{body}");
    let object = &body["data"]["repository"]["t0"];
    assert_eq!(object["oid"], oid);
    assert_eq!(object["__typename"], "Tag");
    assert_eq!(object["target"]["__typename"], "Commit");
    object["target"]["oid"].as_str().unwrap().to_owned()
}

async fn caller_pin(client: &GoGithub, main: &str, catalog: &Catalog) {
    let caller = file_at(client, TARGET, ".github/workflows/agent.yml", main).await;
    assert!(
        caller
            .lines()
            .any(|line| line.trim() == "workflow_dispatch: {}")
    );
    let pin = caller
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("uses: octo-org/runner/.github/workflows/agent.yml@")
        })
        .unwrap();
    let (commit, comment) = pin.split_once('#').unwrap();
    let commit = commit.trim();
    assert_eq!(commit.len(), 40);
    assert!(commit.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert!(catalog.releases.contains(&Release {
        tag: comment.trim().to_owned(),
        commit: commit.to_owned(),
    }));
    let workflow = file_at(client, RUNTIME, ".github/workflows/agent.yml", commit).await;
    assert!(workflow.lines().any(|line| line.trim() == "workflow_call:"));
}

#[test]
fn the_kubernetes_config_map_holds_the_example_seed_unchanged() {
    // Given
    let seed = std::fs::read_to_string(SEED).unwrap();
    let config_map = std::fs::read_to_string(CONFIG_MAP).unwrap();

    // When
    let (_, block) = config_map.split_once("  client-v5.yaml: |\n").unwrap();
    let embedded: String = block
        .lines()
        .flat_map(|line| [line.strip_prefix("    ").unwrap_or(line), "\n"])
        .collect();

    // Then
    assert_eq!(embedded, seed);
}

#[tokio::test]
async fn the_example_loads_the_same_release_pins_on_two_servers() {
    let (first, _) = example_server().await;
    let (second, _) = example_server().await;
    assert_eq!(snapshot(&first).await, snapshot(&second).await);
}

async fn snapshot(server: &TestServer) -> (Runtime, String) {
    let app = GoGithub::new(server).with_token(JWT);
    let installation = install(server, &app, &json!({})).await;
    let runtime = runtime(&installation).await;
    let target = main_sha(&installation, TARGET).await;
    caller_pin(&installation, &target, &runtime.catalog).await;
    (runtime, target)
}

#[tokio::test]
async fn the_example_installation_can_read_the_source_and_target() {
    let (server, _) = example_server().await;
    let app = GoGithub::new(&server).with_token(JWT);
    let install_list = get(&app, "app/installations", "apps/list-installations").await;
    assert_eq!(install_list.as_array().unwrap().len(), 1);
    let installed = &install_list[0];
    assert_eq!(installed["id"], 962_416);
    assert_eq!(installed["account"]["login"], "octo-org");
    assert_eq!(installed["repository_selection"], "selected");
    assert_eq!(installed["events"], json!(["issue_comment"]));
    assert_eq!(
        installed["permissions"],
        json!({
            "actions": "write", "actions_variables": "read", "checks": "write",
            "contents": "write", "issues": "write", "members": "read",
            "metadata": "read", "pull_requests": "write", "statuses": "write", "workflows": "write"
        })
    );
    let denied = app
        .post(
            TOKEN_PATH,
            &json!({"permissions": {"deployments": "write"}}),
        )
        .await;
    assert_eq!(denied.status, StatusCode::UNPROCESSABLE_ENTITY);

    let installation = install(&server, &app, &json!({})).await;
    let repos = get(
        &installation,
        "installation/repositories",
        "apps/list-repos-accessible-to-installation",
    )
    .await;
    assert_eq!(repos["total_count"], 3);
    for name in ["agent", "runner", "e2e"] {
        let install = get(
            &app,
            &format!("repos/octo-org/{name}/installation"),
            "apps/get-repo-installation",
        )
        .await;
        assert_eq!(install["id"], 962_416);
    }
    let org_installation = get(&app, "orgs/octo-org/installation", "apps/get-org-installation").await;
    assert_eq!(org_installation["id"], 962_416);

    let source = "repos/octo-org/app";
    let main = main_sha(&installation, source).await;
    let catalog: Catalog =
        serde_json::from_str(&file_at(&installation, source, ".github/releases.json", &main).await)
            .unwrap();
    assert_eq!(catalog.releases, Vec::<Release>::new());
    let refs = get(
        &installation,
        &format!("{source}/git/matching-refs/tags/agent/"),
        "git/list-matching-refs",
    )
    .await;
    assert_eq!(refs, json!([]));
    let missing = installation.get("repos/octo-org/public-app").await;
    let error = json_body(&missing, "repos/get", StatusCode::NOT_FOUND);
    assert!(error.get("errors").is_none());
    assert!(!missing.headers.contains_key("x-github-sso"));

    check_pull(&GoGithub::new(&server).with_token(fixture::ALICE_TOKEN)).await;
}

async fn check_pull(user: &GoGithub) {
    let pull = get(user, &format!("{TARGET}/pulls/1"), "pulls/get").await;
    assert_eq!(pull["number"], 1);
    assert_eq!(pull["state"], "open");
    assert_eq!(pull["base"]["ref"], "main");
    assert_eq!(pull["user"]["login"], USER);
    let files = get(user, &format!("{TARGET}/pulls/1/files"), "pulls/list-files").await;
    assert_eq!(files[0]["filename"], "guide.md");
    let commits = get(
        user,
        &format!("{TARGET}/pulls/1/commits"),
        "pulls/list-commits",
    )
    .await;
    assert_eq!(commits[0]["author"]["login"], USER);
    let base = pull["base"]["sha"].as_str().unwrap();
    let head = pull["head"]["sha"].as_str().unwrap();
    let comparison = get(
        user,
        &format!("{TARGET}/compare/{base}...{head}"),
        "repos/compare-commits",
    )
    .await;
    assert_eq!(comparison["files"][0]["filename"], "guide.md");
    assert_eq!(comparison["commits"][0]["author"]["login"], USER);
}

#[tokio::test]
async fn the_smoke_comment_reaches_the_app_and_the_user_can_read_its_reaction() {
    let (server, receiver) = example_server().await;
    let app = GoGithub::new(&server).with_token(JWT);
    let user = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    let profile = get(&app, "app", "apps/get-authenticated").await;
    assert_eq!(profile["id"], 1_000_001);
    assert_eq!(profile["slug"], "agent-local");
    let delivery_list = format!("{DELIVERY_PATH}?per_page=100");
    assert_eq!(
        get(&app, &delivery_list, "apps/list-webhook-deliveries").await,
        json!([])
    );

    let comment = json_body(
        &user
            .post(
                &format!("{TARGET}/issues/1/comments"),
                &json!({"body": "/agent review"}),
            )
            .await,
        "issues/create-comment",
        StatusCode::CREATED,
    );
    let comment_id = comment["id"].as_i64().unwrap();
    let drain = server.admin(Method::POST, "hooks/drain", None).await;
    assert_eq!(drain.status, StatusCode::NO_CONTENT);
    assert!(drain.body.is_empty());

    let deliveries = get(&app, &delivery_list, "apps/list-webhook-deliveries").await;
    assert_eq!(deliveries.as_array().unwrap().len(), 1);
    assert_eq!(deliveries[0]["event"], "issue_comment");
    assert_eq!(deliveries[0]["action"], "created");
    assert_eq!(deliveries[0]["status_code"], 200);
    let id = deliveries[0]["id"].as_i64().unwrap();
    let detail = get(
        &app,
        &format!("{DELIVERY_PATH}/{id}"),
        "apps/get-webhook-delivery",
    )
    .await;
    assert_eq!(detail["status_code"], 200);
    let payload = &detail["request"]["payload"];
    assert_eq!(payload["comment"]["id"], comment_id);
    assert_eq!(payload["issue"]["number"], 1);
    assert_eq!(payload["installation"]["id"], 962_416);
    let requests = receiver.received().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(*payload, requests[0].json());
    assert!(requests[0].signed_with(WEBHOOK_SECRET));

    let installation = install(
        &server,
        &app,
        &json!({"repositories": ["e2e"], "permissions": {"issues": "write"}}),
    )
    .await;
    let reactions = format!("{TARGET}/issues/comments/{comment_id}/reactions");
    let reaction = installation
        .post(&reactions, &json!({"content": "eyes"}))
        .await;
    json_body(
        &reaction,
        "reactions/create-for-issue-comment",
        StatusCode::CREATED,
    );
    let listed = get(&user, &reactions, "reactions/list-for-issue-comment").await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["content"], "eyes");
    assert_eq!(listed[0]["user"]["type"], "Bot");
    let slug = profile["slug"].as_str().unwrap();
    assert_eq!(listed[0]["user"]["login"], format!("{slug}[bot]"));
}

#[tokio::test]
async fn the_user_can_read_the_one_run_created_by_the_app() {
    let (server, _) = example_server().await;
    let user = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    let app = GoGithub::new(&server).with_token(JWT);
    let workflow = format!("{TARGET}/actions/workflows/agent.yml");
    let runs = format!("{workflow}/runs?per_page=100");
    let launch = format!("{workflow}/dispatches");
    let installation = install(
        &server,
        &app,
        &json!({
            "repositories": ["e2e"],
            "permissions": {"actions": "write", "actions_variables": "read"}
        }),
    )
    .await;
    let trigger = get(
        &installation,
        &format!("{TARGET}/actions/variables/AGENT_TRIGGER_PHRASE"),
        "actions/get-repo-variable",
    )
    .await;
    assert_eq!(trigger["value"], "");

    let rejected = installation
        .post(
            &launch,
            &json!({"ref": "main", "inputs": {"pr": "1"}, "return_run_details": true}),
        )
        .await;
    assert_eq!(rejected.status, StatusCode::UNPROCESSABLE_ENTITY);
    let empty = get(&user, &runs, "actions/list-workflow-runs").await;
    assert_eq!(empty, json!({"total_count": 0, "workflow_runs": []}));

    let dispatched = json_body(
        &installation
            .post(&launch, &json!({"ref": "main", "return_run_details": true}))
            .await,
        "actions/create-workflow-dispatch",
        StatusCode::OK,
    );
    let list = get(&user, &runs, "actions/list-workflow-runs").await;
    assert_eq!(list["total_count"], 1);
    let run = &list["workflow_runs"][0];
    assert_eq!(run["id"], dispatched["workflow_run_id"]);
    assert_eq!(run["path"], ".github/workflows/agent.yml");
    assert_eq!(run["head_branch"], "main");
    assert_eq!(run["status"], "queued");
    assert_eq!(run["event"], "workflow_dispatch");
}

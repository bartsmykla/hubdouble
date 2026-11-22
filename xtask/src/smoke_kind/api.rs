use std::time::Duration;

use reqwest::header::CONTENT_TYPE;
use reqwest::{Client, Method, Response, StatusCode};
use serde_json::{Value, json};

use super::manifests::Credentials;
use super::tokens::{self, AppKey};
use super::{Result, require};

const REPO: &str = "/repos/octo-org/e2e";
const WORKFLOW: &str = "/repos/octo-org/e2e/actions/workflows/agent.yml";
const REVIEW: &str = "Smoke review";
const ADD_THREAD: &str = r#"
    mutation($id: ID!, $body: String!) {
      addPullRequestReviewThread(input: {
        pullRequestId: $id, path: "guide.md", line: 1, side: RIGHT, body: $body
      }) { thread { id path isResolved comments(first: 1) { nodes { body } } } }
    }
"#;
const READ_THREADS: &str = r#"
    query($number: Int!) {
      repository(owner: "octo-org", name: "e2e") {
        pullRequest(number: $number) { reviewThreads(first: 10) { nodes { id } } }
      }
    }
"#;

struct Api {
    client: Client,
    base: String,
}

impl Api {
    async fn request(
        &self,
        method: Method,
        path: &str,
        status: StatusCode,
        credential: Option<&str>,
        body: Option<Value>,
    ) -> Result<Response> {
        let mut request = self
            .client
            .request(method.clone(), format!("{}{path}", self.base))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2026-03-10");
        if let Some(credential) = credential {
            request = request.bearer_auth(credential);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await?;
        require(
            response.status() == status,
            &format!(
                "{method} {path} returned {}, expected {status}",
                response.status()
            ),
        )?;
        Ok(response)
    }

    async fn get(&self, path: &str, credential: Option<&str>) -> Result<Value> {
        json_response(
            self.request(Method::GET, path, StatusCode::OK, credential, None)
                .await?,
        )
        .await
    }

    async fn post(
        &self,
        path: &str,
        status: StatusCode,
        credential: &str,
        body: Value,
    ) -> Result<Value> {
        json_response(
            self.request(Method::POST, path, status, Some(credential), Some(body))
                .await?,
        )
        .await
    }

    async fn graphql(&self, user: &str, query: &str, variables: Value) -> Result<Value> {
        let mut reply = self
            .post(
                "/graphql",
                StatusCode::OK,
                user,
                json!({"query": query, "variables": variables}),
            )
            .await?;
        if let Some(errors) = reply.get("errors") {
            require(
                errors.as_array().is_some_and(Vec::is_empty),
                &format!("GraphQL errors: {errors}"),
            )?;
        }
        Ok(reply
            .get_mut("data")
            .ok_or("GraphQL response has no data")?
            .take())
    }
}

async fn json_response(response: Response) -> Result<Value> {
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|header| header.to_str().ok())
        .and_then(|value| value.split(';').next());
    require(
        content_type.is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json")),
        "Expected an application/json response",
    )?;
    Ok(response.json().await?)
}

pub(super) async fn check(port: u16, app: &AppKey, data: &Credentials) -> Result<()> {
    let api = Api {
        client: Client::builder()
            .timeout(Duration::from_secs(15))
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        base: format!("http://127.0.0.1:{port}"),
    };
    let ready = api
        .request(Method::GET, "/readyz", StatusCode::OK, None, None)
        .await?;
    require(
        ready.bytes().await?.as_ref() == b"ok",
        "Unexpected readiness response",
    )?;
    let token = installation(&api, app).await?;
    let pull = api.get(&format!("{REPO}/pulls/1"), Some(&token)).await?;
    require(
        pull["number"] == 1
            && pull["state"] == "open"
            && pull["head"]["ref"] == "add-review-guide"
            && pull["base"]["ref"] == "main",
        "Unexpected seeded pull request",
    )?;
    check_review(&api, &pull, &data.user).await?;
    check_run(&api, &token, text(&pull["head"], "sha")?).await?;
    let dispatched = dispatch(&api, &token, &data.user).await?;
    let pin = workflow_pin(&api, &token).await?;
    check_oidc(&api, &data.admin, &dispatched, &pin).await
}

async fn installation(api: &Api, app: &AppKey) -> Result<String> {
    const PATH: &str = "/app/installations/962416/access_tokens";
    let jwt = app.jwt(tokens::now()?)?;
    api.request(
        Method::POST,
        PATH,
        StatusCode::UNAUTHORIZED,
        None,
        Some(json!({})),
    )
    .await?;
    let token = api.post(PATH, StatusCode::CREATED, &jwt, json!({})).await?;
    Ok(text(&token, "token")?.to_owned())
}

async fn check_review(api: &Api, pull: &Value, user: &str) -> Result<()> {
    let answer = api
        .graphql(
            user,
            ADD_THREAD,
            json!({"id": pull["node_id"], "body": REVIEW}),
        )
        .await?;
    let thread = &answer["addPullRequestReviewThread"]["thread"];
    let id = text(thread, "id")?;
    require(
        thread["path"] == "guide.md"
            && thread["isResolved"] == false
            && thread["comments"]["nodes"] == json!([{"body": REVIEW}]),
        "The review thread does not match the request",
    )?;
    let answer = api
        .graphql(user, READ_THREADS, json!({"number": pull["number"]}))
        .await?;
    let threads = &answer["repository"]["pullRequest"]["reviewThreads"]["nodes"];
    require(
        threads
            .as_array()
            .is_some_and(|threads| threads.contains(&json!({"id": id}))),
        "The new review thread is absent from the pull request",
    )
}

async fn check_run(api: &Api, token: &str, head_sha: &str) -> Result<()> {
    let created = api
        .post(
            &format!("{REPO}/check-runs"),
            StatusCode::CREATED,
            token,
            json!({"name": "hubdouble smoke", "head_sha": head_sha, "status": "in_progress"}),
        )
        .await?;
    let id = number(&created, "id")?;
    let read = api
        .get(&format!("{REPO}/check-runs/{id}"), Some(token))
        .await?;
    require(
        read["id"] == id
            && read["head_sha"] == head_sha
            && read["status"] == "in_progress"
            && read["name"] == "hubdouble smoke",
        "The check run does not match the request",
    )
}

async fn dispatch(api: &Api, token: &str, user: &str) -> Result<Value> {
    let launch = api
        .post(
            &format!("{WORKFLOW}/dispatches"),
            StatusCode::OK,
            token,
            json!({"ref": "main", "return_run_details": true}),
        )
        .await?;
    let runs = api.get(&format!("{WORKFLOW}/runs"), Some(user)).await?;
    let Some([run]) = runs["workflow_runs"].as_array().map(Vec::as_slice) else {
        return Err("Expected exactly one workflow run".into());
    };
    require(
        runs["total_count"] == 1
            && run["id"] == number(&launch, "workflow_run_id")?
            && run["head_branch"] == "main"
            && run["path"] == ".github/workflows/agent.yml"
            && run["status"] == "queued"
            && run["event"] == "workflow_dispatch",
        "Unexpected dispatched workflow run",
    )?;
    Ok(run.clone())
}

async fn workflow_pin(api: &Api, token: &str) -> Result<String> {
    const RUNTIME: &str = "/repos/octo-org/runner/git";
    let tag = api
        .get(&format!("{RUNTIME}/ref/tags/v5.1.0"), Some(token))
        .await?;
    let object = &tag["object"];
    require(
        object["type"] == "tag",
        "Expected an annotated workflow pin tag",
    )?;
    let path = format!("{RUNTIME}/tags/{}", text(object, "sha")?);
    let tag = api.get(&path, Some(token)).await?;
    let pin = &tag["object"];
    require(
        pin["type"] == "commit",
        "Expected the workflow pin to point to a commit",
    )?;
    Ok(text(pin, "sha")?.to_owned())
}

async fn check_oidc(api: &Api, admin: &str, run: &Value, pin: &str) -> Result<()> {
    const ISSUER: &str = "http://hubdouble:8000/_actions";
    const JWKS_PATH: &str = "/_actions/.well-known/jwks";
    const TOKEN_PATH: &str = "/_actions/token?api-version=2.0&audience=hubdouble-smoke";
    let discovery = api
        .get("/_actions/.well-known/openid-configuration", None)
        .await?;
    require(discovery["issuer"] == ISSUER, "Unexpected OIDC issuer")?;
    require(
        discovery["jwks_uri"] == format!("{ISSUER}/.well-known/jwks"),
        "Unexpected JWKS URL",
    )?;
    let jwks = api.get(JWKS_PATH, None).await?;
    let run_id = number(run, "id")?;
    let runner = api
        .post(
            "/_hubdouble/actions/oidc/token",
            StatusCode::OK,
            admin,
            json!({"run_id": run_id}),
        )
        .await?;
    require(
        runner["request_url"] == format!("{ISSUER}/token?api-version=2.0"),
        "Unexpected OIDC token URL",
    )?;
    api.request(
        Method::GET,
        TOKEN_PATH,
        StatusCode::UNAUTHORIZED,
        None,
        None,
    )
    .await?;
    let token = api
        .get(TOKEN_PATH, Some(text(&runner, "request_token")?))
        .await?;
    let claims = tokens::verify(text(&token, "value")?, &jwks, tokens::now()?)?;
    for (name, value) in [
        ("aud", "hubdouble-smoke"),
        ("iss", ISSUER),
        ("sub", "repo:octo-org/e2e:ref:refs/heads/main"),
        ("repository", "octo-org/e2e"),
        ("event_name", "workflow_dispatch"),
        ("run_id", &run_id.to_string()),
        ("sha", text(run, "head_sha")?),
        (
            "job_workflow_ref",
            &format!("octo-org/runner/.github/workflows/agent.yml@{pin}"),
        ),
    ] {
        require(
            claims[name] == value,
            &format!("Incorrect OIDC claim: {name}"),
        )?;
    }
    eprintln!("Verified the runner OIDC token against the discovered JWKS");
    Ok(())
}

fn text<'a>(body: &'a Value, field: &str) -> Result<&'a str> {
    body[field]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("Missing string field: {field}").into())
}

fn number(body: &Value, field: &str) -> Result<u64> {
    body[field]
        .as_u64()
        .ok_or_else(|| format!("Missing numeric field: {field}").into())
}

#![expect(
    clippy::unwrap_used,
    reason = "tests fail immediately when an answer and its contract differ"
)]

use http::{HeaderMap, HeaderValue, StatusCode};
use hubdouble_testkit::fixture::{self, ALICE, ALICE_TOKEN};
use hubdouble_testkit::{GoGithub, Graphql, TestServer};
use serde_json::{Value, json};

const REPO: &str = "repos/acme/public";

async fn whole_server(extra: &[Value]) -> TestServer {
    let mut builder = TestServer::builder().frame(hubdouble_api::register_all);
    for entry in fixture::seed().into_iter().chain(extra.iter().cloned()) {
        builder = builder.entry(entry);
    }
    builder.start().await
}

async fn tip(client: &GoGithub, branch: &str) -> String {
    let answer = client.get(&format!("{REPO}/git/ref/heads/{branch}")).await;
    assert_eq!(answer.status, StatusCode::OK);
    answer.json()["object"]["sha"].as_str().unwrap().to_owned()
}

fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.insert(*name, HeaderValue::from_static(value));
    }
    map
}

#[tokio::test]
async fn graphql_ignores_the_rest_api_version_and_answers_the_v4_media_type() {
    // Given
    let server = TestServer::builder()
        .frame(hubdouble_api::register_all)
        .fixture()
        .start()
        .await;
    let alice = Graphql::new(GoGithub::new(&server).with_token(ALICE_TOKEN));
    let query = "{ viewer { login } }";

    // When the client sends a version GitHub does not publish, as live gh api graphql did
    let unknown = alice
        .query_headers(
            query,
            &json!({}),
            headers(&[("x-github-api-version", "1999-01-01"), ("accept", "*/*")]),
        )
        .await;
    let plain_json = alice
        .query_headers(
            query,
            &json!({}),
            headers(&[("accept", "application/json")]),
        )
        .await;
    let go_github = alice.query(query, &json!({})).await;

    // Then GitHub answers 200 with the v4 media type, no selected version and its scope headers
    assert_eq!(unknown.status, StatusCode::OK);
    assert_eq!(unknown.json()["data"]["viewer"]["login"], ALICE);
    assert_eq!(
        unknown.header("x-github-media-type"),
        "github.v4; format=json"
    );
    assert!(
        !unknown
            .headers
            .contains_key("x-github-api-version-selected")
    );
    assert_eq!(unknown.header("x-ratelimit-resource"), "graphql");
    assert_eq!(unknown.header("x-accepted-oauth-scopes"), "repo");
    assert_ne!(unknown.header("x-oauth-scopes"), "");
    assert_ne!(unknown.header("x-github-request-id"), "");
    assert_eq!(plain_json.header("x-github-media-type"), "github.v4");
    assert_eq!(
        go_github.header("x-github-media-type"),
        "github.v3; format=json"
    );
    assert!(
        !go_github
            .headers
            .contains_key("x-github-api-version-selected")
    );
}

#[tokio::test]
async fn a_commit_status_is_validated_as_github_validates_a_status() {
    // Given
    let server = whole_server(&[]).await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let head = tip(&alice, "main").await;
    let path = format!("{REPO}/statuses/{head}");
    let longest: String = "é".repeat(140);
    let too_long: String = "a".repeat(141);

    // When the client writes statuses that GitHub refuses, as the samples in the plan show
    let long = alice
        .post(&path, &json!({"state": "success", "description": too_long}))
        .await;
    let relative = alice
        .post(
            &path,
            &json!({"state": "success", "target_url": "key-concepts/"}),
        )
        .await;
    let no_state = alice.post(&path, &json!({"context": "ci"})).await;
    let emoji = alice
        .post(&path, &json!({"state": "success", "context": "ci 🚀"}))
        .await;
    let listed = alice.get(&format!("{REPO}/commits/{head}/statuses")).await;
    let fits = alice
        .post(
            &path,
            &json!({"state": "success", "description": longest, "target_url": "HTTPS://ci.example/1"}),
        )
        .await;

    // Then each refusal is a 422 of the Status model and writes nothing
    for answer in [&long, &relative] {
        assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(answer.json()["errors"].is_string(), "{}", answer.json());
    }
    for (answer, field) in [(&no_state, "state"), (&emoji, "context")] {
        assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY);
        let error = &answer.json()["errors"][0];
        assert_eq!(error["resource"], "Status");
        assert_eq!(error["code"], "custom");
        assert_eq!(error["field"], field);
    }
    assert_eq!(listed.json(), json!([]));
    assert_eq!(fits.status, StatusCode::CREATED);
}

#[tokio::test]
async fn status_contexts_match_without_case_and_keep_their_spelling() {
    // Given a ruleset on main that requires the context CI/Build
    let rules = json!({"kind": "ruleset", "repository": "acme/public", "name": "rules",
        "target": "branch", "enforcement": "active", "bypass_actors": [],
        "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}},
        "rules": [{"type": "required_status_checks", "parameters": {
            "strict_required_status_checks_policy": false,
            "required_status_checks": [{"context": "CI/Build"}]}}]});
    let server = whole_server(&[rules]).await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let head = tip(&alice, "feature").await;
    let path = format!("{REPO}/statuses/{head}");

    // When a pending CI/Build is followed by a successful ci/build on the head
    for (state, context) in [("pending", "CI/Build"), ("success", "ci/build")] {
        let made = alice
            .post(&path, &json!({"state": state, "context": context}))
            .await;
        assert_eq!(made.status, StatusCode::CREATED);
    }
    let combined = alice.get(&format!("{REPO}/commits/{head}/status")).await;
    let merged = alice
        .post(
            &format!("{REPO}/merges"),
            &json!({"base": "main", "head": "feature"}),
        )
        .await;

    // Then both are one context, shown as the newest status spelled it, and it passes the rule
    let combined = combined.json();
    assert_eq!(combined["total_count"], 1);
    assert_eq!(combined["state"], "success");
    assert_eq!(combined["statuses"][0]["context"], "ci/build");
    assert_eq!(merged.status, StatusCode::CREATED, "{}", merged.json());
}

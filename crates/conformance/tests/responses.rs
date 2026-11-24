#![expect(
    clippy::unwrap_used,
    reason = "tests fail immediately when an answer and its contract differ"
)]

use http::{HeaderMap, HeaderValue, StatusCode};
use hubdouble_testkit::fixture::{ALICE, ALICE_TOKEN};
use hubdouble_testkit::{GoGithub, Graphql, TestServer};
use serde_json::json;

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
    assert!(!unknown.header("x-oauth-scopes").is_empty());
    assert!(!unknown.header("x-github-request-id").is_empty());
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

use std::sync::LazyLock;

use http::StatusCode;
use hubdouble_gql::Schema;
use serde_json::Value;

use crate::Answer;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| {
    Schema::load().unwrap_or_else(|e| panic!("load GitHub's GraphQL schema: {e}"))
});

pub(crate) fn check(query: &str, variables: &Value, answer: &Answer) {
    assert!(
        !answer.headers.contains_key("x-github-api-version-selected"),
        "GitHub's GraphQL API selects no REST API version"
    );
    assert_eq!(answer.header("x-ratelimit-resource"), "graphql");
    let body = answer.json();
    if answer.status == StatusCode::OK {
        if let Err(error) = SCHEMA.check_answer(query, variables, &body) {
            panic!("{error}\nquery: {query}\nanswer: {body}");
        }
    } else {
        assert!(
            body["message"].as_str().is_some_and(|s| !s.is_empty())
                && body.get("data").is_none()
                && body.get("errors").is_none(),
            "invalid GraphQL HTTP error {}: {body}",
            answer.status,
        );
    }
}

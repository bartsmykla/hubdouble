use http::{Method, StatusCode};
use hubdouble_testkit::{GoGithub, TestServer, fixture};
use serde_json::{Value, json};

const REPO: &str = "repos/acme/public";

async fn start() -> (TestServer, GoGithub) {
    let server = TestServer::builder()
        .frame(hubdouble_api::register_all)
        .fixture()
        .start()
        .await;
    let client = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    (server, client)
}

async fn created(client: &GoGithub, path: &str, body: &Value) -> Value {
    let answer = client.post(path, body).await;
    assert_eq!(answer.status, StatusCode::CREATED, "{path}");
    answer.json()
}

async fn pull(client: &GoGithub) -> u64 {
    created(
        client,
        &format!("{REPO}/pulls"),
        &json!({"title": "Feature", "head": "feature", "base": "main"}),
    )
    .await["number"]
        .as_u64()
        .unwrap_or_else(|| panic!("the pull request has no number"))
}

#[tokio::test]
async fn a_blob_sha_that_is_not_40_hex_is_refused() {
    // Given
    let (_server, client) = start().await;

    // When a blob is read by a SHA that is not 40 hex
    let answer = client.get(&format!("{REPO}/git/blobs/zzzz")).await;

    // Then GitHub answers 422 (gh api repos/cli/cli/git/blobs/zzzz, 2026-11-22)
    assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn the_repository_comment_list_refuses_a_since_that_is_not_a_timestamp() {
    // Given
    let (_server, client) = start().await;

    // When the repository's comments are listed since a value that is not a timestamp
    let answer = client
        .get(&format!("{REPO}/issues/comments?since=notadate"))
        .await;

    // Then GitHub answers 422 (gh api repos/cli/cli/issues/comments?since=notadate, 2026-11-22)
    assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn writes_refuse_a_body_that_breaks_the_operation_schema() {
    // Given an issue and a pull request
    let (_server, client) = start().await;
    let issue =
        created(&client, &format!("{REPO}/issues"), &json!({"title": "Bug"})).await["number"]
            .as_u64()
            .unwrap();
    let number = pull(&client).await;

    // When each body leaves out a required member or breaks a constraint of the pinned schema
    let cases = [
        (
            Method::POST,
            format!("{REPO}/issues/{issue}/comments"),
            json!({}),
        ),
        (
            Method::POST,
            format!("{REPO}/issues/{issue}/labels"),
            json!({"labels": "bug"}),
        ),
        (
            Method::POST,
            format!("{REPO}/issues/{issue}/labels"),
            json!({"labels": []}),
        ),
        (
            Method::POST,
            format!("{REPO}/merges"),
            json!({"base": "main"}),
        ),
        (
            Method::POST,
            format!("{REPO}/pulls/{number}/comments"),
            json!({"body": "a note"}),
        ),
    ];
    for (method, path, body) in cases {
        let answer = client.call_json(method.clone(), &path, &body).await;

        // Then GitHub refuses each with 422
        assert_eq!(
            answer.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{method} {path} {body}"
        );
    }
}

#[tokio::test]
async fn a_review_request_for_a_user_who_is_not_a_collaborator_is_refused() {
    // Given a pull request and carol, who is neither an org member nor a collaborator
    let (_server, client) = start().await;
    let number = pull(&client).await;

    // When carol is asked to review
    let answer = client
        .post(
            &format!("{REPO}/pulls/{number}/requested_reviewers"),
            &json!({"reviewers": [fixture::CAROL]}),
        )
        .await;

    // Then GitHub answers 422, as pulls/request-reviewers documents
    assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY);
}

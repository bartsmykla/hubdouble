use http::StatusCode;
use hubdouble_testkit::{GoGithub, TestServer, fixture};

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

#[tokio::test]
async fn a_blob_sha_that_is_not_40_hex_is_refused() {
    // Given
    let (_server, client) = start().await;

    // When a blob is read by a SHA that is not 40 hex
    let answer = client.get(&format!("{REPO}/git/blobs/zzzz")).await;

    // Then GitHub answers 422 (gh api repos/cli/cli/git/blobs/zzzz, 2026-11-22)
    assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY);
}

use http::Method;
use hubdouble_testkit::{GoGithub, TestServer};
use serde_json::json;

#[tokio::test]
async fn a_request_to_the_seeded_server_shows_in_the_request_log() {
    // Given
    let server = TestServer::builder().fixture().start().await;
    let client = GoGithub::new(&server);
    // When
    let answer = client.get("repos/acme/public").await;
    let log = server.admin(Method::GET, "log", None).await.json();
    // Then
    let entries = log["entries"].as_array().unwrap();
    let entry = entries
        .iter()
        .find(|e| e["path"] == json!("/repos/acme/public"))
        .unwrap_or_else(|| panic!("no log entry in {entries:?}"));
    assert_eq!(entry["method"], json!("GET"));
    assert_eq!(entry["operation_id"], json!("repos/get"));
    assert_eq!(entry["status"], json!(answer.status.as_u16()));
}

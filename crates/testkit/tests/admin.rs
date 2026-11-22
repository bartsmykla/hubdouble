use http::{Method, StatusCode};
use hubdouble_testkit::fixture::{self, ALICE_TOKEN, APP_SLUG};
use hubdouble_testkit::{GoGithub, TestServer};
use serde_json::json;

#[tokio::test]
async fn the_admin_api_needs_the_admin_token() {
    // Given
    let server = TestServer::builder().fixture().start().await;
    let without = reqwest::Client::new()
        .get(format!("{}/_hubdouble/log", server.base))
        .send()
        .await
        .unwrap();
    let wrong = reqwest::Client::new()
        .get(format!("{}/_hubdouble/log", server.base))
        .bearer_auth(ALICE_TOKEN)
        .send()
        .await
        .unwrap();
    // When
    let with = server.admin(Method::GET, "log", None).await;
    // Then
    assert_eq!(without.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(with.status, StatusCode::OK);
}

#[tokio::test]
async fn the_fixture_app_gets_its_bot_and_the_receiver_as_its_webhook() {
    let server = TestServer::builder().fixture().start().await;

    let app = server
        .admin(Method::GET, &format!("app/{APP_SLUG}"), None)
        .await
        .json();

    assert_eq!(app["id"], json!(fixture::APP_ID));
    assert_eq!(app["webhook"]["url"], json!(server.receiver.url()));
    assert_eq!(app["bot"]["login"], json!(format!("{APP_SLUG}[bot]")));
    assert_eq!(app["public_key"], json!(fixture::APP_PUBLIC_KEY));
}

#[tokio::test]
async fn a_kind_posted_after_the_seed_takes_a_free_id_and_a_used_id_fails() {
    // Given
    let server = TestServer::builder().fixture().start().await;
    // When
    let created = server
        .admin(
            Method::POST,
            "user",
            Some(&json!({"login": "dave", "tokens": [{}]})),
        )
        .await;
    let taken = server
        .admin(
            Method::POST,
            "user",
            Some(&json!({"id": fixture::ALICE_ID, "login": "erin"})),
        )
        .await;
    let dave = server.admin(Method::GET, "user/dave", None).await.json();
    // Then
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(taken.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(taken.json()["errors"][0]["field"], json!("id"));
    assert!(dave["id"].as_i64().unwrap() > fixture::INSTALLATION_ID);
    assert_eq!(
        dave["tokens"][0]["scopes"],
        json!(["repo", "workflow", "read:org", "user"])
    );
    assert!(
        dave["tokens"][0]["token"]
            .as_str()
            .unwrap()
            .starts_with("ghp_")
    );
}

#[tokio::test]
async fn the_log_names_each_request_and_reset_makes_old_cursors_gone() {
    // Given
    let server = TestServer::builder().fixture().start().await;
    let client = GoGithub::new(&server);
    // When
    let zen = client.get("zen").await;
    let entry = server
        .admin(Method::GET, "log/wait?after=0&match=GET%20/zen", None)
        .await
        .json();
    let reset = server.admin(Method::POST, "reset", None).await;
    let old = server.admin(Method::GET, "log?after=0", None).await;
    let fresh = server.admin(Method::GET, "log", None).await.json();
    // Then
    assert_eq!(entry["path"], json!("/zen"));
    assert_eq!(entry["operation_id"], json!("meta/get-zen"));
    assert_eq!(entry["status"], json!(zen.status.as_u16()));
    assert_eq!(reset.status, StatusCode::NO_CONTENT);
    assert_eq!(old.status, StatusCode::GONE);
    assert_eq!(fresh["entries"], json!([]));
}

#[tokio::test]
async fn a_counted_fault_answers_that_many_requests_then_stops() {
    // Given
    let server = TestServer::builder().fixture().start().await;
    let added = server
        .admin(
            Method::POST,
            "faults",
            Some(&json!({"method": "GET", "path": "/zen", "status": 503,
                         "headers": {"retry-after": "7"}, "body": {"message": "down"},
                         "count": 1})),
        )
        .await;
    let client = GoGithub::new(&server);
    // When
    let first = client.get("zen").await;
    let second = client.get("zen").await;
    // Then
    assert_eq!(added.status, StatusCode::CREATED);
    assert_eq!(first.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(first.header("retry-after"), "7");
    assert_eq!(first.json(), json!({"message": "down"}));
    assert_ne!(second.status, StatusCode::SERVICE_UNAVAILABLE);
    let faults = server.admin(Method::GET, "faults", None).await.json();
    assert_eq!(faults, json!([]));
}

#[tokio::test]
async fn drain_and_reset_answer_no_content() {
    let server = TestServer::builder().fixture().start().await;

    let drained = server.admin(Method::POST, "hooks/drain", None).await;
    let reset = server.admin(Method::POST, "reset", None).await;

    assert_eq!(drained.status, StatusCode::NO_CONTENT);
    assert_eq!(reset.status, StatusCode::NO_CONTENT);
}

use http::{Method, StatusCode};
use hubdouble_testkit::fixture::{self, ALICE_TOKEN, PUBLIC_REPO_ID};
use hubdouble_testkit::{Answer, GoGithub, Graphql, TestServer};
use serde_json::{Value, json};

const FOLLOW_REDIRECTS: &str =
    "https://docs.github.com/rest/guides/best-practices-for-using-the-rest-api#follow-redirects";

async fn server() -> TestServer {
    let mut builder = TestServer::builder().frame(hubdouble_api::register_all);
    for entry in fixture::seed() {
        builder = builder.entry(entry);
    }
    builder.start().await
}

async fn admin(server: &TestServer, method: Method, path: &str, body: Option<&Value>) {
    let answer = server.admin(method, path, body).await;
    assert!(
        answer.status.is_success(),
        "{path}: {} {}",
        answer.status,
        String::from_utf8_lossy(&answer.body)
    );
}

fn assert_redirect(answer: &Answer, status: StatusCode, location: &str) {
    assert_eq!(answer.status, status);
    assert_eq!(answer.header("location"), location);
    let body = answer.json();
    assert_eq!(body["url"], location);
    assert_eq!(body["documentation_url"], FOLLOW_REDIRECTS);
}

#[tokio::test]
async fn an_old_name_redirects_reads_and_writes_to_the_repository_id() {
    let server = server().await;
    let client = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let by_id = format!("{}repositories/{PUBLIC_REPO_ID}", client.base());

    // Given
    admin(
        &server,
        Method::PATCH,
        "repos/acme/public",
        Some(&json!({"name": "shiny"})),
    )
    .await;

    // When
    let read = client.get("repos/acme/public").await;
    let listed = client.get("repos/ACME/Public/issues?state=all").await;
    let written = client
        .post("repos/acme/public/issues", &json!({"title": "Lost"}))
        .await;

    // Then
    assert_redirect(&read, StatusCode::MOVED_PERMANENTLY, &by_id);
    assert_eq!(read.json()["message"], "Moved Permanently");
    assert_redirect(
        &listed,
        StatusCode::MOVED_PERMANENTLY,
        &format!("{by_id}/issues?state=all"),
    );
    assert_redirect(
        &written,
        StatusCode::TEMPORARY_REDIRECT,
        &format!("{by_id}/issues"),
    );
    let followed = client
        .get(&format!("repositories/{PUBLIC_REPO_ID}"))
        .await
        .json();
    assert_eq!(followed["full_name"], "acme/shiny");
    let issues = client.get("repos/acme/shiny/issues?state=all").await.json();
    assert!(
        issues
            .as_array()
            .is_some_and(|all| all.iter().all(|i| i["title"] != "Lost"))
    );
    assert_eq!(
        client.get("repos/acme/elsewhere").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn an_old_name_of_a_hidden_repository_answers_not_found() {
    let server = server().await;

    // Given
    admin(
        &server,
        Method::PATCH,
        "repos/acme/private",
        Some(&json!({"name": "secret"})),
    )
    .await;

    // When
    let anonymous = GoGithub::new(&server).get("repos/acme/private").await;
    let member = GoGithub::new(&server)
        .with_token(ALICE_TOKEN)
        .get("repos/acme/private")
        .await;

    // Then
    assert_eq!(anonymous.status, StatusCode::NOT_FOUND);
    assert_eq!(member.status, StatusCode::MOVED_PERMANENTLY);
}

#[tokio::test]
async fn a_transfer_and_an_owner_rename_keep_every_old_name() {
    let server = server().await;
    let client = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let by_id = format!("{}repositories/{PUBLIC_REPO_ID}", client.base());

    // Given
    admin(
        &server,
        Method::PATCH,
        "repos/acme/public",
        Some(&json!({"owner": "alice"})),
    )
    .await;
    admin(
        &server,
        Method::PATCH,
        "installation-target/alice",
        Some(&json!({"login": "alice-next"})),
    )
    .await;

    // When
    let first = client.get("repos/acme/public").await;
    let second = client.get("repos/alice/public").await;
    let current = client.get("repos/alice-next/public").await;

    // Then
    assert_redirect(&first, StatusCode::MOVED_PERMANENTLY, &by_id);
    assert_redirect(&second, StatusCode::MOVED_PERMANENTLY, &by_id);
    assert_eq!(current.status, StatusCode::OK);
    assert_eq!(current.json()["full_name"], "alice-next/public");
}

#[tokio::test]
async fn a_new_repository_takes_an_old_name_back() {
    let server = server().await;
    let client = GoGithub::new(&server).with_token(ALICE_TOKEN);

    // Given
    admin(
        &server,
        Method::PATCH,
        "repos/acme/public",
        Some(&json!({"name": "shiny"})),
    )
    .await;
    admin(
        &server,
        Method::POST,
        "repository",
        Some(
            &json!({"owner": "acme", "name": "public", "visibility": "public",
                     "files": {"README.md": "# new\n"}}),
        ),
    )
    .await;
    let taken = client.get("repos/acme/public").await;

    // When
    admin(&server, Method::DELETE, "repos/acme/public", None).await;

    // Then
    assert_eq!(taken.status, StatusCode::OK);
    assert_ne!(taken.json()["id"], PUBLIC_REPO_ID);
    assert_eq!(
        client.get("repos/acme/public").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn graphql_follows_renames_unless_asked_not_to() {
    let server = server().await;
    let graphql = Graphql::new(GoGithub::new(&server).with_token(ALICE_TOKEN));
    let query = "query($follow: Boolean!) { repository(owner: \"acme\", name: \"public\", \
                 followRenames: $follow) { nameWithOwner } }";

    // Given
    admin(
        &server,
        Method::PATCH,
        "repos/acme/public",
        Some(&json!({"name": "shiny"})),
    )
    .await;

    // When
    let followed = graphql.query(query, &json!({"follow": true})).await.json();
    let refused = graphql.query(query, &json!({"follow": false})).await.json();

    // Then
    assert_eq!(
        followed["data"]["repository"]["nameWithOwner"],
        "acme/shiny"
    );
    assert_eq!(refused["data"]["repository"], Value::Null);
    assert_eq!(refused["errors"][0]["type"], "NOT_FOUND");
}

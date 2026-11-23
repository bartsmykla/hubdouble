use http::{Method, StatusCode};
use hubdouble_model::Timestamp;
use hubdouble_render::query_escape;
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

#[tokio::test]
async fn a_repeated_body_key_takes_its_last_value_on_every_route() {
    // Given an issue and the tip of main
    let (_server, client) = start().await;
    let issue =
        created(&client, &format!("{REPO}/issues"), &json!({"title": "Bug"})).await["number"]
            .as_u64()
            .unwrap();
    let main = client
        .get(&format!("{REPO}/git/ref/heads/main"))
        .await
        .json();
    let sha = main["object"]["sha"].as_str().unwrap().to_owned();

    // When an issue edit, a commit status and a ref create each repeat a key
    let cases = [
        (
            Method::PATCH,
            format!("{REPO}/issues/{issue}"),
            r#"{"state": "open", "state": "closed"}"#.to_owned(),
            StatusCode::OK,
            "state",
            "closed",
        ),
        (
            Method::POST,
            format!("{REPO}/statuses/{sha}"),
            r#"{"state": "pending", "context": "ci", "state": "success"}"#.to_owned(),
            StatusCode::CREATED,
            "state",
            "success",
        ),
        (
            Method::POST,
            format!("{REPO}/git/refs"),
            format!(r#"{{"ref": "refs/heads/one", "sha": "{sha}", "ref": "refs/heads/two"}}"#),
            StatusCode::CREATED,
            "ref",
            "refs/heads/two",
        ),
    ];
    for (method, path, body, status, field, value) in cases {
        let answer = client
            .call_bytes(method.clone(), &path, body.into_bytes())
            .await;

        // Then GitHub takes the last value, as it does for a repeated pull request edit key
        assert_eq!(answer.status, status, "{method} {path} {}", answer.json());
        assert_eq!(answer.json()[field], value, "{method} {path}");
    }
}

#[tokio::test]
async fn the_issue_list_refuses_a_filter_value_with_the_value_and_the_list_documentation() {
    // Given
    let (_server, client) = start().await;
    let list_docs = "https://docs.github.com/v3/issues/#list-issues";

    // When the issue list gets a value that no issue filter knows, as live cli/cli did
    for (query, field, value) in [
        ("state=bogus", "state", "bogus"),
        ("milestone=bogus", "milestone", "bogus"),
        ("assignee=nobody-here", "assignee", "nobody-here"),
        ("assignee=", "assignee", ""),
        ("type=bogus", "type", "bogus"),
        ("state=bogus&type=bogus", "state", "bogus"),
    ] {
        let answer = client.get(&format!("{REPO}/issues?{query}")).await;

        // Then GitHub names the field and echoes its value
        let body = answer.json();
        assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY, "{query}");
        assert_eq!(body["documentation_url"], list_docs, "{query}");
        assert_eq!(body["errors"][0]["resource"], "Issue", "{query}");
        assert_eq!(body["errors"][0]["field"], field, "{query}");
        assert_eq!(body["errors"][0]["code"], "invalid", "{query}");
        assert_eq!(body["errors"][0]["value"], value, "{query}");
    }
    for query in [
        "milestone=*",
        "assignee=*",
        "assignee=none",
        "type=bug",
        "type=*",
    ] {
        let answer = client.get(&format!("{REPO}/issues?{query}")).await;
        assert_eq!(answer.status, StatusCode::OK, "{query}");
    }
}

#[tokio::test]
async fn a_list_refuses_a_since_that_is_not_iso_8601_with_its_operation_documentation() {
    // Given an issue with a comment and a pull request
    let (_server, client) = start().await;
    let issue =
        created(&client, &format!("{REPO}/issues"), &json!({"title": "Bug"})).await["number"]
            .as_u64()
            .unwrap();
    created(
        &client,
        &format!("{REPO}/issues/{issue}/comments"),
        &json!({"body": "first"}),
    )
    .await;
    let number = pull(&client).await;
    let docs = "https://docs.github.com/rest";

    // When each list gets a since that is not a timestamp, as live cli/cli did
    for (path, operation) in [
        (
            format!("{REPO}/issues?since=yesterday"),
            format!("{docs}/issues/issues#list-repository-issues"),
        ),
        (
            format!("{REPO}/issues?since="),
            "https://docs.github.com/v3/issues/#list-issues".to_owned(),
        ),
        (
            format!("{REPO}/issues/{issue}/comments?since=yesterday"),
            format!("{docs}/issues/comments#list-issue-comments"),
        ),
        (
            format!("{REPO}/issues/comments?since=yesterday"),
            format!("{docs}/issues/comments#list-issue-comments-for-a-repository"),
        ),
        (
            format!("{REPO}/pulls/comments?since=yesterday"),
            format!("{docs}/pulls/comments#list-review-comments-in-a-repository"),
        ),
        (
            format!("{REPO}/pulls/{number}/comments?since=yesterday"),
            format!("{docs}/pulls/comments#list-review-comments-on-a-pull-request"),
        ),
    ] {
        let answer = client.get(&path).await;

        // Then GitHub answers a message with the operation documentation and no errors
        let body = answer.json();
        assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY, "{path}");
        assert_eq!(body["documentation_url"], operation, "{path}");
        assert!(body.get("errors").is_none(), "{path} {body}");
    }

    // When the lists get a date, an empty since or an unknown reaction, as live cli/cli did
    for path in [
        format!("{REPO}/issues?since=2020-01-01"),
        format!("{REPO}/issues?since=2020-01-01T00:00"),
        format!("{REPO}/issues/{issue}/comments?since="),
        format!("{REPO}/issues/comments?since=20200101"),
        format!("{REPO}/pulls/comments?since="),
    ] {
        let answer = client.get(&path).await;

        // Then GitHub serves the list
        assert_eq!(answer.status, StatusCode::OK, "{path}");
    }
    let comment = client.get(&format!("{REPO}/issues/comments")).await.json()[0]["id"].clone();
    created(
        &client,
        &format!("{REPO}/issues/comments/{comment}/reactions"),
        &json!({"content": "heart"}),
    )
    .await;
    let reactions = client
        .get(&format!(
            "{REPO}/issues/comments/{comment}/reactions?content=bogus"
        ))
        .await;
    assert_eq!(reactions.status, StatusCode::OK);
    assert_eq!(reactions.json().as_array().map(Vec::len), Some(1));

    // When the repository's comments are listed by an unknown sort, newest first
    let newer = created(
        &client,
        &format!("{REPO}/issues/{issue}/comments"),
        &json!({"body": "second"}),
    )
    .await["id"]
        .clone();
    let ordered = client
        .get(&format!("{REPO}/issues/comments?sort=bogus&direction=desc"))
        .await;

    // Then GitHub sorts them by creation, newest first, as live cli/cli did
    assert_eq!(ordered.status, StatusCode::OK);
    assert_eq!(ordered.json()[0]["id"], newer);
    assert_eq!(ordered.json()[1]["id"], comment);
}

async fn titles_since(client: &GoGithub, since: &str) -> (StatusCode, Vec<String>) {
    let answer = client
        .get(&format!(
            "{REPO}/issues?state=all&per_page=100&since={}",
            query_escape(since)
        ))
        .await;
    let titles = answer
        .json()
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i["title"].as_str().map(str::to_owned))
        .collect();
    (answer.status, titles)
}

#[tokio::test]
async fn every_since_reads_free_text_as_ruby_time_parse_in_pacific_time() {
    // Given an issue and its comment at 2026-01-01T00:00:00Z, and a newer issue, comment
    // and pull request at 2026-01-03T00:00:00Z, which is Jan 2 16:00 in Pacific time
    let (server, client) = start().await;
    let old = created(&client, &format!("{REPO}/issues"), &json!({"title": "old"})).await["number"]
        .clone();
    created(
        &client,
        &format!("{REPO}/issues/{old}/comments"),
        &json!({"body": "old"}),
    )
    .await;
    server.clock.advance(2 * 86_400);
    let new = created(&client, &format!("{REPO}/issues"), &json!({"title": "new"})).await["number"]
        .clone();
    created(
        &client,
        &format!("{REPO}/issues/{new}/comments"),
        &json!({"body": "new"}),
    )
    .await;
    let number = pull(&client).await;

    // When the issue list reads each since form that live microsoft/vscode read, 2026-11-23
    for (since, has_old, has_new) in [
        ("Jan 2 2026", false, true),
        ("2 January 2026", false, true),
        ("Fri Jan 2 2026", false, true),
        ("Jan 2nd, 26", false, true),
        ("02/01/2026", false, true),
        ("2026/01/02", false, true),
        ("2.1.2026", false, true),
        ("20260102", false, true),
        ("2026-002", false, true),
        ("R8.1.2", false, true),
        ("issues since Jan 2 2026, please", false, true),
        ("2025-12-31 17:00", false, true),
        ("2025-12-31T17:00:00", false, true),
        ("2025-12-31 17:00 CET", false, true),
        ("2025-12-31 16:00 PST", true, true),
        ("2025-12-31 19:00 EST", true, true),
        ("2025-12-31 17:00 -0700", true, true),
        ("2025-12-31 5 pm", false, true),
        ("Dec 31 2025 24:00", false, true),
        ("2025-12-31 23:59:60", false, true),
        ("2025-11-31", true, true),
        ("16:00", false, true),
        ("Jan", false, true),
        ("1970-01-01 00:00 PST", true, true),
    ] {
        let (status, titles) = titles_since(&client, since).await;

        // Then GitHub reads it as Ruby's Time.parse in America/Los_Angeles
        assert_eq!(status, StatusCode::OK, "{since}");
        assert_eq!(titles.iter().any(|t| t == "old"), has_old, "{since}");
        assert_eq!(titles.iter().any(|t| t == "new"), has_new, "{since}");
    }

    // When the issue list reads a since before 1970-01-01T08:00:00Z, as live did
    let (status, titles) = titles_since(&client, "1970-01-01T07:59:59Z").await;

    // Then GitHub lists no issue
    assert_eq!(status, StatusCode::OK);
    assert!(titles.is_empty(), "{titles:?}");

    // When every list reads a since that Ruby cannot read or a SQL DATETIME cannot hold
    let too_long = format!("2026-01-02 {}", "x".repeat(118));
    let refused = [
        "Friday",
        "yesterday",
        "2026-01-02 25:00",
        "2026-13-01",
        "01/13/2026",
        "2025-11-62",
        "Jan 1 0999",
        "Jan 1 10000",
        too_long.as_str(),
    ];
    let lists = [
        format!("{REPO}/issues"),
        format!("{REPO}/issues/comments"),
        format!("{REPO}/issues/{new}/comments"),
        format!("{REPO}/pulls/comments"),
        format!("{REPO}/pulls/{number}/comments"),
    ];
    for list in &lists {
        for since in refused {
            let answer = client
                .get(&format!("{list}?since={}", query_escape(since)))
                .await;

            // Then GitHub refuses it with 422
            assert_eq!(
                answer.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{list} {since}"
            );
        }

        // When the list reads free text with trailing words
        let answer = client
            .get(&format!(
                "{list}?since={}",
                query_escape("Jan 2 2026 and later")
            ))
            .await;

        // Then GitHub serves it
        assert_eq!(answer.status, StatusCode::OK, "{list}");
    }
    let comments = client
        .get(&format!(
            "{REPO}/issues/comments?since={}",
            query_escape("Jan 2 2026 and later")
        ))
        .await
        .json();
    assert_eq!(comments.as_array().map(Vec::len), Some(1));
    assert_eq!(comments[0]["body"], "new");
}

#[tokio::test]
async fn every_since_maps_local_time_through_the_pacific_zone_history() {
    // Given an issue at 2025-11-02T09:00:00Z, in the hour that repeats at the end of
    // daylight saving time
    let (server, client) = start().await;
    server
        .clock
        .set(Timestamp::parse("2025-11-02T09:00:00Z").unwrap());
    created(
        &client,
        &format!("{REPO}/issues"),
        &json!({"title": "repeated"}),
    )
    .await;

    // When the issue list reads 01:30:60 in that hour
    let (status, titles) = titles_since(&client, "2025-11-02 01:30:60").await;

    // Then Ruby's Time.local reads the earlier hour, 08:31:00Z
    assert_eq!(status, StatusCode::OK);
    assert_eq!(titles, ["repeated"]);

    // When the issue list reads the last local times before 1000-01-01T00:00:00Z, which
    // the zone keeps in local mean time, -07:52:58, as live octocat/Hello-World did, 2026-11-23
    let (before, _) = titles_since(&client, "0999-12-31T16:07:01").await;
    let (at, _) = titles_since(&client, "0999-12-31T16:07:02").await;

    // Then GitHub refuses only the one before the DATETIME range
    assert_eq!(before, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(at, StatusCode::OK);
}

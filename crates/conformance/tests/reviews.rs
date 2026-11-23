#[path = "../../api-reviews/tests/support/conformance.rs"]
mod support;

use http::{Method, StatusCode};
use hubdouble_testkit::{GoGithub, Graphql, fixture};
use serde_json::json;
use support::{
    COMMENTS, PULL, PULLS_WRITE_TOKEN, TOKEN, api_call, assert_reactions, check, drain, field_id,
    fixture_server, graphql, head_sha, update_feature, webhook,
};

macro_rules! rest {
    ($client:expr, $method:ident, $path:expr, $op:literal, $status:literal) => {
        api_call(&$client, Method::$method, &$path, None, ($op, $status)).await
    };
    ($client:expr, $method:ident, $path:expr, $op:literal, $status:literal, $body:expr) => {
        api_call(
            &$client,
            Method::$method,
            &$path,
            Some(&$body),
            ($op, $status),
        )
        .await
    };
}

macro_rules! doc {
    ($name:literal) => {
        include_str!(concat!(
            "../../../docs/inventory/graphql/",
            $name,
            ".graphql"
        ))
    };
}

#[tokio::test]
async fn pending_reviews_publish_file_and_line_comments_with_the_review() {
    let server = fixture_server().await;
    let client = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    let api = Graphql::new(client.clone());
    let list = format!("{PULL}/reviews");
    let draft = json!({
        "body": "Private", "commit_id": head_sha(&server), "comments": [
            {"body": "Line", "path": "src/feature.txt", "line": 3, "side": "RIGHT", "start_line": 1, "start_side": "RIGHT"}
        ]
    });
    let review = rest!(client, POST, list, "pulls/create-review", 200, draft);
    let id = field_id(&review, "id");
    let path = format!("{list}/{id}");
    assert_eq!(review["state"], "PENDING");
    assert!(review.get("submitted_at").is_none());
    assert_eq!(rest!(client, GET, path, "pulls/get-review", 200)["id"], id);
    assert_eq!(
        rest!(GoGithub::new(&server), GET, list, "pulls/list-reviews", 200),
        json!([])
    );
    assert_eq!(client.post(&list, &json!({})).await.status.as_u16(), 422);
    let thread = graphql(
        &api,
        doc!("fileThread"),
        &json!({
            "review": review["node_id"], "path": "src/feature.txt", "body": "File",
        }),
    )
    .await["addPullRequestReviewThread"]["thread"]["id"]
        .clone();
    assert!(drain(&server).await.is_empty());

    let submit_path = format!("{path}/events");
    let submit = json!({"event": "REQUEST_CHANGES", "body": "Make this change"});
    let result = rest!(
        client,
        POST,
        submit_path,
        "pulls/submit-review",
        200,
        submit
    );
    assert_eq!(result["state"], "CHANGES_REQUESTED");
    assert_eq!(result["submitted_at"], "2026-01-01T00:00:00Z");
    assert_eq!(result["commit_id"], review["commit_id"]);
    assert_eq!(
        client.call(Method::DELETE, &path).await.status.as_u16(),
        422
    );
    assert_eq!(
        client.post(&submit_path, &submit).await.status.as_u16(),
        422
    );

    let comments = rest!(
        client,
        GET,
        format!("{PULL}/comments"),
        "pulls/list-review-comments",
        200
    );
    assert_eq!(comments[0]["line"], 3);
    assert_eq!(comments[0]["start_line"], 1);
    assert_eq!(comments[1]["subject_type"], "file");
    assert!(comments[1].get("line").is_none());
    assert_eq!(comments[1]["pull_request_review_id"], id);

    let vars = json!({"thread": thread});
    let resolved = graphql(&api, doc!("resolveThread"), &vars).await;
    assert_eq!(
        resolved["resolveReviewThread"]["thread"]["isResolved"],
        true
    );
    let undo = "mutation($thread:ID!){unresolveReviewThread(input:{threadId:$thread}){thread{id isResolved}}}";
    assert_eq!(
        graphql(&api, undo, &vars).await["unresolveReviewThread"]["thread"]["isResolved"],
        false
    );
    let deliveries = drain(&server).await;
    assert_eq!(deliveries.len(), 5);
    assert_eq!(
        webhook(&deliveries, "pull_request_review", "submitted")["review"]["id"],
        id
    );
    let resolved = webhook(&deliveries, "pull_request_review_thread", "resolved");
    assert_eq!(
        resolved["thread"]["comments"][0]["pull_request_review_id"],
        id
    );
    assert_eq!(
        webhook(&deliveries, "pull_request_review_thread", "unresolved")["thread"]["node_id"],
        thread
    );
}

#[tokio::test]
async fn go_github_can_reply_through_the_review_comment_collection() {
    let server = fixture_server().await;
    let client = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    let path = format!("{PULL}/comments");
    let root = rest!(
        client,
        POST,
        path,
        "pulls/create-review-comment",
        201,
        json!({"body": "Root", "path": "src/feature.txt", "line": 1, "commit_id": head_sha(&server)})
    );
    let parent = field_id(&root, "id");
    let reply = rest!(
        client,
        POST,
        path,
        "pulls/create-review-comment",
        201,
        json!({"body": "Reply", "in_reply_to": parent})
    );
    assert_eq!(reply["in_reply_to_id"], parent);
    assert_eq!(reply["body"], "Reply");
}

#[tokio::test]
async fn review_comments_replies_and_reactions_share_the_same_records() {
    let server = fixture_server().await;
    let client = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    let path = format!("{PULL}/comments");
    let body = json!({"body": "Root", "path": "src/feature.txt", "line": 1, "commit_id": head_sha(&server)});
    let root = rest!(client, POST, path, "pulls/create-review-comment", 201, body);
    let id = field_id(&root, "id");
    let comment = format!("{COMMENTS}/{id}");
    let reply_path = format!("{path}/{id}/replies");
    server.clock.advance(2);
    let reply = rest!(
        client,
        POST,
        reply_path,
        "pulls/create-reply-for-review-comment",
        201,
        json!({"body": "Reply"})
    );
    assert_eq!(reply["in_reply_to_id"], id);
    let repo = format!(
        "{COMMENTS}?sort=updated&direction=desc&since=2026-01-01T00%3A00%3A00Z&per_page=1&page=1"
    );
    assert_eq!(
        rest!(
            client,
            GET,
            repo,
            "pulls/list-review-comments-for-repo",
            200
        ),
        json!([reply])
    );
    let first = client.get(&format!("{path}?per_page=1&page=1")).await;
    assert!(first.header("link").contains("page=2>; rel=\"next\""));
    assert_eq!(
        check(&first, 200, "pulls/list-review-comments")[0]["id"],
        id
    );
    let second = format!("{path}?per_page=1&page=2");
    assert_eq!(
        rest!(client, GET, second, "pulls/list-review-comments", 200)[0]["id"],
        reply["id"]
    );

    assert_reactions(&client, &comment).await;

    let edited = rest!(
        client,
        PATCH,
        comment,
        "pulls/update-review-comment",
        200,
        json!({"body": "Edited"})
    );
    assert_eq!(edited["body"], "Edited");
    assert_eq!(
        client.call(Method::DELETE, &comment).await.status.as_u16(),
        204
    );
    assert_eq!(client.get(&comment).await.status.as_u16(), 404);
    let deliveries = drain(&server).await;
    for action in ["created", "edited", "deleted"] {
        assert_eq!(
            webhook(&deliveries, "pull_request_review_comment", action)["comment"]["id"],
            id
        );
    }
}

#[tokio::test]
async fn another_users_review_comment_changes_only_with_push_access() {
    // Given review comments by bob, a reader of acme/public
    let server = fixture_server().await;
    let as_bob = GoGithub::new(&server).with_token(fixture::BOB_TOKEN);
    let path = format!("{PULL}/comments");
    let mut comments = Vec::new();
    for body in ["First", "Second"] {
        let draft = json!({"body": body, "path": "src/feature.txt", "line": 1, "commit_id": head_sha(&server)});
        let created = rest!(
            as_bob,
            POST,
            path,
            "pulls/create-review-comment",
            201,
            draft
        );
        comments.push(format!("{COMMENTS}/{}", field_id(&created, "id")));
    }
    let as_carol = GoGithub::new(&server).with_token(fixture::CAROL_TOKEN);
    let as_alice = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    let as_installation = GoGithub::new(&server).with_token(PULLS_WRITE_TOKEN);
    let edit = json!({"body": "Edited"});

    // When carol, who has no push access, edits and deletes one
    let carol_edit = as_carol.call_json(Method::PATCH, &comments[0], &edit).await;
    let carol_delete = as_carol.call(Method::DELETE, &comments[0]).await;

    // Then both are refused and the comment is unchanged
    for refused in [&carol_edit, &carol_delete] {
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        assert_eq!(refused.json()["status"], "403");
    }
    let kept = rest!(as_bob, GET, comments[0], "pulls/get-review-comment", 200);
    assert_eq!(kept["body"], "First");

    // When alice, an org owner, edits it and an installation with pull_requests write deletes the other
    let edited = rest!(
        as_alice,
        PATCH,
        comments[0],
        "pulls/update-review-comment",
        200,
        edit
    );
    let deleted = as_installation.call(Method::DELETE, &comments[1]).await;

    // Then both changes apply
    assert_eq!(edited["body"], "Edited");
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert_eq!(as_bob.get(&comments[1]).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reviews_can_be_discarded_edited_or_dismissed_in_the_right_state() {
    let server = fixture_server().await;
    let client = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    let list = format!("{PULL}/reviews");
    let draft = json!({"comments": [{"path": "src/feature.txt", "line": 1, "body": "Discard"}]});
    let pending = rest!(client, POST, list, "pulls/create-review", 200, draft);
    let pending = format!("{list}/{}", pending["id"]);
    assert_eq!(
        rest!(client, DELETE, pending, "pulls/delete-pending-review", 200)["state"],
        "PENDING"
    );
    assert_eq!(client.get(&pending).await.status.as_u16(), 404);
    assert_eq!(
        rest!(
            client,
            GET,
            format!("{PULL}/comments"),
            "pulls/list-review-comments",
            200
        ),
        json!([])
    );
    let review = rest!(
        client,
        POST,
        list,
        "pulls/create-review",
        200,
        json!({"event": "APPROVE"})
    );
    let path = format!("{list}/{}", review["id"]);
    let edited = rest!(
        client,
        PUT,
        path,
        "pulls/update-review",
        200,
        json!({"body": "Approved"})
    );
    assert_eq!(edited["body"], "Approved");
    let action = format!("{path}/dismissals");
    let dismissed = rest!(
        client,
        PUT,
        action,
        "pulls/dismiss-review",
        200,
        json!({"message": "Replaced"})
    );
    assert_eq!(dismissed["state"], "DISMISSED");
    let deliveries = drain(&server).await;
    assert_eq!(
        webhook(&deliveries, "pull_request_review", "edited")["changes"]["body"]["from"],
        ""
    );
    assert_eq!(
        webhook(&deliveries, "pull_request_review", "dismissed")["review"]["id"],
        review["id"]
    );
}

#[tokio::test]
async fn requested_reviewers_page_and_support_team_only_removal() {
    let server = fixture_server().await;
    let client = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    let path = format!("{PULL}/requested_reviewers");
    let request = json!({"reviewers": ["alice"], "team_reviewers": ["review-team"]});
    let added = rest!(client, POST, path, "pulls/request-reviewers", 201, request);
    assert_eq!(added["requested_reviewers"][0]["login"], "alice");
    assert_eq!(added["requested_teams"][0]["slug"], "review-team");
    let first = client.get(&format!("{path}?per_page=1&page=1")).await;
    assert!(first.header("link").contains("rel=\"next\""));
    assert_eq!(
        check(&first, 200, "pulls/list-requested-reviewers")["teams"],
        json!([])
    );
    let second = format!("{path}?per_page=1&page=2");
    assert_eq!(
        rest!(client, GET, second, "pulls/list-requested-reviewers", 200)["teams"][0]["slug"],
        "review-team"
    );
    let missing = json!({"team_reviewers": ["review-team"]});
    assert_eq!(
        client
            .call_json(Method::DELETE, &path, &missing)
            .await
            .status
            .as_u16(),
        422
    );
    let remove = json!({"reviewers": [], "team_reviewers": ["review-team"]});
    let removed = rest!(
        client,
        DELETE,
        path,
        "pulls/remove-requested-reviewers",
        200,
        remove
    );
    assert_eq!(removed["requested_teams"], json!([]));
    assert_eq!(removed["requested_reviewers"][0]["login"], "alice");
    let review = json!({"event": "APPROVE"});
    rest!(
        client,
        POST,
        format!("{PULL}/reviews"),
        "pulls/create-review",
        200,
        review
    );
    assert_eq!(
        rest!(client, GET, path, "pulls/list-requested-reviewers", 200),
        json!({"users": [], "teams": []})
    );
    let deliveries = drain(&server).await;
    assert_eq!(
        webhook(&deliveries, "pull_request", "review_requested")["requested_reviewer"]["login"],
        "alice"
    );
    assert_eq!(
        webhook(&deliveries, "pull_request", "review_request_removed")["requested_team"]["slug"],
        "review-team"
    );
}

#[tokio::test]
async fn review_operations_apply_repository_visibility_before_permissions() {
    let server = fixture_server().await;
    let client = GoGithub::new(&server).with_token(TOKEN);
    for (method, route) in [
        (Method::GET, "1/reviews"),
        (Method::POST, "1/reviews"),
        (Method::GET, "1/reviews/8"),
        (Method::DELETE, "1/reviews/8"),
        (Method::POST, "1/reviews/8/events"),
        (Method::PUT, "1/reviews/8"),
        (Method::PUT, "1/reviews/8/dismissals"),
        (Method::GET, "comments"),
        (Method::GET, "comments/8"),
        (Method::PATCH, "comments/8"),
        (Method::DELETE, "comments/8"),
        (Method::GET, "1/comments"),
        (Method::POST, "1/comments"),
        (Method::POST, "1/comments/8/replies"),
        (Method::GET, "comments/8/reactions"),
        (Method::POST, "comments/8/reactions"),
        (Method::GET, "1/requested_reviewers"),
        (Method::POST, "1/requested_reviewers"),
        (Method::DELETE, "1/requested_reviewers"),
    ] {
        let denied = client
            .call(method.clone(), &format!("repos/acme/public/pulls/{route}"))
            .await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN, "{method} {route}");
        assert!(
            denied
                .header("x-accepted-github-permissions")
                .starts_with("pull_requests=")
        );
        let unseen = client
            .call(method.clone(), &format!("repos/acme/private/pulls/{route}"))
            .await;
        assert_eq!(unseen.status, StatusCode::NOT_FOUND, "{method} {route}");
    }
}

#[tokio::test]
async fn inventory_queries_return_reviews_threads_comments_and_cursors() {
    let server = fixture_server().await;
    let client = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    let api = Graphql::new(client.clone());
    let add = json!({"event": "COMMENT", "body": "Review body", "comments": [
        {"path": "src/feature.txt", "line": 1, "body": "Thread opening"},
        {"path": "src/feature.txt", "subject_type": "file", "body": "File opening"}
    ]});
    let review = rest!(
        client,
        POST,
        format!("{PULL}/reviews"),
        "pulls/create-review",
        200,
        add
    );
    let vars = json!({"owner": "acme", "name": "public", "repo": "public", "number": 1, "first": 1, "recent": 1});
    let data = graphql(&api, doc!("threadOpenings"), &vars).await;
    let first = &data["repository"]["pullRequest"]["reviewThreads"]["nodes"][0];
    let thread = &first["id"];
    let comment = &first["comments"]["nodes"][0];
    assert_eq!(comment["originalCommit"]["oid"], review["commit_id"]);
    assert_eq!(comment["body"], "Thread opening");
    assert_eq!(first["line"], 1);
    let discussion = graphql(
        &api,
        doc!("implementThreadComments"),
        &json!({"thread": thread}),
    )
    .await;
    let opening = &discussion["node"]["comments"]["nodes"][0];
    assert_eq!(opening["pullRequestReview"]["databaseId"], review["id"]);
    let reply_path = format!("{PULL}/comments/{}/replies", opening["databaseId"]);
    let reply = rest!(
        client,
        POST,
        reply_path,
        "pulls/create-reply-for-review-comment",
        201,
        json!({"body": "Reply"})
    );
    let node = graphql(
        &api,
        doc!("originalCommentEdit"),
        &json!({"id": reply["node_id"]}),
    )
    .await;
    assert_eq!(node["node"]["replyTo"]["databaseId"], opening["databaseId"]);
    let pull = graphql(&api, doc!("pullOfNode"), &json!({"id": thread})).await;
    assert_eq!(
        pull["node"]["pullRequest"]["repository"]["nameWithOwner"],
        "acme/public"
    );

    let context = graphql(&api, doc!("contextThreads"), &vars).await;
    let connection = &context["repository"]["pullRequest"]["reviewThreads"];
    assert_eq!(connection["pageInfo"]["hasNextPage"], true);
    assert_eq!(
        connection["edges"][0]["node"]["recent"]["nodes"][0]["body"],
        "Reply"
    );
    assert_eq!(
        connection["edges"][0]["node"]["recent"]["pageInfo"]["hasPreviousPage"],
        true
    );
    let mut next = vars.clone();
    next["after"] = connection["pageInfo"]["endCursor"].clone();
    let next = graphql(&api, doc!("threads"), &next).await;
    let connection = &next["repository"]["pullRequest"]["reviewThreads"];
    assert_eq!(connection["nodes"], json!([{"isResolved": false}]));
    assert_eq!(connection["pageInfo"]["hasNextPage"], false);
    let bodies = graphql(&api, doc!("reviewBodies"), &vars).await;
    assert_eq!(
        bodies["repository"]["pullRequest"]["reviews"]["nodes"][0]["body"],
        "Review body"
    );
    let plan = graphql(&api, doc!("planThreads"), &vars).await;
    assert_eq!(
        plan["repository"]["pullRequest"]["reviewThreads"]["nodes"][0]["recent"]["nodes"][1]["body"],
        "Reply"
    );
    let implement = graphql(&api, doc!("implementPull"), &vars).await;
    assert_eq!(
        implement["repository"]["pullRequest"]["reviewThreads"]["nodes"][0]["recent"]["totalCount"],
        2
    );
}

#[tokio::test]
async fn changes_to_the_head_ref_move_or_outdate_line_threads() {
    let server = fixture_server().await;
    let client = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    let add = json!({"event": "COMMENT", "comments": [
        {"path": "src/feature.txt", "line": 1, "body": "Keep this thread"},
        {"path": "src/feature.txt", "line": 2, "body": "Outdate this thread"}
    ]});
    rest!(
        client,
        POST,
        format!("{PULL}/reviews"),
        "pulls/create-review",
        200,
        add
    );
    update_feature(&server, "insert\nfirst\nchanged\nlast\n");
    let api = Graphql::new(client);
    let vars = json!({"owner": "acme", "name": "public", "number": 1});
    let data = graphql(&api, doc!("threadOpenings"), &vars).await;
    let threads = &data["repository"]["pullRequest"]["reviewThreads"]["nodes"];
    assert_eq!(threads[0]["line"], 2);
    assert_eq!(threads[0]["originalLine"], 1);
    assert_eq!(threads[0]["isOutdated"], false);
    assert_eq!(threads[1]["originalLine"], 2);
    assert_eq!(threads[1]["isOutdated"], true);
    assert!(threads[1]["line"].is_null());
}

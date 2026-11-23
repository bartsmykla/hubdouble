use std::fmt::Write as _;

use super::{
    GoGithub, Inventory, Method, OPENING, PENDING, REPO, REVIEW, StatusCode, THREAD, TestServer,
    Value, args, json, node_id, post, success,
};

pub(super) async fn run(server: &TestServer, alice: &GoGithub, cases: &mut Inventory) {
    let v = |extra| {
        let mut vars = args(extra);
        vars["number"] = json!(1);
        vars
    };
    let data = cases.run("threadOpenings", v(json!({}))).await;
    let opening = &data["repository"]["pullRequest"]["reviewThreads"]["nodes"][0];
    let thread = &opening["id"];
    assert_eq!(thread, &node_id::entity(node_id::REVIEW_THREAD, THREAD));
    assert_eq!(opening["subjectType"], "LINE");
    assert_eq!(opening["path"], "guide.md");
    assert_eq!(opening["line"], 1);
    assert_eq!(opening["originalLine"], 1);
    assert_eq!(opening["diffSide"], "RIGHT");
    assert_eq!(opening["isResolved"], false);
    assert_eq!(opening["isOutdated"], false);
    let comment = post(
        alice,
        &format!("{REPO}/pulls/1/comments/{OPENING}/replies"),
        json!({"body":"Reply"}),
        StatusCode::CREATED,
    )
    .await;
    let data = cases
        .run("implementThreadComments", json!({"thread":thread}))
        .await;
    let connection = &data["node"]["comments"];
    assert_eq!(connection["nodes"][0]["body"], "Thread opening");
    assert_eq!(
        connection["nodes"][0]["pullRequestReview"]["databaseId"],
        REVIEW
    );
    assert_eq!(connection["nodes"][1]["body"], "Reply");
    assert_eq!(
        connection["nodes"][1]["author"],
        json!({"login":"alice","__typename":"User"})
    );
    assert_eq!(
        connection["nodes"][1]["replyTo"],
        json!({"databaseId":OPENING})
    );
    assert_eq!(connection["pageInfo"]["hasNextPage"], false);
    let data = cases
        .run("contextThreads", v(json!({"first":1,"recent":1})))
        .await;
    let edge = &data["repository"]["pullRequest"]["reviewThreads"]["edges"][0];
    assert_eq!(&edge["node"]["id"], thread);
    assert_eq!(edge["node"]["recent"]["nodes"][0]["body"], "Reply");
    assert_eq!(edge["node"]["recent"]["pageInfo"]["hasPreviousPage"], true);
    let data = cases.run("reviewBodies", v(json!({}))).await;
    let reviews = data["repository"]["pullRequest"]["reviews"]["nodes"]
        .as_array()
        .unwrap_or_else(|| panic!("invalid reviews: {data}"));
    let review = reviews
        .iter()
        .find(|r| r["databaseId"] == REVIEW)
        .unwrap_or_else(|| panic!("missing review: {data}"));
    assert_eq!(review["author"], json!({"login":"bob","__typename":"User"}));
    assert_eq!(review["body"], "Review body");
    assert_eq!(review["submittedAt"], "2026-01-01T00:00:00Z");
    let data = cases.run("planThreads", v(json!({}))).await;
    assert_eq!(
        data["repository"]["pullRequest"]["reviewThreads"]["nodes"][0]["recent"]["nodes"][1]["body"],
        "Reply"
    );
    let data = cases.run("implementPull", v(json!({}))).await;
    let pull = &data["repository"]["pullRequest"];
    assert_eq!(pull["state"], "OPEN");
    assert_eq!(pull["baseRefName"], "main");
    assert_ne!(pull["baseRefOid"], pull["headRefOid"]);
    assert_eq!(pull["baseRepository"], json!({"nameWithOwner":"acme/docs"}));
    assert_eq!(pull["headRepository"], json!({"nameWithOwner":"acme/docs"}));
    assert_eq!(pull["reviewThreads"]["nodes"][0]["recent"]["totalCount"], 2);
    let data = cases.run("pullOfNode", json!({"id":thread})).await;
    assert_eq!(
        data["node"]["pullRequest"],
        json!({"number":1,"repository":{"nameWithOwner":"acme/docs"}})
    );
    let data = cases.run("resolveThread", json!({"thread":thread})).await;
    assert_eq!(
        data["resolveReviewThread"]["thread"],
        json!({"id":thread,"isResolved":true})
    );
    let data = cases.run("threads", v(json!({}))).await;
    assert_eq!(
        data["repository"]["pullRequest"]["reviewThreads"]["nodes"],
        json!([{"isResolved":true}])
    );
    additional(alice, cases, &comment).await;
    issue(server, alice, cases).await;
}

async fn additional(alice: &GoGithub, cases: &mut Inventory, comment: &Value) {
    let data = cases
        .run("originalCommentEdit", json!({"id":comment["node_id"]}))
        .await;
    assert_eq!(data["node"]["__typename"], "PullRequestReviewComment");
    assert_eq!(data["node"]["replyTo"], json!({"databaseId":OPENING}));
    assert_eq!(data["node"]["pullRequest"]["number"], 1);
    let data = cases
        .run(
            "fileThread",
            json!({"review":node_id::entity(node_id::REVIEW,PENDING),
        "path":"guide.md","body":"File opening"}),
        )
        .await;
    let thread = &data["addPullRequestReviewThread"]["thread"]["id"];
    let data = cases.run("pullOfNode", json!({"id":thread})).await;
    assert_eq!(data["node"]["pullRequest"]["number"], 1);
    let draft = alice.get(&format!("{REPO}/pulls/3")).await;
    assert_eq!(draft.status, StatusCode::OK);
    let draft = draft.json();
    assert_eq!(draft["draft"], true);
    let data = cases.run("ready", json!({"id":draft["node_id"]})).await;
    assert_eq!(
        data["markPullRequestReadyForReview"]["pullRequest"]["isDraft"],
        false
    );
}

async fn issue(server: &TestServer, alice: &GoGithub, cases: &mut Inventory) {
    let issue = post(
        alice,
        &format!("{REPO}/issues"),
        json!({"title":"Report","body":"Details"}),
        StatusCode::CREATED,
    )
    .await;
    let number = issue["number"]
        .as_u64()
        .unwrap_or_else(|| panic!("invalid issue: {issue}"));
    let answer = alice
        .call_json(
            Method::PATCH,
            &format!("{REPO}/pulls/2"),
            &json!({"body":format!("Fixes #{number}")}),
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK);
    let comment = post(
        alice,
        &format!("{REPO}/issues/{number}/comments"),
        json!({"body":"Original"}),
        StatusCode::CREATED,
    )
    .await;
    server.clock.advance(60);
    let path = format!("{REPO}/issues/comments/{}", comment["id"]);
    let answer = alice
        .call_json(Method::PATCH, &path, &json!({"body":"Revised"}))
        .await;
    assert_eq!(answer.status, StatusCode::OK);
    let data = cases
        .run("lastEdits", json!({"ids":[comment["node_id"]]}))
        .await;
    assert_eq!(
        data["nodes"],
        json!([{"id":comment["node_id"],"lastEditedAt":"2026-01-01T00:01:00Z"}])
    );
    let data = cases
        .run("originalCommentEdit", json!({"id":comment["node_id"]}))
        .await;
    let node = &data["node"];
    assert_eq!(node["__typename"], "IssueComment");
    assert_eq!(node["body"], "Revised");
    assert_eq!(node["issue"]["number"], number);
    assert_eq!(node["pullRequest"], Value::Null);
    let data = cases
        .run("implementIssue", args(json!({"number":number})))
        .await;
    let repo = &data["repository"];
    assert_eq!(
        repo["t0"],
        json!({"text":"# Template\n","isTruncated":false,
            "oid":"c296a5f621e8f1d1536d8b671d272a96a5d22a7d","byteSize":11})
    );
    assert_eq!(repo["t1"], Value::Null);
    assert_eq!(
        repo["issue"],
        json!({"state":"OPEN","title":"Report","body":"Details",
        "closedByPullRequestsReferences":{"nodes":[{"number":2,"state":"OPEN",
            "headRefName":"prompts/proposal-pending","headRepository":{"nameWithOwner":"acme/docs"}}]}})
    );
    for count in [1, 100] {
        let sample = &cases.sources["implementIssue"];
        let (Some(head), Some(tail)) = (sample.find("t0:"), sample.find("issue(number:")) else {
            panic!("implementIssue has no template alias or issue field");
        };
        let mut doc = sample[..head].to_owned();
        for i in 0..count {
            let _ = writeln!(
                doc,
                "t{i}: object(expression: \"HEAD:.github/pull_request_template.md\") {{ ... on Blob {{ text isTruncated oid byteSize }} }}"
            );
        }
        doc.push_str(&sample[tail..]);
        let got = success(&cases.api, &doc, args(json!({"number":number}))).await;
        assert_eq!(got["repository"]["issue"], repo["issue"]);
        for i in 0..count {
            assert_eq!(got["repository"][format!("t{i}")], repo["t0"]);
        }
    }
}

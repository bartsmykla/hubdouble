use super::{
    Graphql, Inventory, Method, REPO, StatusCode, TestServer, Value, args, json, post, success,
};
use hubdouble_testkit::GoGithub;

fn references(data: &Value) -> &Value {
    &data["repository"]["pullRequest"]["closingIssuesReferences"]
}

pub(super) async fn run(
    server: &TestServer,
    alice: &GoGithub,
    bob: &Graphql,
    cases: &mut Inventory,
) {
    // Given a pull request with a keyword link, a manual link, and a link to a private issue
    let issue_path = format!("{REPO}/issues");
    let visible = post(
        alice,
        &issue_path,
        json!({"title": "Visible"}),
        StatusCode::CREATED,
    )
    .await;
    let manual = post(
        alice,
        &issue_path,
        json!({"title": "Manual"}),
        StatusCode::CREATED,
    )
    .await;
    let other = post(
        alice,
        &issue_path,
        json!({"title": "Unrelated"}),
        StatusCode::CREATED,
    )
    .await;
    let private = post(
        alice,
        "repos/acme/private/issues",
        json!({"title": "Private"}),
        StatusCode::CREATED,
    )
    .await;
    let body = format!(
        "Fixes #{0}, resolves #{0}, closes Acme/private#{1}. See #{2}.",
        visible["number"], private["number"], other["number"]
    );
    let answer = alice
        .call_json(
            Method::PATCH,
            &format!("{REPO}/pulls/3"),
            &json!({"body": body}),
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK);
    let answer = server
        .admin(
            Method::POST,
            "issue_link",
            Some(
                &json!({"repository":"acme/docs","issue_number":manual["number"],"pull_number":3}),
            ),
        )
        .await;
    assert_eq!(answer.status, StatusCode::CREATED);

    // When each user reads the references with the client query
    let vars = args(json!({"number":3,"after":null}));
    let all = cases.run("closingIssues", vars.clone()).await;
    let filtered = success(bob, &cases.sources["closingIssues"], vars.clone()).await;

    // Then the private issue is visible only to the user who has access to it
    let first = json!({"number":visible["number"],"repository":{"nameWithOwner":"acme/docs"}});
    let second = json!({"number":manual["number"],"repository":{"nameWithOwner":"acme/docs"}});
    let third = json!({"number":private["number"],"repository":{"nameWithOwner":"acme/private"}});
    assert_eq!(references(&all)["nodes"], json!([first, second, third]));
    assert_eq!(references(&filtered)["nodes"], json!([first, second]));
    assert_eq!(references(&all)["pageInfo"]["hasNextPage"], false);

    // When the client uses the end cursor of its first page
    let paged = cases.sources["closingIssues"].replace("first: 100", "first: 1");
    let page = success(bob, &paged, vars.clone()).await;
    let page = references(&page);
    assert_eq!(page["nodes"], json!([first]));
    assert_eq!(page["pageInfo"]["hasNextPage"], true);
    let mut vars = vars;
    vars["after"] = page["pageInfo"]["endCursor"].clone();
    let next = success(bob, &paged, vars).await;
    assert_eq!(references(&next)["nodes"], json!([second]));
    assert_eq!(references(&next)["pageInfo"]["hasNextPage"], false);
}

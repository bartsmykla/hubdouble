use std::fmt::Write as _;

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
    server.clock.advance(1);
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

    server.clock.advance(1);
    assert_orders(
        alice,
        bob,
        &cases.sources["closingIssues"],
        &visible["number"],
        &second,
        &first,
    )
    .await;
}

async fn assert_orders(
    alice: &GoGithub,
    bob: &Graphql,
    document: &str,
    issue_number: &Value,
    newer: &Value,
    recently_updated: &Value,
) {
    let issue = format!("{REPO}/issues/{issue_number}");
    let updated = alice
        .call_json(Method::PATCH, &issue, &json!({"title": "Updated"}))
        .await;
    assert_eq!(updated.status, StatusCode::OK);
    post(
        alice,
        &format!("{issue}/comments"),
        json!({"body":"A comment"}),
        StatusCode::CREATED,
    )
    .await;

    for (field, first, second) in [
        ("CREATED_AT", newer, recently_updated),
        ("UPDATED_AT", recently_updated, newer),
        ("COMMENTS", recently_updated, newer),
    ] {
        let ordered = document.replace(
            "first: 100",
            &format!("first: 100, orderBy: {{field: {field}, direction: DESC}}"),
        );
        let data = success(bob, &ordered, args(json!({"number":3}))).await;
        assert_eq!(
            references(&data)["nodes"],
            json!([first, second]),
            "{field}"
        );
    }
}

#[tokio::test]
async fn a_pull_request_exposes_the_same_ten_issues_it_closes_on_merge() {
    const PUBLIC: &str = "repos/acme/public";
    let server = TestServer::builder()
        .fixture()
        .frame(hubdouble_api::register_all)
        .start()
        .await;
    let client = super::client(&server, hubdouble_testkit::fixture::ALICE_TOKEN);
    let mut issue_numbers = Vec::new();
    let mut body = String::new();
    for i in 1..=11 {
        let issue = post(
            &client,
            &format!("{PUBLIC}/issues"),
            json!({"title":format!("Issue {i}")}),
            StatusCode::CREATED,
        )
        .await;
        let _ = writeln!(body, "Fixes #{}", issue["number"]);
        issue_numbers.push(issue["number"].clone());
    }
    let pull = post(
        &client,
        &format!("{PUBLIC}/pulls"),
        json!({"title": "Fix issues", "head": "feature", "base": "main", "body": body}),
        StatusCode::CREATED,
    )
    .await;
    let document = include_str!("../../../../docs/inventory/graphql/closingIssues.graphql");
    let api = Graphql::new(client.clone());
    let data = success(
        &api,
        document,
        json!({"owner": "acme", "name": "public", "number": pull["number"]}),
    )
    .await;
    let listed = references(&data)["nodes"]
        .as_array()
        .unwrap_or_else(|| panic!("invalid closing issues: {data}"));
    let listed_numbers: Vec<_> = listed.iter().map(|issue| issue["number"].clone()).collect();
    assert_eq!(listed_numbers, issue_numbers[..10]);
    assert_eq!(references(&data)["pageInfo"]["hasNextPage"], false);

    let merged = client
        .call_json(
            Method::PUT,
            &format!("{PUBLIC}/pulls/{}/merge", pull["number"]),
            &json!({}),
        )
        .await;
    assert_eq!(merged.status, StatusCode::OK, "{}", merged.json());
    for (index, number) in issue_numbers.iter().enumerate() {
        let issue = client.get(&format!("{PUBLIC}/issues/{number}")).await;
        assert_eq!(issue.status, StatusCode::OK);
        let state = if index < 10 { "closed" } else { "open" };
        assert_eq!(issue.json()["state"], state, "issue {number}");
    }
}

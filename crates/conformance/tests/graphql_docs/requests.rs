use std::fmt::Write as _;

use super::{Graphql, REPO, StatusCode, Value, client, fixture, json, node_id, server, success};

#[tokio::test]
async fn rest_ids_and_legacy_ids_resolve_through_node_and_nodes() {
    let server = server().await;
    let alice = client(&server, fixture::ALICE_TOKEN);
    let api = Graphql::new(alice.clone());
    let rest = alice.get(&format!("{REPO}/pulls/1")).await;
    assert_eq!(rest.status, StatusCode::OK);
    let id = &rest.json()["node_id"];
    let legacy = node_id::encode_legacy("PullRequest", rest.json()["id"].as_i64().unwrap());
    let data = success(
        &api,
        "query($id:ID!, $ids:[ID!]!) {
        node(id:$id) { ... on PullRequest { id number } }
        nodes(ids:$ids) { __typename ... on PullRequest { id number } }
    }",
        json!({"id":id, "ids":[id, legacy]}),
    )
    .await;
    assert_eq!(data["node"], json!({"id":id,"number":1}));
    assert_eq!(
        data["nodes"],
        json!([
            {"__typename":"PullRequest","id":id,"number":1},
            {"__typename":"PullRequest","id":id,"number":1}
        ])
    );
    let response = api
        .query(
            "query($id: ID!) { node(id:$id) { id } }",
            &json!({"id":legacy}),
        )
        .await
        .json();
    assert_eq!(response["extensions"]["warnings"][0]["type"], "DEPRECATION");
    assert_eq!(
        response["extensions"]["warnings"][0]["data"]["next_global_id"],
        *id
    );
}

#[tokio::test]
async fn access_keeps_visible_aliases_when_a_repository_is_hidden() {
    let server = server().await;
    let api = Graphql::new(client(&server, fixture::BOB_TOKEN));
    let allowed = json!({"collaborators":{"edges":[
        {"permission":"ADMIN","node":{"databaseId":fixture::ALICE_ID}}
    ]}});
    for count in [1, 100] {
        let mut vars = json!({"owner":"acme","login":"alice"});
        let mut declarations = String::new();
        let mut selections = String::new();
        for i in 0..count {
            write!(declarations, ", $n{i}:String!").unwrap();
            writeln!(
                selections,
                "r{i}:repository(owner:$owner,name:$n{i}) {{
                collaborators(login:$login,first:1) {{edges {{permission node {{databaseId}}}}}}
            }}"
            )
            .unwrap();
            vars[format!("n{i}")] = json!("docs");
        }
        let doc = format!("query($owner:String!,$login:String!{declarations}){{{selections}}}");
        let output = success(&api, &doc, vars.clone()).await;
        assert_eq!(output.as_object().unwrap().len(), count);
        for i in 0..count {
            assert_eq!(output[format!("r{i}")], allowed);
        }
        vars[format!("n{}", count - 1)] = json!("private");
        let response = api.query(&doc, &vars).await;
        assert_eq!(response.status, StatusCode::OK);
        let result = response.json();
        assert_eq!(result["errors"].as_array().unwrap().len(), 1);
        assert_eq!(result["errors"][0]["type"], "NOT_FOUND");
        assert_eq!(
            result["errors"][0]["path"],
            json!([format!("r{}", count - 1)])
        );
        assert_eq!(result["data"][format!("r{}", count - 1)], Value::Null);
        for i in 0..count - 1 {
            assert_eq!(result["data"][format!("r{i}")], allowed);
        }
    }
}

#[tokio::test]
async fn connections_check_boundaries_and_return_cursors_in_both_directions() {
    let server = server().await;
    let api = Graphql::new(client(&server, fixture::ALICE_TOKEN));
    let doc = |args: &str| {
        format!(
            r#"{{ repository(owner:"acme",name:"docs") {{
        name p: refs(refPrefix:"refs/heads/prompts/", {args}) {{
            nodes {{ name }} pageInfo {{ startCursor endCursor hasPreviousPage hasNextPage }}
        }}
    }} }}"#
        )
    };
    for (boundary, error) in [
        ("", "MISSING_PAGINATION_BOUNDARIES"),
        ("first:0", "INVALID_PAGINATION"),
        ("last:-1", "INVALID_PAGINATION"),
        ("first:101", "EXCESSIVE_PAGINATION"),
        ("last:101", "EXCESSIVE_PAGINATION"),
    ] {
        let answer = api.query(&doc(boundary), &Value::Null).await;
        assert_eq!(answer.status, StatusCode::OK);
        let body = answer.json();
        assert_eq!(body["data"]["repository"], json!({"name":"docs","p":null}));
        assert_eq!(body["errors"][0]["type"], error);
        assert_eq!(body["errors"][0]["path"], json!(["repository", "p"]));
    }
    let bad_prefix = doc("first:1").replace("refs/heads/prompts/", "refs/heads/prompts");
    let response = api.query(&bad_prefix, &Value::Null).await.json();
    assert_eq!(
        response["data"]["repository"],
        json!({"name":"docs","p":null})
    );
    assert_eq!(response["errors"][0]["type"], "VALIDATION");
    assert_eq!(response["errors"][0]["path"], json!(["repository", "p"]));
    let page = |data: Value| data["repository"]["p"].clone();
    let first = page(success(&api, &doc("first:1"), Value::Null).await);
    let last = page(success(&api, &doc("last:1"), Value::Null).await);
    let both = page(success(&api, &doc("first:100"), Value::Null).await);
    assert_eq!(
        both["nodes"].as_array().unwrap(),
        &vec![first["nodes"][0].clone(), last["nodes"][0].clone()]
    );
    assert_eq!(first["pageInfo"]["hasNextPage"], true);
    assert_eq!(first["pageInfo"]["hasPreviousPage"], false);
    assert_eq!(last["pageInfo"]["hasNextPage"], false);
    assert_eq!(last["pageInfo"]["hasPreviousPage"], true);
    let next = page(
        success(
            &api,
            &doc(&format!("first:1,after:{}", first["pageInfo"]["endCursor"])),
            Value::Null,
        )
        .await,
    );
    let previous = page(
        success(
            &api,
            &doc(&format!(
                "last:1,before:{}",
                last["pageInfo"]["startCursor"]
            )),
            Value::Null,
        )
        .await,
    );
    assert_eq!(next["nodes"], last["nodes"]);
    assert_eq!(previous["nodes"], first["nodes"]);
    let empty = page(
        success(
            &api,
            &doc(&format!("first:1,after:{}", last["pageInfo"]["endCursor"])),
            Value::Null,
        )
        .await,
    );
    assert_eq!(empty["nodes"], json!([]));
    assert_eq!(empty["pageInfo"]["startCursor"], Value::Null);
    assert_eq!(empty["pageInfo"]["endCursor"], Value::Null);
}

#[tokio::test]
async fn depth_and_node_limits_apply_before_field_resolution() {
    let server = server().await;
    let api = Graphql::new(client(&server, fixture::ALICE_TOKEN));
    let nested = |n: usize, leaf: &str| {
        format!(
            "{{ repository(owner:\"acme\",name:\"docs\") {{ {} {leaf} {} }} }}",
            "owner { repository(name:\"docs\") { ".repeat(n),
            "} } ".repeat(n)
        )
    };
    let allowed = api
        .query(&nested(11, "owner { __typename }"), &Value::Null)
        .await
        .json();
    assert_eq!(allowed["errors"][0]["type"], "HUBDOUBLE_NOT_IMPLEMENTED");
    let too_deep = nested(12, "__typename");
    let answer = api.query(&too_deep, &Value::Null).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert!(answer.json().get("data").is_none());
    assert_eq!(answer.json()["errors"].as_array().unwrap().len(), 1);
    let query = "query($limit:Int!) { viewer { repositories(first:100) {
        nodes { issues(first:100) { nodes { ...Many } } }
    } } } fragment Many on Issue {
        a: comments(first:$limit) { totalCount }
        b: comments(first:$limit) { totalCount }
    }";
    let answer = api.query(query, &json!({"limit":25})).await;
    assert_eq!(answer.status, StatusCode::OK);
    let body = answer.json();
    assert!(body.get("data").is_none());
    assert_eq!(body["errors"][0]["type"], "MAX_NODE_LIMIT_EXCEEDED");
    let query = query.replace("b: comments(first:$limit)", "b: comments(first:23)");
    let answer = api.query(&query, &json!({"limit":25})).await.json();
    assert_eq!(answer["errors"][0]["type"], "HUBDOUBLE_NOT_IMPLEMENTED");
}

#[tokio::test]
async fn invalid_documents_and_mutation_inputs_return_graphql_request_errors() {
    let server = server().await;
    let api = Graphql::new(client(&server, fixture::ALICE_TOKEN));
    for (doc, vars, path, code) in [
        (
            "query Q { a: viewer { wrong: absent } }",
            Value::Null,
            json!(["query Q", "a", "wrong"]),
            Some("undefinedField"),
        ),
        (
            "mutation M { updateRefs(input:{refUpdates:[]}) { clientMutationId } }",
            Value::Null,
            json!(["mutation M", "updateRefs", "input", "repositoryId"]),
            Some("missingRequiredInputObjectAttribute"),
        ),
        (
            "mutation M($input:UpdateRefsInput!) { updateRefs(input:$input) { clientMutationId } }",
            json!({"input":{"refUpdates":[]}}),
            Value::Null,
            None,
        ),
    ] {
        let answer = api.query(doc, &vars).await;
        assert_eq!(answer.status, StatusCode::OK);
        let body = answer.json();
        assert!(body.get("data").is_none());
        let error = &body["errors"][0];
        assert!(error.get("type").is_none());
        assert_eq!(error["path"], path, "{body}");
        if let Some(code) = code {
            assert_eq!(error["extensions"]["code"], code);
        } else {
            assert_eq!(error["extensions"]["value"], vars["input"]);
            assert_eq!(
                error["extensions"]["problems"][0]["path"],
                json!(["repositoryId"])
            );
        }
    }
    let doc =
        "mutation M($input:UpdateRefsInput!) { updateRefs(input:$input) { clientMutationId } }";
    let vars = json!({"input":{"repositoryId":"unknown","refUpdates":[
        {"name":"short","afterOid":"0000000000000000000000000000000000000000"}
    ]}});
    let answer = api.query(doc, &vars).await;
    assert_eq!(answer.status, StatusCode::OK);
    let body = answer.json();
    assert!(body.get("data").is_none());
    assert_eq!(body["errors"][0]["extensions"]["value"], vars["input"]);
    assert_eq!(
        body["errors"][0]["extensions"]["problems"][0]["path"],
        json!(["refUpdates", 0, "name"])
    );
}

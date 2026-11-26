use std::fmt::Write as _;

use super::{
    GoGithub, Graphql, Method, REPO, StatusCode, Value, client, fixture, json, node_id, server,
    success,
};

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
async fn zero_page_sizes_keep_counts_and_return_empty_pages() {
    let server = server().await;
    let api = Graphql::new(client(&server, fixture::ALICE_TOKEN));
    for (size, next, previous) in [("first", true, false), ("last", false, true)] {
        let doc = format!(
            r#"{{ repository(owner:"acme",name:"docs") {{
                refs(refPrefix:"refs/heads/prompts/", {size}:0) {{
                    totalCount nodes {{ name }} edges {{ cursor }}
                    pageInfo {{ hasNextPage hasPreviousPage startCursor endCursor }}
                }}
            }} }}"#
        );
        let data = success(&api, &doc, Value::Null).await;
        assert_eq!(
            data["repository"]["refs"],
            json!({
                "totalCount":2,"nodes":[],"edges":[],"pageInfo":{
                    "hasNextPage":next,"hasPreviousPage":previous,"startCursor":null,"endCursor":null
                }
            })
        );
    }
}

#[tokio::test]
async fn connections_without_page_fields_do_not_require_a_size() {
    let server = server().await;
    let api = Graphql::new(client(&server, fixture::ALICE_TOKEN));
    let doc = r#"{
        repository(owner:"acme",name:"docs") {
            count:refs(refPrefix:"refs/heads/prompts/") {totalCount}
            kind:refs(refPrefix:"refs/heads/prompts/") {__typename}
        }
    }"#;
    let data = success(&api, doc, Value::Null).await;
    assert_eq!(
        data["repository"],
        json!({
            "count":{"totalCount":2},"kind":{"__typename":"RefConnection"}
        })
    );
    for selection in [
        "items:nodes {name}",
        "items:edges {cursor}",
        "meta:pageInfo {__typename}",
    ] {
        let doc = format!(
            r#"{{
            repository(owner:"acme",name:"docs") {{
                page:refs(refPrefix:"refs/heads/prompts/") {{ ...Items }}
                name
            }}
        }} fragment Items on RefConnection {{ {selection} }}"#
        );
        let response = api.query(&doc, &Value::Null).await;
        assert_eq!(response.status, StatusCode::OK);
        let body = response.json();
        assert_eq!(
            body["data"]["repository"],
            json!({"page":null,"name":"docs"})
        );
        assert_eq!(body["errors"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["errors"][0]["type"], "MISSING_PAGINATION_BOUNDARIES");
        assert_eq!(body["errors"][0]["path"], json!(["repository", "page"]));
    }
}

#[tokio::test]
async fn a_root_search_cursor_error_omits_data_when_it_reaches_the_root() {
    let server = server().await;
    let api = Graphql::new(client(&server, fixture::ALICE_TOKEN));
    let doc = r#"{
        broken:search(type:ISSUE,query:"repo:acme/docs is:pr",first:1,after:"bogus") {
            issueCount pageInfo{startCursor more:hasNextPage}
        }
        healthy:search(type:ISSUE,query:"repo:acme/docs is:pr",first:1) {__typename}
    }"#;
    let response = api.query(doc, &Value::Null).await;
    assert_eq!(response.status, StatusCode::OK);
    let body = response.json();
    assert!(body.get("data").is_none(), "{body}");
    let errors = body["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 2);
    assert_eq!(errors[0]["type"], "INVALID_CURSOR_ARGUMENTS");
    assert_eq!(
        errors[0]["path"],
        json!(["broken", "pageInfo", "startCursor"])
    );
    assert_eq!(errors[1]["type"], "INVALID_CURSOR_ARGUMENTS");
    assert_eq!(errors[1]["path"], json!(["broken", "pageInfo", "more"]));
}

#[tokio::test]
async fn invalid_cursors_report_errors_on_selected_child_fields() {
    let server = server().await;
    let api = Graphql::new(client(&server, fixture::ALICE_TOKEN));
    let doc = r#"{
        repository(owner:"acme",name:"docs") {
            forward:refs(refPrefix:"refs/heads/prompts/",first:1,after:"bogus") {
                totalCount items:nodes {name} links:edges {cursor}
                meta:pageInfo {start:startCursor end:endCursor}
            }
            backward:refs(refPrefix:"refs/heads/prompts/",last:1,before:"bogus") {
                pageInfo {endCursor previous:hasPreviousPage hasNextPage}
            }
            count:refs(refPrefix:"refs/heads/prompts/",first:1,after:"bogus") {totalCount}
            name
        }
    }"#;
    let response = api.query(doc, &Value::Null).await;
    assert_eq!(response.status, StatusCode::OK);
    let body = response.json();
    assert_eq!(
        body["data"]["repository"],
        json!({
            "forward":{"totalCount":2,"items":null,"links":null,"meta":{"start":null,"end":null}},
            "backward":null,"count":{"totalCount":2},"name":"docs"
        })
    );
    let errors = body["errors"].as_array().unwrap();
    assert!(errors.iter().all(|error| error.get("type").is_none()));
    let paths: Vec<_> = errors.iter().map(|error| &error["path"]).collect();
    assert_eq!(
        paths,
        [
            &json!(["repository", "forward", "items"]),
            &json!(["repository", "forward", "links"]),
            &json!(["repository", "forward", "meta", "start"]),
            &json!(["repository", "forward", "meta", "end"]),
            &json!(["repository", "backward", "pageInfo", "endCursor"]),
            &json!(["repository", "backward", "pageInfo", "previous"]),
        ]
    );
}

#[tokio::test]
async fn typed_cursor_errors_keep_each_connections_partial_data() {
    let server = server().await;
    let api = Graphql::new(client(&server, fixture::ALICE_TOKEN));
    let doc = r#"{
        repository(owner:"acme",name:"docs") {
            collaborators(first:1,after:"bogus") {nodes{login}}
            pullRequest(number:1) {
                threads:reviewThreads(first:1,after:"bogus") {
                    totalCount nodes{id} edges{cursor} pageInfo{startCursor}
                }
                reviews(first:1,after:"bogus") {
                    nodes{id} pageInfo{startCursor}
                }
            }
        }
        organization(login:"acme") {
            teams(first:1,after:"bogus") {nodes{databaseId} pageInfo{startCursor}}
        }
        results:search(type:ISSUE,query:"repo:acme/docs is:pr",first:1,after:"bogus") {
            issueCount nodes{__typename} pageInfo{start:startCursor}
        }
    }"#;
    let response = api.query(doc, &Value::Null).await;
    assert_eq!(response.status, StatusCode::OK);
    let body = response.json();
    assert_eq!(body["data"]["organization"], Value::Null);
    assert_eq!(body["data"]["repository"]["collaborators"], Value::Null);
    let pull = &body["data"]["repository"]["pullRequest"];
    assert_eq!(pull["reviews"], Value::Null);
    let threads = &pull["threads"];
    assert_eq!(threads["totalCount"], 1);
    assert_eq!(threads["nodes"], Value::Null);
    assert!(threads["edges"][0]["cursor"].is_string());
    assert_eq!(
        threads["pageInfo"]["startCursor"],
        threads["edges"][0]["cursor"]
    );
    assert_eq!(
        body["data"]["results"],
        json!({
            "issueCount":3,"nodes":null,"pageInfo":{"start":null}
        })
    );
    let errors = body["errors"].as_array().unwrap();
    assert!(
        errors
            .iter()
            .all(|e| e["type"] == "INVALID_CURSOR_ARGUMENTS")
    );
    let paths = [
        json!(["repository", "collaborators"]),
        json!(["repository", "pullRequest", "threads", "nodes"]),
        json!(["repository", "pullRequest", "reviews", "nodes"]),
        json!(["repository", "pullRequest", "reviews", "pageInfo"]),
        json!(["organization", "teams", "nodes"]),
        json!(["organization", "teams", "pageInfo"]),
        json!(["results", "nodes"]),
        json!(["results", "pageInfo", "start"]),
    ];
    assert_eq!(errors.len(), paths.len());
    for path in paths {
        assert!(errors.iter().any(|error| error["path"] == path), "{body}");
    }
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

async fn ask(api: &GoGithub, query: &str, n: &str, as_variable: bool) -> Value {
    let body = if as_variable {
        format!(r#"{{"query":{},"variables":{{"n":{n}}}}}"#, json!(query))
    } else {
        let literal = query.replace("($n: Int!)", "").replace("$n", n);
        format!(r#"{{"query":{}}}"#, json!(literal))
    };
    api.call_bytes(Method::POST, "graphql", body.into_bytes())
        .await
        .json()
}

#[tokio::test]
async fn variables_nest_255_levels_and_a_deeper_body_answers_an_empty_500() {
    let server = server().await;
    let api = client(&server, fixture::ALICE_TOKEN);
    let body = |levels: usize, unused: usize| {
        let nested = |n: usize| format!("{}1{}", "[".repeat(n), "]".repeat(n));
        format!(
            r#"{{"query":"{{ viewer {{ login }} }}","x":{},"variables":{{"a":{}}}}}"#,
            nested(unused),
            nested(levels - 1)
        )
    };
    // Given variables of 255 levels, the variables object included, and a member that the
    // body does not use nested 300 levels
    for (levels, unused) in [(255, 0), (1, 300)] {
        // When the body is posted
        let answer = api
            .call_bytes(Method::POST, "graphql", body(levels, unused).into_bytes())
            .await;
        // Then the query runs, as on live GitHub on 2026-11-25
        assert_eq!(answer.status, StatusCode::OK, "{levels} {unused}");
        assert_eq!(
            answer.json(),
            json!({"data":{"viewer":{"login":fixture::ALICE}}})
        );
    }
    // When the variables nest 256 levels
    let answer = api
        .call_bytes(Method::POST, "graphql", body(256, 0).into_bytes())
        .await;
    // Then GitHub answers 500 with an empty JSON body and its usual headers
    assert_eq!(answer.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(answer.body.is_empty());
    assert_eq!(
        answer.header("content-type"),
        "application/json; charset=utf-8"
    );
    assert_eq!(answer.header("x-ratelimit-resource"), "graphql");
}

#[tokio::test]
async fn the_token_is_checked_before_the_body() {
    let server = server().await;
    let deep = format!(
        r#"{{"query":"{{ viewer {{ login }} }}","variables":{}1{}}}"#,
        r#"{"a":"#.repeat(300),
        "}".repeat(300)
    );
    let valid = r#"{"query":"{ viewer { login } }"}"#;
    let stranger = client(&server, "ghp_000000000000000000000000000000000000");
    for body in ["{bad", valid, &deep] {
        // When a body is posted with a token that GitHub does not know
        let answer = stranger
            .call_bytes(Method::POST, "graphql", body.as_bytes().to_vec())
            .await;
        // Then it answers 401 whatever the body, as on live GitHub on 2026-11-26
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{}", &body[..4]);
    }
}

#[tokio::test]
async fn a_body_error_keeps_the_answer_that_github_gives() {
    let server = server().await;
    let api = client(&server, fixture::ALICE_TOKEN);
    let failed = |message: &str| json!({"errors":[{"message":message}]});
    let refused = |message: &str| json!({"message":message,"documentation_url":"https://docs.github.com/graphql","status":"400"});
    let no_query = failed("A query attribute must be specified and must be a string.");
    let used = json!({"data":{"viewer":{"login":fixture::ALICE}}});
    let quoted = r#"{"query":"query($s: Boolean!) { viewer { login @include(if: $s) } }","variables":"{\"s\":true}"}"#;
    let not_json = r#"{"query":"{ viewer { login } }","variables":"not json"}"#;
    for (body, status, want) in [
        ("{}", StatusCode::OK, Some(no_query.clone())),
        (r#"{"query":1}"#, StatusCode::OK, Some(no_query.clone())),
        (
            not_json,
            StatusCode::OK,
            Some(failed("Failed to parse variables")),
        ),
        (quoted, StatusCode::OK, Some(used.clone())),
        (r#"{"query":1,}"#, StatusCode::BAD_REQUEST, None),
        (
            "{bad",
            StatusCode::BAD_REQUEST,
            Some(refused("Problems parsing JSON")),
        ),
        (
            "[]",
            StatusCode::BAD_REQUEST,
            Some(refused("Body should be a JSON object")),
        ),
        (
            "\"x\"",
            StatusCode::BAD_REQUEST,
            Some(refused("Body should be a JSON object")),
        ),
        ("null", StatusCode::OK, Some(no_query.clone())),
        ("", StatusCode::OK, Some(no_query.clone())),
        (
            r#"{"variables":"not json"}"#,
            StatusCode::OK,
            Some(no_query),
        ),
        (
            "\u{c}{\"query\":\u{b}\"{ viewer { login } }\"}\u{c}",
            StatusCode::OK,
            Some(used.clone()),
        ),
        (
            r#"{"query":"{ viewer { login } }","variables":{"x":1e309,"y":"\ud800"}}"#,
            StatusCode::OK,
            Some(used.clone()),
        ),
        (
            r#"{"query":"{ viewer { login } }","operationName":null}"#,
            StatusCode::OK,
            Some(used.clone()),
        ),
        (
            r#"{"query":"{ viewer { login } }","operationName":0}"#,
            StatusCode::OK,
            Some(failed(r#"No operation named "0""#)),
        ),
        (
            r#"{"query":"{ viewer { login } }","operationName":{"a":[1,null]}}"#,
            StatusCode::OK,
            Some(failed(r#"No operation named "{"a" => [1, nil]}""#)),
        ),
        (
            r#"{"query":"query($x:Int){viewer{repositories(first:$x){totalCount}}}","variables":{"x":1e309}}"#,
            StatusCode::OK,
            Some(json!({"errors":[{
                "extensions":{"value":null,"problems":[{"path":[],"explanation":"Could not coerce value Infinity to Int"}]},
                "locations":[{"line":1,"column":7}],
                "message":"Variable $x of type Int was provided invalid value",
            }]})),
        ),
        (
            r#"{"query":"query($x:Int){viewer{repositories(first:$x){totalCount}}}","variables":{"x":-1e309}}"#,
            StatusCode::OK,
            Some(json!({"data":null})),
        ),
    ] {
        // When a body is posted with a known token
        let answer = api
            .call_bytes(Method::POST, "graphql", body.as_bytes().to_vec())
            .await;
        // Then it answers as live GitHub did on 2026-11-26
        assert_eq!(answer.status, status, "{body}");
        if let Some(want) = want {
            assert_eq!(answer.json(), want, "{body}");
        }
    }
}

#[tokio::test]
async fn an_operation_name_and_a_variables_string_read_as_github_reads_them() {
    let server = server().await;
    let api = client(&server, fixture::ALICE_TOKEN);
    let failed = |message: &str| json!({"errors":[{"message":message}]});
    let used = json!({"data":{"viewer":{"login":fixture::ALICE}}});
    for (body, status, want) in [
        (
            r#"{"query":"{ viewer { login } }","operationName":{"a":1,"b":2,"a":3}}"#,
            StatusCode::OK,
            Some(failed(r#"No operation named "{"a" => 3, "b" => 2}""#)),
        ),
        (
            r#"{"query":"{ viewer { login } }","operationName":{"a":{"b":1,"b":2}}}"#,
            StatusCode::OK,
            Some(failed(r#"No operation named "{"a" => {"b" => 2}}""#)),
        ),
        (
            r#"{"query":"{ viewer { login } }","operationName":["a\udc00#{"]}"#,
            StatusCode::OK,
            Some(failed(r#"No operation named "["a\xED\xB0\x80\#{"]""#)),
        ),
        (
            r#"{"query":"{ viewer { login } }","operationName":{"\udc00":1}}"#,
            StatusCode::OK,
            Some(failed(r#"No operation named "{"\xED\xB0\x80" => 1}""#)),
        ),
        (
            r#"{"query":"{ viewer { login } }","operationName":["\ud800",{},[]]}"#,
            StatusCode::OK,
            Some(failed(r#"No operation named "["?", {}, []]""#)),
        ),
        (
            r#"{"query":"query($s: Boolean!) { viewer { login @include(if: $s) } }","variables":"\u000b{\"s\":\u000ctrue}"}"#,
            StatusCode::OK,
            Some(used.clone()),
        ),
        (
            r#"{"query":"{ viewer { login } }","variables":"\u000b\u000c"}"#,
            StatusCode::OK,
            Some(used.clone()),
        ),
        (
            r#"{"query":"{ viewer { login } }","variables":"\u000bnull\u000c"}"#,
            StatusCode::OK,
            Some(used.clone()),
        ),
        (
            r#"{"query":"{ viewer { login } }","\udc00":1,"variables":"{\"\udc00\":1,\"\\udc00\":2}"}"#,
            StatusCode::OK,
            Some(used),
        ),
    ] {
        // When a body is posted with a known token
        let answer = api
            .call_bytes(Method::POST, "graphql", body.as_bytes().to_vec())
            .await;
        // Then it answers as live GitHub did on 2026-11-26
        assert_eq!(answer.status, status, "{body}");
        if let Some(want) = want {
            assert_eq!(answer.json(), want, "{body}");
        }
    }
}

fn cursor_lookups(docs: &str) -> Vec<String> {
    let mut lookups = Vec::new();
    let pull = format!("{docs}{{pullRequest(number:1){{");
    let acme = r#"organization(login:\"acme\"){"#;
    for (open, connection, close) in [
        (acme.to_owned(), "teams", "}"),
        (format!("{acme}teams(first:2){{nodes{{"), "ancestors", "}}}"),
        (format!("{docs}{{"), "collaborators", "}"),
        (
            format!(r#"{docs}{{refs(refPrefix:\"refs/heads/\","#),
            "",
            "}",
        ),
        (
            format!(r#"{docs}{{ref(qualifiedName:\"feature\"){{"#),
            "associatedPullRequests",
            "}}",
        ),
        (pull.clone(), "commits", "}}"),
        (pull.clone(), "reviews", "}}"),
        (pull.clone(), "reviewThreads", "}}"),
        (
            format!("{pull}reviewThreads(first:1){{nodes{{"),
            "comments",
            "}}}}",
        ),
        (
            String::new(),
            r#"search(type:REPOSITORY,query:\"docs\","#,
            "",
        ),
    ] {
        for page in ["first:1,after:$s", "last:1,before:$s"] {
            let args = match connection {
                "" => format!("{page})"),
                c if c.ends_with(',') => format!("{c}{page})"),
                c => format!("{c}({page})"),
            };
            let total = if connection.starts_with("search") {
                "repositoryCount"
            } else {
                "totalCount"
            };
            lookups.push(format!("{open}{args}{{{total}}}{close}"));
        }
    }
    lookups
}

fn bodies_that_reach_a_lookup_with_text_github_cannot_store() -> Vec<(String, bool)> {
    let (low, emoji, joined, high, fffd) = (
        r"\udc00",
        r"\ud83d\ude00",
        r"\ud800\ud800",
        r"\ud800",
        r"\ufffd\ufffd\ufffd",
    );
    let docs = r#"repository(owner:\"acme\",name:\"docs\")"#;
    let refs = format!(r#"{docs}{{refs(refPrefix:\"refs/heads/\",first:1,"#);
    let owner = r#"repository(owner:$s,name:\"docs\"){id}"#;
    let mut lookups = vec![
        owner.to_owned(),
        r#"repository(owner:\"acme\",name:$s){id}"#.to_owned(),
        "organization(login:$s){id}".to_owned(),
        format!("{refs}query:$s){{totalCount}}}}"),
        format!("{refs}after:$s){{totalCount}}}}"),
        format!("{docs}{{collaborators(first:1,login:$s){{totalCount}}}}"),
        format!("viewer{{login}} {owner}"),
    ];
    lookups.extend(cursor_lookups(docs));
    let mut cases = Vec::new();
    for lookup in &lookups {
        cases.push(("String!", lookup.clone(), low, true));
    }
    for value in [emoji, joined] {
        cases.push(("String!", lookups[0].clone(), value, true));
        cases.push(("String!", lookups[2].clone(), value, false));
        cases.push(("String!", lookups[4].clone(), value, false));
    }
    cases.push(("String!", lookups[1].clone(), emoji, true));
    for value in [high, fffd] {
        cases.push(("String!", lookups[0].clone(), value, false));
    }
    for (ty, lookup) in [
        ("String!", format!("{docs}{{object(expression:$s){{oid}}}}")),
        ("String!", format!("{docs}{{ref(qualifiedName:$s){{name}}}}")),
        ("String!", "search(query:$s,type:ISSUE,first:1){issueCount}".to_owned()),
        ("ID!", "node(id:$s){id}".to_owned()),
        ("ID!", "nodes(ids:[$s]){id}".to_owned()),
        (
            "String!",
            r#"repository(owner:\"acme\",name:\"gone\"){refs(refPrefix:\"refs/heads/\",first:1,query:$s){totalCount}}"#.to_owned(),
        ),
        ("String!", "viewer{login} organization(login:$s) @skip(if:true){id}".to_owned()),
    ] {
        cases.push((ty, lookup, low, false));
    }
    let mut bodies: Vec<(String, bool)> = cases
        .into_iter()
        .map(|(ty, lookup, value, internal)| {
            let body = format!(
                r#"{{"query":"query($s:{ty}){{{lookup}}}","variables":{{"s":"{value}"}}}}"#
            );
            (body, internal)
        })
        .collect();
    for (variables, internal) in [
        (r#""{\"s\":\"\\udc00\"}""#, true),
        (r#""{\"s\":\"\udc00\"}""#, true),
        (r#"{"s":"\udc00","s":"acme"}"#, false),
        (r#"{"s":"acme","s":"\udc00"}"#, true),
    ] {
        bodies.push((
            format!(
                r#"{{"query":"query($s:String!){{organization(login:$s){{id}}}}","variables":{variables}}}"#
            ),
            internal,
        ));
    }
    bodies.push((
        format!(r#"{{"query":"{{repository(owner:\"{emoji}\",name:\"docs\"){{id}}}}"}}"#),
        true,
    ));
    bodies.push((
        format!(r#"{{"query":"{{viewer{{login}}}}","variables":{{"s":"{low}"}}}}"#),
        false,
    ));
    bodies
}

#[tokio::test]
async fn a_lookup_of_text_that_github_cannot_store_fails_the_whole_query() {
    // Given lone low surrogates and code points past U+FFFF in variables and in the query, each
    // before a lookup of the inventory documents or a field that GitHub reads without failing
    let server = server().await;
    let api = client(&server, fixture::ALICE_TOKEN);
    for (body, internal) in bodies_that_reach_a_lookup_with_text_github_cannot_store() {
        // When the query runs
        let answer = api
            .call_bytes(Method::POST, "graphql", body.clone().into_bytes())
            .await;
        // Then the lookups that live GitHub failed on 2026-11-26 answer only its internal error,
        // which names the request id, and the others answer data, as live GitHub did
        assert_eq!(answer.status, StatusCode::OK, "{body}");
        let json = answer.json();
        if internal {
            let error = json["errors"][0].as_object().unwrap();
            assert_eq!(json.as_object().unwrap().len(), 1, "{body}");
            assert_eq!(json["errors"].as_array().unwrap().len(), 1, "{body}");
            assert_eq!(error.keys().collect::<Vec<_>>(), ["message"], "{body}");
            let message = error["message"].as_str().unwrap();
            assert!(
                message.contains(answer.header("x-github-request-id")),
                "{body}"
            );
        } else {
            assert!(
                json.get("data").is_some_and(|d| !d.is_null()),
                "{body} {json}"
            );
        }
    }
}

#[tokio::test]
async fn an_int_past_64_bits_keeps_its_digits() {
    // Given an issue next to the pulls, and integers past 64 bits, past 128 bits and past the
    // range of a double
    let server = server().await;
    let api = client(&server, fixture::ALICE_TOKEN);
    let issue = api
        .post(&format!("{REPO}/issues"), &json!({"title":"Report"}))
        .await
        .json();
    let issue = issue["number"].to_string();
    let past_doubles = "9".repeat(400);
    let grouped = |digits: &str| {
        let mut out = String::new();
        for (i, digit) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i).is_multiple_of(3) {
                out.push(',');
            }
            out.push(digit);
        }
        out
    };
    let pages = [
        "query($n: Int!) { repository(owner:\"acme\",name:\"docs\") { issues(first: $n) { totalCount } } }",
        "query($n: Int!) { repository(owner:\"acme\",name:\"docs\") { issues(first: $n, last: 1) { totalCount } } }",
    ];
    let lookups = [
        (
            "an Issue",
            "query($n: Int!) { repository(owner:\"acme\",name:\"docs\") { found: issue(number: $n) { title } } }",
            "1",
        ),
        (
            "a PullRequest",
            "query($n: Int!) { repository(owner:\"acme\",name:\"docs\") { found: pullRequest(number: $n) { title } } }",
            issue.as_str(),
        ),
    ];
    let wide = [
        "99999999999999999999",
        "9999999999999999999999999999999999999999",
        past_doubles.as_str(),
    ];
    for as_variable in [false, true] {
        for page in pages {
            for n in wide {
                // When
                let answer = ask(&api, page, n, as_variable).await;

                // Then
                assert!(answer.get("data").is_none_or(Value::is_null), "{page}");
                let error = &answer["errors"][0];
                assert_eq!(error["type"], "MAX_NODE_LIMIT_EXCEEDED", "{page}");
                assert_eq!(
                    error["message"],
                    format!(
                        "By the time this query traverses to the issues connection, it is requesting up to {} possible nodes which exceeds the maximum limit of 500,000.",
                        grouped(n)
                    ),
                );
            }
        }
        for (kind, lookup, other_kind) in lookups {
            for number in [
                "9223372036854775808",
                wide[0],
                wide[1],
                wide[2],
                "999999999",
                other_kind,
            ] {
                // When
                let answer = ask(&api, lookup, number, as_variable).await;

                // Then
                let found = &answer["data"]["repository"]["found"];
                assert_eq!(*found, Value::Null, "{kind} {number} {as_variable}");
                assert_eq!(answer["errors"][0]["type"], "NOT_FOUND", "{kind} {number}");
                assert_eq!(
                    answer["errors"][0]["message"],
                    format!("Could not resolve to {kind} with the number of {number}."),
                );
            }
        }
    }
}

#[tokio::test]
async fn a_variable_in_the_form_of_a_wide_int_is_an_object_that_is_no_int() {
    // Given the object that stands for a wide integer inside hubdouble, alone and next to a
    // number that makes the variables be read again
    let server = server().await;
    let api = client(&server, fixture::ALICE_TOKEN);
    let lookup = "query($n: Int!) { repository(owner:\"acme\",name:\"docs\") { issue(number: $n) { title } } }";
    for others in ["", r#","w":1e30"#] {
        let body = format!(
            r#"{{"query":{},"variables":{{"n":{{"\u0000int":"99999999999999999999"}}{others}}}}}"#,
            json!(lookup)
        );

        // When
        let answer = api
            .call_bytes(Method::POST, "graphql", body.into_bytes())
            .await
            .json();

        // Then it fails as GitHub fails an object for an Int
        assert!(answer.get("data").is_none(), "{answer}");
        let error = &answer["errors"][0];
        assert_eq!(
            error["message"],
            "Variable $n of type Int! was provided invalid value"
        );
        assert_eq!(
            error["extensions"],
            json!({"value":{"\u{0}int":"99999999999999999999"},"problems":[{"path":[],
                "explanation":"Could not coerce value {\u{0}int:\"99999999999999999999\"} to Int"}]})
        );
    }
}

#[tokio::test]
async fn an_int_past_i64_in_an_input_object_misses_as_a_line_that_is_not_there() {
    // Given review threads on lines past i64, in the u64 range and past 64 bits, and one on the
    // largest i64 line, which no diff has
    let server = server().await;
    let api = client(&server, fixture::ALICE_TOKEN);
    let pull = api.get(&format!("{REPO}/pulls/1")).await.json();
    let thread = format!(
        "mutation($n: Int!) {{ addPullRequestReviewThread(input: {{pullRequestId: {}, \
         path: \"guide.md\", line: $n, body: \"Note\"}}) {{ thread {{ id }} }} }}",
        pull["node_id"]
    );
    for as_variable in [false, true] {
        // When
        let largest = ask(&api, &thread, &i64::MAX.to_string(), as_variable).await;
        for n in [
            "9223372036854775808",
            "18446744073709551615",
            "99999999999999999999",
        ] {
            let past = ask(&api, &thread, n, as_variable).await;

            // Then each fails as the missing line does
            assert!(past["errors"][0]["type"].is_string(), "{past}");
            assert_eq!(past, largest, "{n} {as_variable}");
        }
    }
}

#[tokio::test]
async fn a_variable_error_echoes_every_digit_of_an_int_past_64_bits() {
    // Given a String variable that gets integers in and past the u64 range
    let server = server().await;
    let api = client(&server, fixture::ALICE_TOKEN);
    let lookup = "query($s: String!) { repository(owner: $s, name: \"docs\") { id } }";
    let past_doubles = "9".repeat(400);
    for n in [
        "18446744073709551615",
        "99999999999999999999",
        past_doubles.as_str(),
    ] {
        let body = format!(r#"{{"query":{},"variables":{{"s":{n}}}}}"#, json!(lookup));

        // When
        let answer = api
            .call_bytes(Method::POST, "graphql", body.into_bytes())
            .await;

        // Then the echoed value and the problem keep every digit, as GitHub's do
        let text = std::str::from_utf8(&answer.body).unwrap();
        assert!(text.contains(&format!(r#""value":{n}"#)), "{text}");
        assert!(
            text.contains(&format!("Could not coerce value {n} to String")),
            "{text}"
        );
    }
}

#[tokio::test]
async fn an_id_takes_an_integer_of_any_size() {
    // Given integers for an ID, as a literal and as a variable, which live GitHub answered on
    // 2026-11-25 with the global id NOT_FOUND of the digits
    let server = server().await;
    let api = client(&server, fixture::ALICE_TOKEN);
    let cases = [
        ("123", false),
        ("123", true),
        ("9223372036854775808", true),
        ("99999999999999999999", true),
        ("9223372036854775808", false),
    ];
    for (n, as_variable) in cases {
        let body = if as_variable {
            let node = "query($n: ID!) { node(id: $n) { id } }";
            format!(r#"{{"query":{},"variables":{{"n":{n}}}}}"#, json!(node))
        } else {
            format!(
                r#"{{"query":{}}}"#,
                json!(format!("{{ node(id: {n}) {{ id }} }}"))
            )
        };

        // When
        let answer = api
            .call_bytes(Method::POST, "graphql", body.into_bytes())
            .await
            .json();

        // Then
        assert_eq!(
            answer["errors"][0]["type"], "NOT_FOUND",
            "{n} {as_variable}: {answer}"
        );
        assert_eq!(
            answer["errors"][0]["message"],
            format!("Could not resolve to a node with the global id of '{n}'"),
        );
    }
}

#[tokio::test]
async fn a_missing_node_ends_its_message_with_a_period_when_its_id_decodes() {
    // Given ids that name no node, with the end of the message live GitHub gave each on
    // 2026-11-25: free text, an id with bytes past its value, an unknown type, a next and a
    // legacy repository id, and next pull request ids of another shape
    let server = server().await;
    let api = Graphql::new(client(&server, fixture::ALICE_TOKEN));
    let ids = [
        ("nope", ""),
        ("I_kwDOAAAAAAAAAAAA", ""),
        ("ZZ_kgDO_____w", ""),
        ("R_kgDO_____w", "."),
        ("MDEwOlJlcG9zaXRvcnk5OTk5OTk5OTk5OQ==", "."),
        ("MDU6SXNzdWUw", "."),
        ("PR_kwDOAAAAAc4AAAAB", "."),
    ];
    let update = "mutation($id: ID!) { updateRefs(input: {repositoryId: $id, refUpdates: []}) \
                  { clientMutationId } }";
    for (id, end) in ids {
        for document in ["query($id: ID!) { node(id: $id) { id } }", update] {
            // When the id is looked up as a node, and as the repository of updateRefs
            let answer = api.query(document, &json!({"id":id})).await.json();

            // Then both answer the same NOT_FOUND
            assert_eq!(answer["errors"][0]["type"], "NOT_FOUND", "{id} {document}");
            assert_eq!(
                answer["errors"][0]["message"],
                format!("Could not resolve to a node with the global id of '{id}'{end}"),
                "{document}"
            );
        }
    }
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

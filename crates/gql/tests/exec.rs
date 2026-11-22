mod fake;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use fake::{Notes, Probe, World};
use hubdouble_auth::Actor;
use hubdouble_gql::{Ctx, ErrorType, GqlError, Obj, Registry, Resolved, Target};
use hubdouble_model::{Account, AccountKind, Repo, Timestamp, Visibility, node_id};
use hubdouble_store::{Accounts, Repos};
use serde_json::{Value, json};

fn world() -> World {
    World::new(fake::registry())
}

/// The 1-based line and column where `needle` starts in `query`.
fn at(query: &str, needle: &str) -> Value {
    let offset = query
        .find(needle)
        .unwrap_or_else(|| panic!("{needle} not in query"));
    let before = &query[..offset];
    let line = before.matches('\n').count() + 1;
    let column = before.len() - before.rfind('\n').map_or(0, |i| i + 1) + 1;
    json!({"line": line, "column": column})
}

fn types(response: &Value) -> Vec<&str> {
    response["errors"].as_array().map_or_else(Vec::new, |e| {
        e.iter().filter_map(|e| e["type"].as_str()).collect()
    })
}

#[test]
fn aliases_fragments_variables_and_directives() {
    let w = world();
    let r = w.run(
        "query($owner: String!, $skip: Boolean!) {
           a: repository(owner: $owner, name: \"one\") { ...repo }
           b: repository(owner: $owner, name: \"two\") {
             nameWithOwner @skip(if: $skip)
             n: name @include(if: $skip)
             ... on Repository @skip(if: true) { name }
           }
         }
         fragment repo on Repository { nameWithOwner }",
        &json!({"owner": "o", "skip": true}),
    );
    assert_eq!(
        r,
        json!({"data": {"a": {"nameWithOwner": "o/one"}, "b": {"n": "two"}}})
    );
}

#[test]
fn interfaces_and_unions_resolve_to_the_concrete_type() {
    let w = world();
    let r = w.run(
        "query($id: ID!) {
           node(id: $id) { __typename ... on PullRequest { number } ... on Issue { title } }
           search(query: \"x\", type: ISSUE, first: 10) {
             nodes { __typename ... on PullRequest { number } ... on Issue { number title } }
           }
         }",
        &json!({"id": node_id::entity("PR", 5)}),
    );
    assert_eq!(r["errors"], Value::Null, "{r}");
    assert_eq!(r["data"]["node"]["__typename"], "PullRequest");
    assert_eq!(r["data"]["node"]["number"], 5);
    assert_eq!(
        r["data"]["search"]["nodes"],
        json!([
            {"__typename": "PullRequest", "number": 1},
            {"__typename": "Issue", "number": 2, "title": "issue 2"},
            {"__typename": "PullRequest", "number": 3},
        ])
    );
}

#[test]
fn a_null_in_a_non_null_field_nulls_the_nearest_nullable_parent() {
    // Given pull request 2, whose title (String!) resolves to null
    let w = world();
    let q = "{ repository(owner: \"o\", name: \"one\") {
                 name
                 pullRequest(number: 2) { number title } } }";
    // When it is selected
    let r = w.run(q, &Value::Null);
    // Then pullRequest, the nearest nullable parent, is null and the error names title's path
    assert_eq!(
        r["data"],
        json!({"repository": {"name": "one", "pullRequest": null}})
    );
    assert_eq!(
        r["errors"][0]["path"],
        json!(["repository", "pullRequest", "title"])
    );
    assert_eq!(r["errors"][0]["locations"], json!([at(q, "title")]));
}

#[test]
fn a_null_that_reaches_a_non_null_root_nulls_data() {
    // Given search: SearchResultItemConnection! whose issueCount: Int! has no resolver
    let w = world();
    let r = w.run(
        "{ repository(owner: \"o\", name: \"one\") { name } \
           search(query: \"x\", type: ISSUE, first: 1) { issueCount } }",
        &Value::Null,
    );
    // Then the null propagates through the non-null search to data
    assert_eq!(r["data"], Value::Null);
    assert_eq!(r.get("data"), Some(&Value::Null));
    assert_eq!(types(&r), vec!["HUBDOUBLE_NOT_IMPLEMENTED"]);
    assert_eq!(r["errors"][0]["path"], json!(["search", "issueCount"]));
}

#[test]
fn a_hidden_alias_is_null_with_not_found_while_others_answer() {
    let w = world();
    let q = "{ ok: repository(owner: \"o\", name: \"one\") { name }
               gone: repository(owner: \"o\", name: \"hidden\") { name } }";
    let r = w.run(q, &Value::Null);
    assert_eq!(r["data"], json!({"ok": {"name": "one"}, "gone": null}));
    assert_eq!(types(&r), vec!["NOT_FOUND"]);
    assert_eq!(r["errors"][0]["path"], json!(["gone"]));
    assert_eq!(r["errors"][0]["locations"], json!([at(q, "gone:")]));
}

#[test]
fn an_unimplemented_field_errors_on_its_path_only() {
    let w = world();
    let r = w.run(
        "{ repository(owner: \"o\", name: \"one\") { name description } }",
        &Value::Null,
    );
    assert_eq!(
        r["data"],
        json!({"repository": {"name": "one", "description": null}})
    );
    assert_eq!(types(&r), vec!["HUBDOUBLE_NOT_IMPLEMENTED"]);
    assert_eq!(r["errors"][0]["path"], json!(["repository", "description"]));
}

#[test]
fn a_type_without_a_serde_form_answers_only_its_resolved_fields() {
    // Given Ref declared with object_resolved and only its name resolver
    let w = world();
    // When a resolved and an unresolved field are selected
    let r = w.run(
        "{ repository(owner: \"o\", name: \"one\") { ref(qualifiedName: \"main\") { name target { oid } } } }",
        &Value::Null,
    );
    // Then the resolver answers and the other field is NOT_IMPLEMENTED on its own path
    assert_eq!(
        r["data"],
        json!({"repository": {"ref": {"name": "main", "target": null}}})
    );
    assert_eq!(types(&r), vec!["HUBDOUBLE_NOT_IMPLEMENTED"]);
    assert_eq!(
        r["errors"][0]["path"],
        json!(["repository", "ref", "target"])
    );
}

#[test]
fn the_serde_form_is_built_once_per_parent_and_only_when_selected() {
    let w = world();
    let probe = || {
        w.store
            .view(|tx| Ok(tx.table::<Probe>().0.load(Ordering::SeqCst)))
            .unwrap_or_default()
    };
    let _ = w.run(
        "{ repository(owner: \"o\", name: \"one\") { pullRequest(number: 1) { number } } }",
        &Value::Null,
    );
    assert_eq!(probe(), 0);
    let _ = w.run(
        "{ a: repository(owner: \"o\", name: \"one\") { name nameWithOwner }
           b: repository(owner: \"o\", name: \"two\") { name nameWithOwner } }",
        &Value::Null,
    );
    assert_eq!(probe(), 2);
}

#[test]
fn connections_page_with_cursors() {
    let w = world();
    let page = |vars: Value| {
        w.run(
            "query($first: Int, $last: Int, $after: String, $before: String) {
               repository(owner: \"o\", name: \"one\") { pullRequest(number: 1) {
                 reviewThreads(first: $first, last: $last, after: $after, before: $before) {
                   totalCount
                   edges { cursor node { path } }
                   pageInfo { hasNextPage hasPreviousPage startCursor endCursor } } } } }",
            &vars,
        )["data"]["repository"]["pullRequest"]["reviewThreads"]
            .clone()
    };
    assert_eq!(
        page(json!({"first": 2})),
        json!({
            "totalCount": 3,
            "edges": [
                {"cursor": "t1", "node": {"path": "src/1.rs"}},
                {"cursor": "t2", "node": {"path": "src/2.rs"}},
            ],
            "pageInfo": {"hasNextPage": true, "hasPreviousPage": false, "startCursor": "t1", "endCursor": "t2"},
        })
    );
    let next = page(json!({"first": 2, "after": "t2"}));
    assert_eq!(
        next["edges"],
        json!([{"cursor": "t3", "node": {"path": "src/3.rs"}}])
    );
    assert_eq!(next["pageInfo"]["hasNextPage"], false);
    assert_eq!(next["pageInfo"]["hasPreviousPage"], true);
    let back = page(json!({"last": 1, "before": "t3"}));
    assert_eq!(
        back["edges"],
        json!([{"cursor": "t2", "node": {"path": "src/2.rs"}}])
    );
    let empty = page(json!({"first": 5, "after": "t3"}));
    assert_eq!(
        empty,
        json!({
            "totalCount": 3,
            "edges": [],
            "pageInfo": {"hasNextPage": false, "hasPreviousPage": true, "startCursor": null, "endCursor": null},
        })
    );
}

#[test]
fn connections_need_first_or_last_up_to_100() {
    let w = world();
    let threads = |args: &str| {
        w.run(
            &format!(
                "{{ repository(owner: \"o\", name: \"one\") {{ pullRequest(number: 1) {{
                     reviewThreads{args} {{ totalCount }} }} }} }}"
            ),
            &Value::Null,
        )
    };
    let r = threads("");
    assert_eq!(types(&r), vec!["MISSING_PAGINATION_BOUNDARIES"]);
    assert_eq!(
        r["errors"][0]["path"],
        json!(["repository", "pullRequest", "reviewThreads"])
    );
    assert_eq!(
        r["data"]["repository"]["pullRequest"]["reviewThreads"],
        Value::Null
    );
    assert_eq!(
        types(&threads("(first: 101)")),
        vec!["EXCESSIVE_PAGINATION"]
    );
    assert_eq!(types(&threads("(last: 101)")), vec!["EXCESSIVE_PAGINATION"]);
    assert_eq!(threads("(first: 100)")["errors"], Value::Null);
}

#[test]
fn node_and_nodes_resolve_current_and_legacy_ids() {
    let w = world();
    let current = node_id::entity("PR", 7);
    let legacy = node_id::encode_legacy("PullRequest", 7);
    let r = w.run(
        "query($id: ID!) { node(id: $id) { ... on PullRequest { number } } }",
        &json!({"id": legacy}),
    );
    assert_eq!(r["data"], json!({"node": {"number": 7}}));
    assert_eq!(r["extensions"]["warnings"][0]["type"], "DEPRECATION");
    assert_eq!(
        r["extensions"]["warnings"][0]["data"],
        json!({"legacy_global_id": legacy, "next_global_id": current})
    );
    let r = w.run(
        "query($id: ID!) { node(id: $id) { ... on PullRequest { number } } }",
        &json!({"id": current}),
    );
    assert_eq!(r, json!({"data": {"node": {"number": 7}}}));
}

#[test]
fn unknown_nodes_are_null_with_not_found() {
    let w = world();
    let r = w.run(
        "query($ids: [ID!]!) { nodes(ids: $ids) { ... on PullRequest { number } } }",
        &json!({"ids": [node_id::entity("PR", 1), node_id::entity("PR", 0), "garbage", node_id::entity("ZZ", 1)]}),
    );
    assert_eq!(
        r["data"],
        json!({"nodes": [{"number": 1}, null, null, null]})
    );
    assert_eq!(types(&r), vec!["NOT_FOUND", "NOT_FOUND", "NOT_FOUND"]);
    let paths: Vec<&Value> = r["errors"]
        .as_array()
        .map_or_else(Vec::new, |e| e.iter().map(|e| &e["path"]).collect());
    assert_eq!(
        paths,
        vec![
            &json!(["nodes", 1]),
            &json!(["nodes", 2]),
            &json!(["nodes", 3])
        ]
    );
    let r = w.run("{ node(id: \"garbage\") { id } }", &Value::Null);
    assert_eq!(r["data"], json!({"node": null}));
    assert_eq!(types(&r), vec!["NOT_FOUND"]);
    assert_eq!(r["errors"][0]["path"], json!(["node"]));
}

#[test]
fn mutations_run_serially_each_in_its_own_update() {
    // Given three mutations where the second is denied after it wrote
    let w = world();
    let r = w.run(
        "mutation {
           a: addComment(input: {subjectId: \"x\", body: \"one\"}) { clientMutationId }
           b: addComment(input: {subjectId: \"x\", body: \"deny\"}) { clientMutationId }
           c: addComment(input: {subjectId: \"x\", body: \"two\"}) { clientMutationId }
         }",
        &Value::Null,
    );
    // Then each sees the ones committed before it, and the denied one changed nothing
    assert_eq!(
        r["data"],
        json!({"a": {"clientMutationId": "1"}, "b": null, "c": {"clientMutationId": "2"}})
    );
    assert_eq!(types(&r), vec!["FORBIDDEN"]);
    assert_eq!(r["errors"][0]["path"], json!(["b"]));
    let notes = w.store.view(|tx| Ok(tx.table::<Notes>().0.clone()));
    assert_eq!(notes.ok(), Some(vec!["one".to_owned(), "two".to_owned()]));
}

#[test]
fn an_unregistered_mutation_is_not_implemented() {
    let w = world();
    let r = w.run(
        "mutation { addStar(input: {starrableId: \"x\"}) { clientMutationId } }",
        &Value::Null,
    );
    assert_eq!(r["data"], json!({"addStar": null}));
    assert_eq!(types(&r), vec!["HUBDOUBLE_NOT_IMPLEMENTED"]);
}

#[test]
fn document_and_variable_errors_answer_without_data_or_type() {
    let w = world();
    for (query, vars) in [
        (
            "{ repository(owner: \"o\", name: \"one\") { nope } }",
            Value::Null,
        ),
        ("{ repository(", Value::Null),
        ("query($id: ID!) { node(id: $id) { id } }", Value::Null),
        ("query($n: Int!) { viewer { login } }", json!({"n": "x"})),
    ] {
        let r = w.run(query, &vars);
        assert_eq!(r.get("data"), None, "{query}");
        let errors = r["errors"].as_array().cloned().unwrap_or_default();
        assert_ne!(errors.len(), 0, "{query}");
        assert!(errors.iter().all(|e| e.get("type").is_none()), "{query}");
    }
}

fn inventory_variables(document: &str) -> Value {
    let oid = "0123456789abcdef0123456789abcdef01234567";
    let input = match document {
        "createCommit" => json!({
            "branch": {"repositoryNameWithOwner": "o/one", "branchName": "main"},
            "message": {"headline": "change"},
            "expectedHeadOid": oid,
        }),
        _ => json!({
            "repositoryId": node_id::entity("R", 1),
            "refUpdates": [{"name": "refs/heads/main", "afterOid": oid}],
        }),
    };
    let after = (document == "workflows").then_some("HEAD");
    json!({
        "owner": "o", "name": "one", "repo": "one", "org": "o", "login": "u",
        "n0": "one", "n1": "hidden", "number": 1, "first": 5, "recent": 2,
        "id": node_id::entity("PR", 1), "thread": node_id::entity("PRRT", 1),
        "ids": [node_id::entity("PR", 1)], "review": node_id::entity("PRR", 1),
        "path": "a.md", "body": "b", "query": "repo:o/one", "branch": "refs/heads/main",
        "ref": "refs/heads/main", "r0": "refs/tags/v1", "e0": "HEAD:a", "e1": "HEAD:b",
        "before": "HEAD~1", "after": after, "input": input,
    })
}

#[test]
fn every_inventory_document_executes_or_reports_unimplemented_fields() {
    // Given every inventory document with realistic variables on the fake resolver set
    let w = world();
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/inventory/graphql");
    let mut count = 0;
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let query = std::fs::read_to_string(&path).unwrap_or_default();
        // When it runs
        let r = w.run(&query, &inventory_variables(name));
        // Then the request is accepted, and only fields the fake set lacks fail
        assert!(r.get("data").is_some(), "{name}: {r}");
        for kind in types(&r) {
            assert!(
                ["HUBDOUBLE_NOT_IMPLEMENTED", "NOT_FOUND"].contains(&kind),
                "{name}: {r}"
            );
        }
        count += 1;
    }
    assert_eq!(count, 30);
}

#[test]
fn the_largest_inventory_document_executes_without_errors() {
    let w = world();
    let query = include_str!("../../../docs/inventory/graphql/implementPull.graphql");
    let r = w.run(query, &json!({"owner": "o", "name": "one", "number": 1}));
    assert_eq!(r.get("errors"), None, "{r}");
    let threads = &r["data"]["repository"]["pullRequest"]["reviewThreads"];
    assert_eq!(threads["nodes"].as_array().map(Vec::len), Some(3));
    let recent = &threads["nodes"][1]["recent"];
    assert_eq!(recent["totalCount"], 40);
    assert_eq!(recent["nodes"].as_array().map(Vec::len), Some(30));
    assert_eq!(recent["nodes"][0]["databaseId"], 210);
    assert_eq!(recent["nodes"][0]["replyTo"], json!({"databaseId": 209}));
    assert_eq!(
        threads["nodes"][1]["comments"]["nodes"][0]["replyTo"],
        Value::Null
    );
}

fn account(id: i64, login: &str, kind: AccountKind) -> Account {
    Account {
        id,
        node_id: node_id::entity(node_id::USER, id),
        login: login.to_owned(),
        kind,
        name: None,
        email: None,
        company: None,
        blog: None,
        location: None,
        bio: None,
        twitter_username: None,
        description: None,
        site_admin: false,
        verified_email: true,
        sso_enforced: false,
        app_id: None,
        created_at: Timestamp::SEED,
        updated_at: Timestamp::SEED,
    }
}

fn private_repo(id: i64, owner_id: i64, name: &str) -> Repo {
    Repo {
        id,
        node_id: node_id::entity(node_id::REPOSITORY, id),
        owner_id,
        name: name.to_owned(),
        description: None,
        homepage: None,
        language: None,
        topics: Vec::new(),
        visibility: Visibility::Private,
        fork_of: None,
        default_branch: "main".to_owned(),
        archived: false,
        disabled: false,
        is_template: false,
        has_issues: true,
        has_projects: true,
        has_wiki: true,
        has_discussions: false,
        allow_merge_commit: true,
        allow_squash_merge: true,
        allow_rebase_merge: true,
        allow_auto_merge: false,
        delete_branch_on_merge: false,
        collaborators: Vec::new(),
        created_at: Timestamp::SEED,
        updated_at: Timestamp::SEED,
        pushed_at: None,
    }
}

#[test]
fn load_checks_the_target_and_the_actors_grant() {
    // Given an org, a user and a private repository of the org, read anonymously
    let w = world();
    let seeded = w.store.update(|tx| {
        tx.table_mut::<Accounts>()
            .insert(account(1, "o", AccountKind::Organization))?;
        tx.table_mut::<Accounts>()
            .insert(account(2, "u", AccountKind::User))?;
        tx.table_mut::<Repos>()
            .insert(private_repo(3, 1, "secret"))?;
        Ok(())
    });
    assert_eq!(seeded, Ok(()));
    let kinds = w.store.view(|tx| {
        let cx = Ctx {
            tx,
            git: &w.git,
            actor: &Actor::Anonymous,
            clock: &w.clock,
            urls: &w.urls,
            registry: &w.registry,
        };
        let kind = |t: Target<'_>| match cx.load(t) {
            Ok(Resolved::Object { type_name, .. }) => Ok(type_name),
            Ok(_) => Ok("other"),
            Err(e) => Err(e.kind),
        };
        Ok(vec![
            kind(Target::Repo {
                owner: "o",
                name: "secret",
            }),
            kind(Target::RepoId(3)),
            kind(Target::Repo {
                owner: "o",
                name: "missing",
            }),
            kind(Target::Repo {
                owner: "nobody",
                name: "secret",
            }),
            kind(Target::Org("o")),
            kind(Target::Org("u")),
            kind(Target::Node(&node_id::entity("PR", 4))),
            kind(Target::Node(&node_id::encode_legacy("PullRequest", 4))),
            kind(Target::Node(&node_id::entity("PR", 0))),
        ])
    });
    // Then a repository the actor cannot see is NOT_FOUND like a missing one
    assert_eq!(
        kinds,
        Ok(vec![
            Err(ErrorType::NotFound),
            Err(ErrorType::NotFound),
            Err(ErrorType::NotFound),
            Err(ErrorType::NotFound),
            Ok("Organization"),
            Err(ErrorType::NotFound),
            Ok("PullRequest"),
            Ok("PullRequest"),
            Err(ErrorType::NotFound),
        ])
    );
}

#[derive(serde::Serialize)]
struct Login {
    login: String,
}

#[expect(clippy::unnecessary_wraps, reason = "a resolver returns a Result")]
fn wrong_value(
    _: &hubdouble_gql::Ctx<'_>,
    _: &Obj,
    _: &hubdouble_gql::Args,
) -> Result<Resolved, GqlError> {
    Ok(Resolved::Object {
        type_name: "User",
        value: Arc::new(42_u32),
    })
}

#[test]
#[should_panic(expected = "User resolved to a value that is not a")]
fn a_value_of_the_wrong_rust_type_panics_in_test_builds() {
    let mut r = Registry::with_schema(fake::SCHEMA.clone());
    r.object::<Login>("User");
    r.field("Query", "viewer", wrong_value);
    let _ = World::new(r).run("{ viewer { login } }", &Value::Null);
}

#[test]
#[should_panic(expected = "registered twice")]
fn a_second_registration_of_a_field_panics() {
    let mut r = Registry::with_schema(fake::SCHEMA.clone());
    r.field("Query", "viewer", wrong_value);
    r.field("Query", "viewer", wrong_value);
}

#[test]
#[should_panic(expected = "registered twice")]
fn query_node_belongs_to_the_engine() {
    let mut r = Registry::with_schema(fake::SCHEMA.clone());
    r.field("Query", "node", wrong_value);
}

#[test]
#[should_panic(expected = "is not a field")]
fn a_field_outside_the_schema_panics() {
    let mut r = Registry::with_schema(fake::SCHEMA.clone());
    r.field("Repository", "nope", wrong_value);
}

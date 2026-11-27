use super::*;

#[tokio::test]
async fn branch_rules_include_active_push_rules_from_the_fork_root() {
    let server = start(vec![
        json!({"kind": "repository", "owner": "alice", "name": "fork", "private": true,
               "fork_of": "acme/private"}),
        json!({"kind": "ruleset", "id": 8251, "repository": "acme/private", "name": "push",
               "target": "push", "enforcement": "active",
               "bypass_actors": [{"actor_type": "OrganizationAdmin", "actor_id": 1}],
               "rules": [{"type": "file_path_restriction",
                          "parameters": {"restricted_file_paths": ["test/**"]}}]}),
        json!({"kind": "ruleset", "id": 8252, "repository": "acme/private", "name": "evaluate",
               "target": "push", "enforcement": "evaluate",
               "rules": [{"type": "file_extension_restriction",
                          "parameters": {"restricted_file_extensions": ["zip"]}}]}),
    ])
    .await;
    let answer = alice(&server)
        .get("repos/alice/fork/rules/branches/new-branch")
        .await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(
        valid("repos/get-branch-rules", &answer),
        json!([{"type": "file_path_restriction",
                "parameters": {"restricted_file_paths": ["test/**"]},
                "ruleset_source_type": "Repository", "ruleset_source": "acme/private",
                "ruleset_id": 8251}])
    );
    let id = answer.json()[0]["ruleset_id"].as_i64().unwrap();
    let path = format!("repos/alice/fork/rulesets/{id}");
    let detail = alice(&server)
        .get(&format!("{path}?includes_parents=true"))
        .await;
    assert_eq!(detail.status, StatusCode::OK);
    assert_eq!(detail.json()["current_user_can_bypass"], "always");
    let excluded = alice(&server)
        .get(&format!("{path}?includes_parents=false"))
        .await;
    assert_eq!(excluded.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn repository_property_conditions_distinguish_forks_from_other_repositories() {
    let fork = json!({"name": "fork", "source": "system", "property_values": ["true"]});
    let nonfork = json!({"name": "fork", "source": "system", "property_values": ["false"]});
    let rule = |id, include: Value, exclude: Value| {
        json!({
            "kind": "ruleset", "org": "acme", "id": id, "name": "policy", "target": "repository",
            "conditions": {"repository_property": {"include": include, "exclude": exclude}},
            "rules": [{"type": "repository_delete"}]
        })
    };
    let server = start(vec![
        json!({"kind": "repository", "owner": "alice", "name": "source"}),
        json!({"kind": "repository", "owner": "acme", "name": "fork", "fork_of": "alice/source"}),
        rule(8253, json!([fork]), json!([])),
        rule(8254, json!([]), json!([nonfork])),
    ])
    .await;
    for (repo, expected) in [("public", vec![]), ("fork", vec![8253, 8254])] {
        let response = alice(&server)
            .get(&format!("repos/acme/{repo}/rules/branches/main"))
            .await;
        assert_eq!(response.status, StatusCode::OK);
        let mut ids: Vec<i64> = response
            .json()
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["ruleset_id"].as_i64().unwrap())
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, expected, "{repo}");
    }
}

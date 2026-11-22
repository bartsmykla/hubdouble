use std::fmt::Write as _;

use super::{GoGithub, Inventory, REPO, StatusCode, Value, args, json, node_id, success};

pub(super) async fn run(alice: &GoGithub, cases: &mut Inventory) {
    let empty = || args(json!({}));
    let data = cases.run("defaultHead", empty()).await;
    let main = &data["repository"]["defaultBranchRef"]["target"]["oid"];
    assert_eq!(data["repository"]["defaultBranchRef"]["name"], "main");
    let head = cases
        .run("head", args(json!({"ref":"refs/heads/feature"})))
        .await;
    let feature = &head["repository"]["ref"]["target"]["oid"];
    assert_ne!(feature, main);
    let data = cases
        .run(
            "workflows",
            args(json!({"before":"main:.github/workflows/ci.yml",
        "after":"feature:.github/workflows/ci.yml"})),
        )
        .await;
    assert_ne!(
        data["repository"]["before"]["oid"],
        data["repository"]["after"]["oid"]
    );
    assert_ne!(data["repository"]["before"]["oid"], main.clone());
    let data = cases
        .run("releasesProbe", args(json!({"branch":"refs/heads/main"})))
        .await;
    assert_eq!(data["repository"]["refs"]["totalCount"], 1);
    assert_eq!(data["repository"]["main"]["target"]["oid"], *main);
    let tag = data["repository"]["refs"]["nodes"][0]["target"]["oid"].clone();
    let data = cases.run("tags", empty()).await;
    assert_eq!(
        data["repository"]["refs"]["nodes"],
        json!([{
            "name":"v5.0.0","target":{"oid":tag,"target":{"oid":main}}
        }])
    );
    assert_eq!(data["repository"]["refs"]["pageInfo"]["hasNextPage"], false);
    let data = cases
        .run("versionRef", args(json!({"r0":"refs/tags/agent/v5.0.0"})))
        .await;
    assert_eq!(
        data["repository"]["r0"]["target"],
        json!({"oid":tag,"target":{"oid":main}})
    );
    let data = cases
        .run(
            "versionFiles",
            args(json!({"e0":"main:README.md","e1":"feature:draft.md"})),
        )
        .await;
    assert_eq!(
        data["repository"],
        json!({
            "f0":{"text":"# Docs\n","isTruncated":false},"f1":{"text":"# Draft\n","isTruncated":false}
        })
    );
    let data = cases
        .run(
            "holdsFiles",
            args(json!({"e0":"main:.github/workflows","e1":"feature:"})),
        )
        .await;
    assert_eq!(
        data["repository"]["t0"]["entries"],
        json!([{"name":"ci.yml","mode":33188}])
    );
    assert_eq!(
        data["repository"]["t1"]["entries"][2],
        json!({"name":"draft.md","mode":33188})
    );
    assert_eq!(
        cases.run("peelTags", empty()).await["repository"],
        json!({"t0":null})
    );
    assert_eq!(
        cases.run("filesAt", empty()).await["repository"],
        json!({"t0":null,"t1":null})
    );
    alias_cases(
        cases,
        main.as_str().unwrap_or_default(),
        tag.as_str().unwrap_or_default(),
    )
    .await;
    mutations(alice, cases, main, feature).await;
}

async fn alias_cases(cases: &Inventory, main: &str, tag: &str) {
    for count in [1, 100] {
        let open =
            "query($owner: String!, $name: String!) { repository(owner: $owner, name: $name) {";
        let mut doc = open.to_owned();
        for i in 0..count {
            let _ = writeln!(
                doc,
                "t{i}: object(expression: \"{main}:.github/pull_request_template.md\") {{ ... on Blob {{ text isTruncated }} }}"
            );
            let _ = writeln!(
                doc,
                "p{i}: object(oid: \"{tag}\") {{ oid __typename ... on Tag {{ target {{ oid __typename }} }} }}"
            );
        }
        doc.push_str("}}");
        let output = success(&cases.api, &doc, args(json!({}))).await;
        for i in 0..count {
            assert_eq!(
                output["repository"][format!("t{i}")],
                json!({"text":"# Template\n","isTruncated":false})
            );
            assert_eq!(
                output["repository"][format!("p{i}")],
                json!({
                    "oid":tag,"__typename":"Tag","target":{"oid":main,"__typename":"Commit"}
                })
            );
        }
        for (field, arg, value, suffix, expected) in [
            (
                "object",
                "expression",
                "main:.github/workflows",
                "... on Tree { entries { name mode } }",
                json!({"entries":[{"name":"ci.yml","mode":33188}]}),
            ),
            (
                "object",
                "expression",
                "main:README.md",
                "... on Blob { text isTruncated }",
                json!({"text":"# Docs\n","isTruncated":false}),
            ),
            (
                "ref",
                "qualifiedName",
                "refs/tags/agent/v5.0.0",
                "target { oid ... on Tag { target { oid } } }",
                json!({"target":{"oid":tag,"target":{"oid":main}}}),
            ),
        ] {
            let mut query = "query($owner: String!, $name: String!, $value: String!) { repository(owner: $owner, name: $name) {".to_owned();
            for i in 0..count {
                let _ = writeln!(query, "a{i}: {field}({arg}: $value) {{ {suffix} }}");
            }
            query.push_str("}}");
            let result = success(&cases.api, &query, args(json!({"value":value}))).await;
            assert_eq!(
                result["repository"].as_object().map(serde_json::Map::len),
                Some(count)
            );
            for i in 0..count {
                assert_eq!(result["repository"][format!("a{i}")], expected);
            }
        }
    }
}

async fn mutations(alice: &GoGithub, cases: &mut Inventory, main: &Value, feature: &Value) {
    let repo = alice.get(REPO).await;
    assert_eq!(repo.status, StatusCode::OK);
    let raw = repo.json();
    let id = node_id::encode_legacy(
        "Repository",
        raw["id"]
            .as_i64()
            .unwrap_or_else(|| panic!("invalid repository: {raw}")),
    );
    let input = json!({"branch":{"repositoryNameWithOwner":"acme/docs","branchName":"main"},
        "expectedHeadOid":main,"message":{"headline":"Add a file"},
        "fileChanges":{"additions":[{"path":"file.txt","contents":"YQo="}]}});
    let data = cases.run("createCommit", json!({"input":input})).await;
    let commit = &data["createCommitOnBranch"]["commit"];
    assert_ne!(commit["oid"], *main);
    assert_eq!(commit["signature"]["state"], "VALID");
    let oid = commit["oid"].as_str().unwrap_or_default();
    let stored = alice.get(&format!("{REPO}/git/commits/{oid}")).await;
    assert_eq!(stored.status, StatusCode::OK);
    assert_eq!(stored.json()["tree"]["sha"], commit["tree"]["oid"]);
    let data = cases
        .run(
            "updateRefs",
            json!({"input":{"repositoryId":id,"clientMutationId":"move",
        "refUpdates":[{"name":"refs/heads/develop","beforeOid":main,"afterOid":feature}]}}),
        )
        .await;
    assert_eq!(data["updateRefs"]["clientMutationId"], "move");
    let data = cases
        .run("head", args(json!({"ref":"refs/heads/develop"})))
        .await;
    assert_eq!(data["repository"]["ref"]["target"]["oid"], *feature);
}

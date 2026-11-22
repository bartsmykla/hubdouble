use std::collections::BTreeMap;

use hubdouble_model::{Access, Clock, Token, TokenKind};
use hubdouble_store::{Installations, Tokens};
use hubdouble_testkit::{GoGithub, Graphql, TestServer, fixture};
use serde_json::json;

const DOC: &str = include_str!("../../../docs/inventory/graphql/createCommit.graphql");

#[tokio::test]
async fn workflow_changes_require_a_separate_installation_grant() {
    let server = TestServer::builder()
        .frame(hubdouble_api::register_all)
        .fixture()
        .start()
        .await;
    let services = server.server.services();
    let main = services
        .store
        .update_quiet(|tx| {
            let mut installation = (**tx
                .table::<Installations>()
                .get(fixture::INSTALLATION_ID)
                .unwrap())
            .clone();
            installation
                .permissions
                .insert("contents".into(), Access::Write);
            installation
                .permissions
                .insert("workflows".into(), Access::Write);
            tx.table_mut::<Installations>().replace(installation)?;
            for (token, workflow) in [
                ("ghs_contents_only", false),
                ("ghs_contents_and_workflows", true),
            ] {
                let mut permissions = BTreeMap::from([("contents".into(), Access::Write)]);
                if workflow {
                    permissions.insert("workflows".into(), Access::Write);
                }
                let id = tx.next_id();
                tx.table_mut::<Tokens>().insert(Token {
                    id,
                    token: token.into(),
                    kind: TokenKind::Installation,
                    app_id: Some(fixture::APP_ID),
                    installation_id: Some(fixture::INSTALLATION_ID),
                    user_id: None,
                    repo_ids: Some(vec![fixture::PUBLIC_REPO_ID]),
                    permissions,
                    scopes: vec![],
                    expires_at: None,
                    revoked: false,
                    owner_seeded: false,
                    created_at: server.clock.now(),
                })?;
            }
            Ok(services
                .git
                .resolve_ref(tx, fixture::PUBLIC_REPO_ID, "refs/heads/main")?)
        })
        .unwrap();
    let input = json!({"input": {
        "branch": {"repositoryNameWithOwner": "acme/public", "branchName": "main"},
        "expectedHeadOid": main.to_string(),
        "message": {"headline": "Add a workflow"},
        "fileChanges": {"additions": [{"path": ".github/workflows/check.yml", "contents": "e30K"}]}
    }});

    let denied = Graphql::new(GoGithub::new(&server).with_token("ghs_contents_only"))
        .query(DOC, &input)
        .await
        .json();
    assert_eq!(denied["errors"][0]["type"], "FORBIDDEN");

    let allowed = Graphql::new(GoGithub::new(&server).with_token("ghs_contents_and_workflows"))
        .query(DOC, &input)
        .await
        .json();
    assert!(allowed.get("errors").is_none(), "{allowed}");
    let oid = &allowed["data"]["createCommitOnBranch"]["commit"]["oid"];
    assert!(oid.is_string());
    assert_ne!(*oid, json!(main.to_string()));
}

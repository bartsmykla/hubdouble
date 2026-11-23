use std::collections::BTreeMap;

use http::{Method, StatusCode};
use hubdouble_model::{Access, Timestamp, Token, TokenKind};
use hubdouble_store::{Apps, Installations, Tokens};
use hubdouble_testkit::fixture::{
    self, ALICE_TOKEN, APP_ID, APP_PRIVATE_KEY, INSTALLATION_ID, PUBLIC_REPO_ID,
};
use hubdouble_testkit::{Answer, GoGithub, TestServer};
use serde_json::{Value, json};

pub(super) const ACTIONS: &str = "repos/acme/public/actions";
pub(super) const PRIVATE_ACTIONS: &str = "repos/acme/private/actions";
pub(super) const WORKFLOW: &str = ".github/workflows/agent.yml";
pub(super) const RUNTIME_ID: i64 = 3003;
pub(super) const RUNTIME_TAG: &str = "v5.0.0";
pub(super) const WRITER: &str = "ghs_actions_writer";
pub(super) const LIMITED: &str = "ghs_actions_limited";

pub(super) async fn server(auto_complete: &str) -> TestServer {
    let mut builder = TestServer::builder()
        .register(hubdouble_api_checks::register)
        .register(hubdouble_api_actions::register)
        .entry(json!({
            "kind": "repository", "id": RUNTIME_ID, "owner": "acme", "name": "runtime",
            "files": {WORKFLOW: "on: workflow_call\njobs: {check: {}}\n"},
            "tags": [{"name": RUNTIME_TAG, "message": "v5 release"}],
        }));
    for mut entry in fixture::seed() {
        match entry["kind"].as_str() {
            Some("repository") => {
                entry["files"][WORKFLOW] =
                    json!("name: AGENT\non: workflow_dispatch\njobs: {agent: {}}\n");
                if entry["name"] == "public" {
                    entry["commits"].as_array_mut().unwrap().push(json!({
                        "branch": "main", "message": "Pin the reusable workflow",
                        "files": {WORKFLOW: concat!(
                            "name: AGENT\non: {workflow_dispatch: {}}\njobs:\n  agent:\n",
                            "    uses: acme/runtime/.github/workflows/agent.yml@{{sha \"acme/runtime\" \"v5.0.0\"}}\n",
                        )},
                    }));
                }
                entry["files"][".github/workflows/ci.yml"] =
                    json!("on: [push]\njobs: {test: {}}\n");
                entry["files"][".github/workflows/input.yml"] = json!(
                    "on:\n  workflow_dispatch:\n    inputs:\n      ref-name: {required: true}\njobs: {test: {}}\n"
                );
                entry["files"][".github/workflows/multi.yml"] =
                    json!("name: Multi\non: workflow_dispatch\njobs: {fail: {}, pass: {}}\n");
                entry["files"][".github/workflows/chain.yml"] = json!(concat!(
                    "name: Chain\non: workflow_dispatch\njobs:\n  build: {}\n  lint: {}\n",
                    "  test: {needs: build}\n  deploy: {needs: [lint, test]}\n",
                ));
            }
            Some("app") => {
                entry["permissions"]["actions"] = json!("write");
                entry["events"] = json!(["workflow_run", "workflow_job"]);
            }
            Some("installation") => entry["permissions"]["actions"] = json!("write"),
            _ => {}
        }
        builder = builder.entry(entry);
    }
    let server = builder
        .entry(json!({"kind": "actions", "auto_complete": auto_complete, "oidc_signing_key": APP_PRIVATE_KEY}))
        .entry(json!({"kind": "actions_secret", "repository": "acme/public", "name": "has_secret"}))
        .entry(json!({"kind": "actions_variable", "repository": "acme/public", "name": "mode", "value": "local"}))
        .entry(json!({"kind": "actions_secret", "organization": "acme", "name": "shared", "visibility": "selected", "repositories": ["acme/public"]}))
        .entry(json!({"kind": "actions_variable", "organization": "acme", "name": "visible", "value": "yes", "visibility": "all"}))
        .entry(json!({"kind": "actions_variable", "organization": "acme", "name": "hidden", "value": "no", "visibility": "private"}))
        .start().await;
    server
        .server
        .services()
        .store
        .update(|tx| {
            assert!(tx.table::<Apps>().get(APP_ID).is_some());
            assert!(tx.table::<Installations>().get(INSTALLATION_ID).is_some());
            for (token, name, access) in [
                (WRITER, "actions", Access::Write),
                (LIMITED, "metadata", Access::Read),
            ] {
                let row = Token {
                    id: tx.next_id(),
                    token: token.to_owned(),
                    kind: TokenKind::Installation,
                    app_id: Some(APP_ID),
                    installation_id: Some(INSTALLATION_ID),
                    user_id: None,
                    repo_ids: Some(vec![PUBLIC_REPO_ID]),
                    permissions: BTreeMap::from([(name.to_owned(), access)]),
                    scopes: Vec::new(),
                    expires_at: None,
                    revoked: false,
                    owner_seeded: true,
                    created_at: Timestamp::SEED,
                };
                tx.table_mut::<Tokens>().insert(row)?;
            }
            Ok(())
        })
        .unwrap();
    server
}

pub(super) fn client(server: &TestServer) -> GoGithub {
    GoGithub::enterprise(server).with_token(ALICE_TOKEN)
}

pub(super) fn valid(op: &str, answer: &Answer) -> Value {
    serde_json::from_slice(&answer.body)
        .unwrap_or_else(|e| panic!("actions/{op} {}: not JSON: {e}", answer.status))
}

pub(super) async fn dispatch(client: &GoGithub, git_ref: &str) -> Value {
    dispatch_workflow(client, "agent.yml", git_ref).await
}

pub(super) async fn dispatch_workflow(client: &GoGithub, workflow: &str, git_ref: &str) -> Value {
    let answer = client
        .post(
            &format!("{ACTIONS}/workflows/{workflow}/dispatches"),
            &json!({"ref":git_ref,"return_run_details":true}),
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK);
    valid("create-workflow-dispatch", &answer)
}

pub(super) async fn drain(server: &TestServer) {
    assert_eq!(
        server.admin(Method::POST, "hooks/drain", None).await.status,
        StatusCode::NO_CONTENT
    );
}

use http::{Method, StatusCode};
use hubdouble_model::Timestamp;
use hubdouble_testkit::fixture::{ALICE_ID, ALICE_TOKEN, ORG_ID, PUBLIC_REPO_ID};
use hubdouble_testkit::{GoGithub, TestServer};
use openidconnect::core::{
    CoreGenderClaim, CoreIdTokenVerifier, CoreJsonWebKeySet, CoreJweContentEncryptionAlgorithm,
    CoreJwsSigningAlgorithm,
};
use openidconnect::{AdditionalClaims, ClientId, IdToken, IssuerUrl};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::support::{ACTIONS, RUNTIME_ID, RUNTIME_TAG, dispatch, server, valid};

const OIDC: &str = "_actions/.well-known/openid-configuration";
const JWKS: &str = "_actions/.well-known/jwks";

#[derive(Debug, Deserialize, Serialize)]
struct ExtraClaims {
    #[serde(flatten)]
    extra: Map<String, Value>,
}

impl AdditionalClaims for ExtraClaims {}
type TestToken = IdToken<
    ExtraClaims,
    CoreGenderClaim,
    CoreJweContentEncryptionAlgorithm,
    CoreJwsSigningAlgorithm,
>;

#[tokio::test]
async fn stock_oidc_verifier_checks_admin_and_runner_tokens_against_the_discovered_key() {
    let server = server("none").await;
    let client = GoGithub::new(&server);
    let authed = client.clone().with_token(ALICE_TOKEN);
    let services = server.server.services();
    let pin = services
        .store
        .view(|tx| {
            let tag =
                services
                    .git
                    .resolve_ref(tx, RUNTIME_ID, &format!("refs/tags/{RUNTIME_TAG}"))?;
            Ok(services
                .git
                .peel_to_commit(tx, RUNTIME_ID, tag)?
                .to_string())
        })
        .unwrap();
    let id = dispatch(&authed, "main").await["workflow_run_id"]
        .as_i64()
        .unwrap();
    let run = valid(
        "get-workflow-run",
        &authed.get(&format!("{ACTIONS}/runs/{id}")).await,
    );
    let jobs = valid(
        "list-jobs-for-workflow-run",
        &authed.get(&format!("{ACTIONS}/runs/{id}/jobs")).await,
    );
    let job = jobs["jobs"][0]["id"].as_i64().unwrap();
    let admin = server.admin(Method::POST, "actions/oidc/token", Some(&json!({
        "run_id": id, "job_id": job, "audience": "example", "claims": {
            "environment": "local:ci", "runner_environment": "self-hosted", "jti": "test-id",
        },
    }))).await;
    assert_eq!(admin.status, StatusCode::OK);
    assert_eq!(admin.header("cache-control"), "no-store");
    let tokens = admin.json();
    let issuer = format!("{}/_actions", server.base);
    let request_url = tokens["request_url"].as_str().unwrap();
    assert!(request_url.starts_with(&format!("{issuer}/token?")));
    let key_set = discover(&client, &server.base, &issuer).await;
    let time = serde_json::from_value(json!("2026-01-01T00:00:00Z")).unwrap();
    let verifier = |audience: &str| {
        CoreIdTokenVerifier::new_public_client(
            ClientId::new(audience.to_owned()),
            IssuerUrl::new(issuer.clone()).unwrap(),
            key_set.clone(),
        )
        .set_time_fn(move || time)
    };
    let token: TestToken = tokens["token"].as_str().unwrap().parse().unwrap();
    let accept = |_: Option<&openidconnect::Nonce>| Ok(());
    let claims = token.claims(&verifier("example"), accept).unwrap();
    assert_eq!(
        token.signing_alg().unwrap(),
        &CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256
    );
    assert_eq!(claims.issuer().as_str(), issuer);
    assert_eq!(claims.audiences()[0].as_str(), "example");
    assert_eq!(
        claims.subject().as_str(),
        "repo:acme/public:environment:local%3Aci"
    );
    assert_eq!(claims.issue_time().timestamp(), Timestamp::SEED.unix());
    assert_eq!(
        claims.expiration().timestamp(),
        Timestamp::SEED.unix() + 300
    );
    assert!(token.claims(&verifier("different"), accept).is_err());
    let sha = run["head_sha"].as_str().unwrap();
    assert_admin_claims(claims.additional_claims(), id, job, sha, &pin);

    let path = format!(
        "{}&audience=local%2Ftest",
        request_url.strip_prefix(&server.base).unwrap()
    );
    assert_eq!(client.get(&path).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(authed.get(&path).await.status, StatusCode::UNAUTHORIZED);
    let runner = client.with_token(tokens["request_token"].as_str().unwrap());
    let response = runner.get(&path).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.header("cache-control"), "no-store");
    let token: TestToken = response.json()["value"].as_str().unwrap().parse().unwrap();
    let claims = token.claims(&verifier("local/test"), accept).unwrap();
    assert_eq!(
        claims.subject().as_str(),
        "repo:acme/public:ref:refs/heads/main"
    );
    assert_eq!(claims.additional_claims().extra["run_id"], id.to_string());
    assert_eq!(
        runner.get(&format!("{ACTIONS}/runs/{id}")).await.status,
        StatusCode::UNAUTHORIZED
    );
}

async fn discover(client: &GoGithub, base: &str, issuer: &str) -> CoreJsonWebKeySet {
    let discovery = client.get(OIDC).await;
    assert_eq!(discovery.status, StatusCode::OK);
    let discovery = discovery.json();
    assert_eq!(discovery["issuer"], issuer);
    assert_eq!(
        discovery["id_token_signing_alg_values_supported"],
        json!(["RS256"])
    );
    let jwks = discovery["jwks_uri"]
        .as_str()
        .unwrap()
        .strip_prefix(base)
        .unwrap();
    let answer = client.get(jwks).await;
    assert_eq!(answer.status, StatusCode::OK);
    serde_json::from_value(answer.json()).unwrap()
}

fn assert_admin_claims(claims: &ExtraClaims, id: i64, job: i64, sha: &str, pin: &str) {
    assert_eq!(
        json!(claims.extra),
        json!({
            "nbf": Timestamp::SEED.unix(), "jti":"test-id", "repository":"acme/public",
            "repository_id":PUBLIC_REPO_ID.to_string(), "repository_owner":"acme", "repository_owner_id":ORG_ID.to_string(),
            "repository_visibility":"public", "ref":"refs/heads/main", "ref_type":"branch", "sha":sha,
            "workflow":"AGENT", "workflow_ref":"acme/public/.github/workflows/agent.yml@refs/heads/main",
        "workflow_sha":sha, "job_workflow_ref":format!("acme/runtime/.github/workflows/agent.yml@{pin}"),
        "job_workflow_sha":pin, "run_id":id.to_string(), "run_number":"1", "run_attempt":"1",
            "check_run_id":job.to_string(), "actor":"alice", "actor_id":ALICE_ID.to_string(),
            "event_name":"workflow_dispatch", "environment":"local:ci", "runner_environment":"self-hosted",
        })
    );
}

#[tokio::test]
async fn generated_oidc_key_survives_reset() {
    let server = TestServer::builder()
        .register(hubdouble_api_actions::register)
        .start()
        .await;
    let client = GoGithub::new(&server);
    let first = client.get(JWKS).await;
    assert_eq!(first.status, StatusCode::OK);
    let first = first.json();
    let keys: CoreJsonWebKeySet = serde_json::from_value(first.clone()).unwrap();
    assert_eq!(keys.keys().len(), 1);
    assert_eq!(first["keys"][0]["alg"], "RS256");
    assert!(first["keys"][0].get("d").is_none());
    assert_eq!(
        server.admin(Method::POST, "reset", None).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(client.get(JWKS).await.json(), first);
}

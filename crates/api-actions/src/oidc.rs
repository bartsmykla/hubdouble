use std::sync::{Arc, LazyLock};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use http::{Method, StatusCode, header};
use hubdouble_admin::{AdminRequest, AdminResponse, Registrar, error_response, json_response};
use hubdouble_model::{ApiError, Requirement, Timestamp, WorkflowJob, WorkflowRun};
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::{Accounts, Repos, Table, Tx, TxMut, WorkflowJobs};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use rsa::pkcs1::{DecodeRsaPrivateKey as _, EncodeRsaPrivateKey as _};
use rsa::pkcs8::DecodePrivateKey as _;
use rsa::traits::PublicKeyParts as _;
use rsa::{Generate as _, PrivateKey};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::runs::{self, RunContext, RunContexts};

const TOKEN_TTL: i64 = 300;
const TOKEN_PATH: &str = "/_actions/token";
const DISCOVERY_PATH: &str = "/_actions/.well-known/openid-configuration";
const JWKS_PATH: &str = "/_actions/.well-known/jwks";
const TIME_CLAIMS: [&str; 3] = ["exp", "nbf", "iat"];
const CLAIMS: [&str; 29] = [
    "iss",
    "sub",
    "aud",
    "exp",
    "nbf",
    "iat",
    "jti",
    "repository",
    "repository_id",
    "repository_owner",
    "repository_owner_id",
    "repository_visibility",
    "ref",
    "ref_type",
    "sha",
    "workflow",
    "workflow_ref",
    "workflow_sha",
    "job_workflow_ref",
    "job_workflow_sha",
    "run_id",
    "run_number",
    "run_attempt",
    "check_run_id",
    "actor",
    "actor_id",
    "event_name",
    "environment",
    "runner_environment",
];

#[derive(Clone)]
pub(crate) struct SigningKey {
    default: Arc<LazyLock<Result<Key, ApiError>>>,
    configured: Option<Arc<Key>>,
}

impl Default for SigningKey {
    fn default() -> Self {
        Self {
            default: Arc::new(LazyLock::new(|| {
                let private = PrivateKey::try_generate()
                    .map_err(|_| ApiError::internal("Could not generate an OIDC key"))?;
                Key::new(&private)
            })),
            configured: None,
        }
    }
}

impl Table for SigningKey {}

impl SigningKey {
    fn get(&self) -> Result<&Key, ApiError> {
        match &self.configured {
            Some(key) => Ok(key),
            None => self.default.as_ref().as_ref().map_err(Clone::clone),
        }
    }
}

struct Key {
    signing: EncodingKey,
    public: Jwk,
}

#[derive(Serialize)]
struct Jwk {
    kty: &'static str,
    alg: &'static str,
    #[serde(rename = "use")]
    purpose: &'static str,
    kid: String,
    n: String,
    e: String,
}

impl Key {
    fn new(private: &PrivateKey) -> Result<Self, ApiError> {
        let der = private
            .to_pkcs1_der()
            .map_err(|_| ApiError::internal("Could not encode an OIDC key"))?;
        let n = private.n().to_be_bytes_trimmed_vartime();
        let mut thumbprint = Sha256::new();
        thumbprint.update(&n);
        let e = private.e().to_be_bytes_trimmed_vartime();
        thumbprint.update(&e);
        Ok(Self {
            signing: EncodingKey::from_rsa_der(der.as_bytes()),
            public: Jwk {
                kty: "RSA",
                alg: "RS256",
                purpose: "sig",
                kid: URL_SAFE_NO_PAD.encode(thumbprint.finalize()),
                n: URL_SAFE_NO_PAD.encode(n),
                e: URL_SAFE_NO_PAD.encode(e),
            },
        })
    }
}

pub(crate) fn configure(tx: &mut TxMut<'_>, pem: &str) -> Result<(), ApiError> {
    let private = PrivateKey::from_pkcs8_pem(pem)
        .or_else(|_| PrivateKey::from_pkcs1_pem(pem))
        .map_err(|_| crate::invalid("OIDC", "oidc_signing_key", "Invalid RSA private key PEM"))?;
    if private.n().bit_width() < 2048 {
        return Err(crate::invalid(
            "OIDC",
            "oidc_signing_key",
            "The signing key must be at least 2048 bits",
        ));
    }
    tx.table_mut::<SigningKey>().get_mut().configured = Some(Arc::new(Key::new(&private)?));
    Ok(())
}

pub(crate) fn register(r: &mut Registrar<'_>) {
    let req = Requirement::public();
    r.rest.extra(
        Method::GET,
        DISCOVERY_PATH,
        "actions/oidc-discovery",
        req,
        discovery,
    );
    r.rest
        .extra(Method::GET, JWKS_PATH, "actions/oidc-jwks", req, jwks);
    r.rest
        .extra_authenticating(Method::GET, TOKEN_PATH, "actions/oidc-token", req, runner);
    r.admin.route(
        Method::POST,
        "/actions/oidc/token",
        Arc::new(|req| Box::pin(admin(req))),
    );
}

fn discovery(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let issuer = &ctx.tx().config().oidc_issuer;
    Reply::json(&json!({
        "issuer": issuer,
        "jwks_uri": format!("{issuer}/.well-known/jwks"),
        "id_token_signing_alg_values_supported": ["RS256"],
        "subject_types_supported": ["public", "pairwise"],
        "response_types_supported": ["id_token"],
        "claims_supported": CLAIMS,
    }))
}

fn jwks(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let key = ctx.tx().table::<SigningKey>().get()?;
    Reply::json(&json!({ "keys": [&key.public] }))
}

fn runner(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let secret = ctx
        .header("authorization")
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, value)| value.trim())
        .ok_or_else(ApiError::unauthorized)?;
    let context = ctx
        .tx()
        .table::<RunContexts>()
        .by_key(secret)
        .ok_or_else(ApiError::unauthorized)?;
    let run = runs::find(ctx.tx(), context.run).map_err(|_| ApiError::unauthorized())?;
    let token = mint(
        ctx.tx(),
        run,
        context,
        None,
        ctx.now(),
        ctx.query().get("audience"),
        Map::new(),
    )?;
    Reply::json(&json!({ "value": token }))?.header(header::CACHE_CONTROL, "no-store")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MintRequest {
    run_id: i64,
    job_id: Option<i64>,
    audience: Option<String>,
    #[serde(default)]
    claims: Map<String, Value>,
}

async fn admin(req: AdminRequest) -> AdminResponse {
    match admin_token(req).await {
        Ok(value) => {
            let mut response = json_response(StatusCode::OK, &value);
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                http::HeaderValue::from_static("no-store"),
            );
            response
        }
        Err(error) => error_response(&error),
    }
}

async fn admin_token(req: AdminRequest) -> Result<Value, ApiError> {
    let value = hubdouble_admin::read_json(req.request, req.services.config.max_body_bytes).await?;
    let input: MintRequest =
        serde_json::from_value(value).map_err(|e| ApiError::unprocessable(e.to_string()))?;
    req.services.store.view(|tx| {
        let run = runs::find(tx, input.run_id)?;
        let context = tx
            .table::<RunContexts>()
            .get(run.id)
            .ok_or_else(ApiError::not_found)?;
        let token = mint(
            tx,
            run,
            context,
            input.job_id,
            req.services.clock.now(),
            input.audience.as_deref(),
            input.claims,
        )?;
        Ok(json!({
            "token": token,
            "request_token": context.request_token,
            "request_url": format!("{}/token?api-version=2.0", tx.config().oidc_issuer),
        }))
    })
}

fn job<'a>(
    tx: &'a Tx,
    run: &WorkflowRun,
    id: Option<i64>,
) -> Result<Option<&'a Arc<WorkflowJob>>, ApiError> {
    let jobs = tx.table::<WorkflowJobs>();
    let job = if let Some(id) = id {
        Some(
            jobs.get(id)
                .filter(|job| job.run_id == run.id && job.run_attempt == run.run_attempt)
                .ok_or_else(ApiError::not_found)?,
        )
    } else {
        jobs.children(run.id)
            .find(|job| job.run_attempt == run.run_attempt)
    };
    Ok(job)
}

fn claims(
    tx: &Tx,
    run: &WorkflowRun,
    context: &RunContext,
    check: Option<i64>,
    now: Timestamp,
    audience: Option<&str>,
    mut overrides: Map<String, Value>,
) -> Result<Value, ApiError> {
    let repo = tx
        .table::<Repos>()
        .get(run.repo_id)
        .ok_or_else(ApiError::not_found)?;
    let (full_name, owner) = crate::workflows::repo_name(tx, repo)?;
    let actor = tx
        .table::<Accounts>()
        .get(run.actor_id)
        .ok_or_else(ApiError::not_found)?;
    let workflow_ref = format!("{full_name}/{}@{}", run.path, context.git_ref);
    let (job_workflow_ref, job_workflow_sha) = context
        .reusable
        .as_ref()
        .map_or((workflow_ref.as_str(), run.head_sha.as_str()), |r| {
            (&r.workflow_ref, &r.sha)
        });
    let environment = overrides
        .get("environment")
        .and_then(Value::as_str)
        .or(context.environment.as_deref())
        .unwrap_or_default();
    let subject = if environment.is_empty() {
        format!("repo:{full_name}:ref:{}", context.git_ref)
    } else {
        format!(
            "repo:{full_name}:environment:{}",
            environment.replace(':', "%3A")
        )
    };
    let default_audience = format!("https://github.com/{owner}");
    let mut claims = json!({
        "iss": tx.config().oidc_issuer,
        "sub": subject,
        "aud": audience.unwrap_or(&default_audience),
        "exp": now.plus_seconds(TOKEN_TTL).unix(),
        "nbf": now.unix(),
        "iat": now.unix(),
        "jti": Uuid::new_v4().to_string(),
        "repository": full_name,
        "repository_id": repo.id.to_string(),
        "repository_owner": owner,
        "repository_owner_id": repo.owner_id.to_string(),
        "repository_visibility": repo.visibility.as_str(),
        "ref": context.git_ref,
        "ref_type": if context.git_ref.starts_with("refs/tags/") { "tag" } else { "branch" },
        "sha": run.head_sha,
        "workflow": run.name,
        "workflow_ref": workflow_ref,
        "workflow_sha": run.head_sha,
        "job_workflow_ref": job_workflow_ref,
        "job_workflow_sha": job_workflow_sha,
        "run_id": run.id.to_string(),
        "run_number": run.run_number.to_string(),
        "run_attempt": run.run_attempt.to_string(),
        "check_run_id": check.map_or_else(String::new, |id| id.to_string()),
        "actor": actor.login,
        "actor_id": actor.id.to_string(),
        "event_name": run.event,
        "environment": environment,
        "runner_environment": "github-hosted",
    });
    let Some(values) = claims.as_object_mut() else {
        return Err(ApiError::internal("Could not render OIDC claims"));
    };
    for (key, value) in &overrides {
        let valid_type = if TIME_CLAIMS.contains(&key.as_str()) {
            value.as_i64().is_some()
        } else {
            value.is_string()
        };
        if !values.contains_key(key) || !valid_type {
            return Err(crate::invalid("OIDC", key, "Invalid claim name or type"));
        }
    }
    values.append(&mut overrides);
    Ok(claims)
}

fn mint(
    tx: &Tx,
    run: &WorkflowRun,
    context: &RunContext,
    job_id: Option<i64>,
    now: Timestamp,
    audience: Option<&str>,
    overrides: Map<String, Value>,
) -> Result<String, ApiError> {
    let check = job(tx, run, job_id)?.and_then(|job| job.check_run_id);
    let claims = claims(tx, run, context, check, now, audience, overrides)?;
    let key = tx.table::<SigningKey>().get()?;
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(key.public.kid.clone());
    jsonwebtoken::encode(&header, &claims, &key.signing)
        .map_err(|_| ApiError::internal("Could not sign an OIDC token"))
}

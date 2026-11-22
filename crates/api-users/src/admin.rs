use std::sync::Arc;

use http::{Method, StatusCode};
use hubdouble_admin::{Admin, AdminRequest, AdminResponse, SeedCtx, error_response, read_json};
use hubdouble_model::ApiError;
use serde::Deserialize;

use crate::state::{self, Decision};

pub(crate) fn register(admin: &mut Admin) {
    admin.kind("oauth", oauth_kind);
    admin.route(
        Method::POST,
        "/oauth/device/{user_code}",
        Arc::new(|request| Box::pin(approve(request))),
    );
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OAuthSeed {
    default_user: String,
}

fn oauth_kind(ctx: &mut SeedCtx<'_>, v: serde_json::Value) -> Result<(), ApiError> {
    let s: OAuthSeed =
        serde_json::from_value(v).map_err(|e| ApiError::unprocessable(e.to_string()))?;
    let user = state::person(&ctx.tx, &s.default_user)?.id;
    state::set_default_user(&mut ctx.tx, user)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Approval {
    login: Option<String>,
    #[serde(default = "yes")]
    approve: bool,
}

fn yes() -> bool {
    true
}

async fn approve(request: AdminRequest) -> AdminResponse {
    let services = request.services.clone();
    let params = request.params.clone();
    let result = async {
        let value = read_json(request.request, services.config.max_body_bytes).await?;
        let input: Approval =
            serde_json::from_value(value).map_err(|e| ApiError::unprocessable(e.to_string()))?;
        let code = params
            .get("user_code")
            .ok_or_else(ApiError::not_found)?
            .to_owned();
        services.store.update(|tx| {
            let decision = if input.approve {
                Decision::Approved(state::signed_in(tx, input.login.as_deref(), None)?.id)
            } else {
                Decision::Denied
            };
            state::decide(tx, &code, decision, services.clock.now())
        })
    }
    .await;
    match result {
        Ok(()) => {
            let mut response = AdminResponse::default();
            *response.status_mut() = StatusCode::NO_CONTENT;
            response
        }
        Err(error) => error_response(&error),
    }
}

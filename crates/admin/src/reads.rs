use std::sync::Arc;

use http::StatusCode;
use hubdouble_model::ApiError;
use hubdouble_store::{Accounts, Apps, Tokens};
use serde_json::json;

use crate::http::{error_response, json_response};
use crate::{AdminHandler, AdminRequest, AdminResponse};

fn answer(r: Result<serde_json::Value, ApiError>) -> AdminResponse {
    match r {
        Ok(v) => json_response(StatusCode::OK, &v),
        Err(e) => error_response(&e),
    }
}

fn user(req: &AdminRequest) -> Result<serde_json::Value, ApiError> {
    let login = req.params.get("login").unwrap_or_default();
    req.services.store.view(|tx| {
        let a = tx
            .table::<Accounts>()
            .by_login(login)
            .ok_or_else(ApiError::not_found)?;
        let tokens: Vec<_> = tx
            .table::<Tokens>()
            .children(a.id)
            .filter(|t| t.user_id == Some(a.id))
            .map(|t| {
                json!({
                    "id": t.id, "token": t.token, "kind": t.kind.as_str(), "scopes": t.scopes,
                    "expires_at": t.expires_at, "revoked": t.revoked,
                })
            })
            .collect();
        Ok(json!({
            "id": a.id, "node_id": a.node_id, "login": a.login, "type": a.kind.as_str(),
            "tokens": tokens,
        }))
    })
}

fn app(req: &AdminRequest) -> Result<serde_json::Value, ApiError> {
    let slug = req.params.get("slug").unwrap_or_default();
    req.services.store.view(|tx| {
        let apps = tx.table::<Apps>();
        let app = slug
            .parse::<i64>()
            .ok()
            .and_then(|id| apps.get(id))
            .or_else(|| apps.by_slug(slug))
            .ok_or_else(ApiError::not_found)?;
        let bot = tx.table::<Accounts>().get(app.bot_id);
        Ok(json!({
            "id": app.id, "slug": app.slug, "node_id": app.node_id,
            "client_id": app.client_id, "client_secret": app.client_secret,
            "public_key": app.public_key_pem, "private_key": app.private_key_pem,
            "webhook": app.webhook,
            "bot": bot.map(|b| json!({"id": b.id, "login": b.login})),
        }))
    })
}

pub(crate) fn user_route() -> AdminHandler {
    Arc::new(|req| Box::pin(async move { answer(user(&req)) }))
}

pub(crate) fn app_route() -> AdminHandler {
    Arc::new(|req| Box::pin(async move { answer(app(&req)) }))
}

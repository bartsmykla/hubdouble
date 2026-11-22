use std::sync::Arc;

use http::{Method, StatusCode};
use hubdouble_admin::{
    Admin, AdminRequest, AdminResponse, error_response, json_response, read_json,
};
use hubdouble_model::ApiError;
use hubdouble_render::Urls;
use serde_json::Value;

use crate::{lifecycle, runs, views};

pub(crate) fn register(admin: &mut Admin) {
    admin.route(
        Method::POST,
        "/actions/runs/{id}",
        Arc::new(|req| Box::pin(async move { answer(update(req).await) })),
    );
}

fn answer(result: Result<Value, ApiError>) -> AdminResponse {
    match result {
        Ok(value) => json_response(StatusCode::OK, &value),
        Err(error) => error_response(&error),
    }
}

async fn update(req: AdminRequest) -> Result<Value, ApiError> {
    let id = req
        .params
        .get("id")
        .and_then(|value| value.parse().ok())
        .ok_or_else(ApiError::not_found)?;
    let patch = read_json(req.request, req.services.config.max_body_bytes).await?;
    let patch =
        serde_json::from_value(patch).map_err(|e| ApiError::unprocessable(e.to_string()))?;
    let now = req.services.clock.now();
    let git = &req.services.git;
    req.services.store.update(|tx| {
        let run = Arc::clone(runs::find(tx, id)?);
        let run = lifecycle::apply(tx, git, &run, now, run.actor_id, patch)?;
        let urls = Urls::new(tx.config(), "");
        serde_json::to_value(views::run(tx, git, &urls, &run, false)?)
            .map_err(|e| ApiError::internal(e.to_string()))
    })
}

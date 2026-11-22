use std::sync::Arc;

use http::{Method, StatusCode};
use hubdouble_admin::{Admin, AdminRequest, AdminResponse, SeedCtx, error_response, read_json};
use hubdouble_model::{AccountKind, ApiError, Repo};
use hubdouble_store::{Accounts, Apps, CheckRuns, Repos, Tx};
use serde::Deserialize;
use serde_json::Value;

use crate::runs::{self, RunInput};
use crate::statuses::{self, StatusInput};

pub fn register(admin: &mut Admin) {
    admin.kind("check_run", check_run);
    admin.kind("status", status);
    admin.route(
        Method::POST,
        "/check-runs/{id}/requested-action",
        Arc::new(|request| Box::pin(requested_action(request))),
    );
}

fn unprocessable(e: &impl ToString) -> ApiError {
    ApiError::unprocessable(e.to_string())
}

fn repository(tx: &Tx, full_name: &str) -> Result<Arc<Repo>, ApiError> {
    let unknown = || ApiError::unprocessable("No such repository");
    let (owner, name) = full_name.split_once('/').ok_or_else(unknown)?;
    let owner = tx.table::<Accounts>().by_login(owner).ok_or_else(unknown)?;
    tx.table::<Repos>()
        .by_name(owner.id, name)
        .cloned()
        .ok_or_else(unknown)
}

fn account(tx: &Tx, login: &str) -> Result<i64, ApiError> {
    tx.table::<Accounts>()
        .by_login(login)
        .filter(|a| a.kind != AccountKind::Organization)
        .map(|a| a.id)
        .ok_or_else(|| ApiError::unprocessable("No such account"))
}

#[derive(Deserialize)]
struct RunSeed {
    id: Option<i64>,
    repository: String,
    app: String,
    #[serde(flatten)]
    run: RunInput,
}

fn check_run(ctx: &mut SeedCtx<'_>, body: Value) -> Result<(), ApiError> {
    let seed: RunSeed = serde_json::from_value(body).map_err(|e| unprocessable(&e))?;
    let repo = repository(&ctx.tx, &seed.repository)?;
    let app = ctx
        .tx
        .table::<Apps>()
        .by_slug(&seed.app)
        .cloned()
        .ok_or_else(|| ApiError::unprocessable("No such App"))?;
    let now = ctx.clock.now();
    runs::create_run(
        &mut ctx.tx,
        ctx.git,
        (&repo, app.id, app.bot_id),
        seed.run,
        seed.id,
        now,
    )?;
    Ok(())
}

#[derive(Deserialize)]
struct StatusSeed {
    id: Option<i64>,
    repository: String,
    sha: String,
    creator: String,
    #[serde(flatten)]
    status: StatusInput,
}

fn status(ctx: &mut SeedCtx<'_>, body: Value) -> Result<(), ApiError> {
    let seed: StatusSeed = serde_json::from_value(body).map_err(|e| unprocessable(&e))?;
    let repo = repository(&ctx.tx, &seed.repository)?;
    let creator = account(&ctx.tx, &seed.creator)?;
    let now = ctx.clock.now();
    statuses::insert(
        &mut ctx.tx,
        ctx.git,
        (&repo, creator),
        &seed.sha,
        seed.status,
        (seed.id, now),
    )?;
    Ok(())
}

#[derive(Deserialize)]
struct ActionPress {
    identifier: String,
    sender: String,
}

async fn requested_action(request: AdminRequest) -> AdminResponse {
    let services = request.services;
    let result = async {
        let id: i64 = request
            .params
            .get("id")
            .and_then(|id| id.parse().ok())
            .ok_or_else(ApiError::not_found)?;
        let value = read_json(request.request, services.config.max_body_bytes).await?;
        let press: ActionPress = serde_json::from_value(value).map_err(|e| unprocessable(&e))?;
        services.store.update(|tx| {
            let run = tx
                .table::<CheckRuns>()
                .get(id)
                .cloned()
                .ok_or_else(ApiError::not_found)?;
            let sender = account(tx, &press.sender)?;
            runs::requested_action(tx, &services.git, &run, sender, &press.identifier)
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

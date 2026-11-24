use std::sync::Arc;

use http::StatusCode;
use hubdouble_hooks::Injection;
use hubdouble_model::{AccountKind, ApiError};
use hubdouble_store::{Accounts, Apps, Repos, Tx};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::http::{error_response, read_json};
use crate::kinds::AppRef;
use crate::{AdminHandler, AdminRequest, AdminResponse};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Inject {
    event: String,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    app: Option<AppRef>,
    #[serde(default)]
    hook: Option<i64>,
    #[serde(default)]
    installation: Option<i64>,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    sender: Option<String>,
    #[serde(default)]
    payload: Map<String, Value>,
}

fn named(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

fn app_id(tx: &Tx, input: &Inject) -> Result<i64, ApiError> {
    let apps = tx.table::<Apps>();
    let app = match (&input.app, input.hook.as_ref()) {
        (Some(AppRef::Id(id)), None) | (None, Some(id)) => apps.get(*id),
        (Some(AppRef::Slug(slug)), None) => apps.by_slug(slug),
        _ => return Err(ApiError::unprocessable("give one of app and hook")),
    };
    app.map(|a| a.id)
        .ok_or_else(|| ApiError::unprocessable("No such App"))
}

fn injection(tx: &Tx, input: Inject) -> Result<Injection, ApiError> {
    if !named(&input.event) || input.action.as_deref().is_some_and(|a| !named(a)) {
        return Err(ApiError::unprocessable(
            "event and action are lowercase names",
        ));
    }
    let accounts = tx.table::<Accounts>();
    let repo_id = input
        .repository
        .as_deref()
        .map(|full_name| {
            full_name
                .split_once('/')
                .and_then(|(owner, name)| {
                    let owner = accounts.by_login(owner)?;
                    tx.table::<Repos>().by_name(owner.id, name)
                })
                .map(|repo| repo.id)
                .ok_or_else(|| ApiError::unprocessable("No such repository"))
        })
        .transpose()?;
    let sender_id = input
        .sender
        .as_deref()
        .map(|login| {
            accounts
                .by_login(login)
                .filter(|a| a.kind != AccountKind::Organization)
                .map(|a| a.id)
                .ok_or_else(|| ApiError::unprocessable("No such account"))
        })
        .transpose()?;
    Ok(Injection {
        app_id: app_id(tx, &input)?,
        installation_id: input.installation,
        repo_id,
        sender_id,
        event: input.event,
        action: input.action,
        payload: input.payload,
    })
}

async fn inject(request: AdminRequest) -> AdminResponse {
    let services = request.services;
    let result = async {
        let value = read_json(request.request, services.config.max_body_bytes).await?;
        let input: Inject =
            serde_json::from_value(value).map_err(|e| ApiError::unprocessable(e.to_string()))?;
        services.store.update(|tx| {
            let injection = injection(tx, input)?;
            hubdouble_hooks::inject(tx, injection).map_err(ApiError::from)
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

pub(crate) fn route() -> AdminHandler {
    Arc::new(|request| Box::pin(inject(request)))
}

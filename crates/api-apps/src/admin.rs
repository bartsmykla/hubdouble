use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use http::{Method, StatusCode, Uri};
use hubdouble_admin::{
    Admin, AdminHandler, AdminRequest, AdminResponse, error_response, json_response, read_json,
};
use hubdouble_model::{
    Access, Account, AccountKind, ApiError, App, Installation, RepositorySelection, Timestamp,
    Token, TokenKind, Webhook,
};
use hubdouble_render::Urls;
use hubdouble_rest::Params;
use hubdouble_store::{
    Accounts, Apps, Deleted, Deliveries, Installations, RepoRedirects, Repos, Tokens, Tx, TxMut,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::{events, installations};

pub(crate) fn register(admin: &mut Admin) {
    admin.route(Method::PATCH, "/app/{slug}", patch(update_app));
    admin.route(Method::DELETE, "/app/{slug}", delete(delete_app));
    admin.route(
        Method::GET,
        "/installation/{id}",
        Arc::new(|request| {
            Box::pin(async move {
                answer(request.services.store.view(|tx| {
                    let installation = find_installation(tx, &request.params)?;
                    let urls = Urls::new(tx.config(), "");
                    let value = installations::render(tx, &urls, installation)?;
                    Ok(json!(value))
                }))
            })
        }),
    );
    admin.route(
        Method::PATCH,
        "/installation/{id}",
        patch(update_installation),
    );
    admin.route(
        Method::DELETE,
        "/installation/{id}",
        delete(delete_installation),
    );
    admin.route(
        Method::PATCH,
        "/installation-target/{login}",
        patch(rename_target),
    );
    admin.route(
        Method::DELETE,
        "/app/{slug}/authorizations/{login}",
        delete(revoke_authorization),
    );
}

type Patch<T> = fn(&mut TxMut<'_>, &Params, Timestamp, T) -> Result<Value, ApiError>;
type Delete = fn(&mut TxMut<'_>, &Params) -> Result<(), ApiError>;

fn patch<T: DeserializeOwned + 'static>(update: Patch<T>) -> AdminHandler {
    Arc::new(move |request| Box::pin(async move { answer(apply_patch(request, update).await) }))
}

async fn apply_patch<T: DeserializeOwned>(
    request: AdminRequest,
    update: Patch<T>,
) -> Result<Value, ApiError> {
    let limit = request.services.config.max_body_bytes;
    let value = read_json(request.request, limit).await?;
    let input =
        serde_json::from_value::<T>(value).map_err(|e| ApiError::unprocessable(e.to_string()))?;
    request
        .services
        .store
        .update(|tx| update(tx, &request.params, request.services.clock.now(), input))
}

fn delete(update: Delete) -> AdminHandler {
    Arc::new(move |request| {
        Box::pin(async move {
            match request
                .services
                .store
                .update(|tx| update(tx, &request.params))
            {
                Ok(()) => {
                    let mut response = AdminResponse::default();
                    *response.status_mut() = StatusCode::NO_CONTENT;
                    response
                }
                Err(error) => error_response(&error),
            }
        })
    })
}

fn answer(result: Result<Value, ApiError>) -> AdminResponse {
    match result {
        Ok(value) => json_response(StatusCode::OK, &value),
        Err(error) => error_response(&error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AppPatch {
    name: Option<String>,
    permissions: Option<BTreeMap<String, Access>>,
    events: Option<Vec<String>>,
    webhook: Option<WebhookInput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WebhookInput {
    url: String,
    secret: Option<String>,
    #[serde(default = "active")]
    active: bool,
}

fn active() -> bool {
    true
}

fn update_app(
    tx: &mut TxMut<'_>,
    params: &Params,
    now: Timestamp,
    input: AppPatch,
) -> Result<Value, ApiError> {
    let mut app = find_app(tx, params)?.clone();
    if let Some(name) = input.name {
        app.name = name;
    }
    if let Some(permissions) = input.permissions {
        if permissions
            .keys()
            .any(|p| !hubdouble_auth::APP_PERMISSIONS.contains(&p.as_str()))
        {
            return Err(ApiError::unprocessable("Invalid App permission"));
        }
        app.permissions = permissions;
    }
    if let Some(events) = input.events {
        app.events = events;
    }
    let ping = if let Some(hook) = input.webhook {
        let url = hook
            .url
            .parse::<Uri>()
            .map_err(|_| ApiError::unprocessable("Invalid webhook URL"))?;
        if !matches!(url.scheme_str(), Some("http" | "https")) || url.host().is_none() {
            return Err(ApiError::unprocessable("Invalid webhook URL"));
        }
        let webhook = Webhook {
            url: hook.url,
            secret: hook.secret,
            active: hook.active,
        };
        let changed = app.webhook.as_ref() != Some(&webhook);
        app.webhook = Some(webhook);
        changed
    } else {
        false
    };
    app.updated_at = now;
    let id = app.id;
    tx.table_mut::<Apps>().replace(app)?;
    if ping {
        hubdouble_hooks::ping(tx, id);
    }
    Ok(json!({ "id": id }))
}

fn delete_app(tx: &mut TxMut<'_>, params: &Params) -> Result<(), ApiError> {
    let app = find_app(tx, params)?;
    let (id, bot_id) = (app.id, app.bot_id);
    if tx.table::<Installations>().children(id).next().is_some() {
        return Err(ApiError::conflict("Delete the App's installations first"));
    }
    revoke_tokens(tx, |token| token.app_id == Some(id))?;
    if let Some(bot) = tx.table::<Accounts>().get(bot_id).cloned() {
        tx.delete(&Deleted::Account(bot))?;
    }
    tx.table_mut::<Deliveries>().remove_children(id);
    tx.table_mut::<Apps>().remove(id);
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallationPatch {
    suspended: Option<bool>,
    repository_selection: Option<RepositorySelection>,
    repositories: Option<Vec<String>>,
    permissions: Option<BTreeMap<String, Access>>,
}

fn update_installation(
    tx: &mut TxMut<'_>,
    params: &Params,
    now: Timestamp,
    input: InstallationPatch,
) -> Result<Value, ApiError> {
    let before = find_installation(tx, params)?.clone();
    let app = tx
        .table::<Apps>()
        .get(before.app_id)
        .ok_or_else(ApiError::not_found)?;
    let mut next = before.clone();
    apply_repositories(
        tx,
        &mut next,
        input.repository_selection,
        input.repositories,
    )?;
    if let Some(permissions) = input.permissions {
        if permissions
            .iter()
            .any(|(p, v)| app.permissions.get(p).is_none_or(|g| g < v))
        {
            return Err(ApiError::unprocessable(
                "Permission is beyond the App's grant",
            ));
        }
        next.permissions = permissions;
    }
    let suspension = match (input.suspended, before.suspended_at) {
        (Some(true), None) => {
            next.suspended_at = Some(now);
            next.suspended_by = Some(app.bot_id);
            Some("suspend")
        }
        (Some(false), Some(_)) => {
            next.suspended_at = None;
            next.suspended_by = None;
            Some("unsuspend")
        }
        _ => None,
    };
    if next != before {
        next.updated_at = now;
        tx.table_mut::<Installations>().replace(next.clone())?;
        if next.repo_ids != before.repo_ids
            || next.repository_selection != before.repository_selection
        {
            events::repository_change(tx, &before, &next)?;
        }
        if next.permissions != before.permissions {
            events::installation(tx, &next, "new_permissions_accepted")?;
        }
        if let Some(action) = suspension {
            events::installation(tx, &next, action)?;
        }
    }
    let rendered = installations::render(tx, &Urls::new(tx.config(), ""), &next)?;
    Ok(json!(rendered))
}

fn apply_repositories(
    tx: &Tx,
    installation: &mut Installation,
    selection: Option<RepositorySelection>,
    names: Option<Vec<String>>,
) -> Result<(), ApiError> {
    if let Some(selection) = selection {
        installation.repository_selection = selection;
    } else if names.is_some() {
        installation.repository_selection = RepositorySelection::Selected;
    }
    match installation.repository_selection {
        RepositorySelection::All => {
            if names.as_ref().is_some_and(|names| !names.is_empty()) {
                return Err(ApiError::unprocessable(
                    "Repositories require selected access",
                ));
            }
            installation.repo_ids.clear();
        }
        RepositorySelection::Selected => {
            if let Some(names) = names {
                installation.repo_ids = names
                    .iter()
                    .map(|name| {
                        tx.table::<Repos>()
                            .by_name(installation.account_id, name)
                            .map(|repo| repo.id)
                            .ok_or_else(|| {
                                ApiError::unprocessable("No such repository in this account")
                            })
                    })
                    .collect::<Result<BTreeSet<_>, _>>()?
                    .into_iter()
                    .collect();
            }
        }
    }
    Ok(())
}

fn delete_installation(tx: &mut TxMut<'_>, params: &Params) -> Result<(), ApiError> {
    let installation = find_installation(tx, params)?.clone();
    events::installation(tx, &installation, "deleted")?;
    revoke_tokens(tx, |token| token.installation_id == Some(installation.id))?;
    tx.table_mut::<Installations>().remove(installation.id);
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rename {
    login: String,
}

fn rename_target(
    tx: &mut TxMut<'_>,
    params: &Params,
    now: Timestamp,
    input: Rename,
) -> Result<Value, ApiError> {
    if input.login.is_empty()
        || !input
            .login
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(ApiError::unprocessable("Invalid account login"));
    }
    let login = param(params, "login")?;
    let account = tx
        .table::<Accounts>()
        .by_login(login)
        .filter(|a| a.kind != AccountKind::Bot)
        .ok_or_else(ApiError::not_found)?;
    if Account::is_ghost(account.id, &input.login) {
        return Err(ApiError::unprocessable(
            "The login is reserved for the ghost user",
        ));
    }
    let mut account = (**account).clone();
    if account.login != input.login {
        let old_login = std::mem::replace(&mut account.login, input.login);
        account.updated_at = now;
        tx.table_mut::<Accounts>().replace(account.clone())?;
        let names: Vec<_> = tx
            .table::<Repos>()
            .children(account.id)
            .map(|r| (r.id, r.name.clone()))
            .collect();
        let mut redirects = tx.table_mut::<RepoRedirects>();
        let redirects = redirects.get_mut();
        for (id, name) in names {
            redirects.moved(&old_login, &name, id);
            redirects.claim(&account.login, &name);
        }
        let affected: Vec<_> = tx
            .table::<Installations>()
            .iter()
            .filter(|i| i.account_id == account.id)
            .cloned()
            .collect();
        for installation in affected {
            events::target_renamed(tx, &installation, &account, &old_login)?;
        }
    }
    Ok(json!({ "id": account.id, "login": account.login }))
}

fn revoke_authorization(tx: &mut TxMut<'_>, params: &Params) -> Result<(), ApiError> {
    let app_id = find_app(tx, params)?.id;
    let account = tx
        .table::<Accounts>()
        .by_login(param(params, "login")?)
        .filter(|a| a.kind == AccountKind::User)
        .ok_or_else(ApiError::not_found)?
        .clone();
    revoke_tokens(tx, |token| {
        token.app_id == Some(app_id)
            && token.user_id == Some(account.id)
            && matches!(token.kind, TokenKind::User | TokenKind::Refresh)
    })?;
    events::authorization_revoked(tx, app_id, &account)
}

fn revoke_tokens(tx: &mut TxMut<'_>, matches: impl Fn(&Token) -> bool) -> Result<(), ApiError> {
    let tokens: Vec<_> = tx
        .table::<Tokens>()
        .iter()
        .filter(|token| !token.revoked && matches(token))
        .map(|t| Token {
            revoked: true,
            ..(**t).clone()
        })
        .collect();
    let mut table = tx.table_mut::<Tokens>();
    for token in tokens {
        table.replace(token)?;
    }
    Ok(())
}

fn param<'a>(params: &'a Params, key: &str) -> Result<&'a str, ApiError> {
    params.get(key).ok_or_else(ApiError::not_found)
}

fn find_app<'a>(tx: &'a Tx, params: &Params) -> Result<&'a App, ApiError> {
    let slug = param(params, "slug")?;
    let apps = tx.table::<Apps>();
    slug.parse::<i64>()
        .ok()
        .and_then(|id| apps.get(id))
        .or_else(|| apps.by_slug(slug))
        .map(AsRef::as_ref)
        .ok_or_else(ApiError::not_found)
}

fn find_installation<'a>(tx: &'a Tx, params: &Params) -> Result<&'a Installation, ApiError> {
    let id = param(params, "id")?
        .parse()
        .map_err(|_| ApiError::not_found())?;
    tx.table::<Installations>()
        .get(id)
        .map(AsRef::as_ref)
        .ok_or_else(ApiError::not_found)
}

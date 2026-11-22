use std::collections::BTreeSet;

use hubdouble_model::{Account, ApiError, Event, Installation, Repo};
use hubdouble_render::{SimpleUser, Urls};
use hubdouble_store::{Accounts, Apps, Tx, TxMut};
use serde_json::{Map, Value, json};

use crate::installations::{render, repos};

pub(crate) fn installation(
    tx: &mut TxMut<'_>,
    installation: &Installation,
    action: &'static str,
) -> Result<(), ApiError> {
    let account = owner(tx, installation)?;
    let repos = repos(tx, installation, None)
        .map(|repo| minimal_repo(repo, account))
        .collect::<Vec<_>>();
    let rendered = installation_value(tx, installation)?;
    emit(
        tx,
        installation,
        "installation",
        action,
        Map::from_iter([
            ("installation".into(), rendered),
            ("repositories".into(), json!(repos)),
        ]),
    )
}

pub(crate) fn repository_change(
    tx: &mut TxMut<'_>,
    prior: &Installation,
    current: &Installation,
) -> Result<(), ApiError> {
    let before = repos(tx, prior, None)
        .map(|repo| repo.id)
        .collect::<BTreeSet<_>>();
    let after = repos(tx, current, None)
        .map(|repo| repo.id)
        .collect::<BTreeSet<_>>();
    for (installation, action, field, other, excluded) in [
        (
            prior,
            "removed",
            "repositories_removed",
            "repositories_added",
            &after,
        ),
        (
            current,
            "added",
            "repositories_added",
            "repositories_removed",
            &before,
        ),
    ] {
        let account = owner(tx, installation)?;
        let changed = repos(tx, installation, None)
            .filter(|repo| !excluded.contains(&repo.id))
            .map(|repo| minimal_repo(repo, account))
            .collect::<Vec<_>>();
        if changed.is_empty() {
            continue;
        }
        let rendered = installation_value(tx, installation)?;
        let payload = Map::from_iter([
            ("installation".into(), rendered),
            (
                "repository_selection".into(),
                json!(installation.repository_selection.as_str()),
            ),
            ("requester".into(), Value::Null),
            (field.into(), json!(changed)),
            (other.into(), json!([])),
        ]);
        emit(
            tx,
            installation,
            "installation_repositories",
            action,
            payload,
        )?;
    }
    Ok(())
}

pub(crate) fn target_renamed(
    tx: &mut TxMut<'_>,
    installation: &Installation,
    account: &Account,
    old_login: &str,
) -> Result<(), ApiError> {
    let urls = Urls::new(tx.config(), "");
    let mut account_value = user(&urls, account)?;
    if let Some(fields) = account_value.as_object_mut() {
        fields.remove("user_view_type");
    }
    let payload = Map::from_iter([
        ("target_type".into(), json!(account.kind.as_str())),
        ("account".into(), account_value),
        ("changes".into(), json!({ "login": { "from": old_login } })),
    ]);
    emit(tx, installation, "installation_target", "renamed", payload)
}

pub(crate) fn authorization_revoked(
    tx: &mut TxMut<'_>,
    app_id: i64,
    account: &Account,
) -> Result<(), ApiError> {
    let sender = user(&Urls::new(tx.config(), ""), account)?;
    hubdouble_hooks::emit(
        tx,
        Event {
            name: "github_app_authorization",
            action: Some("revoked"),
            repo_id: None,
            org_id: None,
            sender_id: account.id,
            app_id: Some(app_id),
            installation_id: None,
            payload: Map::from_iter([("sender".into(), sender)]),
        },
    );
    Ok(())
}

fn emit(
    tx: &mut TxMut<'_>,
    installation: &Installation,
    name: &'static str,
    action: &'static str,
    mut payload: Map<String, Value>,
) -> Result<(), ApiError> {
    let sender_id = tx
        .table::<Apps>()
        .get(installation.app_id)
        .ok_or_else(ApiError::not_found)?
        .bot_id;
    let sender = tx
        .table::<Accounts>()
        .get(sender_id)
        .ok_or_else(ApiError::not_found)?;
    let sender = user(&Urls::new(tx.config(), ""), sender)?;
    payload.insert("sender".into(), sender);
    hubdouble_hooks::emit(
        tx,
        Event {
            name,
            action: Some(action),
            repo_id: None,
            org_id: None,
            sender_id,
            app_id: Some(installation.app_id),
            installation_id: Some(installation.id),
            payload,
        },
    );
    Ok(())
}

fn owner<'a>(tx: &'a Tx, installation: &Installation) -> Result<&'a Account, ApiError> {
    tx.table::<Accounts>()
        .get(installation.account_id)
        .map(AsRef::as_ref)
        .ok_or_else(ApiError::not_found)
}

fn minimal_repo(repo: &Repo, account: &Account) -> Value {
    json!({
        "id": repo.id,
        "node_id": repo.node_id,
        "name": repo.name,
        "full_name": format!("{}/{}", account.login, repo.name),
        "private": repo.is_private(),
    })
}

fn installation_value(tx: &Tx, installation: &Installation) -> Result<Value, ApiError> {
    let urls = Urls::new(tx.config(), "");
    let rendered = render(tx, &urls, installation)?;
    let mut value =
        serde_json::to_value(rendered).map_err(|e| ApiError::internal(e.to_string()))?;
    if let Some(installation) = value.as_object_mut() {
        installation.remove("client_id");
    }
    for key in ["account", "suspended_by"] {
        if let Some(account) = value.get_mut(key).and_then(Value::as_object_mut) {
            webhook_user(&urls, account);
        }
    }
    Ok(value)
}

fn user(urls: &Urls, account: &Account) -> Result<Value, ApiError> {
    let mut value = serde_json::to_value(SimpleUser::new(urls, account))
        .map_err(|e| ApiError::internal(e.to_string()))?;
    if let Some(object) = value.as_object_mut() {
        webhook_user(urls, object);
    }
    Ok(value)
}

fn webhook_user(urls: &Urls, user: &mut Map<String, Value>) {
    let Some(login) = user.get("login").and_then(Value::as_str) else {
        return;
    };
    let escaped = hubdouble_render::escape(login);
    if escaped == login {
        return;
    }
    let old = format!("{}/users/{login}", urls.api);
    let prefix = format!("{}/users/{escaped}", urls.api);
    for field in user.values_mut() {
        if let Value::String(uri) = field
            && let Some(suffix) = uri.strip_prefix(&old)
        {
            *uri = format!("{prefix}{suffix}");
        }
    }
}

use hubdouble_model::{Account, ApiError, App, Event, Installation, RepositorySelection};
use hubdouble_render::Urls;
use hubdouble_store::{Repos, TxMut};
use serde_json::{Map, json};

pub(crate) fn created(
    tx: &mut TxMut<'_>,
    installation: &Installation,
    app: &App,
    account: &Account,
) -> Result<(), ApiError> {
    if app.webhook.as_ref().is_none_or(|hook| !hook.active) {
        return Ok(());
    }
    let urls = Urls::new(tx.config(), "");
    let mut rendered = serde_json::to_value(hubdouble_render::Installation::new(
        &urls,
        installation,
        app,
        account,
        None,
    ))
    .map_err(|e| ApiError::internal(e.to_string()))?;
    if let Some(fields) = rendered.as_object_mut() {
        fields.remove("client_id");
    }
    let repositories = tx
        .table::<Repos>()
        .children(account.id)
        .filter(|repo| {
            installation.repository_selection == RepositorySelection::All
                || installation.repo_ids.contains(&repo.id)
        })
        .map(|repo| {
            json!({
                "id": repo.id, "node_id": repo.node_id, "name": repo.name,
                "full_name": format!("{}/{}", account.login, repo.name),
                "private": repo.is_private(),
            })
        })
        .collect::<Vec<_>>();
    hubdouble_hooks::emit(
        tx,
        Event {
            name: "installation",
            action: Some("created"),
            repo_id: None,
            org_id: None,
            sender_id: app.owner_id,
            app_id: Some(app.id),
            installation_id: Some(installation.id),
            payload: Map::from_iter([
                ("installation".into(), rendered),
                ("repositories".into(), json!(repositories)),
            ]),
        },
    );
    Ok(())
}

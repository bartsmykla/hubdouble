use hubdouble_git::Git;
use hubdouble_model::{ApiError, Issue};
use hubdouble_render::Urls;
use hubdouble_store::{Accounts, Repos, TxMut};
use serde_json::{Value, json};

pub(crate) fn emit(
    tx: &mut TxMut<'_>,
    git: &Git,
    issue: &Issue,
    sender: i64,
    action: &'static str,
    extra: Value,
) -> Result<(), ApiError> {
    let urls = Urls::new(tx.config(), "");
    let pull_request = crate::render::detail(git, tx, &urls, issue)?;
    let Value::Object(mut payload) = extra else {
        return Err(ApiError::internal(
            "pull request event fields must be an object",
        ));
    };
    let mut pull_request = json!(pull_request);
    if let Some(teams) = pull_request["requested_teams"].as_array_mut() {
        teams.iter_mut().for_each(webhook_team);
    }
    payload.insert("number".into(), json!(issue.number));
    payload.insert("pull_request".into(), pull_request);
    let org_id = tx
        .table::<Repos>()
        .get(issue.repo_id)
        .and_then(|repo| tx.table::<Accounts>().get(repo.owner_id))
        .filter(|owner| owner.kind == hubdouble_model::AccountKind::Organization)
        .map(|owner| owner.id);
    hubdouble_hooks::emit(
        tx,
        hubdouble_model::Event {
            name: "pull_request",
            action: Some(action),
            repo_id: Some(issue.repo_id),
            org_id,
            sender_id: sender,
            app_id: None,
            installation_id: None,
            payload,
        },
    );
    Ok(())
}

pub(crate) fn webhook_team(team: &mut Value) {
    without_ids(team);
    if let Some(parent) = team.get_mut("parent") {
        without_ids(parent);
    }
}

fn without_ids(team: &mut Value) {
    if let Some(fields) = team.as_object_mut() {
        fields.remove("type");
        fields.remove("organization_id");
    }
}

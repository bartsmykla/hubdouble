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
    payload.insert("number".into(), json!(issue.number));
    payload.insert("pull_request".into(), json!(pull_request));
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

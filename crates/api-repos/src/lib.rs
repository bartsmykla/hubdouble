mod activity;
mod admin;
mod codeowners;
mod graphql;
mod releases;
mod repos;
mod rules;

use std::sync::Arc;

use hubdouble_admin::Registrar;
use hubdouble_model::{Account, ApiError, Event, Grant, Repo, Requirement, Scope};
use hubdouble_store::{Accounts, Tx, TxMut};

/// Registers what T10 owns: tables, REST operations, GraphQL fields, ref subscribers, seed kinds
/// and admin routes.
pub fn register(r: &mut Registrar<'_>) {
    activity::register(r);
    repos::register(r);
    codeowners::register(r);
    rules::register(r);
    releases::register(r);
    graphql::register(r);
    admin::register(r);
}

const METADATA: Requirement = Requirement {
    any_of: &[Grant::read("metadata")],
    scope: Scope::Repo,
    scopes: &[],
};

const CONTENTS: Requirement = Requirement {
    any_of: &[Grant::read("contents")],
    scope: Scope::Repo,
    scopes: &[],
};

fn owner<'t>(tx: &'t Tx, repo: &Repo) -> Result<&'t Arc<Account>, ApiError> {
    tx.table::<Accounts>()
        .get(repo.owner_id)
        .ok_or_else(|| ApiError::internal("the repository has no owner"))
}

fn full_name(tx: &Tx, repo: &Repo) -> Result<String, ApiError> {
    Ok(format!("{}/{}", owner(tx, repo)?.login, repo.name))
}

fn emit(
    tx: &mut TxMut<'_>,
    name: &'static str,
    action: &'static str,
    repo_id: i64,
    sender_id: i64,
    payload: serde_json::Map<String, serde_json::Value>,
) {
    hubdouble_hooks::emit(
        tx,
        Event {
            name,
            action: Some(action),
            repo_id: Some(repo_id),
            org_id: None,
            sender_id,
            app_id: None,
            installation_id: None,
            payload,
        },
    );
}

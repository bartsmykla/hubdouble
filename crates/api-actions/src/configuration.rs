use std::sync::Arc;

use http::Method;
use hubdouble_admin::{AdminRequest, Registrar, SeedCtx, error_response, json_response};
use hubdouble_auth::Actor;
use hubdouble_model::{
    AccountKind, ApiError, Conclusion, Grant, Repo, RepositorySelection, Requirement, Timestamp,
};
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::{Accounts, Deleted, Entity, Repos, Rows, Table, Tx, TxMut};
use serde::{Deserialize, Serialize};

use crate::oidc;

const SECRETS: Requirement = Requirement {
    scopes: &["repo"],
    ..crate::repository(&[Grant::read("secrets")])
};
const VARIABLES: Requirement = Requirement {
    scopes: &["repo"],
    ..crate::repository(&[Grant::read("actions_variables")])
};

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Settings {
    #[default]
    None,
    Success,
    Failure,
}

impl Table for Settings {}

impl Settings {
    pub(crate) fn conclusion(self) -> Option<Conclusion> {
        match self {
            Self::None => None,
            Self::Success => Some(Conclusion::Success),
            Self::Failure => Some(Conclusion::Failure),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    auto_complete: Option<Settings>,
    oidc_signing_key: Option<String>,
}

pub(crate) type Entries = Rows<Entry>;

pub(crate) fn forget(tx: &mut TxMut<'_>, deleted: &Deleted) -> Result<(), ApiError> {
    let id = match deleted {
        Deleted::Repo(repo) => repo.id,
        Deleted::Account(account) => account.id,
        Deleted::Team(_) | Deleted::Environment(_) => return Ok(()),
    };
    tx.table_mut::<Entries>().remove_children(id);
    let selecting: Vec<_> = tx
        .table::<Entries>()
        .iter()
        .filter(|e| e.selected.contains(&id))
        .cloned()
        .collect();
    for entry in selecting {
        let mut entry = (*entry).clone();
        entry.selected.retain(|r| *r != id);
        tx.table_mut::<Entries>().replace(entry)?;
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct Entry {
    id: i64,
    scope: i64,
    variable: bool,
    visibility: Option<Visibility>,
    selected: Vec<i64>,
    value: EntryValue,
}

impl Entity for Entry {
    fn id(&self) -> i64 {
        self.id
    }

    fn parent_id(&self) -> Option<i64> {
        Some(self.scope)
    }

    fn keys(&self) -> Vec<String> {
        vec![key(self.scope, self.variable, &self.value.name)]
    }
}

#[derive(Clone, Serialize)]
struct EntryValue {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,
    created_at: Timestamp,
    updated_at: Timestamp,
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Visibility {
    All,
    Private,
    Selected,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EntryInput {
    repository: Option<String>,
    organization: Option<String>,
    name: String,
    value: Option<String>,
    visibility: Option<Visibility>,
    #[serde(default)]
    repositories: Vec<String>,
}

pub(crate) fn register(r: &mut Registrar<'_>) {
    r.admin.kind("actions", seed_policy);
    r.admin
        .kind("actions_secret", |ctx, body| seed_entry(ctx, body, false));
    r.admin
        .kind("actions_variable", |ctx, body| seed_entry(ctx, body, true));
    r.admin.route(
        Method::GET,
        "/actions",
        Arc::new(|request| {
            Box::pin(async move {
                let result = request
                    .services
                    .store
                    .view(|tx| Ok(serde_json::json!({"auto_complete": tx.table::<Settings>()})));
                match result {
                    Ok(value) => json_response(http::StatusCode::OK, &value),
                    Err(error) => error_response(&error),
                }
            })
        }),
    );
    for (scope, org) in [("repos/{owner}/{repo}", false), ("orgs/{owner}", true)] {
        for (kind, variable) in [("secrets", false), ("variables", true)] {
            let path = format!("/actions/{scope}/{kind}/{{name}}");
            for (method, delete) in [(Method::GET, false), (Method::DELETE, true)] {
                r.admin.route(
                    method,
                    &path,
                    Arc::new(move |request| {
                        Box::pin(async move { admin_entry(&request, org, variable, delete) })
                    }),
                );
            }
        }
    }
    r.rest
        .handle("actions/get-repo-secret", SECRETS, |ctx| get(ctx, false));
    r.rest
        .handle("actions/get-repo-variable", VARIABLES, |ctx| get(ctx, true));
    r.rest.handle("actions/list-repo-secrets", SECRETS, |ctx| {
        list(ctx, false, false)
    });
    r.rest
        .handle("actions/list-repo-variables", VARIABLES, |ctx| {
            list(ctx, true, false)
        });
    r.rest
        .handle("actions/list-repo-organization-secrets", SECRETS, |ctx| {
            list(ctx, false, true)
        });
    r.rest.handle(
        "actions/list-repo-organization-variables",
        VARIABLES,
        |ctx| list(ctx, true, true),
    );
}

fn seed_policy(ctx: &mut SeedCtx<'_>, body: serde_json::Value) -> Result<(), ApiError> {
    let policy: Policy =
        serde_json::from_value(body).map_err(|e| ApiError::unprocessable(e.to_string()))?;
    if let Some(mode) = policy.auto_complete {
        *ctx.tx.table_mut::<Settings>().get_mut() = mode;
    }
    if let Some(key) = policy.oidc_signing_key {
        oidc::configure(&mut ctx.tx, &key)?;
    }
    Ok(())
}

pub(crate) fn find_org(tx: &Tx, name: &str) -> Result<i64, ApiError> {
    tx.table::<Accounts>()
        .by_login(name)
        .filter(|a| a.kind == AccountKind::Organization)
        .map(|a| a.id)
        .ok_or_else(ApiError::not_found)
}

fn find_repo<'a>(tx: &'a Tx, owner: &str, repo: &str) -> Result<&'a Repo, ApiError> {
    let account = tx
        .table::<Accounts>()
        .by_login(owner)
        .ok_or_else(ApiError::not_found)?;
    tx.table::<Repos>()
        .by_name(account.id, repo)
        .map(Arc::as_ref)
        .ok_or_else(ApiError::not_found)
}

fn resolve_repo<'a>(tx: &'a Tx, full_name: &str) -> Result<&'a Repo, ApiError> {
    let (owner, repo) = full_name
        .split_once('/')
        .ok_or_else(|| ApiError::unprocessable("A repository must be named as owner/repo"))?;
    find_repo(tx, owner, repo)
}

fn seed_entry(
    ctx: &mut SeedCtx<'_>,
    body: serde_json::Value,
    variable: bool,
) -> Result<(), ApiError> {
    let input: EntryInput =
        serde_json::from_value(body).map_err(|e| ApiError::unprocessable(e.to_string()))?;
    let valid_name = input
        .name
        .bytes()
        .enumerate()
        .all(|(i, c)| c == b'_' || c.is_ascii_alphabetic() || i > 0 && c.is_ascii_digit());
    if input.name.is_empty() || !valid_name || variable != input.value.is_some() {
        return Err(ApiError::unprocessable("Invalid Actions name or value"));
    }
    let (scope, visibility) = match (input.repository, input.organization) {
        (Some(repo), None) if input.visibility.is_none() => {
            (resolve_repo(&ctx.tx, &repo)?.id, None)
        }
        (None, Some(org)) => (
            find_org(&ctx.tx, &org)?,
            Some(input.visibility.unwrap_or(Visibility::Private)),
        ),
        _ => {
            return Err(ApiError::unprocessable(
                "Choose a repository or an organization",
            ));
        }
    };
    if visibility != Some(Visibility::Selected) && !input.repositories.is_empty() {
        return Err(ApiError::unprocessable(
            "Only selected visibility takes repositories",
        ));
    }
    let selected = input
        .repositories
        .iter()
        .map(|name| {
            let repo = resolve_repo(&ctx.tx, name)?;
            if repo.owner_id != scope {
                return Err(ApiError::unprocessable(
                    "The organization does not own the selected repository",
                ));
            }
            Ok(repo.id)
        })
        .collect::<Result<_, _>>()?;
    let now = ctx.clock.now();
    let existing = ctx
        .tx
        .table::<Entries>()
        .by_key(&key(scope, variable, &input.name))
        .map(|entry| (entry.id, entry.value.created_at));
    let (id, created_at) = existing.unwrap_or_else(|| (ctx.tx.next_id(), now));
    let value = EntryValue {
        name: input.name.to_ascii_uppercase(),
        value: input.value,
        created_at,
        updated_at: now,
    };
    ctx.tx.table_mut::<Entries>().replace(Entry {
        id,
        scope,
        variable,
        visibility,
        selected,
        value,
    })?;
    Ok(())
}

fn key(scope: i64, variable: bool, name: &str) -> String {
    format!("{scope}:{variable}:{}", name.to_ascii_uppercase())
}

fn admin_entry(
    request: &AdminRequest,
    org: bool,
    variable: bool,
    delete: bool,
) -> hubdouble_admin::AdminResponse {
    let storage = &request.services.store;
    let find = |tx: &Tx| {
        let params = &request.params;
        let param = |name| params.get(name).ok_or_else(ApiError::not_found);
        let scope = if org {
            find_org(tx, param("owner")?)?
        } else {
            find_repo(tx, param("owner")?, param("repo")?)?.id
        };
        let found = tx
            .table::<Entries>()
            .by_key(&key(scope, variable, param("name")?))
            .ok_or_else(ApiError::not_found)?;
        Ok(Arc::clone(found))
    };
    if delete {
        return match storage.update(|tx| {
            let entry = find(tx)?;
            tx.table_mut::<Entries>().remove(entry.id);
            Ok(())
        }) {
            Ok(()) => {
                let mut response = hubdouble_admin::AdminResponse::default();
                *response.status_mut() = http::StatusCode::NO_CONTENT;
                response
            }
            Err(error) => error_response(&error),
        };
    }
    match storage.view(find) {
        Ok(entry) => json_response(http::StatusCode::OK, &entry.value),
        Err(error) => error_response(&error),
    }
}

fn collaborator(ctx: &Ctx<'_>, repo: &Repo, permission: &'static str) -> Result<(), ApiError> {
    let has_access = match ctx.actor() {
        Actor::Anonymous => return Err(ApiError::unauthorized()),
        Actor::User { user, .. } => hubdouble_auth::repo_role(ctx.tx(), user.id, repo).is_some(),
        Actor::Installation {
            installation,
            token,
            ..
        } => {
            installation.account_id == repo.owner_id
                && (installation.repository_selection == RepositorySelection::All
                    || installation.repo_ids.contains(&repo.id))
                && token
                    .repo_ids
                    .as_ref()
                    .is_none_or(|ids| ids.contains(&repo.id))
        }
        Actor::App { .. } => false,
    };
    if !has_access {
        return Err(
            ApiError::forbidden("Repository collaborator access required").with_header(
                "X-Accepted-GitHub-Permissions",
                format!("{permission}=read"),
            ),
        );
    }
    Ok(())
}

fn permission(variable: bool) -> &'static str {
    if variable {
        "actions_variables"
    } else {
        "secrets"
    }
}

fn get(ctx: &Ctx<'_>, variable: bool) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?;
    collaborator(ctx, repo, permission(variable))?;
    let name = ctx.param(if variable { "name" } else { "secret_name" })?;
    let entry = ctx
        .tx()
        .table::<Entries>()
        .by_key(&key(repo.id, variable, name))
        .ok_or_else(ApiError::not_found)?;
    Reply::json(&entry.value)
}

fn list(ctx: &Ctx<'_>, variable: bool, org: bool) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?;
    collaborator(ctx, repo, permission(variable))?;
    let scope = if org { repo.owner_id } else { repo.id };
    let mut items: Vec<_> = ctx
        .tx()
        .table::<Entries>()
        .children(scope)
        .filter(|entry| entry.variable == variable && applies(entry, repo))
        .map(|entry| &entry.value)
        .collect();
    items.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    let count = items.len();
    let (start, size) = crate::window(ctx)?;
    let start = start.min(count);
    let end = start.saturating_add(size).min(count);
    crate::page(
        ctx,
        if variable { "variables" } else { "secrets" },
        count,
        &items[start..end],
    )
}

fn applies(entry: &Entry, repo: &Repo) -> bool {
    match entry.visibility {
        None | Some(Visibility::All) => true,
        Some(Visibility::Private) => repo.is_private(),
        Some(Visibility::Selected) => entry.selected.contains(&repo.id),
    }
}

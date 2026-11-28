use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use http::{Method, StatusCode};
use http_body_util::Full;
use hubdouble_admin::{
    AdminHandler, AdminRequest, Registrar, Services, clear_repository_properties, error_response,
    json_response, read_json,
};
use hubdouble_git::{Git, ObjectId};
use hubdouble_model::{AccountKind, ApiError, Release, Repo, Visibility};
use hubdouble_render::{OrganizationSimple, Repository, SimpleUser, Urls};
use hubdouble_rest::Params;
use hubdouble_store::{Accounts, Deleted, Releases, RepoRedirects, Repos, Table, Tx, TxMut};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

pub(crate) fn register(r: &mut Registrar<'_>) {
    let refresh_workflows = r
        .rest
        .entries()
        .any(|entry| entry.operation_id == "actions/get-workflow");
    r.admin.route(
        Method::PATCH,
        "/repos/{owner}/{repo}",
        route(move |tx, services, params, body| {
            edit_repo(tx, services, params, body, refresh_workflows)
        }),
    );
    r.store.register_table::<DeletedRepos>();
    r.admin
        .route(Method::DELETE, "/repos/{owner}/{repo}", route(delete_repo));
    r.admin
        .route(Method::PATCH, "/releases/{release_id}", route(edit_release));
}

pub(crate) fn decode<T: DeserializeOwned>(v: Value) -> Result<T, ApiError> {
    serde_json::from_value(v).map_err(|e| ApiError::unprocessable(e.to_string()))
}

pub(crate) fn repo_by_full_name(tx: &Tx, full: &str) -> Result<Arc<Repo>, ApiError> {
    let unknown = || ApiError::unprocessable(format!("no repository {full}"));
    let (owner, name) = full.split_once('/').ok_or_else(unknown)?;
    let owner = tx.table::<Accounts>().by_login(owner).ok_or_else(unknown)?;
    tx.table::<Repos>()
        .by_name(owner.id, name)
        .cloned()
        .ok_or_else(unknown)
}

fn route(
    change: impl Fn(&mut TxMut<'_>, &Services, &Params, Value) -> Result<Option<Value>, ApiError>
    + Copy
    + Send
    + Sync
    + 'static,
) -> AdminHandler {
    Arc::new(move |req: AdminRequest| {
        Box::pin(async move {
            let AdminRequest {
                request,
                params,
                services,
            } = req;
            let body = match read_json(request, services.config.max_body_bytes).await {
                Ok(b) => b,
                Err(e) => return error_response(&e),
            };
            match services
                .store
                .update(|tx| change(tx, &services, &params, body))
            {
                Ok(Some(v)) => json_response(StatusCode::OK, &v),
                Ok(None) => {
                    let mut response = http::Response::new(Full::new(Bytes::new()));
                    *response.status_mut() = StatusCode::NO_CONTENT;
                    response
                }
                Err(e) => error_response(&e),
            }
        })
    })
}

fn named_repo(tx: &Tx, params: &Params) -> Result<Arc<Repo>, ApiError> {
    let owner = params.get("owner").unwrap_or_default();
    let name = params.get("repo").unwrap_or_default();
    tx.table::<Accounts>()
        .by_login(owner)
        .and_then(|o| tx.table::<Repos>().by_name(o.id, name))
        .cloned()
        .ok_or_else(ApiError::not_found)
}

fn sender(tx: &Tx, login: Option<&str>, default: i64) -> Result<i64, ApiError> {
    login.map_or(Ok(default), |l| {
        tx.table::<Accounts>()
            .by_login(l)
            .map(|a| a.id)
            .ok_or_else(|| ApiError::unprocessable(format!("no account {l}")))
    })
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepoEdit {
    #[serde(default)]
    sender: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    default_branch: Option<String>,
    #[serde(default)]
    topics: Option<Vec<String>>,
    #[serde(default)]
    archived: Option<bool>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    private: Option<bool>,
}

fn blank_to_none(v: String) -> Option<String> {
    (!v.is_empty()).then_some(v)
}

fn from(v: impl serde::Serialize) -> Value {
    json!({ "from": v })
}

fn rendered(tx: &Tx, repo: &Repo) -> Result<Value, ApiError> {
    let owner = crate::owner(tx, repo)?;
    serde_json::to_value(Repository::new(&Urls::new(tx.config(), ""), repo, owner))
        .map_err(|e| ApiError::internal(e.to_string()))
}

fn new_owner(tx: &Tx, login: &str, current: i64) -> Result<Option<i64>, ApiError> {
    let to = tx
        .table::<Accounts>()
        .by_login(login)
        .filter(|a| a.kind != AccountKind::Bot)
        .ok_or_else(|| ApiError::unprocessable(format!("no account {login}")))?;
    Ok((to.id != current).then_some(to.id))
}

fn visibility(
    named: Option<&str>,
    private: Option<bool>,
    old: &Repo,
) -> Result<Option<Visibility>, ApiError> {
    Ok(match (named, private) {
        (Some(v), _) => Some(Visibility::from_rest(v).ok_or_else(|| {
            ApiError::unprocessable(format!("visibility {v} is not public, private or internal"))
        })?),
        (None, Some(true)) if !old.is_private() => Some(Visibility::Private),
        (None, Some(false)) => Some(Visibility::Public),
        (None, _) => None,
    })
}

fn edit_default_branch(
    tx: &Tx,
    git: &Git,
    repo: &mut Repo,
    branch: Option<String>,
    edited: &mut Map<String, Value>,
) -> Result<Option<ObjectId>, ApiError> {
    let Some(name) = branch.filter(|name| *name != repo.default_branch) else {
        return Ok(None);
    };
    let target = git
        .resolve_ref(tx, repo.id, &format!("refs/heads/{name}"))
        .map_err(|_| ApiError::unprocessable(format!("no branch {name}")))?;
    edited.insert("default_branch".to_owned(), from(&repo.default_branch));
    repo.default_branch = name;
    Ok(Some(target))
}

fn edit_repo(
    tx: &mut TxMut<'_>,
    services: &Services,
    params: &Params,
    body: Value,
    refresh_workflows: bool,
) -> Result<Option<Value>, ApiError> {
    let old = named_repo(tx, params)?;
    let e: RepoEdit = decode(body)?;
    let sender = sender(tx, e.sender.as_deref(), old.owner_id)?;
    let mut new = Repo::clone(&old);
    let mut edited = Map::new();
    if let Some(d) = e
        .description
        .map(blank_to_none)
        .filter(|d| *d != old.description)
    {
        edited.insert("description".to_owned(), from(&old.description));
        new.description = d;
    }
    if let Some(h) = e.homepage.map(blank_to_none).filter(|h| *h != old.homepage) {
        edited.insert("homepage".to_owned(), from(&old.homepage));
        new.homepage = h;
    }
    if let Some(t) = e.topics.filter(|t| *t != old.topics) {
        edited.insert("topics".to_owned(), from(&old.topics));
        new.topics = t;
    }
    let branch_target =
        edit_default_branch(tx, &services.git, &mut new, e.default_branch, &mut edited)?;
    let renamed = e.name.filter(|n| *n != old.name);
    if let Some(n) = &renamed {
        if !valid_name(n) {
            return Err(ApiError::unprocessable(format!("{n} is not a valid name")));
        }
        new.name.clone_from(n);
    }
    let previous_owner = crate::owner(tx, &old)?.clone();
    let transferred = match e.owner {
        Some(login) => new_owner(tx, &login, old.owner_id)?,
        None => None,
    };
    if let Some(to) = transferred {
        new.owner_id = to;
        clear_repository_properties(tx, old.id);
    }
    if let Some(v) = visibility(e.visibility.as_deref(), e.private, &old)? {
        new.visibility = v;
    }
    if let Some(a) = e.archived {
        new.archived = a;
    }
    new.updated_at = services.clock.now();
    tx.table_mut::<Repos>().replace(new.clone())?;
    if renamed.is_some() || transferred.is_some() {
        let owner = crate::owner(tx, &new)?.login.clone();
        let mut redirects = tx.table_mut::<RepoRedirects>();
        let redirects = redirects.get_mut();
        redirects.moved(&previous_owner.login, &old.name, old.id);
        redirects.claim(&owner, &new.name);
    }
    if let Some(target) = branch_target.filter(|_| refresh_workflows) {
        hubdouble_api_actions::refresh_default_branch(tx, &services.git, &old, target)?;
    }
    let id = old.id;
    if !edited.is_empty() {
        let changes = Map::from_iter([("changes".to_owned(), Value::Object(edited))]);
        crate::emit(tx, "repository", "edited", id, sender, changes);
    }
    if renamed.is_some() {
        let changes = json!({ "repository": { "name": from(&old.name) } });
        crate::emit(tx, "repository", "renamed", id, sender, changes_of(changes));
    }
    if transferred.is_some() {
        let urls = Urls::new(tx.config(), "");
        let from = if previous_owner.kind == AccountKind::Organization {
            json!({ "organization": OrganizationSimple::new(&urls, &previous_owner) })
        } else {
            json!({ "user": SimpleUser::new(&urls, &previous_owner) })
        };
        let changes = json!({ "owner": { "from": from } });
        crate::emit(
            tx,
            "repository",
            "transferred",
            id,
            sender,
            changes_of(changes),
        );
    }
    if new.archived != old.archived {
        let action = if new.archived {
            "archived"
        } else {
            "unarchived"
        };
        crate::emit(tx, "repository", action, id, sender, Map::new());
    }
    if new.is_private() != old.is_private() {
        let action = if new.is_private() {
            "privatized"
        } else {
            "publicized"
        };
        crate::emit(tx, "repository", action, id, sender, Map::new());
    }
    Ok(Some(rendered(tx, &new)?))
}

fn changes_of(changes: Value) -> Map<String, Value> {
    Map::from_iter([("changes".to_owned(), changes)])
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SenderOnly {
    #[serde(default)]
    sender: Option<String>,
}

fn delete_repo(
    tx: &mut TxMut<'_>,
    _: &Services,
    params: &Params,
    body: Value,
) -> Result<Option<Value>, ApiError> {
    let repo = named_repo(tx, params)?;
    let s: SenderOnly = decode(body)?;
    let sender = sender(tx, s.sender.as_deref(), repo.owner_id)?;
    crate::emit(tx, "repository", "deleted", repo.id, sender, Map::new());
    if !repo.is_private() {
        let owner = tx
            .table::<Accounts>()
            .get(repo.owner_id)
            .map_or_else(String::new, |a| a.login.clone());
        tx.table_mut::<DeletedRepos>()
            .get_mut()
            .0
            .insert(repo.id, format!("{owner}/{}", repo.name));
    }
    tx.delete(&Deleted::Repo(repo))?;
    Ok(None)
}

/// The last `owner/name` of deleted public repositories for issue event commit URLs.
#[derive(Clone, Default)]
pub struct DeletedRepos(HashMap<i64, String>);

impl Table for DeletedRepos {}

impl DeletedRepos {
    #[must_use]
    pub fn name(&self, repo_id: i64) -> Option<&str> {
        self.0.get(&repo_id).map(String::as_str)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseEdit {
    #[serde(default)]
    sender: Option<String>,
    #[serde(default)]
    tag_name: Option<String>,
    #[serde(default)]
    target_commitish: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: Option<bool>,
    #[serde(default)]
    prerelease: Option<bool>,
}

fn edit_release(
    tx: &mut TxMut<'_>,
    services: &Services,
    params: &Params,
    body: Value,
) -> Result<Option<Value>, ApiError> {
    let old = params
        .get("release_id")
        .and_then(|id| id.parse().ok())
        .and_then(|id| tx.table::<Releases>().get(id))
        .cloned()
        .ok_or_else(ApiError::not_found)?;
    let e: ReleaseEdit = decode(body)?;
    let repo = tx
        .table::<Repos>()
        .get(old.repo_id)
        .cloned()
        .ok_or_else(ApiError::not_found)?;
    let sender = sender(tx, e.sender.as_deref(), old.author_id)?;
    let now = services.clock.now();
    let mut new = Release::clone(&old);
    let mut changes = Map::new();
    if let Some(n) = e.name.filter(|n| Some(n) != old.name.as_ref()) {
        changes.insert(
            "name".to_owned(),
            from(old.name.clone().unwrap_or_default()),
        );
        new.name = Some(n);
    }
    if let Some(b) = e.body.filter(|b| Some(b) != old.body.as_ref()) {
        changes.insert(
            "body".to_owned(),
            from(old.body.clone().unwrap_or_default()),
        );
        new.body = Some(b);
    }
    if let Some(t) = e.tag_name {
        new.tag_name = t;
    }
    if let Some(t) = e.target_commitish {
        new.target_commitish = t;
    }
    if let Some(p) = e.prerelease {
        new.prerelease = p;
    }
    if let Some(d) = e.draft {
        new.draft = d;
    }
    let differs = new != *old;
    let published = old.draft && !new.draft;
    if published {
        new.published_at = Some(now);
    }
    if !new.draft {
        crate::releases::ensure_tag(
            tx,
            &services.git,
            &repo,
            sender,
            &new.tag_name,
            &new.target_commitish,
        )?;
    }
    new.updated_at = now;
    tx.table_mut::<Releases>().replace(new.clone())?;
    let action = if published {
        Some("published")
    } else if !new.draft && differs {
        Some("edited")
    } else {
        None
    };
    if let Some(action) = action {
        let mut p = crate::releases::payload(tx, &new)?;
        if action == "edited" {
            p.insert("changes".to_owned(), Value::Object(changes));
        }
        crate::emit(tx, "release", action, repo.id, sender, p);
    }
    let urls = Urls::new(tx.config(), "");
    let out = crate::releases::render(tx, &urls, &new)?;
    Ok(Some(
        serde_json::to_value(out).map_err(|e| ApiError::internal(e.to_string()))?,
    ))
}

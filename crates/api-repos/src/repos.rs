use std::cmp::Ordering;
use std::sync::Arc;

use http::Method;
use hubdouble_admin::Registrar;
use hubdouble_auth::{Actor, can_see, repo_role};
use hubdouble_model::{
    Account, AccountKind, ApiError, Grant, IssueState, Repo, RepoRole, Requirement, Scope,
};
use hubdouble_render::{RepoPermissions, Repository, SimpleUser, Urls};
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::{Accounts, Issues, Repos, Tx};
use serde::Serialize;

use crate::METADATA;

const ORG_METADATA: Requirement = Requirement {
    any_of: &[Grant::read("metadata")],
    scope: Scope::Org,
    scopes: &[],
};

const MAX_FORK_DEPTH: usize = 32;

pub(crate) fn register(r: &mut Registrar<'_>) {
    r.rest.handle("repos/get", METADATA, get);
    r.rest.extra(
        Method::GET,
        "/repositories/{repository_id}",
        "repos/get-by-id",
        METADATA,
        get,
    );
    r.rest
        .handle("repos/list-for-org", ORG_METADATA, list_for_org);
}

#[derive(Serialize)]
struct FullRepository {
    #[serde(flatten)]
    repo: Repository,
    network_count: i64,
    subscribers_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    organization: Option<SimpleUser>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<Repository>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<Repository>,
}

fn count(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

pub(crate) fn render(tx: &Tx, urls: &Urls, actor: &Actor, r: &Repo, owner: &Account) -> Repository {
    let mut out = Repository::new(urls, r, owner);
    let forks = count(
        tx.table::<Repos>()
            .iter()
            .filter(|f| f.fork_of == Some(r.id))
            .count(),
    );
    let open = count(
        tx.table::<Issues>()
            .in_repo(r.id)
            .filter(|i| i.state == IssueState::Open)
            .count(),
    );
    out.forks_count = forks;
    out.forks = forks;
    out.open_issues_count = open;
    out.open_issues = open;
    out.permissions = permissions(tx, actor, r);
    out
}

fn permissions(tx: &Tx, actor: &Actor, r: &Repo) -> Option<RepoPermissions> {
    let Actor::User { user, .. } = actor else {
        return None;
    };
    let role = repo_role(tx, user.id, r).or((!r.is_private()).then_some(RepoRole::Read))?;
    Some(RepoPermissions::of(role))
}

fn render_by_id(tx: &Tx, urls: &Urls, actor: &Actor, id: i64) -> Result<Repository, ApiError> {
    let r = tx
        .table::<Repos>()
        .get(id)
        .ok_or_else(|| ApiError::internal("a fork's parent is missing"))?;
    Ok(render(tx, urls, actor, r, crate::owner(tx, r)?))
}

fn full(tx: &Tx, urls: &Urls, actor: &Actor, r: &Repo) -> Result<FullRepository, ApiError> {
    let owner = crate::owner(tx, r)?;
    let repos = tx.table::<Repos>();
    let mut root = r;
    for _ in 0..MAX_FORK_DEPTH {
        match root.fork_of.and_then(|p| repos.get(p)) {
            Some(p) => root = p,
            None => break,
        }
    }
    let network = count(
        repos
            .iter()
            .filter(|f| in_network(repos, f, root.id))
            .count(),
    );
    Ok(FullRepository {
        repo: render(tx, urls, actor, r, owner),
        network_count: network,
        subscribers_count: 0,
        organization: (owner.kind == AccountKind::Organization)
            .then(|| SimpleUser::new(urls, owner)),
        parent: r
            .fork_of
            .map(|p| render_by_id(tx, urls, actor, p))
            .transpose()?,
        source: (root.id != r.id)
            .then(|| render_by_id(tx, urls, actor, root.id))
            .transpose()?,
    })
}

fn in_network(repos: &Repos, r: &Repo, root: i64) -> bool {
    let mut current = r.fork_of;
    for _ in 0..MAX_FORK_DEPTH {
        match current {
            Some(id) if id == root => return true,
            Some(id) => current = repos.get(id).and_then(|p| p.fork_of),
            None => return false,
        }
    }
    false
}

fn get(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?;
    Reply::json(&full(ctx.tx(), ctx.urls(), ctx.actor(), repo)?)
}

fn keep(kind: &str, tx: &Tx, actor: &Actor, r: &Repo) -> bool {
    match kind {
        "public" => !r.is_private(),
        "private" => r.is_private(),
        "forks" => r.fork_of.is_some(),
        "sources" => r.fork_of.is_none(),
        "member" => match actor {
            Actor::User { user, .. } => repo_role(tx, user.id, r).is_some(),
            _ => false,
        },
        _ => true,
    }
}

fn order(sort: &str, a: &Repo, b: &Repo) -> Ordering {
    match sort {
        "updated" => a.updated_at.cmp(&b.updated_at),
        "pushed" => a.pushed_at.cmp(&b.pushed_at),
        "full_name" => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        _ => a.created_at.cmp(&b.created_at),
    }
    .then(a.id.cmp(&b.id))
}

fn list_for_org(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let tx = ctx.tx();
    let actor = ctx.actor();
    let org = tx
        .table::<Accounts>()
        .by_login(ctx.param("org")?)
        .filter(|a| a.kind == AccountKind::Organization)
        .ok_or_else(ApiError::not_found)?;
    let query = ctx.query();
    let kind = query.get("type").unwrap_or("all");
    let sort = query.get("sort").unwrap_or("created");
    let ascending = match query.get("direction") {
        Some("asc") => true,
        Some("desc") => false,
        _ => sort == "full_name",
    };
    let mut repos: Vec<&Arc<Repo>> = tx
        .table::<Repos>()
        .children(org.id)
        .filter(|r| can_see(tx, actor, r) && keep(kind, tx, actor, r))
        .collect();
    repos.sort_by(|a, b| {
        let o = order(sort, a, b);
        if ascending { o } else { o.reverse() }
    });
    let urls = ctx.urls();
    ctx.page(&repos, |r| render(tx, urls, actor, r, org))
}

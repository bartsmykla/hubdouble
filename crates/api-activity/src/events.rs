use std::collections::BTreeSet;
use std::sync::Arc;

use hubdouble_auth::{Actor, can_see, repo_role};
use hubdouble_model::{Account, AccountKind, ApiError, Repo, RepoRole, Timestamp};
use hubdouble_render::Urls;
use hubdouble_rest::{Ctx, PageLimit, Reply};
use hubdouble_store::{Accounts, ActivityEvent, ActivityEvents, Repos, Tx};
use serde::Serialize;
use serde_json::Value;

const EVENTS: PageLimit = PageLimit {
    results: 300,
    lookahead: 0,
    message: "In order to keep the API fast for everyone, pagination is limited for this resource.",
    documentation_url: "https://docs.github.com/v3/#pagination",
    before_overflow: true,
};

const RETENTION_SECONDS: i64 = 30 * 24 * 60 * 60;
const PUBLIC_PER_PAGE: usize = 15;
const PER_PAGE: usize = 30;
const MOST_PER_PAGE: usize = 100;
const MAX_FORK_DEPTH: usize = 100;

#[derive(Serialize)]
struct EventActor {
    id: i64,
    login: String,
    display_login: String,
    gravatar_id: &'static str,
    url: String,
    avatar_url: String,
}

impl EventActor {
    fn new(urls: &Urls, a: &Account) -> EventActor {
        let segment = if a.kind == AccountKind::Organization {
            "orgs"
        } else {
            "users"
        };
        EventActor {
            id: a.id,
            login: a.login.clone(),
            display_login: display_login(&a.login).to_owned(),
            gravatar_id: "",
            url: format!("{}/{segment}/{}", urls.api, a.login),
            avatar_url: format!("{}/avatars/u/{}?", urls.base, a.id),
        }
    }
}

fn display_login(login: &str) -> &str {
    login.strip_suffix("[bot]").unwrap_or(login)
}

#[derive(Serialize)]
struct EventRepo<'a> {
    id: i64,
    name: &'a str,
    url: String,
}

#[derive(Serialize)]
struct EventView<'a> {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    actor: EventActor,
    repo: EventRepo<'a>,
    payload: &'a Value,
    public: bool,
    created_at: Timestamp,
    #[serde(skip_serializing_if = "Option::is_none")]
    org: Option<EventActor>,
}

fn view<'a>(tx: &Tx, urls: &Urls, (e, actor): &(&'a ActivityEvent, &Account)) -> EventView<'a> {
    let accounts = tx.table::<Accounts>();
    EventView {
        id: e.id.to_string(),
        kind: e.kind,
        actor: EventActor::new(urls, actor),
        repo: EventRepo {
            id: e.repo_id,
            name: &e.repo_name,
            url: format!("{}/repos/{}", urls.api, e.repo_name),
        },
        payload: &e.payload,
        public: e.public,
        created_at: e.created_at,
        org: e
            .org_id
            .and_then(|id| accounts.get(id))
            .map(|o| EventActor::new(urls, o)),
    }
}

fn reply<'e>(
    ctx: &Ctx<'_>,
    per_page: usize,
    events: impl Iterator<Item = &'e Arc<ActivityEvent>>,
) -> Result<Reply, ApiError> {
    let oldest = ctx.now().plus_seconds(-RETENTION_SECONDS);
    let tx = ctx.tx();
    let accounts = tx.table::<Accounts>();
    let mut kept: Vec<(&ActivityEvent, &Account)> = events
        .filter(|e| e.created_at >= oldest)
        .filter_map(|e| Some((e.as_ref(), accounts.get(e.actor_id)?.as_ref())))
        .collect();
    kept.sort_unstable_by_key(|(e, _)| std::cmp::Reverse(e.id));
    let urls = ctx.urls();
    ctx.page_within_sized(&EVENTS, per_page, MOST_PER_PAGE, &kept, |e| {
        view(tx, urls, e)
    })
}

fn person<'t>(tx: &'t Tx, login: &str) -> Result<&'t Arc<Account>, ApiError> {
    tx.table::<Accounts>()
        .by_login(login)
        .filter(|a| a.kind != AccountKind::Organization)
        .ok_or_else(ApiError::not_found)
}

fn organization<'t>(tx: &'t Tx, login: &str) -> Result<&'t Arc<Account>, ApiError> {
    tx.table::<Accounts>()
        .by_login(login)
        .filter(|a| a.kind == AccountKind::Organization)
        .ok_or_else(ApiError::not_found)
}

fn is_user(actor: &Actor, user: &Account) -> bool {
    matches!(actor, Actor::User { user: u, .. } if u.id == user.id)
}

fn shown(ctx: &Ctx<'_>, e: &ActivityEvent, own: bool) -> bool {
    let tx = ctx.tx();
    match tx.table::<Repos>().get(e.repo_id) {
        Some(repo) if repo.is_private() => own && can_see(tx, ctx.actor(), repo),
        Some(_) => e.public || own,
        None => e.public,
    }
}

pub(crate) fn public(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let events = ctx
        .tx()
        .table::<ActivityEvents>()
        .iter()
        .filter(|e| shown(ctx, e, false));
    reply(ctx, PUBLIC_PER_PAGE, events)
}

fn network_root(tx: &Tx, repo: &Repo) -> i64 {
    let repos = tx.table::<Repos>();
    let mut id = repo.id;
    let mut parent = repo.fork_of;
    for _ in 0..MAX_FORK_DEPTH {
        let Some(next) = parent else { break };
        id = next;
        parent = repos.get(next).and_then(|r| r.fork_of);
    }
    id
}

pub(crate) fn network(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let tx = ctx.tx();
    let owner = tx
        .table::<Accounts>()
        .by_login(ctx.param("owner")?)
        .ok_or_else(ApiError::not_found)?;
    let repo = tx
        .table::<Repos>()
        .by_name(owner.id, ctx.param("repo")?)
        .filter(|r| can_see(tx, ctx.actor(), r))
        .ok_or_else(ApiError::not_found)?;
    let root = network_root(tx, repo);
    let members: Vec<i64> = tx
        .table::<Repos>()
        .iter()
        .filter(|r| network_root(tx, r) == root)
        .map(|r| r.id)
        .collect();
    let rows = tx.table::<ActivityEvents>();
    let events = members
        .iter()
        .flat_map(|id| rows.in_repo(*id))
        .filter(|e| shown(ctx, e, false));
    reply(ctx, PER_PAGE, events)
}

pub(crate) fn org(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let tx = ctx.tx();
    let org = organization(tx, ctx.param("org")?)?.id;
    let events = tx
        .table::<ActivityEvents>()
        .iter()
        .filter(|e| e.org_id == Some(org) && shown(ctx, e, false));
    reply(ctx, PER_PAGE, events)
}

pub(crate) fn repo(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?.id;
    let events = ctx.tx().table::<ActivityEvents>().in_repo(repo);
    reply(ctx, PER_PAGE, events)
}

fn by_user(ctx: &Ctx<'_>, only_public: bool) -> Result<Reply, ApiError> {
    let tx = ctx.tx();
    let user = person(tx, ctx.param("username")?)?;
    let own = !only_public && is_user(ctx.actor(), user);
    let events = tx
        .table::<ActivityEvents>()
        .children(user.id)
        .filter(|e| shown(ctx, e, own));
    reply(ctx, PER_PAGE, events)
}

pub(crate) fn user(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    by_user(ctx, false)
}

pub(crate) fn user_public(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    by_user(ctx, true)
}

pub(crate) fn user_org(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let tx = ctx.tx();
    let user = person(tx, ctx.param("username")?)?;
    if !is_user(ctx.actor(), user) {
        return Err(ApiError::not_found());
    }
    let org = organization(tx, ctx.param("org")?)?.id;
    let events = tx
        .table::<ActivityEvents>()
        .iter()
        .filter(|e| e.org_id == Some(org) && shown(ctx, e, true));
    reply(ctx, PER_PAGE, events)
}

fn received_by(ctx: &Ctx<'_>, only_public: bool) -> Result<Reply, ApiError> {
    let tx = ctx.tx();
    let user = person(tx, ctx.param("username")?)?;
    let own = !only_public && is_user(ctx.actor(), user);
    let watched: BTreeSet<i64> = tx
        .table::<Repos>()
        .iter()
        .filter(|r| repo_role(tx, user.id, r) >= Some(RepoRole::Write))
        .map(|r| r.id)
        .collect();
    let rows = tx.table::<ActivityEvents>();
    let events = watched
        .iter()
        .flat_map(|id| rows.in_repo(*id))
        .filter(|e| e.actor_id != user.id && shown(ctx, e, own));
    reply(ctx, PER_PAGE, events)
}

pub(crate) fn received(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    received_by(ctx, false)
}

pub(crate) fn received_public(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    received_by(ctx, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bot_actor_shows_its_login_without_the_bot_suffix() {
        assert_eq!(display_login("renovate[bot]"), "renovate");
        assert_eq!(display_login("octocat"), "octocat");
    }
}

use std::sync::Arc;

use http::Method;
use hubdouble_admin::{Admin, SeedCtx};
use hubdouble_model::{
    Account, ApiError, Repo, RepoRole, Team, TeamMember, TeamPrivacy, TeamRepo, TeamRole,
    Timestamp, node_id,
};
use hubdouble_rest::Params;
use hubdouble_store::{Deleted, Repos, Teams, Tx, TxMut};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};

use crate::admin::{self, Permission, Sender, decode, invalid, org, param, read, sender, write};
use crate::{events, hierarchy, rest};

pub fn register(admin: &mut Admin) {
    admin.kind("team", seed);
    let team = "/team/{org}/{slug}";
    admin.route(Method::GET, team, read(get));
    admin.route(Method::PATCH, team, write(edit));
    admin.route(Method::DELETE, team, write(delete));
    let member = "/team/{org}/{slug}/members/{login}";
    admin.route(Method::PUT, member, write(put_member));
    admin.route(Method::DELETE, member, write(remove_member));
    let repo = "/team/{org}/{slug}/repos/{owner}/{repo}";
    admin.route(Method::PUT, repo, write(put_repo));
    admin.route(Method::DELETE, repo, write(remove_repo));
}

#[derive(Deserialize)]
struct Seed {
    id: Option<i64>,
    #[serde(alias = "organization")]
    org: String,
    name: String,
    slug: Option<String>,
    parent: Option<String>,
    privacy: Option<TeamPrivacy>,
    description: Option<String>,
    notifications_enabled: Option<bool>,
    sender: Option<String>,
    #[serde(default)]
    members: Vec<MemberSeed>,
    #[serde(default)]
    repositories: Vec<RepoSeed>,
}

#[derive(Deserialize)]
struct MemberSeed {
    login: String,
    role: Option<TeamRole>,
}

#[derive(Deserialize)]
struct RepoSeed {
    #[serde(alias = "name")]
    repository: String,
    permission: Option<RepoRole>,
}

fn seed(ctx: &mut SeedCtx<'_>, body: Value) -> Result<(), ApiError> {
    let seed: Seed = decode(body)?;
    let tx = &mut ctx.tx;
    let org = rest::organization(tx, &seed.org)?.clone();
    let source = events::Source {
        org: &org,
        sender: sender(tx, &org, seed.sender.as_deref())?,
    };
    let id = seed.id.unwrap_or_else(|| tx.next_id());
    let now = ctx.clock.now();
    let slug = seed.slug.unwrap_or_else(|| slugify(&seed.name));
    check_slug(&slug)?;
    let privacy = seed.privacy.unwrap_or(TeamPrivacy::Closed);
    let parent_id = parent(
        tx.table::<Teams>(),
        org.id,
        id,
        privacy,
        seed.parent.as_deref(),
    )?;
    let team = tx.table_mut::<Teams>().insert(Team {
        id,
        node_id: node_id::entity(node_id::TEAM, id),
        org_id: org.id,
        name: seed.name,
        slug,
        description: seed.description,
        privacy,
        parent_id,
        notifications_enabled: seed.notifications_enabled.unwrap_or(true),
        members: Vec::new(),
        repos: Vec::new(),
        created_at: now,
        updated_at: now,
    })?;
    events::team(tx, &source, "created", &team, None, None);
    for member in seed.members {
        let user = rest::user(tx, &member.login)?.clone();
        let team = current(tx, id)?;
        change_member(
            tx,
            &source,
            &team,
            &user,
            now,
            Some(member.role.unwrap_or(TeamRole::Member)),
        )?;
    }
    for repo in seed.repositories {
        let (owner, name) = repo
            .repository
            .split_once('/')
            .unwrap_or((&org.login, &repo.repository));
        let repository = admin::repo(tx, owner, name)?;
        let team = current(tx, id)?;
        change_repo(
            tx,
            &source,
            &team,
            &repository,
            now,
            Some(repo.permission.unwrap_or(RepoRole::Read)),
        )?;
    }
    Ok(())
}

fn slugify(name: &str) -> String {
    name.split(|c: char| !c.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join("-")
}

fn check_slug(slug: &str) -> Result<(), ApiError> {
    if slug.is_empty() || !slug.chars().all(|c| c.is_alphanumeric() || c == '-') {
        return Err(invalid(
            "slug",
            "A team slug contains only letters, numbers and hyphens",
        ));
    }
    Ok(())
}

fn parent(
    teams: &Teams,
    org_id: i64,
    team_id: i64,
    privacy: TeamPrivacy,
    slug: Option<&str>,
) -> Result<Option<i64>, ApiError> {
    let Some(slug) = slug else { return Ok(None) };
    let team = teams
        .by_slug(org_id, slug)
        .ok_or_else(|| invalid("parent", "No such parent team"))?;
    if team.id == team_id
        || hierarchy::ancestors(teams, team).any(|ancestor| ancestor.id == team_id)
    {
        return Err(invalid("parent", "A team cannot be its own ancestor"));
    }
    if privacy == TeamPrivacy::Secret || team.privacy == TeamPrivacy::Secret {
        return Err(invalid("parent", "A secret team cannot be nested"));
    }
    Ok(Some(team.id))
}

fn current(tx: &Tx, id: i64) -> Result<Arc<Team>, ApiError> {
    tx.table::<Teams>()
        .get(id)
        .cloned()
        .ok_or_else(ApiError::not_found)
}

fn lookup(tx: &Tx, params: &Params) -> Result<(Arc<Account>, Arc<Team>), ApiError> {
    let org = org(tx, params)?;
    let team = tx
        .table::<Teams>()
        .by_slug(org.id, param(params, "slug")?)
        .ok_or_else(ApiError::not_found)?;
    Ok((org.clone(), team.clone()))
}

fn get(tx: &Tx, params: &Params) -> Result<Value, ApiError> {
    let (_, team) = lookup(tx, params)?;
    serde_json::to_value(team).map_err(|e| ApiError::internal(e.to_string()))
}

#[derive(Deserialize)]
struct Edit {
    name: Option<String>,
    slug: Option<String>,
    #[serde(default)]
    description: PatchText,
    #[serde(default)]
    parent: PatchText,
    privacy: Option<TeamPrivacy>,
    notifications_enabled: Option<bool>,
    sender: Option<String>,
}

#[derive(Default)]
enum PatchText {
    #[default]
    Unset,
    Set(Option<String>),
}

impl<'de> Deserialize<'de> for PatchText {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Option::<String>::deserialize(d).map(Self::Set)
    }
}

fn edit(tx: &mut TxMut<'_>, params: &Params, now: Timestamp, body: Value) -> Result<(), ApiError> {
    let input: Edit = decode(body)?;
    let (org, old) = lookup(tx, params)?;
    let source = events::Source {
        org: &org,
        sender: sender(tx, &org, input.sender.as_deref())?,
    };
    let mut team = (*old).clone();
    let mut changes = Map::new();
    if let Some(name) = input.name.filter(|name| *name != team.name) {
        changes.insert("name".into(), json!({ "from": team.name }));
        team.slug = slugify(&name);
        team.name = name;
    }
    if let Some(slug) = input.slug {
        team.slug = slug;
    }
    check_slug(&team.slug)?;
    if let PatchText::Set(description) = input.description
        && description != team.description
    {
        if let Some(before) = &team.description {
            changes.insert("description".into(), json!({ "from": before }));
        }
        team.description = description;
    }
    if let Some(privacy) = input.privacy.filter(|privacy| *privacy != team.privacy) {
        changes.insert("privacy".into(), json!({ "from": team.privacy.as_str() }));
        team.privacy = privacy;
    }
    let teams = tx.table::<Teams>();
    if let PatchText::Set(parent_slug) = input.parent {
        team.parent_id = parent(teams, org.id, team.id, team.privacy, parent_slug.as_deref())?;
    }
    if team.privacy == TeamPrivacy::Secret
        && (team.parent_id.is_some()
            || teams
                .children(org.id)
                .any(|child| child.parent_id == Some(team.id)))
    {
        return Err(invalid("privacy", "A secret team cannot be nested"));
    }
    if let Some(notify) = input.notifications_enabled {
        team.notifications_enabled = notify;
    }
    if team != *old {
        team.updated_at = now;
        tx.table_mut::<Teams>().replace(team.clone())?;
        events::team(tx, &source, "edited", &team, None, Some(changes));
    }
    Ok(())
}

fn delete(
    tx: &mut TxMut<'_>,
    params: &Params,
    now: Timestamp,
    body: Value,
) -> Result<(), ApiError> {
    let input: Sender = decode(body)?;
    let (org, team) = lookup(tx, params)?;
    let source = events::Source {
        org: &org,
        sender: sender(tx, &org, input.sender.as_deref())?,
    };
    let children: Vec<_> = tx
        .table::<Teams>()
        .children(org.id)
        .filter(|child| child.parent_id == Some(team.id))
        .cloned()
        .collect();
    for child in children {
        let mut updated = (*child).clone();
        updated.parent_id = None;
        updated.updated_at = now;
        tx.table_mut::<Teams>().replace(updated)?;
    }
    tx.delete(&Deleted::Team(team.clone()))?;
    events::team(tx, &source, "deleted", &team, None, None);
    Ok(())
}

#[derive(Deserialize)]
struct MemberInput {
    role: Option<TeamRole>,
    sender: Option<String>,
}

fn put_member(
    tx: &mut TxMut<'_>,
    params: &Params,
    now: Timestamp,
    body: Value,
) -> Result<(), ApiError> {
    let input: MemberInput = decode(body)?;
    member_action(
        tx,
        params,
        now,
        input.sender.as_deref(),
        Some(input.role.unwrap_or(TeamRole::Member)),
    )
}

fn remove_member(
    tx: &mut TxMut<'_>,
    params: &Params,
    now: Timestamp,
    body: Value,
) -> Result<(), ApiError> {
    let input: Sender = decode(body)?;
    member_action(tx, params, now, input.sender.as_deref(), None)
}

fn member_action(
    tx: &mut TxMut<'_>,
    params: &Params,
    now: Timestamp,
    author: Option<&str>,
    role: Option<TeamRole>,
) -> Result<(), ApiError> {
    let (org, team) = lookup(tx, params)?;
    let user = rest::user(tx, param(params, "login")?)?.clone();
    let source = events::Source {
        org: &org,
        sender: sender(tx, &org, author)?,
    };
    change_member(tx, &source, &team, &user, now, role)
}

pub fn change_member(
    tx: &mut TxMut<'_>,
    source: &events::Source<'_>,
    team: &Team,
    user: &Account,
    now: Timestamp,
    role: Option<TeamRole>,
) -> Result<(), ApiError> {
    if role.is_some() && hierarchy::org_role(tx, team.org_id, user.id).is_none() {
        return Err(invalid(
            "login",
            "A team member must be an active organization member",
        ));
    }
    let mut updated = team.clone();
    let old = updated
        .members
        .iter()
        .find(|member| member.user_id == user.id)
        .map(|m| m.role);
    if old == role {
        return Ok(());
    }
    updated.members.retain(|member| member.user_id != user.id);
    if let Some(role) = role {
        updated.members.push(TeamMember {
            user_id: user.id,
            role,
        });
    }
    updated.updated_at = now;
    tx.table_mut::<Teams>().replace(updated)?;
    match (old, role) {
        (None, Some(_)) => events::membership(tx, source, "added", team, user),
        (Some(_), None) => events::membership(tx, source, "removed", team, user),
        _ => {}
    }
    Ok(())
}

fn put_repo(
    tx: &mut TxMut<'_>,
    params: &Params,
    now: Timestamp,
    body: Value,
) -> Result<(), ApiError> {
    let input: Permission = decode(body)?;
    repo_action(
        tx,
        params,
        now,
        input.sender.as_deref(),
        Some(input.permission),
    )
}

fn remove_repo(
    tx: &mut TxMut<'_>,
    params: &Params,
    now: Timestamp,
    body: Value,
) -> Result<(), ApiError> {
    let input: Sender = decode(body)?;
    repo_action(tx, params, now, input.sender.as_deref(), None)
}

fn repo_action(
    tx: &mut TxMut<'_>,
    params: &Params,
    now: Timestamp,
    author: Option<&str>,
    role: Option<RepoRole>,
) -> Result<(), ApiError> {
    let (org, team) = lookup(tx, params)?;
    let repository = admin::repo(tx, param(params, "owner")?, param(params, "repo")?)?;
    let source = events::Source {
        org: &org,
        sender: sender(tx, &org, author)?,
    };
    change_repo(tx, &source, &team, &repository, now, role)
}

fn change_repo(
    tx: &mut TxMut<'_>,
    source: &events::Source<'_>,
    team: &Team,
    repository: &Repo,
    now: Timestamp,
    role: Option<RepoRole>,
) -> Result<(), ApiError> {
    let fork_owner = repository
        .fork_of
        .and_then(|id| tx.table::<Repos>().get(id))
        .map(|repo| repo.owner_id);
    if repository.owner_id != team.org_id && fork_owner != Some(team.org_id) {
        return Err(invalid(
            "repository",
            "The organization must own the repository or its parent",
        ));
    }
    let mut updated = team.clone();
    let old = updated
        .repos
        .iter()
        .find(|r| r.repo_id == repository.id)
        .map(|r| r.role);
    if old == role {
        return Ok(());
    }
    updated.repos.retain(|r| r.repo_id != repository.id);
    if let Some(role) = role {
        updated.repos.push(TeamRepo {
            repo_id: repository.id,
            role,
        });
    }
    updated.updated_at = now;
    tx.table_mut::<Teams>().replace(updated.clone())?;
    let (action, changes) = match (old, role) {
        (None, Some(_)) => ("added_to_repository", None),
        (Some(_), None) => ("removed_from_repository", None),
        (Some(old), Some(_)) => ("edited", Some(repo_changes(old))),
        (None, None) => return Ok(()),
    };
    events::team(tx, source, action, &updated, Some(repository.id), changes);
    Ok(())
}

fn repo_changes(role: RepoRole) -> Map<String, Value> {
    Map::from_iter([(
        "repository".into(),
        json!({
            "permissions": { "from": { "pull": true, "push": role >= RepoRole::Write, "admin": role == RepoRole::Admin } },
        }),
    )])
}

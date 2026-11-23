use std::sync::Arc;

use hubdouble_auth::{self as auth, Actor, Target};
use hubdouble_model::{
    Account, AccountKind, ApiError, MembershipState, Repo, Requirement, Team, TeamMember,
};
use hubdouble_render::{RepoPermissions, Repository, TeamMembership};
use hubdouble_rest::{Ctx, Registry, Reply};
use hubdouble_store::{Accounts, Memberships, Teams, Tx};

use crate::hierarchy::{TeamViewer, pushes};
use crate::{MEMBERS, METADATA, PublicMemberships, TEAM_REPOSITORY, hierarchy, view};

pub fn register(rest: &mut Registry) {
    rest.handle("orgs/check-membership-for-user", MEMBERS, check_member);
    rest.handle(
        "orgs/check-public-membership-for-user",
        Requirement::public(),
        check_public_member,
    );
    rest.handle("teams/list", MEMBERS, list_teams);
    rest.handle("teams/get-by-name", MEMBERS, get_team);
    rest.handle("teams/get-membership-for-user-in-org", MEMBERS, membership);
    rest.handle(
        "teams/check-permissions-for-repo-in-org",
        TEAM_REPOSITORY,
        team_repo,
    );
    rest.handle(
        "repos/get-collaborator-permission-level",
        METADATA,
        permission,
    );
    rest.extra(
        http::Method::GET,
        "/organizations/{org_id}/public_members/{username}",
        "orgs/check-public-membership-for-user-by-id",
        Requirement::public(),
        check_public_member_by_id,
    );
}

fn signed_in(ctx: &Ctx<'_>) -> Result<(), ApiError> {
    if matches!(ctx.actor(), Actor::Anonymous) {
        Err(ApiError::new(401, "Requires authentication"))
    } else {
        Ok(())
    }
}

pub fn organization<'a>(tx: &'a Tx, login: &str) -> Result<&'a Arc<Account>, ApiError> {
    tx.table::<Accounts>()
        .by_login(login)
        .filter(|org| org.kind == AccountKind::Organization)
        .ok_or_else(ApiError::not_found)
}

pub fn user<'a>(tx: &'a Tx, login: &str) -> Result<&'a Arc<Account>, ApiError> {
    tx.table::<Accounts>()
        .by_login(login)
        .filter(|user| user.kind != AccountKind::Organization)
        .ok_or_else(ApiError::not_found)
}

fn check_member(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let org = organization(ctx.tx(), ctx.param("org")?)?;
    let requester_is_member = match ctx.actor() {
        Actor::Installation { installation, .. } => installation.account_id == org.id,
        Actor::User { user, .. } => hierarchy::org_role(ctx.tx(), org.id, user.id).is_some(),
        Actor::Anonymous | Actor::App { .. } => false,
    };
    let login = ctx.param("username")?;
    if !requester_is_member {
        return Reply::redirect(&format!(
            "{}/organizations/{}/public_members/{}",
            ctx.urls().api,
            org.id,
            hubdouble_render::escape(login)
        ));
    }
    let user = user(ctx.tx(), login)?;
    hierarchy::org_role(ctx.tx(), org.id, user.id).ok_or_else(ApiError::not_found)?;
    Ok(Reply::empty())
}

fn check_public_member(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let org = organization(ctx.tx(), ctx.param("org")?)?;
    public_member(ctx, org)
}

fn check_public_member_by_id(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let org = ctx
        .tx()
        .table::<Accounts>()
        .get(ctx.param_i64("org_id")?)
        .filter(|org| org.kind == AccountKind::Organization)
        .ok_or_else(ApiError::not_found)?;
    public_member(ctx, org)
}

fn public_member(ctx: &Ctx<'_>, org: &Account) -> Result<Reply, ApiError> {
    let tx = ctx.tx();
    let user = user(tx, ctx.param("username")?)?;
    tx.table::<Memberships>()
        .of(org.id, user.id)
        .filter(|m| m.state == MembershipState::Active)
        .filter(|m| tx.table::<PublicMemberships>().0.contains(&m.id))
        .ok_or_else(ApiError::not_found)?;
    Ok(Reply::empty())
}

fn list_teams(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    signed_in(ctx)?;
    let org = organization(ctx.tx(), ctx.param("org")?)?;
    let teams = ctx.tx().table::<Teams>();
    let viewer = TeamViewer::new(ctx.tx(), ctx.actor(), org.id);
    if matches!(viewer, TeamViewer::Outside) {
        return Err(ApiError::forbidden("Must have admin rights to Repository.")
            .with_docs("https://docs.github.com/rest/teams/teams#list-teams"));
    }
    let items: Vec<_> = teams
        .children(org.id)
        .filter(|t| viewer.can_see(t))
        .collect();
    ctx.page(&items, |team| view::team(ctx, org, team, viewer))
}

fn team<'a>(ctx: &'a Ctx<'_>, org: &Account) -> Result<&'a Team, ApiError> {
    ctx.tx()
        .table::<Teams>()
        .by_slug(org.id, ctx.param("team_slug")?)
        .filter(|t| TeamViewer::new(ctx.tx(), ctx.actor(), org.id).can_see(t))
        .map(AsRef::as_ref)
        .ok_or_else(ApiError::not_found)
}

fn get_team(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    signed_in(ctx)?;
    let org = organization(ctx.tx(), ctx.param("org")?)?;
    Reply::json(&view::full_team(ctx, org, team(ctx, org)?))
}

fn membership(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    signed_in(ctx)?;
    let org = organization(ctx.tx(), ctx.param("org")?)?;
    let t = team(ctx, org)?;
    let u = user(ctx.tx(), ctx.param("username")?)?;
    let role = hierarchy::team_member(ctx.tx(), t, u.id).ok_or_else(ApiError::not_found)?;
    Reply::json(&TeamMembership::new(
        ctx.urls(),
        t,
        &TeamMember {
            user_id: u.id,
            role,
        },
        u,
    ))
}

fn team_repo(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    signed_in(ctx)?;
    let org = organization(ctx.tx(), ctx.param("org")?)?;
    auth::authorize(ctx.tx(), ctx.actor(), &MEMBERS, Target::Org(org))?;
    let t = team(ctx, org)?;
    let repo = ctx.repo()?;
    let role = hierarchy::repo_role(ctx.tx(), t, repo.id).ok_or_else(ApiError::not_found)?;
    let repository_media = ctx
        .headers()
        .get_all(http::header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|item| item.split(';').next())
        .any(|media| {
            media
                .trim()
                .eq_ignore_ascii_case("application/vnd.github.v3.repository+json")
        });
    if !repository_media {
        return Ok(Reply::empty());
    }
    let owner = repository_owner(ctx.tx(), repo)?;
    let mut rendered = Repository::new(ctx.urls(), repo, owner);
    rendered.permissions = Some(RepoPermissions::of(role));
    Reply::json(&view::TeamRepository {
        rendered,
        role_name: role.as_str(),
    })
}

fn repository_owner<'a>(tx: &'a Tx, repo: &Repo) -> Result<&'a Account, ApiError> {
    tx.table::<Accounts>()
        .get(repo.owner_id)
        .map(AsRef::as_ref)
        .ok_or_else(ApiError::not_found)
}

fn permission(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    signed_in(ctx)?;
    let repo = ctx.repo()?;
    if !pushes(ctx.tx(), ctx.actor(), repo) {
        return Err(ApiError::forbidden(
            "Must have push access to view collaborator permission.",
        )
        .with_docs("https://docs.github.com/rest/collaborators/collaborators#get-repository-permissions-for-a-user"));
    }
    let user = user(ctx.tx(), ctx.param("username")?)?;
    let role = auth::repo_role(ctx.tx(), user.id, repo);
    Reply::json(&view::collaborator(ctx.urls(), user, role))
}

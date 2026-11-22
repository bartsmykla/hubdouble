use std::collections::BTreeSet;

use hubdouble_model::{Account, RepoRole, Team as ModelTeam, Timestamp};
use hubdouble_render::{OrganizationSimple, RepoPermissions, Repository, SimpleUser, Team, Urls};
use hubdouble_rest::Ctx;
use hubdouble_store::{Repos, Teams};
use serde::Serialize;

use crate::hierarchy::{TeamViewer, ancestors};

pub fn team(ctx: &Ctx<'_>, org: &Account, t: &ModelTeam, viewer: TeamViewer) -> Team {
    let parent = ancestors(ctx.tx().table::<Teams>(), t)
        .next()
        .filter(|parent| viewer.can_see(parent));
    Team::new(ctx.urls(), t, org, parent.map(AsRef::as_ref))
}

#[derive(Serialize)]
pub struct FullTeam<'a> {
    #[serde(flatten)]
    team: Team,
    created_at: Timestamp,
    updated_at: Timestamp,
    members_count: usize,
    repos_count: usize,
    organization: TeamOrganization<'a>,
}

#[derive(Serialize)]
struct TeamOrganization<'a> {
    #[serde(flatten)]
    org: OrganizationSimple,
    name: Option<&'a str>,
    company: Option<&'a str>,
    blog: Option<&'a str>,
    location: Option<&'a str>,
    email: Option<&'a str>,
    html_url: String,
    has_organization_projects: bool,
    has_repository_projects: bool,
    public_repos: usize,
    public_gists: usize,
    followers: usize,
    following: usize,
    created_at: Timestamp,
    updated_at: Timestamp,
    archived_at: Option<Timestamp>,
    #[serde(rename = "type")]
    kind: &'static str,
}

pub fn full_team<'a>(ctx: &Ctx<'_>, org: &'a Account, t: &ModelTeam) -> FullTeam<'a> {
    let teams = ctx.tx().table::<Teams>();
    let members: BTreeSet<_> = teams
        .children(t.org_id)
        .filter(|other| other.id == t.id || ancestors(teams, other).any(|p| p.id == t.id))
        .flat_map(|other| other.members.iter().map(|m| m.user_id))
        .collect();
    let repos: BTreeSet<_> = std::iter::once(t)
        .chain(ancestors(teams, t).map(AsRef::as_ref))
        .flat_map(|other| other.repos.iter().map(|r| r.repo_id))
        .collect();
    FullTeam {
        team: team(ctx, org, t, TeamViewer::new(ctx.tx(), ctx.actor(), org.id)),
        created_at: t.created_at,
        updated_at: t.updated_at,
        members_count: members.len(),
        repos_count: repos.len(),
        organization: TeamOrganization {
            org: OrganizationSimple::new(ctx.urls(), org),
            name: org.name.as_deref(),
            company: org.company.as_deref(),
            blog: org.blog.as_deref(),
            location: org.location.as_deref(),
            email: org.email.as_deref(),
            html_url: format!("{}/{}", ctx.urls().web, org.login),
            has_organization_projects: false,
            has_repository_projects: false,
            public_repos: ctx
                .tx()
                .table::<Repos>()
                .children(org.id)
                .filter(|r| !r.is_private())
                .count(),
            public_gists: 0,
            followers: 0,
            following: 0,
            created_at: org.created_at,
            updated_at: org.updated_at,
            archived_at: None,
            kind: "Organization",
        },
    }
}

#[derive(Serialize)]
pub struct TeamRepository {
    #[serde(flatten)]
    pub rendered: Repository,
    pub role_name: &'static str,
}

#[derive(Serialize)]
pub struct RepositoryPermission {
    permission: &'static str,
    role_name: &'static str,
    user: Collaborator,
}

#[derive(Serialize)]
struct Collaborator {
    #[serde(flatten)]
    user: SimpleUser,
    role_name: &'static str,
    permissions: RepoPermissions,
}

pub fn base_role(role: Option<RepoRole>) -> &'static str {
    match role {
        Some(RepoRole::Admin) => "admin",
        Some(RepoRole::Maintain | RepoRole::Write) => "write",
        Some(RepoRole::Triage | RepoRole::Read) => "read",
        None => "none",
    }
}

pub fn collaborator(
    urls: &Urls,
    account: &Account,
    role: Option<RepoRole>,
) -> RepositoryPermission {
    let role_name = role.map_or("", RepoRole::as_str);
    RepositoryPermission {
        permission: base_role(role),
        role_name,
        user: Collaborator {
            user: SimpleUser::new(urls, account),
            role_name,
            permissions: role.map_or(
                RepoPermissions {
                    pull: false,
                    triage: false,
                    push: false,
                    maintain: false,
                    admin: false,
                },
                RepoPermissions::of,
            ),
        },
    }
}

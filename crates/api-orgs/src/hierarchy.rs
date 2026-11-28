use std::sync::Arc;

use hubdouble_auth::{Actor, Target};
use hubdouble_model::{
    Access, Account, MembershipState, OrgRole, Repo, RepoRole, Team, TeamPrivacy, TeamRole,
    TokenKind,
};
use hubdouble_store::{Installations, Memberships, Teams, Tx};

pub fn org_role(tx: &Tx, org_id: i64, user_id: i64) -> Option<OrgRole> {
    tx.table::<Memberships>()
        .of(org_id, user_id)
        .filter(|m| m.state == MembershipState::Active)
        .map(|m| m.role)
}

pub fn ancestors<'a>(teams: &'a Teams, team: &'a Team) -> impl Iterator<Item = &'a Arc<Team>> {
    let org_id = team.org_id;
    let parent = move |id| teams.get(id).filter(|t| t.org_id == org_id);
    std::iter::successors(team.parent_id.and_then(parent), move |current| {
        current.parent_id.and_then(parent)
    })
    .take(teams.len())
}

pub fn team_member(tx: &Tx, team: &Team, user_id: i64) -> Option<TeamRole> {
    let teams = tx.table::<Teams>();
    let direct = team.members.iter().find(|m| m.user_id == user_id);
    let inherited = || {
        teams.children(team.org_id).any(|child| {
            child.members.iter().any(|m| m.user_id == user_id)
                && ancestors(teams, child).any(|parent| parent.id == team.id)
        })
    };
    let role = direct
        .map(|m| m.role)
        .or_else(|| inherited().then_some(TeamRole::Member))?;
    if org_role(tx, team.org_id, user_id) == Some(OrgRole::Admin) {
        return Some(TeamRole::Maintainer);
    }
    Some(role)
}

#[derive(Clone, Copy)]
pub enum TeamViewer {
    Admin,
    Member(i64),
    Outside,
}

impl TeamViewer {
    #[must_use]
    pub fn new(tx: &Tx, actor: &Actor, org_id: i64) -> Self {
        match actor {
            Actor::Installation {
                installation,
                token,
                ..
            } if installation.account_id == org_id
                && reads(installation.permissions.get("members"))
                && (token.permissions.is_empty() || reads(token.permissions.get("members"))) =>
            {
                Self::Admin
            }
            Actor::User { token, .. }
                if token.kind == TokenKind::User
                    && token
                        .app_id
                        .is_some_and(|app| !reads_members(tx, app, org_id)) =>
            {
                Self::Outside
            }
            Actor::User { user, .. } => match org_role(tx, org_id, user.id) {
                Some(OrgRole::Admin) => Self::Admin,
                Some(OrgRole::Member) => Self::Member(user.id),
                None => Self::Outside,
            },
            _ => Self::Outside,
        }
    }

    /// A viewer for a caller that has not passed the org's team read check: `Outside` unless the
    /// caller may list the org's teams, SSO and token scopes included.
    #[must_use]
    pub fn authorized(tx: &Tx, actor: &Actor, org: &Arc<Account>) -> Self {
        match hubdouble_auth::authorize_operation(tx, actor, &crate::TEAMS, Target::Org(org), false)
        {
            Ok(()) => Self::new(tx, actor, org.id),
            Err(_) => Self::Outside,
        }
    }

    #[must_use]
    pub fn can_see(self, team: &Team) -> bool {
        match self {
            Self::Admin => true,
            Self::Member(id) => {
                team.privacy == TeamPrivacy::Closed || team.members.iter().any(|m| m.user_id == id)
            }
            Self::Outside => false,
        }
    }
}

fn reads_members(tx: &Tx, app_id: i64, org_id: i64) -> bool {
    tx.table::<Installations>()
        .of(app_id, org_id)
        .filter(|i| i.suspended_at.is_none())
        .is_some_and(|i| reads(i.permissions.get("members")))
}

fn reads(access: Option<&Access>) -> bool {
    access.is_some_and(|access| *access >= Access::Read)
}

pub fn pushes(tx: &Tx, actor: &Actor, repo: &Repo) -> bool {
    match actor {
        Actor::User { user, .. } => {
            hubdouble_auth::repo_role(tx, user.id, repo) >= Some(RepoRole::Write)
        }
        Actor::Installation { .. } => hubdouble_auth::installation_covers(actor, repo),
        Actor::Anonymous | Actor::App { .. } => false,
    }
}

pub fn repo_role(tx: &Tx, team: &Team, repo_id: i64) -> Option<RepoRole> {
    std::iter::once(team)
        .chain(ancestors(tx.table::<Teams>(), team).map(AsRef::as_ref))
        .flat_map(|t| &t.repos)
        .filter(|r| r.repo_id == repo_id)
        .map(|r| r.role)
        .max()
}

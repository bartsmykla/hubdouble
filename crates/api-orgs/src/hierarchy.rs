use std::sync::Arc;

use hubdouble_auth::Actor;
use hubdouble_model::{MembershipState, OrgRole, RepoRole, Team, TeamPrivacy, TeamRole};
use hubdouble_store::{Memberships, Teams, Tx};

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
    pub fn new(tx: &Tx, actor: &Actor, org_id: i64) -> Self {
        match actor {
            Actor::Installation { installation, .. } if installation.account_id == org_id => {
                Self::Admin
            }
            Actor::User { user, .. } => match org_role(tx, org_id, user.id) {
                Some(OrgRole::Admin) => Self::Admin,
                Some(OrgRole::Member) => Self::Member(user.id),
                None => Self::Outside,
            },
            _ => Self::Outside,
        }
    }

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

pub fn repo_role(tx: &Tx, team: &Team, repo_id: i64) -> Option<RepoRole> {
    std::iter::once(team)
        .chain(ancestors(tx.table::<Teams>(), team).map(AsRef::as_ref))
        .flat_map(|t| &t.repos)
        .filter(|r| r.repo_id == repo_id)
        .map(|r| r.role)
        .max()
}

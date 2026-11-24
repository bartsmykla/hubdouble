use hubdouble_model::{BypassActor, MembershipState, OrgRole, Repo, RepoRole, Ruleset};
use hubdouble_store::{Memberships, Teams, Tx};

use crate::check::repo_role;
use crate::contract::Actor;

/// How an actor may bypass a ruleset, ordered from no bypass to a full one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Bypass {
    Never,
    PullRequestsOnly,
    Exempt,
    Always,
}

impl Bypass {
    /// GitHub's `current_user_can_bypass` value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Bypass::Never => "never",
            Bypass::PullRequestsOnly => "pull_requests_only",
            Bypass::Exempt => "exempt",
            Bypass::Always => "always",
        }
    }

    /// Whether the ruleset's rules skip the actor's write: always for `always` and `exempt`,
    /// and for `pull_requests_only` only when the write merges a pull request.
    #[must_use]
    pub fn skips(self, pull_request: bool) -> bool {
        match self {
            Bypass::Never => false,
            Bypass::PullRequestsOnly => pull_request,
            Bypass::Exempt | Bypass::Always => true,
        }
    }
}

const ROLE_IDS: [(i64, RepoRole); 5] = [
    (1, RepoRole::Read),
    (2, RepoRole::Maintain),
    (3, RepoRole::Triage),
    (4, RepoRole::Write),
    (5, RepoRole::Admin),
];

fn matches(tx: &Tx, actor: &Actor, repo: &Repo, b: &BypassActor) -> bool {
    match (b.actor_type.as_str(), actor) {
        ("Integration", Actor::Installation { app, .. } | Actor::App { app }) => {
            b.actor_id.is_none_or(|id| id == app.id)
        }
        ("OrganizationAdmin", Actor::User { user, .. }) => tx
            .table::<Memberships>()
            .of(repo.owner_id, user.id)
            .is_some_and(|m| m.role == OrgRole::Admin && m.state == MembershipState::Active),
        ("RepositoryRole", Actor::User { user, .. }) => {
            let wanted = ROLE_IDS
                .iter()
                .find(|(id, _)| Some(*id) == b.actor_id)
                .map(|(_, role)| *role);
            wanted.is_some_and(|w| repo_role(tx, user.id, repo) >= Some(w))
        }
        ("Team", Actor::User { user, .. }) => b
            .actor_id
            .and_then(|id| tx.table::<Teams>().get(id))
            .is_some_and(|t| t.members.iter().any(|m| m.user_id == user.id)),
        _ => false,
    }
}

/// The widest bypass that the ruleset's bypass list gives the actor on the repository.
#[must_use]
pub fn ruleset_bypass(tx: &Tx, actor: &Actor, repo: &Repo, ruleset: &Ruleset) -> Bypass {
    ruleset
        .bypass_actors
        .iter()
        .filter(|b| matches(tx, actor, repo, b))
        .map(|b| match b.bypass_mode.as_str() {
            "always" => Bypass::Always,
            "exempt" => Bypass::Exempt,
            "pull_request" => Bypass::PullRequestsOnly,
            _ => Bypass::Never,
        })
        .max()
        .unwrap_or(Bypass::Never)
}

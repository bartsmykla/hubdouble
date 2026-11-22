use std::fmt::Write as _;

use hubdouble_model::{
    Access, Account, AccountKind, ApiError, Grant, Installation, MembershipState, OrgRole, Repo,
    RepoRole, RepositorySelection, Requirement, Scope, Token, TokenKind, Visibility,
};
use hubdouble_store::{Accounts, Installations, Memberships, Repos, Teams, Tx};

use crate::contract::{Actor, Target};
use crate::sso::has_sso;

const MAX_TEAM_DEPTH: usize = 32;

pub(crate) fn authorize(
    tx: &Tx,
    actor: &Actor,
    req: &Requirement,
    target: Target<'_>,
    base_url: &str,
) -> Result<(), ApiError> {
    match req.scope {
        Scope::Public => Ok(()),
        Scope::App => match actor {
            Actor::App { .. } => Ok(()),
            _ => Err(ApiError::new(401, "A JSON web token could not be decoded")),
        },
        Scope::Installation => match actor {
            Actor::Installation {
                token,
                installation,
                ..
            } => {
                if satisfies(req.any_of, |p| installation_level(token, installation, p)) {
                    Ok(())
                } else {
                    Err(integration_denied(req))
                }
            }
            Actor::Anonymous => Err(requires_authentication()),
            Actor::App { .. } | Actor::User { .. } => Err(integration_denied(req)),
        },
        Scope::User => match actor {
            Actor::User { token, .. } => classic_scopes(token, req),
            Actor::Anonymous => Err(requires_authentication()),
            Actor::App { .. } | Actor::Installation { .. } => Err(integration_denied(req)),
        },
        Scope::Repo => match target {
            Target::Repo(repo) => repo_access(tx, actor, req, repo, base_url),
            Target::Org(_) | Target::None => Err(no_target()),
        },
        Scope::Org => match target {
            Target::Org(org) => org_access(tx, actor, req, org, base_url),
            Target::Repo(_) | Target::None => Err(no_target()),
        },
    }
}

pub(crate) fn can_see(tx: &Tx, actor: &Actor, repo: &Repo) -> bool {
    if !repo.is_private() {
        return true;
    }
    match actor {
        Actor::Anonymous | Actor::App { .. } => false,
        Actor::Installation {
            token,
            installation,
            ..
        } => covers(installation, Some(token), repo),
        Actor::User { token, user } => {
            let token_reaches = match token.kind {
                TokenKind::Personal => holds_scope(&token.scopes, "repo"),
                _ => token
                    .app_id
                    .is_none_or(|app| app_installation(tx, app, repo).is_some()),
            };
            token_reaches && repo_role(tx, user.id, repo).is_some()
        }
    }
}

/// The highest role a user holds on a repository through ownership, collaboration, org
/// ownership, internal visibility or a team and its parent teams; `None` for no role. A public
/// repository's implicit read is not a role.
#[must_use]
pub fn repo_role(tx: &Tx, user_id: i64, repo: &Repo) -> Option<RepoRole> {
    if repo.owner_id == user_id {
        return Some(RepoRole::Admin);
    }
    let mut best = repo
        .collaborators
        .iter()
        .filter(|c| c.user_id == user_id)
        .map(|c| c.role)
        .max();
    let accounts = tx.table::<Accounts>();
    let is_org = |id: i64| {
        accounts
            .get(id)
            .is_some_and(|o| o.kind == AccountKind::Organization)
    };
    let owner_org = is_org(repo.owner_id).then_some(repo.owner_id);
    if owner_org.is_some() {
        let membership = tx
            .table::<Memberships>()
            .of(repo.owner_id, user_id)
            .filter(|m| m.state == MembershipState::Active);
        if let Some(m) = membership {
            if m.role == OrgRole::Admin {
                return Some(RepoRole::Admin);
            }
            if repo.visibility == Visibility::Internal {
                best = best.max(Some(RepoRole::Read));
            }
        }
    }
    let parent_org = repo
        .fork_of
        .and_then(|id| tx.table::<Repos>().get(id))
        .map(|parent| parent.owner_id)
        .filter(|&org| org != repo.owner_id && is_org(org));
    let teams = tx.table::<Teams>();
    for team in [owner_org, parent_org]
        .into_iter()
        .flatten()
        .flat_map(|org| teams.children(org))
        .filter(|t| t.members.iter().any(|m| m.user_id == user_id))
    {
        let mut current = Some(team);
        for _ in 0..MAX_TEAM_DEPTH {
            let Some(t) = current else { break };
            let role = t
                .repos
                .iter()
                .filter(|r| r.repo_id == repo.id)
                .map(|r| r.role)
                .max();
            best = best.max(role);
            current = t.parent_id.and_then(|p| teams.get(p));
        }
    }
    best
}

fn repo_access(
    tx: &Tx,
    actor: &Actor,
    req: &Requirement,
    repo: &Repo,
    base_url: &str,
) -> Result<(), ApiError> {
    if !can_see(tx, actor, repo) {
        return Err(ApiError::not_found());
    }
    match actor {
        Actor::Anonymous if is_read(req) => Ok(()),
        Actor::Anonymous => Err(requires_authentication()),
        Actor::App { .. } => Err(integration_denied(req)),
        Actor::Installation {
            token,
            installation,
            ..
        } => {
            let allowed = if covers(installation, Some(token), repo) {
                satisfies(req.any_of, |p| installation_level(token, installation, p))
            } else {
                is_read(req)
            };
            if allowed {
                Ok(())
            } else {
                Err(integration_denied(req))
            }
        }
        Actor::User { token, user } => {
            if repo.is_private() {
                sso(tx, token, user, repo.owner_id, base_url)?;
            }
            user_to_server(token, req, |app| app_installation(tx, app, repo))?;
            classic_scopes(token, req)?;
            if token.kind == TokenKind::Personal && !is_read(req) {
                let wanted = if repo.is_private() {
                    &["repo"][..]
                } else {
                    &["repo", "public_repo"][..]
                };
                if !wanted.iter().any(|w| holds_scope(&token.scopes, w)) {
                    return Err(scope_denied(wanted));
                }
            }
            let held =
                repo_role(tx, user.id, repo).or((!repo.is_private()).then_some(RepoRole::Read));
            if held >= Some(needed_role(req)) {
                Ok(())
            } else {
                Err(user_denied(token, req))
            }
        }
    }
}

fn org_access(
    tx: &Tx,
    actor: &Actor,
    req: &Requirement,
    org: &Account,
    base_url: &str,
) -> Result<(), ApiError> {
    match actor {
        Actor::Anonymous if is_read(req) => Ok(()),
        Actor::Anonymous => Err(requires_authentication()),
        Actor::App { .. } => Err(integration_denied(req)),
        Actor::Installation {
            token,
            installation,
            ..
        } => {
            let allowed = if installation.account_id == org.id {
                satisfies(req.any_of, |p| installation_level(token, installation, p))
            } else {
                is_read(req)
            };
            if allowed {
                Ok(())
            } else {
                Err(integration_denied(req))
            }
        }
        Actor::User { token, user } => {
            sso(tx, token, user, org.id, base_url)?;
            user_to_server(token, req, |app| {
                tx.table::<Installations>()
                    .of(app, org.id)
                    .filter(|i| i.suspended_at.is_none())
                    .map(AsRef::as_ref)
            })?;
            classic_scopes(token, req)?;
            if is_read(req) {
                return Ok(());
            }
            let role = tx
                .table::<Memberships>()
                .of(org.id, user.id)
                .filter(|m| m.state == MembershipState::Active)
                .map(|m| m.role);
            let needs_owner = req.any_of.iter().all(|g| {
                g.access == Access::Admin
                    || g.permission == "members"
                    || g.permission.starts_with("organization_")
            });
            let allowed = match role {
                Some(OrgRole::Admin) => true,
                Some(OrgRole::Member) => !needs_owner,
                None => false,
            };
            if allowed {
                Ok(())
            } else {
                Err(user_denied(token, req))
            }
        }
    }
}

fn user_to_server<'t>(
    token: &Token,
    req: &Requirement,
    installation: impl FnOnce(i64) -> Option<&'t Installation>,
) -> Result<(), ApiError> {
    let Some(app) = token.app_id.filter(|_| token.kind == TokenKind::User) else {
        return Ok(());
    };
    let allowed = match installation(app) {
        Some(i) => satisfies(req.any_of, |p| {
            implicit_metadata(p, i.permissions.get(p).copied())
        }),
        None => is_read(req),
    };
    if allowed {
        Ok(())
    } else {
        Err(integration_denied(req))
    }
}

fn sso(
    tx: &Tx,
    token: &Token,
    user: &Account,
    org_id: i64,
    base_url: &str,
) -> Result<(), ApiError> {
    if token.kind != TokenKind::User {
        return Ok(());
    }
    let Some(org) = tx.table::<Accounts>().get(org_id) else {
        return Ok(());
    };
    if org.kind != AccountKind::Organization || !org.sso_enforced || has_sso(tx, user.id, org.id) {
        return Ok(());
    }
    Err(ApiError::forbidden(
        "Resource protected by organization SAML enforcement. You must grant your OAuth token access to this organization.",
    )
    .with_header(
        "X-GitHub-SSO",
        format!("required; url={base_url}/orgs/{}/sso", org.login),
    ))
}

fn app_installation<'t>(tx: &'t Tx, app_id: i64, repo: &Repo) -> Option<&'t Installation> {
    tx.table::<Installations>()
        .of(app_id, repo.owner_id)
        .map(AsRef::as_ref)
        .filter(|i| covers(i, None, repo))
}

fn covers(installation: &Installation, token: Option<&Token>, repo: &Repo) -> bool {
    installation.suspended_at.is_none()
        && installation.account_id == repo.owner_id
        && (installation.repository_selection == RepositorySelection::All
            || installation.repo_ids.contains(&repo.id))
        && token
            .and_then(|t| t.repo_ids.as_ref())
            .is_none_or(|ids| ids.contains(&repo.id))
}

fn installation_level(
    token: &Token,
    installation: &Installation,
    permission: &str,
) -> Option<Access> {
    let granted = installation.permissions.get(permission).copied();
    let level = if token.permissions.is_empty() {
        granted
    } else {
        token.permissions.get(permission).copied().min(granted)
    };
    implicit_metadata(permission, level)
}

fn implicit_metadata(permission: &str, level: Option<Access>) -> Option<Access> {
    if permission == "metadata" {
        level.max(Some(Access::Read))
    } else {
        level
    }
}

fn satisfies(any_of: &[Grant], level: impl Fn(&str) -> Option<Access>) -> bool {
    any_of.is_empty()
        || any_of
            .iter()
            .any(|g| level(g.permission).is_some_and(|l| l >= g.access))
}

fn is_read(req: &Requirement) -> bool {
    req.any_of.iter().all(|g| g.access == Access::Read)
}

fn needed_role(req: &Requirement) -> RepoRole {
    req.any_of
        .iter()
        .map(|g| match (g.access, g.permission) {
            (Access::Read, _) | (Access::Write, "issues" | "pull_requests" | "discussions") => {
                RepoRole::Read
            }
            (Access::Admin, _) | (Access::Write, "administration") => RepoRole::Admin,
            (Access::Write, _) => RepoRole::Write,
        })
        .min()
        .unwrap_or(RepoRole::Read)
}

fn classic_scopes(token: &Token, req: &Requirement) -> Result<(), ApiError> {
    if token.kind != TokenKind::Personal
        || req.scopes.is_empty()
        || req.scopes.iter().any(|w| holds_scope(&token.scopes, w))
    {
        Ok(())
    } else {
        Err(scope_denied(req.scopes))
    }
}

fn holds_scope(held: &[String], wanted: &str) -> bool {
    held.iter().any(|h| implies(h, wanted))
}

fn implies(held: &str, wanted: &str) -> bool {
    held == wanted
        || matches!(
            (held, wanted),
            (
                "repo",
                "public_repo"
                    | "repo:status"
                    | "repo_deployment"
                    | "repo:invite"
                    | "security_events"
            ) | ("admin:org", "write:org" | "read:org")
                | ("write:org", "read:org")
                | ("user", "read:user" | "user:email" | "user:follow")
                | ("admin:repo_hook", "write:repo_hook" | "read:repo_hook")
                | ("write:repo_hook", "read:repo_hook")
                | ("admin:public_key", "write:public_key" | "read:public_key")
                | ("write:public_key", "read:public_key")
                | ("admin:gpg_key", "write:gpg_key" | "read:gpg_key")
                | ("write:gpg_key", "read:gpg_key")
                | ("write:packages", "read:packages")
                | ("write:discussion", "read:discussion")
                | ("project", "read:project")
        )
}

fn accepted_permissions(any_of: &[Grant]) -> String {
    let mut out = String::new();
    for (i, g) in any_of.iter().enumerate() {
        if i > 0 {
            out.push_str("; ");
        }
        let _ = write!(out, "{}={}", g.permission, g.access.as_str());
    }
    out
}

fn integration_denied(req: &Requirement) -> ApiError {
    let e = ApiError::forbidden("Resource not accessible by integration");
    if req.any_of.is_empty() {
        e
    } else {
        e.with_header(
            "X-Accepted-GitHub-Permissions",
            accepted_permissions(req.any_of),
        )
    }
}

fn user_denied(token: &Token, req: &Requirement) -> ApiError {
    if token.kind == TokenKind::User {
        return integration_denied(req);
    }
    ApiError::forbidden("Resource not accessible by personal access token")
}

fn scope_denied(wanted: &[&str]) -> ApiError {
    ApiError::forbidden("Resource not accessible by personal access token")
        .with_header("X-Accepted-OAuth-Scopes", wanted.join(", "))
}

fn requires_authentication() -> ApiError {
    ApiError::new(401, "Requires authentication")
}

fn no_target() -> ApiError {
    ApiError::internal("the operation's requirement names a target the route does not give")
}

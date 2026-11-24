use std::sync::Arc;

use http::HeaderMap;
use hubdouble_model::{Account, ApiError, App, Clock, Installation, Repo, Requirement, Token};
use hubdouble_store::Tx;

/// Who a request acts as, resolved once per request from its `Authorization` header.
#[derive(Debug, Clone, PartialEq)]
pub enum Actor {
    Anonymous,
    /// An App JWT.
    App {
        app: Arc<App>,
    },
    /// An installation token; `bot` is the App's `<slug>[bot]` account.
    Installation {
        token: Arc<Token>,
        installation: Arc<Installation>,
        app: Arc<App>,
        bot: Arc<Account>,
    },
    /// A user (`ghu_`) or personal (`ghp_`) token.
    User {
        token: Arc<Token>,
        user: Arc<Account>,
    },
}

impl Actor {
    /// The account that writes made by this actor name: the user, or the App's bot.
    #[must_use]
    pub fn account(&self) -> Option<&Arc<Account>> {
        match self {
            Actor::Installation { bot, .. } => Some(bot),
            Actor::User { user, .. } => Some(user),
            Actor::Anonymous | Actor::App { .. } => None,
        }
    }

    /// The login the request log shows.
    #[must_use]
    pub fn login(&self) -> Option<&str> {
        match self {
            Actor::App { app } => Some(&app.slug),
            other => other.account().map(|a| a.login.as_str()),
        }
    }

    /// The classic OAuth scopes answered in `X-OAuth-Scopes`, for user and personal tokens.
    #[must_use]
    pub fn oauth_scopes(&self) -> Option<&[String]> {
        match self {
            Actor::User { token, .. } => Some(&token.scopes),
            _ => None,
        }
    }
}

/// What an operation's [`Requirement`] is checked against.
#[derive(Debug, Clone, Copy)]
pub enum Target<'a> {
    None,
    Repo(&'a Arc<Repo>),
    Org(&'a Arc<Account>),
}

/// GitHub's REST refusal, `403`, of a write to an archived repository.
pub const ARCHIVED_REST: &str = "Repository was archived so is read-only.";
/// GitHub's GraphQL refusal, `UNPROCESSABLE`, of a mutation on an archived repository.
pub const ARCHIVED_GRAPHQL: &str = "Repository was archived so is read-only";

/// Refuses a comment on a locked conversation, `403` as GitHub answers it, unless `actor` is a
/// user with write access to `repo`. An installation token is refused whatever it holds.
///
/// # Errors
///
/// The `403` refusal.
pub fn refuse_locked(tx: &Tx, actor: &Actor, repo: &Repo, locked: bool) -> Result<(), ApiError> {
    let writer = match actor {
        Actor::User { user, .. } => {
            crate::repo_role(tx, user.id, repo) >= Some(hubdouble_model::RepoRole::Write)
        }
        _ => false,
    };
    if !locked || writer {
        return Ok(());
    }
    Err(
        ApiError::new(403, "Unable to create comment because issue is locked.")
            .with_docs("https://docs.github.com/articles/locking-conversations/"),
    )
}

/// Resolves the actor of a request.
///
/// # Errors
///
/// `401` for a bad, expired or revoked credential, a refresh token, or an App JWT that breaks
/// a rule of [`crate::verify_app_jwt`]; `403` for a token of a suspended installation. No
/// `Authorization` header is [`Actor::Anonymous`]. The schemes `Bearer` and `token` are accepted.
pub fn authenticate(tx: &Tx, clock: &dyn Clock, headers: &HeaderMap) -> Result<Actor, ApiError> {
    crate::credential::authenticate(tx, clock.now(), headers)
}

/// The choke point: checks `req` against the actor's grant and the target.
///
/// # Errors
///
/// `404` for a repository the actor cannot see, `403` with `X-Accepted-GitHub-Permissions` for a
/// missing permission, `403` with `X-GitHub-SSO: required; url=<base_url>/orgs/<org>/sso` for an
/// org that needs SSO, `base_url` being the store's [`Tx::config`].
pub fn authorize(
    tx: &Tx,
    actor: &Actor,
    req: &Requirement,
    target: Target<'_>,
) -> Result<(), ApiError> {
    crate::check::authorize(tx, actor, req, target, &tx.config().base_url)
}

/// Whether the actor can see the repository, for GraphQL's per-object check. A public
/// repository is visible to everyone; a private or internal one to an installation that covers
/// it and to a user with a role on it whose token reaches it.
#[must_use]
pub fn can_see(tx: &Tx, actor: &Actor, repo: &Repo) -> bool {
    crate::check::can_see(tx, actor, repo)
}

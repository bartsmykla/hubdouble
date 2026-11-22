use std::sync::Arc;

use http::HeaderMap;
use hubdouble_model::{
    Account, ApiError, App, Clock, Installation, Repo, Requirement, Scope, Token,
};
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

/// Resolves the actor of a request.
///
/// # Errors
///
/// `401` for a bad, expired or revoked credential.
pub fn authenticate(tx: &Tx, clock: &dyn Clock, headers: &HeaderMap) -> Result<Actor, ApiError> {
    let _ = (tx, clock);
    if headers.contains_key(http::header::AUTHORIZATION) {
        return Err(ApiError::not_implemented("auth/authenticate"));
    }
    Ok(Actor::Anonymous)
}

/// The choke point: checks `req` against the actor's grant and the target.
///
/// # Errors
///
/// `404` for a repository the actor cannot see, `403` with `X-Accepted-GitHub-Permissions` for a
/// missing permission, `403` with `X-GitHub-SSO` for an org that needs SSO.
pub fn authorize(
    tx: &Tx,
    actor: &Actor,
    req: &Requirement,
    target: Target<'_>,
) -> Result<(), ApiError> {
    let _ = (tx, actor, target);
    if req.scope == Scope::Public && req.any_of.is_empty() {
        return Ok(());
    }
    Err(ApiError::not_implemented("auth/authorize"))
}

/// Whether the actor can see the repository, for GraphQL's per-object check.
#[must_use]
pub fn can_see(tx: &Tx, actor: &Actor, repo: &Repo) -> bool {
    let _ = (tx, actor, repo);
    false
}

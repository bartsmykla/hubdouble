use std::collections::BTreeMap;

use hubdouble_model::{Access, Installation, RepositorySelection};
use hubdouble_store::{Repos, Tx};

use crate::error::AuthError;

/// The most repositories one installation token may name, as GitHub limits it.
pub const MAX_TOKEN_REPOSITORIES: usize = 500;

/// The narrowing body of `POST /app/installations/{id}/access_tokens`.
#[derive(Debug, Clone, Copy, Default)]
pub struct NarrowRequest<'a> {
    /// Repository names, without the owner, in the installation's account.
    pub repositories: &'a [String],
    pub repository_ids: &'a [i64],
    pub permissions: Option<&'a BTreeMap<String, Access>>,
}

/// What a new installation token holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Narrowed {
    /// `None` when the request named no repository: the token reaches the installation's set.
    pub repo_ids: Option<Vec<i64>>,
    pub permissions: BTreeMap<String, Access>,
}

/// Applies a token request's `repositories`, `repository_ids` and `permissions` to an
/// installation. Names and IDs are joined, in request order, without repeats. No permissions,
/// or an empty map, means the installation's permissions.
///
/// # Errors
///
/// [`AuthError::RepositoryNotAccessible`] for a repository that does not exist or that the
/// installation does not cover, [`AuthError::TooManyRepositories`] above
/// [`MAX_TOKEN_REPOSITORIES`], and [`AuthError::PermissionNotGranted`] for a permission above the
/// installation's level. Each answers `422`.
pub fn narrow(
    tx: &Tx,
    installation: &Installation,
    req: &NarrowRequest<'_>,
) -> Result<Narrowed, AuthError> {
    let repos = tx.table::<Repos>();
    let covered = |id: i64| match repos.get(id) {
        Some(r) => {
            r.owner_id == installation.account_id
                && (installation.repository_selection == RepositorySelection::All
                    || installation.repo_ids.contains(&id))
        }
        None => false,
    };
    let mut ids = Vec::with_capacity(req.repositories.len() + req.repository_ids.len());
    for name in req.repositories {
        let id = repos
            .by_name(installation.account_id, name)
            .map(|r| r.id)
            .ok_or(AuthError::RepositoryNotAccessible)?;
        ids.push(id);
    }
    ids.extend_from_slice(req.repository_ids);
    let mut repo_ids = Vec::with_capacity(ids.len());
    for id in ids {
        if !covered(id) {
            return Err(AuthError::RepositoryNotAccessible);
        }
        if !repo_ids.contains(&id) {
            repo_ids.push(id);
        }
    }
    if repo_ids.len() > MAX_TOKEN_REPOSITORIES {
        return Err(AuthError::TooManyRepositories);
    }
    let permissions = match req.permissions {
        Some(asked) if !asked.is_empty() => {
            for (name, level) in asked {
                if installation.permissions.get(name).is_none_or(|g| g < level) {
                    return Err(AuthError::PermissionNotGranted);
                }
            }
            asked.clone()
        }
        _ => installation.permissions.clone(),
    };
    Ok(Narrowed {
        repo_ids: (!repo_ids.is_empty()).then_some(repo_ids),
        permissions,
    })
}

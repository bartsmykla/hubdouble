use std::collections::BTreeSet;

use hubdouble_git::{Git, RefChange};
use hubdouble_model::{AccountKind, ApiError, MembershipState, Repo};
use hubdouble_store::{Entity, Memberships, Repos, Rows, Tx, TxMut};

pub(crate) type Contributors = Rows<RepoContributors>;

#[derive(Clone)]
pub(crate) struct RepoContributors {
    repo_id: i64,
    users: BTreeSet<i64>,
}

impl Entity for RepoContributors {
    fn id(&self) -> i64 {
        self.repo_id
    }
}

/// GitHub's `author_association` of an account on a repository, the same on every REST
/// answer, webhook payload and GraphQL `authorAssociation`: `OWNER` for the repository's
/// owner, `MEMBER` for an active member of the owning organization, `COLLABORATOR` for a
/// collaborator, `CONTRIBUTOR` for the author of a commit on the default branch or of a merged
/// pull request, and `NONE` otherwise. GitHub gives a first pull request `NONE` too, so
/// `FIRST_TIMER`, `FIRST_TIME_CONTRIBUTOR` and `MANNEQUIN` never occur.
#[must_use]
pub fn association(tx: &Tx, repo: &Repo, user_id: i64) -> &'static str {
    if repo.owner_id == user_id {
        "OWNER"
    } else if tx
        .table::<Memberships>()
        .of(repo.owner_id, user_id)
        .is_some_and(|m| m.state == MembershipState::Active)
    {
        "MEMBER"
    } else if repo.collaborators.iter().any(|c| c.user_id == user_id) {
        "COLLABORATOR"
    } else if tx
        .table::<Contributors>()
        .get(repo.id)
        .is_some_and(|c| c.users.contains(&user_id))
    {
        "CONTRIBUTOR"
    } else {
        "NONE"
    }
}

pub(crate) fn add(
    tx: &mut TxMut<'_>,
    repo_id: i64,
    users: impl IntoIterator<Item = i64>,
) -> Result<(), ApiError> {
    let mut row = tx.table::<Contributors>().get(repo_id).map_or_else(
        || RepoContributors {
            repo_id,
            users: BTreeSet::new(),
        },
        |row| RepoContributors::clone(row),
    );
    let before = row.users.len();
    row.users.extend(users);
    if row.users.len() != before {
        tx.table_mut::<Contributors>().replace(row)?;
    }
    Ok(())
}

pub(crate) fn pushed(tx: &mut TxMut<'_>, git: &Git, change: &RefChange) -> Result<(), ApiError> {
    let Some(repo) = tx.table::<Repos>().get(change.repo_id) else {
        return Ok(());
    };
    if change.new.is_none() || change.name != format!("refs/heads/{}", repo.default_branch) {
        return Ok(());
    }
    let authors: Vec<i64> = git
        .newly_reachable_commits(tx, change)?
        .iter()
        .filter_map(|(_, commit)| hubdouble_api_gitdata::account_by_email(tx, &commit.author.email))
        .filter(|account| account.kind != AccountKind::Organization)
        .map(|account| account.id)
        .collect();
    add(tx, change.repo_id, authors)
}

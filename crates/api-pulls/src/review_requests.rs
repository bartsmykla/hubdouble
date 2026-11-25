use std::sync::Arc;

use hubdouble_git::Git;
use hubdouble_model::{ApiError, Issue};
use hubdouble_render::{SimpleUser, Team, Urls};
use hubdouble_store::{Accounts, Issues, Repos, Teams, Tx, TxMut};
use serde_json::{Value, json};

use crate::{events, merge_state, reviewing, state};

/// A requested reviewer of a pull request: a user or a team, by id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reviewer {
    User(i64),
    Team(i64),
}

/// Adds the reviewers that the pull request does not have yet, or removes those it has, and
/// sends one `review_requested` or `review_request_removed` event for each that changed.
/// Answers the pull request as it is after the change.
///
/// # Errors
///
/// The store or render error of the write or of an event.
pub fn change_reviewers(
    tx: &mut TxMut<'_>,
    git: &Git,
    issue: &Arc<Issue>,
    sender: i64,
    reviewers: &[Reviewer],
    add: bool,
) -> Result<Arc<Issue>, ApiError> {
    let mut updated = Issue::clone(issue);
    let pull = updated.pull.as_mut().ok_or_else(ApiError::not_found)?;
    let mut changed = Vec::new();
    for &reviewer in reviewers {
        let (requested, id) = match reviewer {
            Reviewer::User(id) => (&mut pull.requested_reviewers, id),
            Reviewer::Team(id) => (&mut pull.requested_teams, id),
        };
        match (add, requested.iter().position(|&value| value == id)) {
            (true, None) => requested.push(id),
            (false, Some(index)) => {
                requested.remove(index);
            }
            _ => continue,
        }
        changed.push(reviewer);
    }
    if changed.is_empty() {
        return Ok(issue.clone());
    }
    let updated = Arc::new(updated);
    tx.table_mut::<Issues>().replace(updated.clone())?;
    let action = if add {
        "review_requested"
    } else {
        "review_request_removed"
    };
    let urls = Urls::new(tx.config(), "");
    for reviewer in changed {
        let extra = match reviewer {
            Reviewer::User(id) => json!({"requested_reviewer": user(tx, &urls, id)?}),
            Reviewer::Team(id) => json!({"requested_team": team(tx, &urls, id)?}),
        };
        events::emit(tx, git, &updated, sender, action, extra)?;
    }
    Ok(updated)
}

/// Requests reviews from the code owners of the files that a ready pull request changes, as
/// GitHub does when one opens or becomes ready for review: each owner once, a team as a team,
/// and never the author.
pub(crate) fn request_code_owners(
    tx: &mut TxMut<'_>,
    git: &Git,
    issue: &Arc<Issue>,
    sender: i64,
) -> Result<Arc<Issue>, ApiError> {
    if state::pull(issue)?.draft {
        return Ok(issue.clone());
    }
    let reviewers = code_owners(tx, git, issue)?;
    change_reviewers(tx, git, issue, sender, &reviewers, true)
}

fn code_owners(tx: &Tx, git: &Git, issue: &Issue) -> Result<Vec<Reviewer>, ApiError> {
    let Some(repo) = tx.table::<Repos>().get(issue.repo_id) else {
        return Ok(Vec::new());
    };
    let mut reviewers = Vec::new();
    for owner in merge_state::current_owner_groups(tx, git, issue)?
        .iter()
        .flatten()
    {
        let found: Vec<Reviewer> = match owner.strip_prefix('@') {
            Some(name) if name.contains('/') => reviewing::team_of(tx, repo, name)
                .map(|team| Reviewer::Team(team.id))
                .into_iter()
                .collect(),
            _ => reviewing::owner_users(tx, repo, owner)
                .into_iter()
                .filter(|&id| id != issue.user_id)
                .map(Reviewer::User)
                .collect(),
        };
        for reviewer in found {
            if !reviewers.contains(&reviewer) {
                reviewers.push(reviewer);
            }
        }
    }
    Ok(reviewers)
}

fn user(tx: &Tx, urls: &Urls, id: i64) -> Result<Value, ApiError> {
    let account = tx
        .table::<Accounts>()
        .get(id)
        .ok_or_else(ApiError::not_found)?;
    object(&SimpleUser::new(urls, account))
}

fn team(tx: &Tx, urls: &Urls, id: i64) -> Result<Value, ApiError> {
    let teams = tx.table::<Teams>();
    let team = teams.get(id).ok_or_else(ApiError::not_found)?;
    let org = tx
        .table::<Accounts>()
        .get(team.org_id)
        .ok_or_else(ApiError::not_found)?;
    let parent = team.parent_id.and_then(|id| teams.get(id)).map(Arc::as_ref);
    let mut value = object(&Team::new(urls, team, org, parent))?;
    events::webhook_team(&mut value);
    Ok(value)
}

fn object(value: &impl serde::Serialize) -> Result<Value, ApiError> {
    serde_json::to_value(value).map_err(|e| ApiError::internal(e.to_string()))
}

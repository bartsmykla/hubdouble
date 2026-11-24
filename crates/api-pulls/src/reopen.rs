use hubdouble_git::{Git, GitError, ObjectId};
use hubdouble_model::{ApiError, FieldError, Issue, IssueState, Pull, PullRef};
use hubdouble_store::{Tx, TxMut};
use serde_json::json;

use crate::timeline::{self, StateKind};
use crate::{change, events, mergeability, state};

/// Reopens a closed pull request the way GitHub does for `PATCH .../pulls/{n}`,
/// `PATCH .../issues/{n}` and `reopenPullRequest` alike.
///
/// It refuses, in this order, a merged pull request, a deleted base or head branch (a deleted
/// head repository counts as a deleted head branch), another open pull request from the same
/// head into the same base, and a head branch whose tip no longer descends from the pull
/// request's head, each with GitHub's `422` and a `custom` error on `state`. Otherwise it opens
/// `issue`, takes `head.sha` and `base.sha` from the live branches and sets `mergeable` to
/// `null` until [`reopened`] starts the computation of the test merge. It does not store
/// `issue`: the caller stores it and then sends [`reopened`].
///
/// # Errors
///
/// The refusals above, or a git error while it reads the branches.
pub fn reopen(tx: &Tx, git: &Git, issue: &mut Issue) -> Result<(), ApiError> {
    let (repo_id, id) = (issue.repo_id, issue.id);
    let p = issue.pull.as_mut().ok_or_else(ApiError::not_found)?;
    if p.merged_at.is_some() {
        return Err(refused("The pull request cannot be reopened."));
    }
    let base = tip(git, tx, &p.base)?;
    let head = tip(git, tx, &p.head)?;
    let (base, head) = match (base, head) {
        (Some(base), Some(head)) => (base, head),
        (None, Some(_)) => return Err(deleted(&p.base.ref_name)),
        (Some(_), None) => return Err(deleted(&p.head.ref_name)),
        (None, None) => {
            return Err(refused(&format!(
                "The {} and {} branches have been deleted.",
                p.base.ref_name, p.head.ref_name
            )));
        }
    };
    let head_repo = p.head.repo_id.ok_or_else(ApiError::not_found)?;
    if change::open_between(
        tx,
        repo_id,
        &p.base.ref_name,
        head_repo,
        &p.head.ref_name,
        Some(id),
    ) {
        return Err(refused(&format!(
            "There is already an open pull request from {} to {}.",
            p.head.ref_name, p.base.ref_name
        )));
    }
    if !descends(git, tx, repo_id, p, head)? {
        return Err(refused(&format!(
            "The {} branch was force-pushed or recreated.",
            p.head.ref_name
        )));
    }
    p.base.sha = base.to_string();
    p.head.sha = head.to_string();
    p.mergeable = None;
    issue.state = IssueState::Open;
    issue.state_reason = None;
    issue.closed_at = None;
    issue.closed_by = None;
    Ok(())
}

/// Records the `reopened` issue event, at `issue.updated_at`, by `sender` (through the App
/// `via_app` when an installation acted), sends the `pull_request` `reopened` webhook, for
/// `issue` as [`reopen`] and the store left it, and then starts the computation of its test
/// merge.
///
/// # Errors
///
/// A git error while it renders the pull request.
pub fn reopened(
    tx: &mut TxMut<'_>,
    git: &Git,
    issue: &Issue,
    sender: i64,
    via_app: Option<i64>,
) -> Result<(), ApiError> {
    state::sync_refs(tx, git, issue)?;
    timeline::record(tx, issue, (sender, via_app), StateKind::Reopened)?;
    events::emit(tx, git, issue, sender, "reopened", json!({}))?;
    mergeability::start(tx, git, issue, sender)
}

fn tip(git: &Git, tx: &Tx, side: &PullRef) -> Result<Option<ObjectId>, ApiError> {
    let Some(repo) = side.repo_id else {
        return Ok(None);
    };
    match state::branch(git, tx, repo, &side.ref_name) {
        Ok(id) => Ok(Some(id)),
        Err(GitError::NotFound) => Ok(None),
        Err(other) => Err(other.into()),
    }
}

fn descends(git: &Git, tx: &Tx, repo: i64, p: &Pull, head: ObjectId) -> Result<bool, ApiError> {
    let Ok(old) = p.head.sha.parse::<ObjectId>() else {
        return Ok(false);
    };
    match git.compare(tx, repo, old, head) {
        Ok(diff) => Ok(diff.behind_by == 0),
        Err(GitError::NotFound) => Ok(false),
        Err(other) => Err(other.into()),
    }
}

fn deleted(branch: &str) -> ApiError {
    refused(&format!("The {branch} branch has been deleted."))
}

fn refused(reason: &str) -> ApiError {
    ApiError::unprocessable("Validation Failed").with_error(FieldError {
        resource: "PullRequest".into(),
        field: "state".into(),
        code: "custom".into(),
        message: Some(format!("state cannot be changed. {reason}")),
        value: None,
    })
}

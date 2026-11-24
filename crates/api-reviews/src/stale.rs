use hubdouble_api_gitdata::branch_rules::{self, diff_state, flag};
use hubdouble_api_pulls::{StateKind, review_dismissed};
use hubdouble_git::{Git, ObjectId, RefChange};
use hubdouble_model::{ApiError, Issue, IssueState, ReviewState};
use hubdouble_store::{Issues, Repos, Reviews, TxMut};

use crate::events;

pub(crate) fn ref_changed(
    tx: &mut TxMut<'_>,
    git: &Git,
    change: &RefChange,
) -> Result<(), ApiError> {
    let (Some(old), Some(_)) = (change.old, change.new) else {
        return Ok(());
    };
    let Some(branch) = change.name.strip_prefix("refs/heads/") else {
        return Ok(());
    };
    let open: Vec<_> = tx
        .table::<Issues>()
        .pulls_on_branch(change.repo_id, branch)
        .filter(|issue| issue.state == IssueState::Open)
        .cloned()
        .collect();
    for issue in open {
        dismiss_stale(tx, git, &issue, change, branch, old)?;
    }
    Ok(())
}

fn dismiss_stale(
    tx: &mut TxMut<'_>,
    git: &Git,
    issue: &Issue,
    change: &RefChange,
    branch: &str,
    old: ObjectId,
) -> Result<(), ApiError> {
    let Some(p) = issue.pull.as_ref() else {
        return Ok(());
    };
    let Some(repo) = p.base.repo_id.and_then(|id| tx.table::<Repos>().get(id)) else {
        return Ok(());
    };
    let enabled = branch_rules::rules(tx, None, repo, &p.base.ref_name, true)
        .iter()
        .any(|r| r.rule_type == "pull_request" && flag(r, "dismiss_stale_reviews_on_push"));
    let approved: Vec<_> = tx
        .table::<Reviews>()
        .children(issue.id)
        .filter(|r| r.state == ReviewState::Approved)
        .cloned()
        .collect();
    if !enabled || approved.is_empty() {
        return Ok(());
    }
    let (base, head): (ObjectId, ObjectId) = (p.base.sha.parse()?, p.head.sha.parse()?);
    let pushed_head = p.head.repo_id == Some(change.repo_id) && p.head.ref_name == branch;
    let pushed_base = issue.repo_id == change.repo_id && p.base.ref_name == branch;
    let before = (
        if pushed_base { old } else { base },
        if pushed_head { old } else { head },
    );
    let repo_id = issue.repo_id;
    if diff_state(tx, git, repo_id, before.0, before.1)?
        == diff_state(tx, git, repo_id, base, head)?
    {
        return Ok(());
    }
    let now = git.now();
    for review in approved {
        let mut dismissed = (*review).clone();
        dismissed.state = ReviewState::Dismissed;
        tx.table_mut::<Reviews>()
            .replace(dismissed.clone())
            .map_err(|e| ApiError::internal(e.to_string()))?;
        let kind = StateKind::ReviewDismissed {
            review_id: review.id,
            state: "approved",
            message: None,
            commit: Some(p.head.sha.clone()),
        };
        review_dismissed(tx, issue, (change.sender, None), kind, now)?;
        events::review(tx, issue, change.sender, &dismissed, "dismissed", None)?;
    }
    Ok(())
}

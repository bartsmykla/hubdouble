use hubdouble_api_gitdata::branch_rules::{self, diff_state, flag};
use hubdouble_api_pulls::{StateKind, review_dismissed};
use hubdouble_git::{Git, ObjectData, ObjectId, RefChange};
use hubdouble_model::{ApiError, Issue, IssueState, ReviewState, Rule};
use hubdouble_store::{Issues, Repos, Reviews, Rulesets, TxMut};

use crate::events;

fn dismisses(rule: &Rule) -> bool {
    rule.rule_type == "pull_request" && flag(rule, "dismiss_stale_reviews_on_push")
}

pub(crate) fn ref_changed(
    tx: &mut TxMut<'_>,
    git: &Git,
    change: &RefChange,
) -> Result<(), ApiError> {
    let (Some(old), Some(_)) = (change.old, change.new) else {
        return Ok(());
    };
    if !tx
        .table::<Rulesets>()
        .iter()
        .any(|r| r.rules.iter().any(dismisses))
    {
        return Ok(());
    }
    if let Some(branch) = change.name.strip_prefix("refs/heads/") {
        let open: Vec<_> = tx
            .table::<Issues>()
            .pulls_on_branch(change.repo_id, branch)
            .filter(|issue| issue.state == IssueState::Open)
            .cloned()
            .collect();
        for issue in open {
            let Some(p) = issue.pull.as_ref() else {
                continue;
            };
            let pushed_head = p.head.repo_id == Some(change.repo_id) && p.head.ref_name == branch;
            let pushed_base = issue.repo_id == change.repo_id && p.base.ref_name == branch;
            let before = (
                if pushed_base {
                    old
                } else {
                    p.base.sha.parse()?
                },
                if pushed_head {
                    old
                } else {
                    p.head.sha.parse()?
                },
            );
            dismiss_stale(tx, git, &issue, change.sender, before)?;
        }
    } else if let Some(number) = change
        .name
        .strip_prefix("refs/pull/")
        .and_then(|rest| rest.strip_suffix("/merge"))
        .and_then(|n| n.parse().ok())
    {
        let Some(issue) = tx
            .table::<Issues>()
            .by_number(change.repo_id, number)
            .cloned()
        else {
            return Ok(());
        };
        let ObjectData::Commit(tested) = git.object(tx, change.repo_id, old)?.data else {
            return Ok(());
        };
        if let [base, head] = tested.parents[..] {
            dismiss_stale(tx, git, &issue, change.sender, (base, head))?;
        }
    }
    Ok(())
}

fn merge_base_advanced(
    tx: &TxMut<'_>,
    git: &Git,
    repo: i64,
    old_base: ObjectId,
    (base, head): (ObjectId, ObjectId),
) -> Result<bool, ApiError> {
    let (Some(old), Some(new)) = (
        git.compare(tx, repo, old_base, head)?.merge_base,
        git.compare(tx, repo, base, head)?.merge_base,
    ) else {
        return Ok(false);
    };
    Ok(git.compare(tx, repo, old, new)?.behind_by == 0)
}

fn dismiss_stale(
    tx: &mut TxMut<'_>,
    git: &Git,
    issue: &Issue,
    sender: i64,
    before: (ObjectId, ObjectId),
) -> Result<(), ApiError> {
    let Some(p) = issue.pull.as_ref() else {
        return Ok(());
    };
    if issue.state != IssueState::Open {
        return Ok(());
    }
    let Some(repo) = p.base.repo_id.and_then(|id| tx.table::<Repos>().get(id)) else {
        return Ok(());
    };
    if !branch_rules::rules(tx, None, repo, &p.base.ref_name, true)
        .into_iter()
        .any(dismisses)
    {
        return Ok(());
    }
    let approved: Vec<_> = tx
        .table::<Reviews>()
        .children(issue.id)
        .filter(|r| r.state == ReviewState::Approved)
        .cloned()
        .collect();
    if approved.is_empty() {
        return Ok(());
    }
    let (base, head): (ObjectId, ObjectId) = (p.base.sha.parse()?, p.head.sha.parse()?);
    let repo_id = issue.repo_id;
    if diff_state(tx, git, repo_id, before.0, before.1)?
        == diff_state(tx, git, repo_id, base, head)?
        || (before.1 == head && merge_base_advanced(tx, git, repo_id, before.0, (base, head))?)
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
        review_dismissed(tx, issue, (sender, None), kind, now)?;
        events::review(tx, issue, sender, &dismissed, "dismissed", None)?;
    }
    Ok(())
}

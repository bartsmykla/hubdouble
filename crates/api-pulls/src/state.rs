use std::sync::Arc;

use hubdouble_git::{Comparison, Git, GitError, ObjectId, RefChange};
use hubdouble_model::{ApiError, Issue, IssueState, Pull, PullRef};
use hubdouble_store::{Entity, Issues, Rows, Tx, TxMut};

use crate::events;

pub(crate) type PullBases = Rows<PullBase>;

#[derive(Clone)]
pub(crate) struct PullBase {
    issue_id: i64,
    oid: ObjectId,
}

impl Entity for PullBase {
    fn id(&self) -> i64 {
        self.issue_id
    }
}

pub(crate) fn pull(issue: &Issue) -> Result<&Pull, ApiError> {
    issue.pull.as_ref().ok_or_else(ApiError::not_found)
}

pub(crate) fn numbered(tx: &Tx, repo_id: i64, number: i64) -> Result<&Arc<Issue>, ApiError> {
    tx.table::<Issues>()
        .by_number(repo_id, number)
        .filter(|issue| issue.pull.is_some())
        .ok_or_else(ApiError::not_found)
}

pub(crate) fn branch(git: &Git, tx: &Tx, repo: i64, name: &str) -> Result<ObjectId, GitError> {
    git.resolve_ref(tx, repo, &format!("refs/heads/{name}"))
}

pub(crate) fn comparison_base(tx: &Tx, issue: &Issue, p: &Pull) -> Result<ObjectId, GitError> {
    match tx.table::<PullBases>().get(issue.id) {
        Some(base) => Ok(base.oid),
        None => p.base.sha.parse(),
    }
}

pub(crate) fn compare(git: &Git, tx: &Tx, issue: &Issue) -> Result<Comparison, ApiError> {
    let p = pull(issue)?;
    Ok(git.compare(
        tx,
        issue.repo_id,
        comparison_base(tx, issue, p)?,
        p.head.sha.parse()?,
    )?)
}

pub(crate) fn commit_node(
    git: &Git,
    tx: &Tx,
    repo_id: i64,
    id: ObjectId,
) -> Result<hubdouble_git::CommitNode, GitError> {
    let hubdouble_git::ObjectData::Commit(commit) = git.object(tx, repo_id, id)?.data else {
        return Err(GitError::WrongKind);
    };
    Ok(hubdouble_git::CommitNode {
        repo_id,
        id,
        commit,
    })
}

pub(crate) fn ref_updated(
    tx: &mut TxMut<'_>,
    git: &Git,
    change: &RefChange,
) -> Result<(), ApiError> {
    let Some(name) = change.name.strip_prefix("refs/heads/") else {
        return Ok(());
    };
    let affected: Vec<_> = tx
        .table::<Issues>()
        .pulls_on_branch(change.repo_id, name)
        .filter(|issue| issue.state == IssueState::Open)
        .cloned()
        .collect();
    for issue in affected {
        update_refs(tx, git, change, name, &issue)?;
    }
    Ok(())
}

fn update_refs(
    tx: &mut TxMut<'_>,
    git: &Git,
    change: &RefChange,
    name: &str,
    issue: &Issue,
) -> Result<(), ApiError> {
    let mut issue = issue.clone();
    let repo_id = issue.repo_id;
    let Some(p) = issue.pull.as_mut() else {
        return Ok(());
    };
    let head_changed = p.head.repo_id == Some(change.repo_id) && p.head.ref_name == name;
    let base_changed = repo_id == change.repo_id && p.base.ref_name == name;
    let old_base = p.base.sha.parse()?;
    if let Some(id) = change.new {
        if head_changed {
            p.head.sha = id.to_string();
        }
        if base_changed {
            p.base.sha = id.to_string();
        }
    }
    let base = live_tip(git, tx, &p.base)?;
    let head = live_tip(git, tx, &p.head)?;
    let now = git.now();
    let closes = if let (Some(base), Some(head)) = (base, head) {
        let reaches = git.compare(tx, repo_id, base, head)?.ahead_by == 0;
        if reaches {
            p.merged_at = Some(now);
            p.merge_commit_sha = Some(base.to_string());
            p.merged_by = Some(change.sender);
            p.mergeable = None;
            let comparison_base = if base_changed {
                old_base
            } else {
                change
                    .old
                    .and_then(|old_head| git.compare(tx, repo_id, base, old_head).ok()?.merge_base)
                    .unwrap_or(old_base)
            };
            tx.table_mut::<PullBases>().replace(PullBase {
                issue_id: issue.id,
                oid: comparison_base,
            })?;
        } else {
            p.mergeable = Some(git.mergeable(tx, repo_id, base, head)?);
        }
        reaches
    } else {
        p.mergeable = None;
        false
    };
    if closes {
        issue.state = IssueState::Closed;
        issue.closed_at = Some(now);
        issue.closed_by = Some(change.sender);
    }
    if closes || head_changed {
        issue.updated_at = now;
    }
    tx.table_mut::<Issues>().replace(issue.clone())?;
    if head_changed && let Some(after) = change.new {
        let before = change.old.unwrap_or(ObjectId::NULL);
        events::emit(
            tx,
            git,
            &issue,
            change.sender,
            "synchronize",
            serde_json::json!({"before": before, "after": after}),
        )?;
    }
    if closes {
        events::emit(
            tx,
            git,
            &issue,
            change.sender,
            "closed",
            serde_json::json!({}),
        )?;
    }
    Ok(())
}

fn live_tip(git: &Git, tx: &Tx, r: &PullRef) -> Result<Option<ObjectId>, GitError> {
    let Some(repo) = r.repo_id else {
        return Ok(None);
    };
    match branch(git, tx, repo, &r.ref_name) {
        Ok(id) => Ok(Some(id)),
        Err(GitError::NotFound) => Ok(None),
        Err(other) => Err(other),
    }
}

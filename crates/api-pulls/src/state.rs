use std::sync::Arc;

use hubdouble_git::{Comparison, Git, GitError, ObjectId, RefChange};
use hubdouble_model::{ApiError, Issue, IssueState, Pull, PullRef};
use hubdouble_store::{Entity, Issues, Rows, Tx, TxMut};

use crate::events;
use crate::timeline::{self, StateKind};

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

pub(crate) fn pull_mut(issue: &mut Issue) -> Result<&mut Pull, ApiError> {
    issue.pull.as_mut().ok_or_else(ApiError::not_found)
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

pub(crate) fn test_merge(
    git: &Git,
    tx: &Tx,
    (repo, author): (i64, i64),
    p: &mut Pull,
    (base, head): (ObjectId, ObjectId),
) -> Result<(), GitError> {
    let merge = git.test_merge(tx, repo, author, base, head)?;
    p.mergeable = Some(merge.is_some());
    p.merge_commit_sha = merge.map(|id| id.to_string());
    Ok(())
}

/// Points `refs/pull/{n}/head` at the pull request's head and, while it is open,
/// `refs/pull/{n}/merge` at its test merge commit, as GitHub keeps them. A pull request with
/// a conflict keeps its last merge ref, and a closed one loses it. Call it after the store
/// holds `issue`.
///
/// # Errors
///
/// A git error while it moves the refs.
pub fn sync_refs(tx: &mut TxMut<'_>, git: &Git, issue: &Issue) -> Result<(), ApiError> {
    crate::merge_state::record(tx, git, issue)?;
    let p = pull(issue)?;
    let (repo, sender) = (issue.repo_id, issue.user_id);
    let head = format!("refs/pull/{}/head", issue.number);
    set_ref(tx, git, (repo, sender), &head, Some(p.head.sha.parse()?))?;
    let merge = match (issue.state, &p.merge_commit_sha) {
        (IssueState::Open, None) => return Ok(()),
        (IssueState::Open, Some(sha)) => Some(sha.parse()?),
        (IssueState::Closed, _) => None,
    };
    let name = format!("refs/pull/{}/merge", issue.number);
    set_ref(tx, git, (repo, sender), &name, merge)?;
    Ok(())
}

fn set_ref(
    tx: &mut TxMut<'_>,
    git: &Git,
    (repo, sender): (i64, i64),
    name: &str,
    new: Option<ObjectId>,
) -> Result<(), GitError> {
    let old = match git.resolve_ref(tx, repo, name) {
        Ok(id) => Some(id),
        Err(GitError::NotFound) => None,
        Err(other) => return Err(other),
    };
    if old == new {
        return Ok(());
    }
    git.update_ref(tx, repo, sender, name, old, new, true)
}

pub(crate) fn ref_updated(
    tx: &mut TxMut<'_>,
    git: &Git,
    change: &RefChange,
) -> Result<(), ApiError> {
    let Some(name) = change.name.strip_prefix("refs/heads/") else {
        return Ok(());
    };
    crate::association::pushed(tx, git, change)?;
    if change.new.is_none() {
        return deleted(tx, git, change, name);
    }
    if change.old.is_none() {
        restored(tx, git, change, name)?;
    }
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

fn deleted(tx: &mut TxMut<'_>, git: &Git, change: &RefChange, name: &str) -> Result<(), ApiError> {
    let now = git.now();
    let who = (change.sender, None);
    let on_branch: Vec<_> = tx
        .table::<Issues>()
        .pulls_on_branch(change.repo_id, name)
        .cloned()
        .collect();
    for issue in on_branch {
        let mut issue = Issue::clone(&issue);
        let open = issue.state == IssueState::Open;
        let p = pull_mut(&mut issue)?;
        let head = p.head.repo_id == Some(change.repo_id) && p.head.ref_name == name;
        if !head && !open {
            continue;
        }
        if open && head {
            p.mergeable = None;
        } else if open {
            p.mergeable = Some(false);
            p.merge_commit_sha = None;
        }
        issue.updated_at = now;
        if head {
            tx.table_mut::<Issues>().replace(issue.clone())?;
            timeline::record(tx, &issue, who, StateKind::HeadRefDeleted)?;
            continue;
        }
        issue.state = IssueState::Closed;
        issue.closed_at = Some(now);
        issue.closed_by = Some(change.sender);
        tx.table_mut::<Issues>().replace(issue.clone())?;
        sync_refs(tx, git, &issue)?;
        timeline::record(tx, &issue, who, StateKind::BaseRefDeleted)?;
        timeline::record(tx, &issue, who, StateKind::Closed)?;
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

fn restored(tx: &mut TxMut<'_>, git: &Git, change: &RefChange, name: &str) -> Result<(), ApiError> {
    let heads: Vec<_> =
        tx.table::<Issues>()
            .pulls_on_branch(change.repo_id, name)
            .filter(|issue| {
                issue.pull.as_ref().is_some_and(|p| {
                    p.head.repo_id == Some(change.repo_id) && p.head.ref_name == name
                })
            })
            .filter(|issue| head_deleted(tx, issue.id))
            .cloned()
            .collect();
    for issue in heads {
        let mut issue = Issue::clone(&issue);
        issue.updated_at = git.now();
        tx.table_mut::<Issues>().replace(issue.clone())?;
        timeline::record(
            tx,
            &issue,
            (change.sender, None),
            StateKind::HeadRefRestored,
        )?;
    }
    Ok(())
}

fn head_deleted(tx: &Tx, issue_id: i64) -> bool {
    tx.table::<timeline::StateEvents>()
        .children(issue_id)
        .rev()
        .find_map(|e| match e.kind {
            StateKind::HeadRefDeleted => Some(true),
            StateKind::HeadRefRestored => Some(false),
            _ => None,
        })
        .unwrap_or(false)
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
    let moved = head_changed && change.new.is_some_and(|id| p.head.sha != id.to_string());
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
        let reaches = git.is_ancestor(tx, repo_id, head, base)?;
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
            test_merge(git, tx, (repo_id, issue.user_id), p, (base, head))?;
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
        crate::association::add(tx, repo_id, [issue.user_id])?;
    }
    if closes || head_changed {
        issue.updated_at = now;
    }
    tx.table_mut::<Issues>().replace(issue.clone())?;
    sync_refs(tx, git, &issue)?;
    if moved && let Some(after) = change.new {
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

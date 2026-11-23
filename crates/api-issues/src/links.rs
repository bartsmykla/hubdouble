use std::sync::Arc;

use hubdouble_admin::SeedCtx;
use hubdouble_git::{Git, RefChange};
use hubdouble_model::{ApiError, Issue, IssueState, Repo, Timestamp};
use hubdouble_store::{Accounts, Entity, Issues, Repos, Rows, Tx, TxMut};
use serde::Deserialize;
use serde_json::Value;

use crate::events::{self, What};
use crate::graphql::keyword_link;
use crate::issues::apply_state;
use crate::shared::{emit_issue, full_name};

/// A pull request linked to an issue by hand, in the issue's Development sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IssueLink {
    pub(crate) id: i64,
    pub(crate) repo_id: i64,
    pub(crate) issue_id: i64,
    pub(crate) pull_issue_id: i64,
}

impl Entity for IssueLink {
    fn id(&self) -> i64 {
        self.id
    }
    fn repo_id(&self) -> Option<i64> {
        Some(self.repo_id)
    }
    fn parent_id(&self) -> Option<i64> {
        Some(self.issue_id)
    }
    fn keys(&self) -> Vec<String> {
        vec![format!("{}:{}", self.issue_id, self.pull_issue_id)]
    }
}

pub(crate) type IssueLinks = Rows<IssueLink>;

pub(crate) struct MergeClose {
    pull_issue_id: i64,
}

impl Entity for MergeClose {
    fn id(&self) -> i64 {
        self.pull_issue_id
    }
}

pub(crate) type MergeCloses = Rows<MergeClose>;

pub(crate) fn ref_updated(
    tx: &mut TxMut<'_>,
    git: &Git,
    change: &RefChange,
) -> Result<(), ApiError> {
    let Some(name) = change.name.strip_prefix("refs/heads/") else {
        return Ok(());
    };
    let Some(repo) = tx.table::<Repos>().get(change.repo_id) else {
        return Ok(());
    };
    let issues = tx.table::<Issues>();
    let done = tx.table::<MergeCloses>();
    let merged: Vec<Arc<Issue>> = [Some(repo.id), repo.fork_of]
        .into_iter()
        .flatten()
        .flat_map(|id| issues.in_repo(id))
        .filter(|pull| {
            pull.pull.as_ref().is_some_and(|p| {
                p.merged_at.is_some()
                    && (pull.repo_id == change.repo_id && p.base.ref_name == name
                        || p.head.repo_id == Some(change.repo_id) && p.head.ref_name == name)
            }) && done.get(pull.id).is_none()
        })
        .cloned()
        .collect();
    for pull in merged {
        tx.table_mut::<MergeCloses>().insert(MergeClose {
            pull_issue_id: pull.id,
        })?;
        let sender = pull
            .pull
            .as_ref()
            .and_then(|p| p.merged_by)
            .unwrap_or(change.sender);
        for issue in closed_by_merge(tx, &pull) {
            close(tx, &issue, sender, git.now())?;
        }
    }
    Ok(())
}

fn closed_by_merge(tx: &Tx, pull: &Issue) -> Vec<Arc<Issue>> {
    let issues = tx.table::<Issues>();
    let Some(repo) = tx.table::<Repos>().get(pull.repo_id) else {
        return Vec::new();
    };
    if pull
        .pull
        .as_ref()
        .is_none_or(|p| p.base.ref_name != repo.default_branch)
    {
        return Vec::new();
    }
    let mut closed: Vec<Arc<Issue>> = tx
        .table::<Repos>()
        .iter()
        .flat_map(|target| {
            let full = full_name(tx, target);
            issues.in_repo(target.id).filter(move |i| {
                i.pull.is_none() && keyword_link(repo, pull, target.id, &full, i.number)
            })
        })
        .chain(
            tx.table::<IssueLinks>()
                .iter()
                .filter(|l| l.pull_issue_id == pull.id)
                .filter_map(|l| issues.get(l.issue_id)),
        )
        .filter(|i| i.state == IssueState::Open)
        .cloned()
        .collect();
    closed.sort_by_key(|i| i.id);
    closed.dedup_by_key(|i| i.id);
    closed
}

fn close(tx: &mut TxMut<'_>, issue: &Issue, sender: i64, now: Timestamp) -> Result<(), ApiError> {
    let repo: Arc<Repo> = tx
        .table::<Repos>()
        .get(issue.repo_id)
        .cloned()
        .ok_or_else(ApiError::not_found)?;
    let mut closed = issue.clone();
    apply_state(&mut closed, Some(IssueState::Closed), None, sender, now);
    closed.updated_at = now;
    tx.table_mut::<Issues>().replace(closed.clone())?;
    let via_app = tx.table::<Accounts>().get(sender).and_then(|a| a.app_id);
    let what = What::Closed(closed.state_reason);
    events::record(tx, &closed, sender, via_app, what, now)?;
    emit_issue(tx, &repo, &closed, "closed", sender, Vec::new())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkSeed {
    repository: String,
    issue_number: i64,
    pull_repository: Option<String>,
    pull_number: i64,
}

fn repo_named(tx: &Tx, full_name: &str) -> Result<i64, ApiError> {
    let unknown = || ApiError::unprocessable("No such repository");
    let (owner, name) = full_name.split_once('/').ok_or_else(unknown)?;
    let owner = tx.table::<Accounts>().by_login(owner).ok_or_else(unknown)?;
    Ok(tx
        .table::<Repos>()
        .by_name(owner.id, name)
        .ok_or_else(unknown)?
        .id)
}

pub(crate) fn seed(ctx: &mut SeedCtx<'_>, body: Value) -> Result<(), ApiError> {
    let seed: LinkSeed =
        serde_json::from_value(body).map_err(|e| ApiError::unprocessable(e.to_string()))?;
    let tx = &mut ctx.tx;
    let repo = repo_named(tx, &seed.repository)?;
    let pull_repo = match &seed.pull_repository {
        Some(name) => repo_named(tx, name)?,
        None => repo,
    };
    let issues = tx.table::<Issues>();
    let issue = issues
        .by_number(repo, seed.issue_number)
        .filter(|i| i.pull.is_none())
        .ok_or_else(|| ApiError::unprocessable("No such issue"))?
        .id;
    let pull = issues
        .by_number(pull_repo, seed.pull_number)
        .filter(|i| i.pull.is_some())
        .ok_or_else(|| ApiError::unprocessable("No such pull request"))?
        .id;
    let id = tx.next_id();
    tx.table_mut::<IssueLinks>().insert(IssueLink {
        id,
        repo_id: repo,
        issue_id: issue,
        pull_issue_id: pull,
    })?;
    Ok(())
}

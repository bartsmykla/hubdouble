use std::sync::Arc;

use hubdouble_admin::SeedCtx;
use hubdouble_auth::repo_role;
use hubdouble_git::{Git, ObjectId, RefChange};
use hubdouble_model::{
    Access, ApiError, Issue, IssueState, Repo, RepoRole, RepositorySelection, Timestamp,
};
use hubdouble_store::{Accounts, Entity, Installations, Issues, Repos, Rows, Tx, TxMut};
use serde::Deserialize;
use serde_json::Value;

use crate::events::{self, What};
use crate::issues::apply_state;
use crate::shared::emit_issue;

const CLOSING_KEYWORDS: [&str; 9] = [
    "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
];

pub(crate) fn closing_references(text: &str) -> impl Iterator<Item = (&str, i64)> {
    let mut previous = "";
    text.split_whitespace().filter_map(move |word| {
        let last = std::mem::replace(&mut previous, word);
        let target = word.trim_end_matches(['.', ',', ';', ')', '!', '?']);
        let (qualifier, digits) = target.split_once('#')?;
        let keyword = last.trim_start_matches('(').trim_end_matches(':');
        if CLOSING_KEYWORDS
            .iter()
            .any(|k| k.eq_ignore_ascii_case(keyword))
        {
            Some((qualifier, digits.parse().ok()?))
        } else {
            None
        }
    })
}

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

pub(crate) struct CommitClose {
    id: i64,
    key: String,
}

impl Entity for CommitClose {
    fn id(&self) -> i64 {
        self.id
    }

    fn keys(&self) -> Vec<String> {
        vec![self.key.clone()]
    }
}

pub(crate) type CommitCloses = Rows<CommitClose>;

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
    let default = repo.default_branch == name;
    from_commits(tx, git, change, default)?;
    let issues = tx.table::<Issues>();
    let done = tx.table::<MergeCloses>();
    let merged: Vec<Arc<Issue>> = issues
        .pulls_on_branch(change.repo_id, name)
        .filter(|pull| {
            pull.pull.as_ref().is_some_and(|p| p.merged_at.is_some()) && done.get(pull.id).is_none()
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
        for issue in closed_by_merge(tx, &pull, sender) {
            close(tx, &issue, sender, git.now(), None)?;
        }
    }
    Ok(())
}

fn references(text: &str) -> impl Iterator<Item = (&str, i64)> {
    text.split_whitespace().filter_map(|word| {
        let target = word
            .trim_start_matches('(')
            .trim_end_matches(['.', ',', ';', ':', ')', '!', '?']);
        let (qualifier, digits) = target.split_once('#')?;
        if !qualifier.is_empty() && qualifier.matches('/').count() != 1 {
            return None;
        }
        Some((qualifier, digits.parse().ok()?))
    })
}

fn from_commits(
    tx: &mut TxMut<'_>,
    git: &Git,
    change: &RefChange,
    default: bool,
) -> Result<(), ApiError> {
    let source = change.repo_id;
    let sender = change.sender;
    let via_app = tx.table::<Accounts>().get(sender).and_then(|a| a.app_id);
    let now = git.now();
    for (sha, commit) in git.newly_reachable_commits(tx, change)? {
        let closed = if default {
            let Some(closed) = close_from(tx, (source, sender, now), sha, &commit.message)? else {
                continue;
            };
            closed
        } else {
            Vec::new()
        };
        let mut named: Vec<Arc<Issue>> = references(&commit.message)
            .filter_map(|(qualifier, number)| named_issue(tx, source, qualifier, number))
            .filter(|i| !closed.contains(&i.id))
            .cloned()
            .collect();
        named.sort_unstable_by_key(|i| i.id);
        named.dedup_by_key(|i| i.id);
        for issue in named {
            hubdouble_api_pulls::referenced(
                tx,
                &issue,
                (sender, via_app),
                (source, sha.to_string()),
                now,
            )?;
        }
    }
    Ok(())
}

fn close_from(
    tx: &mut TxMut<'_>,
    (source, sender, now): (i64, i64, Timestamp),
    sha: ObjectId,
    message: &str,
) -> Result<Option<Vec<i64>>, ApiError> {
    let mut refs = closing_references(message);
    let Some(first) = refs.next() else {
        return Ok(Some(Vec::new()));
    };
    let key = format!("{source}/{sha}");
    if tx.table::<CommitCloses>().by_key(&key).is_some() {
        return Ok(None);
    }
    let mut issues: Vec<_> = std::iter::once(first)
        .chain(refs)
        .filter_map(|(qualifier, number)| referenced_issue(tx, source, qualifier, number))
        .filter(|i| i.state == IssueState::Open)
        .filter(|i| i.repo_id == source || can_push(tx, sender, i.repo_id))
        .cloned()
        .collect();
    issues.sort_unstable_by_key(|i| i.id);
    issues.dedup_by_key(|i| i.id);
    let id = tx.next_id();
    tx.table_mut::<CommitCloses>()
        .insert(CommitClose { id, key })?;
    for issue in &issues {
        close(tx, issue, sender, now, Some((source, sha)))?;
    }
    Ok(Some(issues.iter().map(|i| i.id).collect()))
}

fn can_push(tx: &Tx, sender: i64, repo_id: i64) -> bool {
    let Some(repo) = tx.table::<Repos>().get(repo_id) else {
        return false;
    };
    match tx.table::<Accounts>().get(sender).and_then(|a| a.app_id) {
        Some(app) => tx
            .table::<Installations>()
            .of(app, repo.owner_id)
            .is_some_and(|i| {
                i.suspended_at.is_none()
                    && (i.repository_selection == RepositorySelection::All
                        || i.repo_ids.contains(&repo.id))
                    && i.permissions.get("contents") >= Some(&Access::Write)
            }),
        None => repo_role(tx, sender, repo) >= Some(RepoRole::Write),
    }
}

fn closed_by_merge(tx: &Tx, pull: &Issue, sender: i64) -> Vec<Arc<Issue>> {
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
    let mut closed: Vec<Arc<Issue>> = closing_references(pull.body.as_deref().unwrap_or_default())
        .filter_map(|(qualifier, number)| referenced_issue(tx, repo.id, qualifier, number))
        .chain(
            tx.table::<IssueLinks>()
                .iter()
                .filter(|l| l.pull_issue_id == pull.id)
                .filter_map(|l| issues.get(l.issue_id)),
        )
        .filter(|i| i.state == IssueState::Open)
        .filter(|i| i.repo_id == repo.id || can_push(tx, sender, i.repo_id))
        .cloned()
        .collect();
    closed.sort_by_key(|i| i.id);
    closed.dedup_by_key(|i| i.id);
    closed
}

fn referenced_issue<'a>(
    tx: &'a Tx,
    source: i64,
    qualifier: &str,
    number: i64,
) -> Option<&'a Arc<Issue>> {
    named_issue(tx, source, qualifier, number).filter(|i| i.pull.is_none())
}

fn named_issue<'a>(
    tx: &'a Tx,
    source: i64,
    qualifier: &str,
    number: i64,
) -> Option<&'a Arc<Issue>> {
    let repo = if qualifier.is_empty() {
        source
    } else {
        lookup_repo(tx, qualifier)?.id
    };
    tx.table::<Issues>().by_number(repo, number)
}

fn close(
    tx: &mut TxMut<'_>,
    issue: &Issue,
    sender: i64,
    now: Timestamp,
    commit: Option<(i64, ObjectId)>,
) -> Result<(), ApiError> {
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
    let what = commit.map_or(What::Closed(closed.state_reason), |(repo_id, sha)| {
        What::CommitClosed { repo_id, sha }
    });
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

fn lookup_repo<'a>(tx: &'a Tx, full_name: &str) -> Option<&'a Repo> {
    let (owner, name) = full_name.split_once('/')?;
    let owner = tx.table::<Accounts>().by_login(owner)?;
    tx.table::<Repos>()
        .by_name(owner.id, name)
        .map(AsRef::as_ref)
}

/// Each pull request and an issue that it closes, as `(pull issue ID, issue ID)`: a closing
/// keyword in the body of a pull request on its repository's default branch, or a manual link.
#[must_use]
pub fn closing_links(tx: &Tx) -> Vec<(i64, i64)> {
    let issues = tx.table::<Issues>();
    let repos = tx.table::<Repos>();
    let mut links: Vec<(i64, i64)> = tx
        .table::<IssueLinks>()
        .iter()
        .map(|l| (l.pull_issue_id, l.issue_id))
        .collect();
    for pull in issues.iter() {
        let (Some(p), Some(body), Some(source)) =
            (&pull.pull, pull.body.as_deref(), repos.get(pull.repo_id))
        else {
            continue;
        };
        if p.base.ref_name != source.default_branch {
            continue;
        }
        for (qualifier, number) in closing_references(body) {
            let target = if qualifier.is_empty() {
                Some(source.id)
            } else {
                lookup_repo(tx, qualifier).map(|r| r.id)
            };
            if let Some(issue) = target
                .and_then(|repo| issues.by_number(repo, number))
                .filter(|i| i.pull.is_none())
            {
                links.push((pull.id, issue.id));
            }
        }
    }
    links
}

fn repo_named(tx: &Tx, full_name: &str) -> Result<i64, ApiError> {
    lookup_repo(tx, full_name)
        .map(|repo| repo.id)
        .ok_or_else(|| ApiError::unprocessable("No such repository"))
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

#[cfg(test)]
mod tests {
    use super::closing_references;

    #[test]
    fn closing_references_return_only_the_named_targets() {
        let text = "See #2. (Fixes #12), resolves: Acme/other#13!\nClosed #bad; close #15.";
        assert_eq!(
            closing_references(text).collect::<Vec<_>>(),
            [("", 12), ("Acme/other", 13), ("", 15)]
        );
    }
}

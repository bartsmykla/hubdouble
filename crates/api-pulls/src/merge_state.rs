use hubdouble_api_gitdata::branch_rules::{self, CheckResults, RequiredChecks};
use hubdouble_api_gitdata::codeowners;
use hubdouble_git::{Git, GitError, ObjectId};
use hubdouble_model::{ApiError, Issue, IssueState, Pull};
use hubdouble_store::{Entity, Repos, Rows, Tx, TxMut};

use crate::reviewing;

pub(crate) type MergeStates = Rows<MergeFacts>;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct MergeFacts {
    issue_id: i64,
    behind: bool,
    closed: Option<&'static str>,
    owners: Owners,
}

#[derive(Clone, Default, PartialEq, Eq)]
struct Owners {
    at: Option<(ObjectId, ObjectId)>,
    groups: Vec<Vec<String>>,
}

pub(crate) fn owner_groups<'t>(tx: &'t Tx, issue: &Issue) -> &'t [Vec<String>] {
    tx.table::<MergeStates>()
        .get(issue.id)
        .map_or(&[], |f| f.owners.groups.as_slice())
}

impl Entity for MergeFacts {
    fn id(&self) -> i64 {
        self.issue_id
    }
}

/// GitHub's `mergeable_state` of a pull request. An open one answers from its last test merge
/// and the base branch's rulesets, reviews and checks as they are now: `unknown` while
/// `mergeable` is `null`, `dirty` on a conflict, `behind` when a strict status check rule
/// needs the base tip in the head, `blocked` while a required check, a required approval or a
/// required thread resolution is missing, `unstable` while another check or status does not
/// pass, and `clean` otherwise. A required check reads the test merge commit when that commit
/// has a result for it and the head otherwise, and a check run and a commit status of one name
/// must both pass. A draft answers as a ready one. A closed one keeps the state it had when it
/// closed.
#[must_use]
pub fn mergeable_state(tx: &Tx, issue: &Issue) -> &'static str {
    let Some(p) = issue.pull.as_ref() else {
        return "unknown";
    };
    let facts = tx.table::<MergeStates>().get(issue.id);
    match issue.state {
        IssueState::Closed => facts
            .and_then(|f| f.closed)
            .unwrap_or_else(|| open_state(tx, issue, p, false)),
        IssueState::Open => open_state(tx, issue, p, facts.is_some_and(|f| f.behind)),
    }
}

pub(crate) fn record(tx: &mut TxMut<'_>, git: &Git, issue: &Issue) -> Result<(), ApiError> {
    let Some(p) = issue.pull.as_ref() else {
        return Ok(());
    };
    let old = tx.table::<MergeStates>().get(issue.id).cloned();
    let kept = old.as_ref().map_or_default(|f| f.owners.clone());
    let facts = match issue.state {
        IssueState::Open => MergeFacts {
            issue_id: issue.id,
            behind: p.mergeable == Some(true) && behind(git, tx, issue.repo_id, p)?,
            closed: None,
            owners: owners(git, tx, issue.repo_id, p, kept)?,
        },
        IssueState::Closed => {
            let behind = old.as_ref().is_some_and(|f| f.behind);
            MergeFacts {
                issue_id: issue.id,
                behind,
                closed: Some(
                    old.as_ref()
                        .and_then(|f| f.closed)
                        .unwrap_or_else(|| open_state(tx, issue, p, behind)),
                ),
                owners: kept,
            }
        }
    };
    if old.as_deref() != Some(&facts) {
        tx.table_mut::<MergeStates>().replace(facts)?;
    }
    Ok(())
}

fn base_tip(git: &Git, tx: &Tx, p: &Pull) -> Result<ObjectId, ApiError> {
    match crate::state::live_tip(git, tx, &p.base)? {
        Some(tip) => Ok(tip),
        None => Ok(p.base.sha.parse()?),
    }
}

fn behind(git: &Git, tx: &Tx, repo: i64, p: &Pull) -> Result<bool, ApiError> {
    let base = base_tip(git, tx, p)?;
    let head: ObjectId = p.head.sha.parse()?;
    Ok(!git.is_ancestor(tx, repo, base, head)?)
}

fn owners(git: &Git, tx: &Tx, repo: i64, p: &Pull, kept: Owners) -> Result<Owners, ApiError> {
    let at = (base_tip(git, tx, p)?, p.head.sha.parse()?);
    if kept.at == Some(at) {
        return Ok(kept);
    }
    let mut groups: Vec<Vec<String>> = Vec::new();
    if let Some((_, text)) = codeowners::file_at(tx, git, repo, at.0)? {
        let rules: Vec<_> = codeowners::lines(&text).collect();
        let paths = match git.changed_paths(tx, repo, at.0, at.1) {
            Ok(paths) => paths,
            Err(GitError::NotFound) => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        for path in paths {
            let Some(owners) = codeowners::owners_of(&rules, &path).filter(|o| !o.is_empty())
            else {
                continue;
            };
            let group: Vec<String> = owners.iter().map(|(_, o)| (*o).to_owned()).collect();
            if !groups.contains(&group) {
                groups.push(group);
            }
        }
    }
    Ok(Owners {
        at: Some(at),
        groups,
    })
}

fn open_state(tx: &Tx, issue: &Issue, p: &Pull, behind: bool) -> &'static str {
    match p.mergeable {
        None => return "unknown",
        Some(false) => return "dirty",
        Some(true) => {}
    }
    let Some(repo) = p.base.repo_id.and_then(|id| tx.table::<Repos>().get(id)) else {
        return "clean";
    };
    let rules = branch_rules::rules(tx, None, repo, &p.base.ref_name, true);
    let required = RequiredChecks::of(&rules);
    if required.strict && behind {
        return "behind";
    }
    let Ok(via) = reviewing::tested(p) else {
        return "unknown";
    };
    let Ok(head) = p.head.sha.parse() else {
        return "unknown";
    };
    if !required.unmet(tx, repo.id, &via.tested(head)).is_empty()
        || !reviewing::blocks(tx, issue, p, &rules).is_empty()
    {
        "blocked"
    } else if CheckResults::of(tx, repo.id, &p.head.sha).all_pass() {
        "clean"
    } else {
        "unstable"
    }
}

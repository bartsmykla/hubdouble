use hubdouble_api_gitdata::branch_rules::{self, CheckResults, RequiredChecks};
use hubdouble_git::{Git, ObjectId};
use hubdouble_model::{ApiError, Issue, IssueState, Pull};
use hubdouble_store::{Entity, Repos, Rows, Tx, TxMut};

use crate::reviewing;

pub(crate) type MergeStates = Rows<MergeFacts>;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct MergeFacts {
    issue_id: i64,
    behind: bool,
    closed: Option<&'static str>,
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
    let facts = match issue.state {
        IssueState::Open => MergeFacts {
            issue_id: issue.id,
            behind: p.mergeable == Some(true) && behind(git, tx, issue.repo_id, p)?,
            closed: None,
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
            }
        }
    };
    if old.as_deref() != Some(&facts) {
        tx.table_mut::<MergeStates>().replace(facts)?;
    }
    Ok(())
}

fn behind(git: &Git, tx: &Tx, repo: i64, p: &Pull) -> Result<bool, ApiError> {
    let base: ObjectId = match crate::state::live_tip(git, tx, &p.base)? {
        Some(tip) => tip,
        None => p.base.sha.parse()?,
    };
    let head: ObjectId = p.head.sha.parse()?;
    Ok(!git.is_ancestor(tx, repo, base, head)?)
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

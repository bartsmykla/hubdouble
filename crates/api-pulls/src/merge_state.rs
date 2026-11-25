use std::sync::Arc;

use hubdouble_api_gitdata::branch_rules::{self, CheckResults, MergeMethod, RequiredChecks, Via};
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
    test_merge: Option<ObjectId>,
    closed: Option<&'static str>,
    owners: Owners,
}

#[derive(Clone, Default, PartialEq, Eq)]
struct Owners {
    at: Option<(ObjectId, ObjectId)>,
    file: Option<Arc<str>>,
    covering: Vec<Vec<usize>>,
}

/// The owners of each changed file, from the last rule that covers it and that names only
/// owners who can write now, as GitHub skips a rule with an error.
pub(crate) fn owner_groups(tx: &Tx, issue: &Issue) -> Vec<Vec<String>> {
    tx.table::<MergeStates>()
        .get(issue.id)
        .map_or_else(Vec::new, |f| groups_of(tx, issue.repo_id, &f.owners))
}

fn groups_of(tx: &Tx, repo: i64, owners: &Owners) -> Vec<Vec<String>> {
    let (Some(text), Some(repo)) = (owners.file.as_deref(), tx.table::<Repos>().get(repo)) else {
        return Vec::new();
    };
    let valid = codeowners::valid_lines(text, &codeowners::RepoOwners { tx, repo });
    let mut groups: Vec<Vec<String>> = Vec::new();
    for lines in &owners.covering {
        let Some(rule) = lines.iter().rev().find_map(|n| {
            valid
                .binary_search_by_key(n, |l| l.number)
                .ok()
                .map(|i| &valid[i])
        }) else {
            continue;
        };
        let group: Vec<String> = rule.owners.iter().map(|(_, o)| (*o).to_owned()).collect();
        if !group.is_empty() && !groups.contains(&group) {
            groups.push(group);
        }
    }
    groups
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
/// merges the head into the base or an older base and has a result for it, and the head
/// otherwise, as a merge does, and a check run and a commit status of one name
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
            .unwrap_or_else(|| open_state(tx, issue, p, (false, None))),
        IssueState::Open => open_state(
            tx,
            issue,
            p,
            facts.map_or((false, None), |f| (f.behind, f.test_merge)),
        ),
    }
}

/// The CODEOWNERS file of the last base tip read, so that a push which moves the base of many
/// pull requests reads it once.
#[derive(Default)]
pub(crate) struct OwnersFile {
    at: Option<(i64, ObjectId)>,
    text: Option<Arc<str>>,
}

impl OwnersFile {
    fn read(
        &mut self,
        tx: &Tx,
        git: &Git,
        repo: i64,
        base: ObjectId,
    ) -> Result<Option<Arc<str>>, ApiError> {
        if self.at != Some((repo, base)) {
            self.text = codeowners::file_at(tx, git, repo, base)?.map(|(_, text)| text.into());
            self.at = Some((repo, base));
        }
        Ok(self.text.clone())
    }
}

pub(crate) fn record(tx: &mut TxMut<'_>, git: &Git, issue: &Issue) -> Result<(), ApiError> {
    record_with(tx, git, issue, &mut OwnersFile::default())
}

pub(crate) fn record_with(
    tx: &mut TxMut<'_>,
    git: &Git,
    issue: &Issue,
    file: &mut OwnersFile,
) -> Result<(), ApiError> {
    let Some(p) = issue.pull.as_ref() else {
        return Ok(());
    };
    let old = tx.table::<MergeStates>().get(issue.id).cloned();
    let kept = old.as_ref().map_or_default(|f| f.owners.clone());
    let facts = match issue.state {
        IssueState::Open => {
            let base = base_tip(git, tx, p)?;
            let head: ObjectId = p.head.sha.parse()?;
            MergeFacts {
                issue_id: issue.id,
                behind: p.mergeable == Some(true)
                    && !git.is_ancestor(tx, issue.repo_id, base, head)?,
                test_merge: reviewing::current_test_merge(tx, git, p, base)?,
                closed: None,
                owners: owners(git, tx, issue.repo_id, (base, head), file, kept)?,
            }
        }
        IssueState::Closed => {
            let behind = old.as_ref().is_some_and(|f| f.behind);
            let test_merge = old.as_ref().and_then(|f| f.test_merge);
            MergeFacts {
                issue_id: issue.id,
                behind,
                test_merge,
                closed: Some(
                    old.as_ref()
                        .and_then(|f| f.closed)
                        .unwrap_or_else(|| open_state(tx, issue, p, (behind, test_merge))),
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

fn owners(
    git: &Git,
    tx: &Tx,
    repo: i64,
    at: (ObjectId, ObjectId),
    file: &mut OwnersFile,
    kept: Owners,
) -> Result<Owners, ApiError> {
    let Some(text) = file.read(tx, git, repo, at.0)? else {
        return Ok(Owners::default());
    };
    if kept.at == Some(at) {
        return Ok(kept);
    }
    let rules: Vec<codeowners::Line<'_>> = codeowners::lines(&text).collect();
    let paths = match git.changed_paths(tx, repo, at.0, at.1) {
        Ok(paths) => paths,
        Err(GitError::NotFound) => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    let mut covering: Vec<Vec<usize>> = Vec::new();
    for path in paths {
        let lines: Vec<usize> = rules
            .iter()
            .filter(|l| codeowners::covers(l.pattern.1, &path))
            .map(|l| l.number)
            .collect();
        if !lines.is_empty() && !covering.contains(&lines) {
            covering.push(lines);
        }
    }
    Ok(Owners {
        at: Some(at),
        file: Some(text),
        covering,
    })
}

fn open_state(
    tx: &Tx,
    issue: &Issue,
    p: &Pull,
    (behind, test_merge): (bool, Option<ObjectId>),
) -> &'static str {
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
    let Ok(head) = p.head.sha.parse() else {
        return "unknown";
    };
    let via = Via::Pull {
        head,
        test_merge,
        method: MergeMethod::Merge,
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

use std::collections::HashMap;
use std::hash::BuildHasher as _;
use std::ops::Range;
use std::sync::Arc;

use foldhash::fast::FixedState;

use hubdouble_api_gitdata::branch_rules::{self, CheckResults, MergeMethod, RequiredChecks, Via};
use hubdouble_api_gitdata::codeowners::{self, Reach};
use hubdouble_git::{Git, GitError, ObjectId};
use hubdouble_model::{ApiError, Issue, IssueState, Pull};
use hubdouble_store::{Entity, Repos, Rows, Tx, TxMut};
use sha2::{Digest as _, Sha256};

use crate::reviewing;

pub(crate) type MergeStates = Rows<MergeFacts>;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct MergeFacts {
    issue_id: i64,
    behind: bool,
    test_merge: Option<ObjectId>,
    merge: Option<reviewing::TestMerge>,
    closed: Option<&'static str>,
    owners: Owners,
}

#[derive(Clone, Default, PartialEq, Eq)]
struct Owners {
    at: Option<(ObjectId, ObjectId)>,
    read: Option<(ObjectId, [u8; 32])>,
    rules: Vec<(usize, Box<str>)>,
    covering: Vec<Vec<usize>>,
}

/// The owners of each changed file, from the last rule that covers it and that names only
/// owners who can write now, as GitHub skips a rule with an error.
pub(crate) fn owner_groups(tx: &Tx, issue: &Issue) -> Vec<Vec<String>> {
    tx.table::<MergeStates>()
        .get(issue.id)
        .map_or_else(Vec::new, |f| groups_of(tx, issue.repo_id, &f.owners))
}

/// [`owner_groups`] of the base tip and the head as they are now.
pub(crate) fn current_owner_groups(
    tx: &Tx,
    git: &Git,
    issue: &Issue,
) -> Result<Vec<Vec<String>>, ApiError> {
    let p = crate::state::pull(issue)?;
    let at = (base_tip(git, tx, p)?, p.head.sha.parse()?);
    let owners = owners(
        git,
        tx,
        issue.repo_id,
        at,
        &mut PushMemo::default(),
        Owners::default(),
    )?;
    Ok(groups_of(tx, issue.repo_id, &owners))
}

fn groups_of(tx: &Tx, repo: i64, owners: &Owners) -> Vec<Vec<String>> {
    let Some(repo) = tx.table::<Repos>().get(repo) else {
        return Vec::new();
    };
    let who = codeowners::RepoOwners { tx, repo };
    let valid: Vec<codeowners::Line<'_>> = owners
        .rules
        .iter()
        .filter_map(|(number, raw)| {
            let line = codeowners::lines(raw).next()?;
            Some(codeowners::Line {
                number: *number,
                ..line
            })
        })
        .filter(|line| line.is_valid(&who))
        .collect();
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

/// What a push that moves the base of many pull requests reads once for all of them: the
/// CODEOWNERS blob of the last base tip, the rules of the last blob read, and each ancestry
/// answer.
#[derive(Default)]
pub(crate) struct PushMemo {
    at: Option<(i64, ObjectId)>,
    blob: Option<ObjectId>,
    rules: Option<(ObjectId, Arc<Rules>)>,
    reaches: Vec<((i64, ObjectId, ObjectId), bool)>,
}

struct Rule {
    number: usize,
    raw: Range<usize>,
    pattern: Range<usize>,
    next: Option<usize>,
}

const ROOT: u8 = 0;
const NAME: u8 = 1;

struct Rules {
    text: String,
    all: Vec<Rule>,
    first: HashMap<u64, usize, FixedState>,
    any: Vec<usize>,
}

impl Rules {
    fn of(text: String) -> Self {
        let lines = text.bytes().filter(|&b| b == b'\n').count() + 1;
        let mut all = Vec::with_capacity(lines);
        let mut first: HashMap<u64, usize, FixedState> =
            HashMap::with_capacity_and_hasher(lines, FixedState::default());
        let mut any = Vec::new();
        for (i, (number, raw, pattern)) in codeowners::spans(&text).enumerate() {
            let key = match codeowners::reach(text.get(pattern.clone()).unwrap_or_default()) {
                Reach::Root(name) => Some(key_of(ROOT, name)),
                Reach::Name(name) => Some(key_of(NAME, name)),
                Reach::Any => None,
            };
            let next = if let Some(key) = key {
                first.insert(key, i)
            } else {
                any.push(i);
                None
            };
            all.push(Rule {
                number,
                raw,
                pattern,
                next,
            });
        }
        Self {
            text,
            all,
            first,
            any,
        }
    }

    fn raw(&self, rule: &Rule) -> &str {
        self.text.get(rule.raw.clone()).unwrap_or_default()
    }

    fn covering(&self, path: &str) -> Vec<usize> {
        let mut found = self.any.clone();
        let root = path.split('/').next().unwrap_or(path);
        self.chain(key_of(ROOT, root), &mut found);
        for segment in path.split('/') {
            self.chain(key_of(NAME, segment), &mut found);
        }
        found.sort_unstable();
        found.dedup();
        found.retain(|&i| {
            self.all.get(i).is_some_and(|rule| {
                codeowners::covers(
                    self.text.get(rule.pattern.clone()).unwrap_or_default(),
                    path,
                )
            })
        });
        found
    }

    fn chain(&self, key: u64, out: &mut Vec<usize>) {
        let mut at = self.first.get(&key).copied();
        while let Some(i) = at {
            out.push(i);
            at = self.all.get(i).and_then(|rule| rule.next);
        }
    }
}

fn key_of(kind: u8, name: &str) -> u64 {
    FixedState::default().hash_one((kind, name))
}

impl PushMemo {
    pub(crate) fn is_ancestor(
        &mut self,
        tx: &Tx,
        git: &Git,
        repo: i64,
        (ancestor, of): (ObjectId, ObjectId),
    ) -> Result<bool, GitError> {
        let key = (repo, ancestor, of);
        if let Some(&(_, known)) = self.reaches.iter().find(|(k, _)| *k == key) {
            return Ok(known);
        }
        let answer = git.is_ancestor(tx, repo, ancestor, of)?;
        self.reaches.push((key, answer));
        Ok(answer)
    }

    fn blob(
        &mut self,
        tx: &Tx,
        git: &Git,
        repo: i64,
        base: ObjectId,
    ) -> Result<Option<ObjectId>, ApiError> {
        if self.at != Some((repo, base)) {
            self.blob = codeowners::blob_at(tx, git, repo, base)?.map(|(_, blob)| blob);
            self.at = Some((repo, base));
        }
        Ok(self.blob)
    }

    fn rules(
        &mut self,
        tx: &Tx,
        git: &Git,
        repo: i64,
        blob: ObjectId,
    ) -> Result<Arc<Rules>, ApiError> {
        if let Some((read, rules)) = &self.rules
            && *read == blob
        {
            return Ok(rules.clone());
        }
        let rules = Arc::new(Rules::of(codeowners::text_of(tx, git, repo, blob)?));
        self.rules = Some((blob, rules.clone()));
        Ok(rules)
    }
}

pub(crate) fn record(tx: &mut TxMut<'_>, git: &Git, issue: &Issue) -> Result<(), ApiError> {
    record_with(tx, git, issue, &mut PushMemo::default())
}

pub(crate) fn record_with(
    tx: &mut TxMut<'_>,
    git: &Git,
    issue: &Issue,
    memo: &mut PushMemo,
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
            let merge = reviewing::test_merge_of(tx, git, p, old.as_ref().and_then(|f| f.merge))?;
            MergeFacts {
                issue_id: issue.id,
                behind: p.mergeable == Some(true)
                    && !git.is_ancestor(tx, issue.repo_id, base, head)?,
                test_merge: reviewing::current_test_merge(tx, git, (p, base, head), merge, memo)?,
                merge,
                closed: None,
                owners: owners(git, tx, issue.repo_id, (base, head), memo, kept)?,
            }
        }
        IssueState::Closed => {
            let behind = old.as_ref().is_some_and(|f| f.behind);
            let test_merge = old.as_ref().and_then(|f| f.test_merge);
            MergeFacts {
                issue_id: issue.id,
                behind,
                test_merge,
                merge: old.as_ref().and_then(|f| f.merge),
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
    memo: &mut PushMemo,
    kept: Owners,
) -> Result<Owners, ApiError> {
    let Some(blob) = memo.blob(tx, git, repo, at.0)? else {
        return Ok(Owners::default());
    };
    if kept.at == Some(at) {
        return Ok(kept);
    }
    let paths = match git.changed_paths(tx, repo, at.0, at.1) {
        Ok(paths) => paths,
        Err(GitError::NotFound) => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    let mut digest = Sha256::new();
    for path in &paths {
        digest.update(path.as_bytes());
        digest.update([0]);
    }
    let read = Some((blob, digest.finalize().into()));
    if kept.read == read {
        return Ok(Owners {
            at: Some(at),
            ..kept
        });
    }
    let rules = memo.rules(tx, git, repo, blob)?;
    let mut covering: Vec<Vec<usize>> = Vec::new();
    let mut used: Vec<usize> = Vec::new();
    for path in &paths {
        let found = rules.covering(path);
        let lines: Vec<usize> = found
            .iter()
            .filter_map(|&i| rules.all.get(i))
            .map(|rule| rule.number)
            .collect();
        if !lines.is_empty() && !covering.contains(&lines) {
            covering.push(lines);
            used.extend(found);
        }
    }
    used.sort_unstable();
    used.dedup();
    Ok(Owners {
        at: Some(at),
        read,
        rules: used
            .iter()
            .filter_map(|&i| rules.all.get(i))
            .map(|rule| (rule.number, rules.raw(rule).into()))
            .collect(),
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

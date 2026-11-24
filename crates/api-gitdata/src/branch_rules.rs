use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use hubdouble_auth::{Actor, ruleset_bypass};
use hubdouble_git::{Commit, Git, ObjectData, ObjectId, SignatureState};
use hubdouble_model::{ApiError, Conclusion, Repo, Rule, RunStatus, StatusState};
use hubdouble_store::{Apps, CheckRuns, CheckSuites, Rulesets, Statuses, Tx};
use serde_json::Value;

const CREATION: &str = "Cannot create ref due to creations being restricted.";
const UPDATE: &str = "Cannot update this protected ref.";
const DELETION: &str = "Cannot delete this protected branch.";
const FORCE_PUSH: &str = "Cannot force-push to this branch";
const PULL_REQUEST: &str = "Changes must be made through a pull request.";
const MERGE_METHOD: &str = "Merge is not an allowed merge method in this repository.";
const LINEAR: &str = "This branch must not contain merge commits.";
const SIGNATURES: &str = "Commits must have verified signatures.";

/// The write that moves a branch. It decides which rules apply and which commit a required
/// status check reads.
#[derive(Debug, Clone, Copy)]
pub enum Via {
    /// A direct write of the ref (`git/refs`, `updateRefs`, `createCommitOnBranch`). Required
    /// checks read the new tip.
    Push,
    /// `POST /repos/{owner}/{repo}/merges`. Required checks read the merged `head`.
    Merge { head: ObjectId },
    /// The merge of a pull request. A required check reads the test merge commit when that
    /// commit has a result for the check, and the head commit otherwise.
    Pull {
        head: ObjectId,
        test_merge: Option<ObjectId>,
    },
}

impl Via {
    /// The commits whose results a required check reads, in order, for a write whose new tip
    /// is `new`.
    #[must_use]
    pub fn tested(self, new: ObjectId) -> Vec<ObjectId> {
        match self {
            Via::Push => vec![new],
            Via::Merge { head } => vec![head],
            Via::Pull { head, test_merge } => test_merge.into_iter().chain([head]).collect(),
        }
    }
}

/// The rules of the active branch rulesets on `branch`, in ruleset and rule order. With an
/// actor, a ruleset whose bypass list lets the actor skip this write is left out; without one,
/// every ruleset counts, as for the merge state that every reader sees.
#[must_use]
pub fn rules<'t>(
    tx: &'t Tx,
    actor: Option<&Actor>,
    repo: &Repo,
    branch: &str,
    pull_request: bool,
) -> Vec<&'t Rule> {
    tx.table::<Rulesets>()
        .iter()
        .filter(|r| r.applies_to_branch(repo, branch))
        .filter(|r| actor.is_none_or(|a| !ruleset_bypass(tx, a, repo, r).skips(pull_request)))
        .flat_map(|r| r.rules.iter())
        .collect()
}

/// A `parameters` value of a rule.
#[must_use]
pub fn parameter<'r>(rule: &'r Rule, key: &str) -> Option<&'r Value> {
    rule.parameters.as_ref().and_then(|p| p.get(key))
}

/// A boolean `parameters` value of a rule, `false` when it is missing.
#[must_use]
pub fn flag(rule: &Rule, key: &str) -> bool {
    parameter(rule, key)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The `required_status_checks` rules merged: each `(context, integration_id)` once, whether
/// any rule is strict, and whether the checks hold for a new branch.
#[derive(Debug, Default)]
pub struct RequiredChecks {
    pub checks: Vec<(String, Option<i64>)>,
    pub strict: bool,
    pub on_create: bool,
}

impl RequiredChecks {
    #[must_use]
    pub fn of(rules: &[&Rule]) -> RequiredChecks {
        let mut out = RequiredChecks::default();
        for rule in rules
            .iter()
            .filter(|r| r.rule_type == "required_status_checks")
        {
            out.strict |= flag(rule, "strict_required_status_checks_policy");
            out.on_create |= !flag(rule, "do_not_enforce_on_create");
            for check in parameter(rule, "required_status_checks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(context) = check.get("context").and_then(Value::as_str) else {
                    continue;
                };
                let key = (
                    context.to_owned(),
                    check.get("integration_id").and_then(Value::as_i64),
                );
                if !out.checks.contains(&key) {
                    out.checks.push(key);
                }
            }
        }
        out
    }

    /// The contexts that do not pass, reading each on the first of `commits` that has a result
    /// for it.
    #[must_use]
    pub fn unmet(&self, tx: &Tx, repo: i64, commits: &[ObjectId]) -> Vec<&str> {
        if self.checks.is_empty() {
            return Vec::new();
        }
        let results: Vec<_> = commits
            .iter()
            .map(|c| CheckResults::of(tx, repo, &c.to_string()))
            .collect();
        self.checks
            .iter()
            .filter(|(context, app)| {
                !results
                    .iter()
                    .find_map(|r| r.required(tx, context, *app))
                    .unwrap_or(false)
            })
            .map(|(context, _)| context.as_str())
            .collect()
    }

    /// GitHub's violation text for the unmet contexts: the context's name when it is the only
    /// one, and a count otherwise.
    #[must_use]
    pub fn violation(&self, unmet: &[&str]) -> Option<String> {
        match unmet {
            [] => None,
            [one] => Some(format!("Required status check \"{one}\" is expected.")),
            _ => Some(format!(
                "{} of {} required status checks are expected.",
                unmet.len(),
                self.checks.len()
            )),
        }
    }
}

/// The latest check run of each name and App and the latest commit status of each context on
/// one commit.
pub struct CheckResults {
    runs: BTreeMap<(String, i64), (RunStatus, Option<Conclusion>)>,
    statuses: BTreeMap<String, (StatusState, i64)>,
}

impl CheckResults {
    #[must_use]
    pub fn of(tx: &Tx, repo: i64, sha: &str) -> CheckResults {
        let mut latest = BTreeMap::new();
        for run in tx
            .table::<CheckSuites>()
            .in_repo(repo)
            .filter(|s| s.head_sha == sha)
            .flat_map(|s| tx.table::<CheckRuns>().children(s.id))
        {
            let entry = latest.entry((run.name.clone(), run.app_id)).or_insert((
                run.id,
                run.status,
                run.conclusion,
            ));
            if run.id > entry.0 {
                *entry = (run.id, run.status, run.conclusion);
            }
        }
        let runs = latest
            .into_iter()
            .map(|(key, (_, status, conclusion))| (key, (status, conclusion)))
            .collect();
        let mut statuses = BTreeMap::new();
        for status in tx
            .table::<Statuses>()
            .in_repo(repo)
            .filter(|s| s.sha == sha)
        {
            statuses.insert(status.context.clone(), (status.state, status.creator_id));
        }
        CheckResults { runs, statuses }
    }

    /// Whether a required check of `context` from `app` (any source when `None`) passes on
    /// this commit, or `None` when the commit has no check run or status for it. When a check
    /// run and a commit status share the name, both must pass.
    #[must_use]
    pub fn required(&self, tx: &Tx, context: &str, app: Option<i64>) -> Option<bool> {
        let mut runs = self
            .runs
            .iter()
            .filter(|((name, id), _)| name == context && app.is_none_or(|app| app == *id))
            .map(|(_, &(status, conclusion))| run_passes(status, conclusion))
            .peekable();
        let run = runs.peek().is_some().then(|| runs.all(|pass| pass));
        let bot = app
            .and_then(|id| tx.table::<Apps>().get(id))
            .map(|a| a.bot_id);
        let status = self
            .statuses
            .get(context)
            .filter(|&&(_, creator)| app.is_none() || bot == Some(creator))
            .map(|&(state, _)| state == StatusState::Success);
        match (run, status) {
            (None, None) => None,
            (run, status) => Some(run.unwrap_or(true) && status.unwrap_or(true)),
        }
    }

    /// Whether every latest check run and commit status passes.
    #[must_use]
    pub fn all_pass(&self) -> bool {
        self.runs
            .values()
            .all(|&(status, conclusion)| run_passes(status, conclusion))
            && self
                .statuses
                .values()
                .all(|&(state, _)| state == StatusState::Success)
    }
}

fn run_passes(status: RunStatus, conclusion: Option<Conclusion>) -> bool {
    status == RunStatus::Completed
        && matches!(
            conclusion,
            Some(Conclusion::Success | Conclusion::Neutral | Conclusion::Skipped)
        )
}

/// The rules that a write of `branch` from `old` to `new` breaks, as GitHub words each
/// violation, in rule order. `None` for `old` creates the branch and `None` for `new` deletes
/// it. Rulesets that the actor may bypass for this write do not count.
///
/// # Errors
///
/// A git error while it reads the commits that the write adds to the branch.
pub fn violations(
    tx: &Tx,
    git: &Git,
    actor: &Actor,
    repo: &Repo,
    branch: &str,
    (old, new): (Option<ObjectId>, Option<ObjectId>),
    via: Via,
) -> Result<Vec<String>, ApiError> {
    let pull_request = matches!(via, Via::Pull { .. });
    let rules = rules(tx, Some(actor), repo, branch, pull_request);
    let mut out: Vec<String> = Vec::new();
    let Some(new) = new else {
        if rules.iter().any(|r| r.rule_type == "deletion") {
            out.push(DELETION.to_owned());
        }
        return Ok(out);
    };
    let mut added: Option<Added> = None;
    let mut checked = false;
    for rule in &rules {
        let violation = match rule.rule_type.as_str() {
            "creation" if old.is_none() => Some(CREATION.to_owned()),
            "update" if old.is_some() => Some(UPDATE.to_owned()),
            "non_fast_forward" => match old {
                Some(old) if !git.is_ancestor(tx, repo.id, old, new)? => {
                    Some(FORCE_PUSH.to_owned())
                }
                _ => None,
            },
            "pull_request" if !pull_request && old.is_some() => Some(PULL_REQUEST.to_owned()),
            "pull_request" if pull_request && !allows_merge(rule) => Some(MERGE_METHOD.to_owned()),
            "required_linear_history" => {
                let commits = cached(&mut added, || introduced(tx, git, repo, branch, old, new))?;
                commits
                    .iter()
                    .any(|(_, c)| c.parents.len() > 1)
                    .then(|| LINEAR.to_owned())
            }
            "required_signatures" => {
                let commits = cached(&mut added, || introduced(tx, git, repo, branch, old, new))?;
                commits
                    .iter()
                    .any(|(_, c)| git.verification(c) != SignatureState::Valid)
                    .then(|| SIGNATURES.to_owned())
            }
            "required_status_checks" if !checked => {
                checked = true;
                let required = RequiredChecks::of(&rules);
                if old.is_none() && !required.on_create {
                    None
                } else {
                    let unmet = required.unmet(tx, repo.id, &via.tested(new));
                    required.violation(&unmet)
                }
            }
            _ => None,
        };
        if let Some(v) = violation
            && !out.contains(&v)
        {
            out.push(v);
        }
    }
    Ok(out)
}

fn allows_merge(rule: &Rule) -> bool {
    parameter(rule, "allowed_merge_methods")
        .and_then(Value::as_array)
        .is_none_or(|methods| methods.iter().any(|m| m.as_str() == Some("merge")))
}

type Added = Vec<(ObjectId, Arc<Commit>)>;

fn cached(
    slot: &mut Option<Added>,
    load: impl FnOnce() -> Result<Added, ApiError>,
) -> Result<&[(ObjectId, Arc<Commit>)], ApiError> {
    if slot.is_none() {
        *slot = Some(load()?);
    }
    Ok(slot.as_deref().unwrap_or_default())
}

fn introduced(
    tx: &Tx,
    git: &Git,
    repo: &Repo,
    branch: &str,
    old: Option<ObjectId>,
    new: ObjectId,
) -> Result<Added, ApiError> {
    if let Some(old) = old {
        return Ok(git.compare(tx, repo.id, old, new)?.commits);
    }
    let own = format!("refs/heads/{branch}");
    let mut reachable = HashSet::new();
    for other in git
        .refs(tx, repo.id, "refs/heads/")
        .into_iter()
        .filter(|r| r.name != own)
    {
        reachable.extend(
            git.log(tx, repo.id, other.target, false, usize::MAX)?
                .into_iter()
                .map(|(id, _)| id),
        );
    }
    Ok(git
        .log(tx, repo.id, new, false, usize::MAX)?
        .into_iter()
        .filter(|(id, _)| !reachable.contains(id))
        .collect())
}

fn tree_of(tx: &Tx, git: &Git, repo: i64, id: ObjectId) -> Result<ObjectId, ApiError> {
    match git.object(tx, repo, id)?.data {
        ObjectData::Commit(c) => Ok(c.tree),
        _ => Err(ApiError::internal("a pull request side is not a commit")),
    }
}

/// The state of a pull request's diff: the trees of the merge base and of the head. GitHub
/// records it when a review approves, and a change of either tree makes the approval stale.
///
/// # Errors
///
/// A git error while it reads the commits.
pub fn diff_state(
    tx: &Tx,
    git: &Git,
    repo: i64,
    base: ObjectId,
    head: ObjectId,
) -> Result<(Option<ObjectId>, ObjectId), ApiError> {
    let merge_base = match git.compare(tx, repo, base, head)?.merge_base {
        Some(id) => Some(tree_of(tx, git, repo, id)?),
        None => None,
    };
    Ok((merge_base, tree_of(tx, git, repo, head)?))
}

pub(crate) struct Guard<'a> {
    pub(crate) tx: &'a Tx,
    pub(crate) git: &'a Git,
    pub(crate) actor: &'a Actor,
    pub(crate) repo: &'a Repo,
}

impl Guard<'_> {
    pub(crate) fn check(
        &self,
        name: &str,
        moves: (Option<ObjectId>, Option<ObjectId>),
        via: Via,
    ) -> Result<Vec<String>, ApiError> {
        match name.strip_prefix("refs/heads/") {
            Some(branch) => {
                violations(self.tx, self.git, self.actor, self.repo, branch, moves, via)
            }
            None => Ok(Vec::new()),
        }
    }
}

/// GitHub's refusal of a write that breaks rules: `Repository rule violations found` followed
/// by each violation, every part ending in a blank line.
#[must_use]
pub fn message(violations: &[String]) -> String {
    let mut message = String::from("Repository rule violations found\n\n");
    for v in violations {
        message.push_str(v);
        message.push_str("\n\n");
    }
    message
}

/// The REST answer that carries [`message`].
#[must_use]
pub fn refusal(status: u16, violations: &[String], documentation_url: &str) -> ApiError {
    ApiError::new(status, message(violations)).with_docs(documentation_url)
}

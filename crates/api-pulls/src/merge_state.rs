use std::collections::BTreeMap;

use hubdouble_git::{Git, ObjectId};
use hubdouble_model::{
    ApiError, Conclusion, Issue, IssueState, Pull, Repo, RepoRole, ReviewState, RunStatus,
    StatusState,
};
use hubdouble_store::{
    Apps, CheckRuns, CheckSuites, Entity, Repos, ReviewThreads, Reviews, Rows, Rulesets, Statuses,
    Tx, TxMut,
};
use serde_json::Value;

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
/// pass, and `clean` otherwise. A draft answers as a ready one. A closed one keeps the state
/// it had when it closed.
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
    let base: ObjectId = p.base.sha.parse()?;
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
    let rules = Rules::of(tx, repo, &p.base.ref_name);
    if rules.strict && behind {
        return "behind";
    }
    let checks = Checks::of(tx, repo.id, &p.head.sha);
    let unresolved = rules.thread_resolution
        && tx
            .table::<ReviewThreads>()
            .children(issue.id)
            .any(|t| !t.is_resolved);
    if rules
        .required_checks
        .iter()
        .any(|(context, app)| !checks.passes(tx, context, *app))
        || review_decision(tx, issue, p).is_some_and(|d| d != "APPROVED")
        || unresolved
    {
        "blocked"
    } else if checks.all_pass() {
        "clean"
    } else {
        "unstable"
    }
}

#[derive(Default)]
struct Rules {
    strict: bool,
    thread_resolution: bool,
    required_checks: Vec<(String, Option<i64>)>,
}

impl Rules {
    fn of(tx: &Tx, repo: &Repo, branch: &str) -> Rules {
        let mut rules = Rules::default();
        for rule in tx
            .table::<Rulesets>()
            .iter()
            .filter(|r| r.applies_to_branch(repo, branch))
            .flat_map(|r| &r.rules)
        {
            let parameter = |key: &str| rule.parameters.as_ref().and_then(|p| p.get(key));
            match rule.rule_type.as_str() {
                "required_status_checks" => {
                    rules.strict |= parameter("strict_required_status_checks_policy")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    rules.required_checks.extend(
                        parameter("required_status_checks")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(|check| {
                                Some((
                                    check.get("context")?.as_str()?.to_owned(),
                                    check.get("integration_id").and_then(Value::as_i64),
                                ))
                            }),
                    );
                }
                "pull_request" => {
                    rules.thread_resolution |= parameter("required_review_thread_resolution")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                }
                _ => {}
            }
        }
        rules
    }
}

struct Checks {
    runs: BTreeMap<(String, i64), (RunStatus, Option<Conclusion>)>,
    statuses: BTreeMap<String, (StatusState, i64)>,
}

impl Checks {
    fn of(tx: &Tx, repo: i64, sha: &str) -> Checks {
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
        Checks { runs, statuses }
    }

    fn passes(&self, tx: &Tx, context: &str, app: Option<i64>) -> bool {
        let run = self
            .runs
            .iter()
            .filter(|((name, id), _)| name == context && app.is_none_or(|app| app == *id))
            .any(|(_, &(status, conclusion))| run_passes(status, conclusion));
        let bot = app
            .and_then(|id| tx.table::<Apps>().get(id))
            .map(|a| a.bot_id);
        let status = self.statuses.get(context).is_some_and(|&(state, creator)| {
            state == StatusState::Success && (app.is_none() || bot == Some(creator))
        });
        run || status
    }

    fn all_pass(&self) -> bool {
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

pub(crate) fn review_decision(tx: &Tx, issue: &Issue, pull: &Pull) -> Option<&'static str> {
    let repo = tx.table::<Repos>().get(pull.base.repo_id?)?;
    let required = tx
        .table::<Rulesets>()
        .iter()
        .filter(|r| r.applies_to_branch(repo, &pull.base.ref_name))
        .flat_map(|r| &r.rules)
        .filter(|r| r.rule_type == "pull_request")
        .filter_map(|r| {
            usize::try_from(
                r.parameters
                    .as_ref()?
                    .get("required_approving_review_count")?
                    .as_u64()?,
            )
            .ok()
        })
        .filter(|&count| count > 0)
        .max()?;
    let mut reviewers = BTreeMap::new();
    for review in tx.table::<Reviews>().children(issue.id) {
        if matches!(review.state, ReviewState::Pending | ReviewState::Commented)
            || review.user_id == issue.user_id
            || hubdouble_auth::repo_role(tx, review.user_id, repo) < Some(RepoRole::Write)
        {
            continue;
        }
        let key = (review.submitted_at, review.id, review.state);
        let last = reviewers.entry(review.user_id).or_insert(key);
        if (key.0, key.1) > (last.0, last.1) {
            *last = key;
        }
    }
    Some(
        if reviewers
            .values()
            .any(|entry| entry.2 == ReviewState::ChangesRequested)
        {
            "CHANGES_REQUESTED"
        } else if reviewers
            .values()
            .filter(|entry| entry.2 == ReviewState::Approved)
            .count()
            >= required
        {
            "APPROVED"
        } else {
            "REVIEW_REQUIRED"
        },
    )
}

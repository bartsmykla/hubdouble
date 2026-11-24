use std::collections::BTreeMap;

use hubdouble_api_gitdata::branch_rules::{self, Via, flag, parameter};
use hubdouble_auth::Actor;
use hubdouble_git::{Git, ObjectId};
use hubdouble_model::{ApiError, Issue, Pull, Repo, RepoRole, ReviewState, Rule};
use hubdouble_store::{Repos, ReviewThreads, Reviews, Tx};
use serde_json::Value;

fn base_repo<'t>(tx: &'t Tx, p: &Pull) -> Option<&'t Repo> {
    tx.table::<Repos>().get(p.base.repo_id?).map(AsRef::as_ref)
}

#[derive(Default)]
struct Asks {
    approvals: usize,
    threads: bool,
}

impl Asks {
    fn of(rules: &[&Rule]) -> Asks {
        let mut asks = Asks::default();
        for rule in rules.iter().filter(|r| r.rule_type == "pull_request") {
            let count = parameter(rule, "required_approving_review_count")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(0);
            asks.approvals = asks.approvals.max(count);
            asks.threads |= flag(rule, "required_review_thread_resolution");
        }
        asks
    }
}

struct Tally {
    approvals: usize,
    changes: usize,
}

fn tally(tx: &Tx, issue: &Issue, repo: &Repo) -> Tally {
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
    let count = |state| reviewers.values().filter(|entry| entry.2 == state).count();
    Tally {
        approvals: count(ReviewState::Approved),
        changes: count(ReviewState::ChangesRequested),
    }
}

pub(crate) fn review_decision(tx: &Tx, issue: &Issue, p: &Pull) -> Option<&'static str> {
    let repo = base_repo(tx, p)?;
    let asks = Asks::of(&branch_rules::rules(tx, None, repo, &p.base.ref_name, true));
    if asks.approvals == 0 {
        return None;
    }
    let tally = tally(tx, issue, repo);
    Some(if tally.changes > 0 {
        "CHANGES_REQUESTED"
    } else if tally.approvals >= asks.approvals {
        "APPROVED"
    } else {
        "REVIEW_REQUIRED"
    })
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {many}")
    }
}

pub(crate) fn blocks(tx: &Tx, issue: &Issue, p: &Pull, rules: &[&Rule]) -> Vec<String> {
    let Some(repo) = base_repo(tx, p) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let asks = Asks::of(rules);
    if asks.approvals > 0 {
        let tally = tally(tx, issue, repo);
        if tally.changes > 0 {
            out.push(format!(
                "{} by reviewers with write access.",
                plural(
                    tally.changes,
                    "review requesting changes",
                    "reviews requesting changes"
                )
            ));
        } else if tally.approvals < asks.approvals {
            let wanted = if asks.approvals == 1 {
                "1 approving review is".to_owned()
            } else {
                format!("{} approving reviews are", asks.approvals)
            };
            out.push(format!(
                "At least {wanted} required by reviewers with write access."
            ));
        }
    }
    if asks.threads
        && tx
            .table::<ReviewThreads>()
            .children(issue.id)
            .any(|t| !t.is_resolved)
    {
        out.push(
            "A conversation must be resolved before this pull request can be merged.".to_owned(),
        );
    }
    out
}

pub(crate) fn tested(p: &Pull) -> Result<Via, ApiError> {
    Ok(Via::Pull {
        head: p.head.sha.parse()?,
        test_merge: p.merge_commit_sha.as_deref().map(str::parse).transpose()?,
    })
}

pub(crate) fn merge_violations(
    tx: &Tx,
    git: &Git,
    actor: &Actor,
    issue: &Issue,
    (old, new): (ObjectId, ObjectId),
) -> Result<Vec<String>, ApiError> {
    let p = crate::state::pull(issue)?;
    let Some(repo) = base_repo(tx, p) else {
        return Ok(Vec::new());
    };
    let branch = &p.base.ref_name;
    let mut out = branch_rules::violations(
        tx,
        git,
        actor,
        repo,
        branch,
        (Some(old), Some(new)),
        tested(p)?,
    )?;
    let rules = branch_rules::rules(tx, Some(actor), repo, branch, true);
    out.extend(blocks(tx, issue, p, &rules));
    Ok(out)
}

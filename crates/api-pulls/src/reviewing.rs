use std::collections::BTreeMap;

use hubdouble_api_gitdata::branch_rules::{self, Via, diff_state, flag, parameter};
use hubdouble_auth::Actor;
use hubdouble_git::{Git, ObjectId, RefChange};
use hubdouble_model::{ApiError, Issue, Pull, Repo, RepoRole, ReviewState, Rule, Timestamp};
use hubdouble_store::{Counters, Entity, Repos, ReviewThreads, Reviews, Rows, Sequence, Tx, TxMut};
use serde_json::Value;

pub(crate) type LastPushes = Rows<LastPush>;

#[derive(Clone)]
pub(crate) struct LastPush {
    issue_id: i64,
    pusher: i64,
    at: (Timestamp, i64),
}

impl Entity for LastPush {
    fn id(&self) -> i64 {
        self.issue_id
    }
}

fn last_push(tx: &Tx, issue: &Issue) -> (i64, (Timestamp, i64)) {
    tx.table::<LastPushes>()
        .get(issue.id)
        .map_or((issue.user_id, (issue.created_at, 0)), |p| (p.pusher, p.at))
}

fn base_repo<'t>(tx: &'t Tx, p: &Pull) -> Option<&'t Repo> {
    tx.table::<Repos>().get(p.base.repo_id?).map(AsRef::as_ref)
}

fn requires_last_push(rules: &[&Rule]) -> bool {
    rules
        .iter()
        .any(|r| r.rule_type == "pull_request" && flag(r, "require_last_push_approval"))
}

pub(crate) fn pushed(
    tx: &mut TxMut<'_>,
    git: &Git,
    issue: &Issue,
    change: &RefChange,
) -> Result<(), ApiError> {
    let (Some(p), Some(new)) = (issue.pull.as_ref(), change.new) else {
        return Ok(());
    };
    let Some(repo) = base_repo(tx, p) else {
        return Ok(());
    };
    let tracked = requires_last_push(&branch_rules::rules(tx, None, repo, &p.base.ref_name, true));
    if tracked && let Some(old) = change.old {
        let base: ObjectId = p.base.sha.parse()?;
        let repo = issue.repo_id;
        if diff_state(tx, git, repo, base, old)? == diff_state(tx, git, repo, base, new)? {
            return Ok(());
        }
    }
    let at = (git.now(), tx.table::<Counters>().current(Sequence::Id));
    tx.table_mut::<LastPushes>().replace(LastPush {
        issue_id: issue.id,
        pusher: change.sender,
        at,
    })?;
    Ok(())
}

#[derive(Default)]
struct Asks {
    approvals: usize,
    last_push: bool,
    threads: bool,
    any: bool,
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
            asks.last_push |= flag(rule, "require_last_push_approval");
            asks.threads |= flag(rule, "required_review_thread_resolution");
        }
        asks.any = asks.approvals > 0 || asks.last_push;
        asks
    }
}

struct Tally {
    approvals: usize,
    changes: usize,
    approved_since_push: bool,
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
    let (pusher, pushed_at) = last_push(tx, issue);
    let approved = || {
        reviewers
            .iter()
            .filter(|(_, entry)| entry.2 == ReviewState::Approved)
    };
    Tally {
        approvals: approved().count(),
        changes: reviewers
            .values()
            .filter(|entry| entry.2 == ReviewState::ChangesRequested)
            .count(),
        approved_since_push: approved().any(|(&user, entry)| {
            user != pusher && entry.0.is_some_and(|at| (at, entry.1) > pushed_at)
        }),
    }
}

pub(crate) fn review_decision(tx: &Tx, issue: &Issue, p: &Pull) -> Option<&'static str> {
    let repo = base_repo(tx, p)?;
    let asks = Asks::of(&branch_rules::rules(tx, None, repo, &p.base.ref_name, true));
    if !asks.any {
        return None;
    }
    let tally = tally(tx, issue, repo);
    Some(if tally.changes > 0 {
        "CHANGES_REQUESTED"
    } else if tally.approvals >= asks.approvals.max(1)
        && (!asks.last_push || tally.approved_since_push)
    {
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
    if asks.any {
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
        } else if asks.last_push && !tally.approved_since_push {
            out.push("Waiting on 1 reapproval from someone other than the last pusher.".to_owned());
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

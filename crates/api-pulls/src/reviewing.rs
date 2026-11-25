use std::collections::{BTreeMap, HashMap};

use hubdouble_api_gitdata::branch_rules::{self, MergeMethod, Via, diff_state, flag, parameter};
use hubdouble_auth::Actor;
use hubdouble_git::{Git, ObjectId, RefChange};
use hubdouble_model::{
    AccountKind, ApiError, Issue, Pull, Repo, RepoRole, ReviewState, Rule, Team,
};
use hubdouble_store::{
    Accounts, Entity, Repos, ReviewThreads, Reviews, Rows, Table, Teams, Tx, TxMut,
};
use serde_json::Value;

pub(crate) type LastPushes = Rows<LastPush>;

#[derive(Clone)]
pub(crate) struct LastPush {
    issue_id: i64,
    pusher: i64,
    order: u64,
}

impl Entity for LastPush {
    fn id(&self) -> i64 {
        self.issue_id
    }
}

#[derive(Clone, Default)]
pub(crate) struct ReviewOrder {
    last: u64,
    submitted: HashMap<i64, u64>,
}

impl Table for ReviewOrder {}

fn next_order(tx: &mut TxMut<'_>) -> u64 {
    let mut order = tx.table_mut::<ReviewOrder>();
    let order = order.get_mut();
    order.last += 1;
    order.last
}

/// Records that the review `review_id` was submitted now, after every push and submission
/// recorded before it, for the approval of the last push.
pub fn review_submitted(tx: &mut TxMut<'_>, review_id: i64) {
    let order = next_order(tx);
    tx.table_mut::<ReviewOrder>()
        .get_mut()
        .submitted
        .insert(review_id, order);
}

fn last_push(tx: &Tx, issue: &Issue) -> (i64, u64) {
    tx.table::<LastPushes>()
        .get(issue.id)
        .map_or((issue.user_id, 0), |p| (p.pusher, p.order))
}

fn submitted_order(tx: &Tx, review_id: i64) -> u64 {
    tx.table::<ReviewOrder>()
        .submitted
        .get(&review_id)
        .copied()
        .unwrap_or(0)
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
    let order = next_order(tx);
    tx.table_mut::<LastPushes>().replace(LastPush {
        issue_id: issue.id,
        pusher: change.sender,
        order,
    })?;
    Ok(())
}

#[derive(Default)]
struct Asks {
    approvals: usize,
    last_push: bool,
    threads: bool,
    code_owners: bool,
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
            asks.code_owners |= flag(rule, "require_code_owner_review");
        }
        asks
    }

    fn any(&self) -> bool {
        self.approvals > 0 || self.last_push || self.code_owners
    }
}

struct Tally {
    approvals: usize,
    changes: usize,
    approved_since_push: bool,
    approvers: Vec<i64>,
}

fn writes(tx: &Tx, repo: &Repo, user: i64) -> bool {
    hubdouble_auth::repo_role(tx, user, repo) >= Some(RepoRole::Write)
}

fn team_of<'t>(tx: &'t Tx, repo: &Repo, name: &str) -> Option<&'t Team> {
    let (org, slug) = name.split_once('/')?;
    let org = tx.table::<Accounts>().by_login(org)?;
    tx.table::<Teams>()
        .by_slug(org.id, slug)
        .map(AsRef::as_ref)
        .filter(|t| {
            t.repos
                .iter()
                .any(|r| r.repo_id == repo.id && r.role >= RepoRole::Write)
        })
}

fn owner_users(tx: &Tx, repo: &Repo, owner: &str) -> Vec<i64> {
    let accounts = tx.table::<Accounts>();
    let users: Vec<i64> = match owner.strip_prefix('@') {
        Some(name) if name.contains('/') => team_of(tx, repo, name)
            .map_or_default(|t| t.members.iter().map(|m| m.user_id).collect()),
        Some(login) => accounts
            .by_login(login)
            .filter(|a| a.kind != AccountKind::Organization)
            .map_or_default(|a| vec![a.id]),
        None => accounts
            .iter()
            .filter(|a| {
                a.email
                    .as_deref()
                    .is_some_and(|e| e.eq_ignore_ascii_case(owner))
            })
            .map(|a| a.id)
            .collect(),
    };
    users.into_iter().filter(|&u| writes(tx, repo, u)).collect()
}

fn valid_owner(tx: &Tx, repo: &Repo, owner: &str) -> bool {
    match owner.strip_prefix('@') {
        Some(name) if name.contains('/') => team_of(tx, repo, name).is_some(),
        _ => !owner_users(tx, repo, owner).is_empty(),
    }
}

fn unmet_owners(tx: &Tx, issue: &Issue, repo: &Repo, approvers: &[i64]) -> Vec<Vec<String>> {
    crate::merge_state::owner_groups(tx, issue)
        .iter()
        .filter_map(|group| {
            let valid: Vec<&String> = group.iter().filter(|o| valid_owner(tx, repo, o)).collect();
            let met = valid.iter().any(|o| {
                owner_users(tx, repo, o)
                    .iter()
                    .any(|u| approvers.contains(u))
            });
            (!valid.is_empty() && !met).then(|| {
                valid
                    .into_iter()
                    .map(|o| o.trim_start_matches('@').to_owned())
                    .collect()
            })
        })
        .collect()
}

fn either(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} and/or {b}"),
        [rest @ .., last] => format!("{}, and/or {last}", rest.join(", ")),
    }
}

fn owner_blocks(tx: &Tx, issue: &Issue, p: &Pull, rules: &[&Rule]) -> Vec<String> {
    let Some(repo) = base_repo(tx, p) else {
        return Vec::new();
    };
    if !Asks::of(rules).code_owners {
        return Vec::new();
    }
    let tally = tally(tx, issue, repo);
    if tally.changes > 0 {
        return Vec::new();
    }
    unmet_owners(tx, issue, repo, &tally.approvers)
        .iter()
        .map(|names| format!("Waiting on code owner review from {}.", either(names)))
        .collect()
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
        let order = submitted_order(tx, review.id);
        let key = ((order, review.submitted_at, review.id), review.state);
        let last = reviewers.entry(review.user_id).or_insert(key);
        if key.0 > last.0 {
            *last = key;
        }
    }
    let (pusher, push_order) = last_push(tx, issue);
    let approved = || {
        reviewers
            .iter()
            .filter(|(_, entry)| entry.1 == ReviewState::Approved)
    };
    Tally {
        approvals: approved().count(),
        changes: reviewers
            .values()
            .filter(|entry| entry.1 == ReviewState::ChangesRequested)
            .count(),
        approved_since_push: approved()
            .any(|(&user, entry)| user != pusher && (push_order == 0 || entry.0.0 > push_order)),
        approvers: approved().map(|(&user, _)| user).collect(),
    }
}

/// GraphQL `reviewDecision`: `None` when no rule on the base branch asks for reviews.
#[must_use]
pub fn review_decision(tx: &Tx, issue: &Issue, p: &Pull) -> Option<&'static str> {
    let repo = base_repo(tx, p)?;
    let asks = Asks::of(&branch_rules::rules(tx, None, repo, &p.base.ref_name, true));
    if !asks.any() {
        return None;
    }
    let tally = tally(tx, issue, repo);
    Some(if tally.changes > 0 {
        "CHANGES_REQUESTED"
    } else if tally.approvals >= asks.approvals.max(1)
        && (!asks.last_push || tally.approved_since_push)
        && (!asks.code_owners || unmet_owners(tx, issue, repo, &tally.approvers).is_empty())
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
    let mut out = owner_blocks(tx, issue, p, rules);
    out.extend(review_blocks(tx, issue, p, rules));
    out
}

fn review_blocks(tx: &Tx, issue: &Issue, p: &Pull, rules: &[&Rule]) -> Vec<String> {
    let Some(repo) = base_repo(tx, p) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let asks = Asks::of(rules);
    if asks.any() {
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

pub(crate) fn tested(p: &Pull, method: MergeMethod) -> Result<Via, ApiError> {
    Ok(Via::Pull {
        head: p.head.sha.parse()?,
        test_merge: p.merge_commit_sha.as_deref().map(str::parse).transpose()?,
        method,
    })
}

/// The code owner reviews that a merge still waits on, from the owners of the base branch
/// before the merge moves it.
pub(crate) fn owner_violations(
    tx: &Tx,
    actor: &Actor,
    issue: &Issue,
) -> Result<Vec<String>, ApiError> {
    let p = crate::state::pull(issue)?;
    let Some(repo) = base_repo(tx, p) else {
        return Ok(Vec::new());
    };
    let rules = branch_rules::rules(tx, Some(actor), repo, &p.base.ref_name, true);
    Ok(owner_blocks(tx, issue, p, &rules))
}

/// Every rule a merge from `old` to `new` breaks, after the `owners` of
/// [`owner_violations`].
pub(crate) fn merge_violations(
    tx: &Tx,
    git: &Git,
    actor: &Actor,
    issue: &Issue,
    (old, new): (ObjectId, ObjectId),
    (method, owners): (MergeMethod, Vec<String>),
) -> Result<Vec<String>, ApiError> {
    let p = crate::state::pull(issue)?;
    let Some(repo) = base_repo(tx, p) else {
        return Ok(Vec::new());
    };
    let branch = &p.base.ref_name;
    let moves = (Some(old), Some(new));
    let rules = branch_rules::rules(tx, Some(actor), repo, branch, true);
    let mut out = owners;
    out.extend(branch_rules::violations(
        tx,
        git,
        actor,
        repo,
        branch,
        moves,
        tested(p, method)?,
    )?);
    out.extend(review_blocks(tx, issue, p, &rules));
    let name = format!("refs/heads/{branch}");
    for v in branch_rules::push_violations(tx, git, actor, repo, &name, moves)? {
        if !out.contains(&v) {
            out.push(v);
        }
    }
    Ok(out)
}

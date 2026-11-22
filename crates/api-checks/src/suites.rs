use std::sync::Arc;

use http::StatusCode;
use hubdouble_git::{Git, RefChange};
use hubdouble_model::{
    Access, ApiError, CheckRun, CheckSuite, Conclusion, Repo, RepositorySelection, RunStatus,
    Timestamp, node_id,
};
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::{Apps, CheckRuns, CheckSuites, Installations, Repos, Tx, TxMut};
use serde_json::json;

use crate::events::{self, To};
use crate::runs::app_actor;
use crate::view::{self, View, latest, no_commit};
use crate::{invalid, page_object};

const ZERO: &str = "0000000000000000000000000000000000000000";

const PRIORITY: [Conclusion; 9] = [
    Conclusion::ActionRequired,
    Conclusion::Cancelled,
    Conclusion::TimedOut,
    Conclusion::Failure,
    Conclusion::StartupFailure,
    Conclusion::Stale,
    Conclusion::Success,
    Conclusion::Neutral,
    Conclusion::Skipped,
];

fn summary(runs: &[&Arc<CheckRun>]) -> (RunStatus, Option<Conclusion>) {
    if runs.is_empty() {
        (RunStatus::Queued, None)
    } else if runs.iter().all(|r| r.status == RunStatus::Completed) {
        let conclusion = PRIORITY
            .into_iter()
            .find(|c| runs.iter().any(|r| r.conclusion == Some(*c)));
        (RunStatus::Completed, conclusion)
    } else if runs.iter().all(|r| r.status == RunStatus::Queued) {
        (RunStatus::Queued, None)
    } else {
        (RunStatus::InProgress, None)
    }
}

fn branch_at(tx: &Tx, git: &Git, repo: &Repo, sha: &str) -> Option<String> {
    let refs = git.refs(tx, repo.id, "refs/heads/");
    let at = |r: &&hubdouble_git::Ref| r.target.to_string() == sha;
    let default = format!("refs/heads/{}", repo.default_branch);
    refs.iter()
        .filter(at)
        .find(|r| r.name == default)
        .or_else(|| refs.iter().find(at))
        .map(|r| r.name.trim_start_matches("refs/heads/").to_owned())
}

fn insert(
    tx: &mut TxMut<'_>,
    (repo, app): (i64, i64),
    head: (Option<String>, &str),
    (before, after): (Option<String>, Option<String>),
    now: Timestamp,
) -> Result<Arc<CheckSuite>, ApiError> {
    let id = tx.next_id();
    Ok(tx.table_mut::<CheckSuites>().insert(CheckSuite {
        id,
        node_id: node_id::entity(node_id::CHECK_SUITE, id),
        repo_id: repo,
        app_id: app,
        head_branch: head.0,
        head_sha: head.1.to_owned(),
        before,
        after,
        status: RunStatus::Queued,
        conclusion: None,
        rerequestable: true,
        created_at: now,
        updated_at: now,
    })?)
}

fn newest(tx: &Tx, repo: i64, app: i64, sha: &str) -> Option<i64> {
    tx.table::<CheckSuites>()
        .in_repo(repo)
        .filter(|s| s.app_id == app && s.head_sha == sha)
        .map(|s| s.id)
        .max()
}

/// The ID of the App's newest suite on `sha`, made when there is none.
pub fn for_run(
    tx: &mut TxMut<'_>,
    git: &Git,
    repo: &Repo,
    app: i64,
    sha: &str,
    now: Timestamp,
) -> Result<i64, ApiError> {
    if let Some(id) = newest(tx, repo.id, app, sha) {
        return Ok(id);
    }
    let branch = branch_at(tx, git, repo, sha);
    Ok(insert(tx, (repo.id, app), (branch, sha), (None, None), now)?.id)
}

/// Sets the suite's status and conclusion from its latest runs and emits
/// `check_suite.completed` when it has just completed.
pub fn refresh(
    tx: &mut TxMut<'_>,
    git: &Git,
    suite: i64,
    sender: i64,
    now: Timestamp,
) -> Result<(), ApiError> {
    let held = tx
        .table::<CheckSuites>()
        .get(suite)
        .cloned()
        .ok_or_else(ApiError::not_found)?;
    let (status, conclusion) = summary(&latest(tx.table::<CheckRuns>().children(suite)));
    if (status, conclusion) == (held.status, held.conclusion) {
        return Ok(());
    }
    let changed = Arc::new(CheckSuite {
        status,
        conclusion,
        updated_at: now,
        ..(*held).clone()
    });
    tx.table_mut::<CheckSuites>().replace(changed.clone())?;
    if status == RunStatus::Completed {
        events::check_suite(tx, git, &changed, ("completed", sender, To::Subscribers))?;
    }
    Ok(())
}

pub fn reset(tx: &mut TxMut<'_>, suite: i64, now: Timestamp) -> Result<Arc<CheckSuite>, ApiError> {
    let held = tx
        .table::<CheckSuites>()
        .get(suite)
        .cloned()
        .ok_or_else(ApiError::not_found)?;
    let changed = Arc::new(CheckSuite {
        status: RunStatus::Queued,
        conclusion: None,
        updated_at: now,
        ..(*held).clone()
    });
    tx.table_mut::<CheckSuites>().replace(changed.clone())?;
    Ok(changed)
}

/// GitHub makes a suite for every App with `checks: write` on the repository when a branch is
/// pushed, and asks that App alone to run its checks.
pub fn pushed(tx: &mut TxMut<'_>, git: &Git, change: &RefChange) -> Result<(), ApiError> {
    let (Some(branch), Some(new)) = (change.name.strip_prefix("refs/heads/"), change.new) else {
        return Ok(());
    };
    let Some(repo) = tx.table::<Repos>().get(change.repo_id).cloned() else {
        return Ok(());
    };
    let sha = new.to_string();
    let apps = tx.table::<Apps>();
    let asked: Vec<i64> = tx
        .table::<Installations>()
        .iter()
        .filter(|i| {
            i.account_id == repo.owner_id
                && i.suspended_at.is_none()
                && (i.repository_selection == RepositorySelection::All
                    || i.repo_ids.contains(&repo.id))
                && i.permissions.get("checks") >= Some(&Access::Write)
                && apps.get(i.app_id).is_some()
        })
        .map(|i| i.app_id)
        .filter(|&app| newest(tx, repo.id, app, &sha).is_none())
        .collect();
    let now = git.now();
    let before = change
        .old
        .map_or_else(|| ZERO.to_owned(), |old| old.to_string());
    for app in asked {
        let suite = insert(
            tx,
            (repo.id, app),
            (Some(branch.to_owned()), &sha),
            (Some(before.clone()), Some(sha.clone())),
            now,
        )?;
        events::check_suite(
            tx,
            git,
            &suite,
            ("requested", change.sender, To::Owner(app)),
        )?;
    }
    Ok(())
}

fn rest_view<'c>(ctx: &'c Ctx<'_>) -> Result<View<'c>, ApiError> {
    View::new(ctx.tx(), ctx.git(), ctx.urls(), ctx.repo()?)
}

pub fn list_for_ref(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?.id;
    let reference = ctx.param("ref")?;
    let sha = view::commit(ctx.tx(), ctx.git(), repo, reference)
        .ok_or_else(|| no_commit(reference))?
        .to_string();
    let app = ctx
        .query()
        .get("app_id")
        .map(|a| a.parse::<i64>().map_err(|_| invalid("app_id")))
        .transpose()?;
    let name = ctx.query().get("check_name");
    let runs = ctx.tx().table::<CheckRuns>();
    let suites: Vec<_> = ctx
        .tx()
        .table::<CheckSuites>()
        .in_repo(repo)
        .filter(|s| s.head_sha == sha && app.is_none_or(|a| s.app_id == a))
        .filter(|s| name.is_none_or(|n| runs.children(s.id).any(|r| r.name == n)))
        .collect();
    let view = rest_view(ctx)?;
    page_object(ctx, "check_suites", &suites, |suite| view.suite(suite))
}

pub fn rerequest(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let id = ctx.param_i64("check_suite_id")?;
    let repo = ctx.repo()?.id;
    let suite = ctx
        .tx()
        .table::<CheckSuites>()
        .get(id)
        .filter(|s| s.repo_id == repo)
        .cloned()
        .ok_or_else(ApiError::not_found)?;
    let (app, sender) = app_actor(ctx.actor())?;
    if app != suite.app_id {
        return Err(ApiError::forbidden("Invalid app_id"));
    }
    let now = ctx.now();
    let (tx, git) = ctx.tx_mut_and_git()?;
    let suite = reset(tx, suite.id, now)?;
    events::check_suite(tx, git, &suite, ("rerequested", sender, To::Owner(app)))?;
    Ok(Reply::json(&json!({}))?.status(StatusCode::CREATED))
}

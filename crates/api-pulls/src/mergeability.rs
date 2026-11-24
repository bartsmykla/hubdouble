use std::sync::Arc;

use http::{Method, StatusCode};
use hubdouble_admin::{
    AdminHandler, AdminRequest, Registrar, SeedCtx, error_response, json_response,
};
use hubdouble_git::{Git, ReadJobs};
use hubdouble_model::{ApiError, Issue, IssueState};
use hubdouble_render::Urls;
use hubdouble_rest::Params;
use hubdouble_store::{Accounts, Entity, Issues, Repos, Rows, Tx, TxMut};
use serde::Deserialize;

use crate::state;

pub(crate) const DEFAULT_READS: u32 = 2;

pub(crate) type MergeJobs = Rows<MergeJob>;

#[derive(Clone)]
pub(crate) struct MergeJob {
    issue_id: i64,
    reads: u32,
    sender: i64,
}

impl Entity for MergeJob {
    fn id(&self) -> i64 {
        self.issue_id
    }
}

pub(crate) type MergeReads = Rows<RepoReads>;

#[derive(Clone)]
pub(crate) struct RepoReads {
    repo_id: i64,
    reads: u32,
}

impl Entity for RepoReads {
    fn id(&self) -> i64 {
        self.repo_id
    }
}

pub(crate) fn register(r: &mut Registrar<'_>) {
    r.store
        .register_table::<MergeJobs>()
        .register_table::<MergeReads>();
    r.admin.kind("mergeability", seed);
    r.admin.route(
        Method::POST,
        "/repos/{owner}/{repo}/pulls/{pull_number}/mergeability",
        settle_route(),
    );
}

pub(crate) fn pending(tx: &Tx, issue_id: i64) -> bool {
    tx.table::<MergeJobs>().get(issue_id).is_some()
}

pub(crate) fn start(
    tx: &mut TxMut<'_>,
    git: &Git,
    issue: &Issue,
    sender: i64,
) -> Result<(), ApiError> {
    let reads = tx
        .table::<MergeReads>()
        .get(issue.repo_id)
        .map_or(DEFAULT_READS, |r| r.reads);
    if reads == 0 {
        return settle(tx, git, issue.id, false, sender);
    }
    tx.table_mut::<MergeJobs>().replace(MergeJob {
        issue_id: issue.id,
        reads,
        sender,
    })?;
    Ok(())
}

pub(crate) fn read(jobs: &ReadJobs, tx: &Tx, issue: &Issue) {
    if pending(tx, issue.id) {
        jobs.ask(counted, issue.id);
    }
}

fn counted(tx: &mut TxMut<'_>, git: &Git, issue_id: i64) -> Result<(), ApiError> {
    let Some(job) = tx.table::<MergeJobs>().get(issue_id).cloned() else {
        return Ok(());
    };
    if job.reads > 1 {
        tx.table_mut::<MergeJobs>().replace(MergeJob {
            reads: job.reads - 1,
            ..*job
        })?;
        return Ok(());
    }
    settle(tx, git, issue_id, false, job.sender)
}

pub(crate) fn settle(
    tx: &mut TxMut<'_>,
    git: &Git,
    issue_id: i64,
    live_base: bool,
    sender: i64,
) -> Result<(), ApiError> {
    tx.table_mut::<MergeJobs>().remove(issue_id);
    let Some(issue) = tx.table::<Issues>().get(issue_id) else {
        return Ok(());
    };
    if issue.state != IssueState::Open {
        return Ok(());
    }
    let mut issue = Issue::clone(issue);
    let (repo, author) = (issue.repo_id, issue.user_id);
    let p = state::pull_mut(&mut issue)?;
    let (Some(tip), Some(_)) = (
        state::live_tip(git, tx, &p.base)?,
        state::live_tip(git, tx, &p.head)?,
    ) else {
        return Ok(());
    };
    let base = if live_base { tip } else { p.base.sha.parse()? };
    let head = p.head.sha.parse()?;
    state::test_merge(git, tx, (repo, author), p, (base, head))?;
    tx.table_mut::<Issues>().replace(issue.clone())?;
    state::sync_refs_by(tx, git, &issue, sender)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Seed {
    #[serde(alias = "repo")]
    repository: String,
    reads: u32,
}

fn seed(ctx: &mut SeedCtx<'_>, value: serde_json::Value) -> Result<(), ApiError> {
    let Seed { repository, reads } =
        serde_json::from_value(value).map_err(|e| ApiError::unprocessable(e.to_string()))?;
    let unknown = || ApiError::unprocessable(format!("no repository {repository}"));
    let (owner, name) = repository.split_once('/').ok_or_else(unknown)?;
    let repo_id = repo_id(&ctx.tx, owner, name).ok_or_else(unknown)?;
    ctx.tx
        .table_mut::<MergeReads>()
        .replace(RepoReads { repo_id, reads })?;
    Ok(())
}

fn repo_id(tx: &Tx, owner: &str, name: &str) -> Option<i64> {
    let owner = tx.table::<Accounts>().by_login(owner)?;
    tx.table::<Repos>().by_name(owner.id, name).map(|r| r.id)
}

fn settle_route() -> AdminHandler {
    Arc::new(|req: AdminRequest| {
        Box::pin(async move {
            let AdminRequest {
                params, services, ..
            } = req;
            let settled = services
                .store
                .update(|tx| settle_named(tx, &services.git, &params));
            match settled {
                Ok(pull) => json_response(StatusCode::OK, &pull),
                Err(e) => error_response(&e),
            }
        })
    })
}

fn settle_named(
    tx: &mut TxMut<'_>,
    git: &Git,
    params: &Params,
) -> Result<hubdouble_render::PullRequest, ApiError> {
    let owner = params.get("owner").unwrap_or_default();
    let name = params.get("repo").unwrap_or_default();
    let repo = repo_id(tx, owner, name).ok_or_else(ApiError::not_found)?;
    let number = params
        .get("pull_number")
        .and_then(|n| n.parse().ok())
        .ok_or_else(ApiError::not_found)?;
    let issue = state::numbered(tx, repo, number)?;
    let id = issue.id;
    let sender = tx
        .table::<MergeJobs>()
        .get(id)
        .map_or(issue.user_id, |job| job.sender);
    settle(tx, git, id, !pending(tx, id), sender)?;
    let issue = tx
        .table::<Issues>()
        .get(id)
        .ok_or_else(ApiError::not_found)?;
    crate::render::detail(git, tx, &Urls::new(tx.config(), ""), issue)
}

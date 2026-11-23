use std::ops::{Bound, RangeBounds as _};
use std::sync::Arc;

use http::StatusCode;
use hubdouble_model::{ApiError, Conclusion, RunStatus, Timestamp, WorkflowJob, WorkflowRun};
use hubdouble_rest::{Ctx, Query, Registry, Reply};
use hubdouble_store::{Accounts, Entity, Rows, Tx, WorkflowJobs, WorkflowRuns};

use crate::{ACTIONS_READ, ACTIONS_WRITE, lifecycle, views, workflows};

pub(crate) type RunNumbers = Rows<RunNumber>;
pub(crate) type RunContexts = Rows<RunContext>;
pub(crate) type Attempts = Rows<Attempt>;
pub(crate) type JobKeys = Rows<JobKey>;

pub(crate) struct JobKey {
    pub job: i64,
    pub key: String,
    pub needs: Vec<String>,
}

impl Entity for JobKey {
    fn id(&self) -> i64 {
        self.job
    }
}

pub(crate) struct RunNumber {
    pub workflow: i64,
    pub number: i64,
}

impl Entity for RunNumber {
    fn id(&self) -> i64 {
        self.workflow
    }

    fn keep_on_reset(&self) -> bool {
        true
    }
}

pub(crate) struct RunContext {
    pub run: i64,
    pub git_ref: String,
    pub reusable: Option<Reusable>,
    pub environment: Option<String>,
    pub request_token: String,
}

pub(crate) struct Reusable {
    pub workflow_ref: String,
    pub sha: String,
}

impl Entity for RunContext {
    fn id(&self) -> i64 {
        self.run
    }

    fn keys(&self) -> Vec<String> {
        vec![self.request_token.clone()]
    }
}

pub(crate) struct Attempt {
    pub id: i64,
    pub snapshot: Arc<WorkflowRun>,
}

impl Entity for Attempt {
    fn id(&self) -> i64 {
        self.id
    }

    fn repo_id(&self) -> Option<i64> {
        Some(self.snapshot.id)
    }

    fn number(&self) -> Option<i64> {
        Some(self.snapshot.run_attempt)
    }
}

pub(crate) fn register(rest: &mut Registry) {
    rest.handle("actions/list-workflow-runs-for-repo", ACTIONS_READ, list);
    rest.handle("actions/list-workflow-runs", ACTIONS_READ, list_workflow);
    rest.handle("actions/get-workflow-run", ACTIONS_READ, get);
    rest.handle(
        "actions/get-workflow-run-attempt",
        ACTIONS_READ,
        get_attempt,
    );
    rest.handle(
        "actions/list-jobs-for-workflow-run",
        ACTIONS_READ,
        list_jobs,
    );
    rest.handle(
        "actions/list-jobs-for-workflow-run-attempt",
        ACTIONS_READ,
        list_attempt_jobs,
    );
    rest.handle("actions/get-job-for-workflow-run", ACTIONS_READ, get_job);
    rest.handle(
        "actions/download-job-logs-for-workflow-run",
        ACTIONS_READ,
        logs,
    );
    rest.handle("actions/cancel-workflow-run", ACTIONS_WRITE, cancel);
    rest.handle("actions/re-run-workflow-failed-jobs", ACTIONS_WRITE, rerun);
}

pub(crate) fn find(tx: &Tx, id: i64) -> Result<&Arc<WorkflowRun>, ApiError> {
    tx.table::<WorkflowRuns>()
        .get(id)
        .ok_or_else(ApiError::not_found)
}

fn for_repo<'a>(ctx: &'a Ctx<'_>) -> Result<&'a Arc<WorkflowRun>, ApiError> {
    let run = find(ctx.tx(), ctx.param_i64("run_id")?)?;
    if run.repo_id != ctx.repo()?.id {
        return Err(ApiError::not_found());
    }
    Ok(run)
}

fn attempt<'a>(ctx: &'a Ctx<'_>) -> Result<&'a WorkflowRun, ApiError> {
    let run = for_repo(ctx)?;
    let number = ctx.param_i64("attempt_number")?;
    if number == run.run_attempt {
        return Ok(run);
    }
    ctx.tx()
        .table::<Attempts>()
        .by_number(run.id, number)
        .map(|a| a.snapshot.as_ref())
        .ok_or_else(ApiError::not_found)
}

fn get(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    Reply::json(&render(ctx, for_repo(ctx)?)?)
}

fn get_attempt(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    Reply::json(&render(ctx, attempt(ctx)?)?)
}

fn render(ctx: &Ctx<'_>, run: &WorkflowRun) -> Result<hubdouble_render::WorkflowRun, ApiError> {
    views::run(
        ctx.tx(),
        ctx.git(),
        ctx.urls(),
        run,
        ctx.query().get("exclude_pull_requests") == Some("true"),
    )
}

fn list(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    list_runs(ctx, None)
}

fn list_workflow(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let workflow = workflows::find(ctx.tx(), ctx.repo()?.id, ctx.param("workflow_id")?)?;
    list_runs(ctx, Some(workflow.id))
}

const SEARCH_LIMIT: usize = 1000;
const SEARCH_PARAMETERS: [&str; 7] = [
    "actor",
    "branch",
    "check_suite_id",
    "created",
    "event",
    "head_sha",
    "status",
];

fn list_runs(ctx: &Ctx<'_>, workflow: Option<i64>) -> Result<Reply, ApiError> {
    let filters = Filters::new(ctx.tx(), ctx.query())?;
    let Some(created) = filters.created else {
        return crate::page::<hubdouble_render::WorkflowRun>(ctx, "workflow_runs", 0, &[]);
    };
    let (start, limit) = crate::window(ctx);
    if start >= SEARCH_LIMIT
        && SEARCH_PARAMETERS
            .iter()
            .any(|p| ctx.query().get(p).is_some())
    {
        return crate::page::<hubdouble_render::WorkflowRun>(ctx, "workflow_runs", 0, &[]);
    }
    let repo = ctx.repo()?.id;
    let table = ctx.tx().table::<WorkflowRuns>();
    let runs: &mut dyn Iterator<Item = &Arc<WorkflowRun>> = match workflow {
        Some(id) => &mut table.children(id).rev(),
        None => &mut table.in_repo(repo).rev(),
    };
    let mut count = 0;
    let mut page = Vec::new();
    for run in runs.filter(|run| filters.matches(run) && created.contains(&run.created_at)) {
        if count >= start && page.len() < limit {
            page.push(render(ctx, run)?);
        }
        count += 1;
    }
    crate::page(ctx, "workflow_runs", count, &page)
}

struct Filters<'a> {
    event: Option<&'a str>,
    branch: Option<&'a str>,
    sha: Option<&'a str>,
    status: Option<&'a str>,
    actor: ActorFilter,
    suite: Option<i64>,
    created: Option<(Bound<Timestamp>, Bound<Timestamp>)>,
}

enum ActorFilter {
    Any,
    Known(i64),
    Unknown,
}

impl<'a> Filters<'a> {
    fn new(tx: &Tx, query: &'a Query) -> Result<Self, ApiError> {
        let actor = match query.get("actor") {
            None => ActorFilter::Any,
            Some(login) => tx
                .table::<Accounts>()
                .by_login(login)
                .map_or(ActorFilter::Unknown, |account| {
                    ActorFilter::Known(account.id)
                }),
        };
        let suite = query
            .get("check_suite_id")
            .map(|value| {
                value.parse().map_err(|_| {
                    crate::invalid("WorkflowRun", "check_suite_id", "Invalid check suite ID")
                })
            })
            .transpose()?;
        let created = query
            .get("created")
            .map_or(Some((Bound::Unbounded, Bound::Unbounded)), created_range);
        Ok(Self {
            event: query.get("event"),
            branch: query.get("branch"),
            sha: query.get("head_sha"),
            status: query.get("status"),
            actor,
            suite,
            created,
        })
    }

    fn matches(&self, run: &WorkflowRun) -> bool {
        self.event.is_none_or(|event| event == run.event)
            && self
                .branch
                .is_none_or(|branch| run.head_branch.as_deref() == Some(branch))
            && self.sha.is_none_or(|sha| sha == run.head_sha)
            && self.status.is_none_or(|status| {
                status == run.status.as_str()
                    || run.conclusion.map(Conclusion::as_str) == Some(status)
            })
            && match self.actor {
                ActorFilter::Any => true,
                ActorFilter::Known(id) => run.actor_id == id,
                ActorFilter::Unknown => false,
            }
            && self.suite.is_none_or(|id| Some(id) == run.check_suite_id)
    }
}

fn created_range(text: &str) -> Option<(Bound<Timestamp>, Bound<Timestamp>)> {
    use Bound::{Excluded, Included, Unbounded};

    fn date(text: &str) -> Option<(Timestamp, Timestamp)> {
        if text.len() == 10 {
            let start = Timestamp::parse(&format!("{text}T00:00:00Z")).ok()?;
            return Some((start, start.plus_seconds(86_399)));
        }
        let instant = Timestamp::parse(text).ok()?;
        Some((instant, instant))
    }

    if let Some((from, to)) = text.split_once("..") {
        return Some((Included(date(from)?.0), Included(date(to)?.1)));
    }
    for prefix in [">=", "<=", ">", "<"] {
        if let Some(value) = text.strip_prefix(prefix) {
            let (start, end) = date(value)?;
            return Some(match prefix {
                ">=" => (Included(start), Unbounded),
                ">" => (Excluded(end), Unbounded),
                "<=" => (Unbounded, Included(end)),
                _ => (Unbounded, Excluded(start)),
            });
        }
    }
    let (start, end) = date(text)?;
    Some((Included(start), Included(end)))
}

fn list_jobs(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let run = for_repo(ctx)?;
    let choice = match ctx.query().get("filter") {
        None | Some("latest") => Some(run.run_attempt),
        Some("all") => None,
        _ => {
            return Err(crate::invalid(
                "WorkflowJob",
                "filter",
                "Invalid jobs filter",
            ));
        }
    };
    jobs(ctx, run, choice)
}

fn list_attempt_jobs(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let run = attempt(ctx)?;
    jobs(ctx, run, Some(run.run_attempt))
}

fn jobs(ctx: &Ctx<'_>, run: &WorkflowRun, number: Option<i64>) -> Result<Reply, ApiError> {
    let (start, size) = crate::window(ctx);
    let (name, _) = workflows::repo_name(ctx.tx(), ctx.repo()?)?;
    let table = ctx.tx().table::<WorkflowJobs>();
    let mut count = 0;
    let mut rows = Vec::new();
    for job in table
        .children(run.id)
        .filter(|job| number.is_none_or(|n| job.run_attempt == n))
    {
        if count >= start && rows.len() < size {
            rows.push(hubdouble_render::Job::new(ctx.urls(), job, &name));
        }
        count += 1;
    }
    crate::page(ctx, "jobs", count, &rows)
}

fn job<'a>(ctx: &'a Ctx<'_>) -> Result<&'a WorkflowJob, ApiError> {
    let repo = ctx.repo()?.id;
    ctx.tx()
        .table::<WorkflowJobs>()
        .get(ctx.param_i64("job_id")?)
        .filter(|job| job.repo_id == repo)
        .map(Arc::as_ref)
        .ok_or_else(ApiError::not_found)
}

fn get_job(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let (name, _) = workflows::repo_name(ctx.tx(), ctx.repo()?)?;
    Reply::json(&hubdouble_render::Job::new(ctx.urls(), job(ctx)?, &name))
}

fn logs(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let id = job(ctx)?
        .log_blob_id
        .as_deref()
        .ok_or_else(ApiError::not_found)?;
    Reply::redirect(&format!("{}/_blobs/{id}", ctx.tx().config().base_url))
}

fn cancel(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let run = Arc::clone(for_repo(ctx)?);
    if run.status == RunStatus::Completed {
        return Err(ApiError::conflict("The workflow run is already complete"));
    }
    let actor = ctx.actor().account().ok_or_else(ApiError::unauthorized)?.id;
    let now = ctx.now();
    let (tx, git) = ctx.tx_mut_and_git()?;
    lifecycle::complete(tx, git, &run, now, actor, Conclusion::Cancelled)?;
    Reply::json(&serde_json::json!({})).map(|reply| reply.status(StatusCode::ACCEPTED))
}

fn rerun(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let run = Arc::clone(for_repo(ctx)?);
    let actor = ctx.actor().account().ok_or_else(ApiError::unauthorized)?.id;
    let now = ctx.now();
    let (tx, git) = ctx.tx_mut_and_git()?;
    lifecycle::rerun(tx, git, run, actor, now)?;
    Reply::json(&serde_json::json!({})).map(|reply| reply.status(StatusCode::CREATED))
}

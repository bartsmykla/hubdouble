use std::ops::{Bound, RangeBounds as _};
use std::sync::Arc;

use http::StatusCode;
use hubdouble_model::{ApiError, Conclusion, RunStatus, Timestamp, WorkflowJob, WorkflowRun};
use hubdouble_rest::{Ctx, Page, PageLimit, Query, Registry, Reply};
use hubdouble_store::{Accounts, Blobs, Entity, Rows, Tx, WorkflowJobs, WorkflowRuns};

use crate::lifecycle::Rerun;
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
    rest.handle("actions/re-run-workflow", ACTIONS_WRITE, rerun);
    rest.handle(
        "actions/re-run-workflow-failed-jobs",
        ACTIONS_WRITE,
        rerun_failed,
    );
    rest.handle(
        "actions/re-run-job-for-workflow-run",
        ACTIONS_WRITE,
        rerun_job,
    );
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

const RUNS: PageLimit = PageLimit {
    results: 40_000,
    lookahead: 0,
    message: "In order to keep the API fast for everyone, pagination is limited for this resource.",
    documentation_url: "https://docs.github.com/v3/#pagination",
    before_overflow: true,
};
const SEARCH_LIMIT: usize = 1000;
const SEARCH_TOTAL_LIMIT: usize = 2500;
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
    let docs = match workflow {
        Some(_) => {
            "https://docs.github.com/rest/actions/workflow-runs#list-workflow-runs-for-a-workflow"
        }
        None => {
            "https://docs.github.com/rest/actions/workflow-runs#list-workflow-runs-for-a-repository"
        }
    };
    let filters = Filters::new(ctx.tx(), ctx.query(), docs)?;
    let Page { page, per_page } = ctx.page_params_within(&RUNS)?;
    let (start, limit) = ((page - 1).saturating_mul(per_page), per_page);
    let Some(created) = filters.created else {
        return crate::page::<hubdouble_render::WorkflowRun>(ctx, "workflow_runs", 0, &[]);
    };
    let filtered = SEARCH_PARAMETERS
        .iter()
        .any(|p| ctx.query().get(p).is_some());
    if filtered && start >= SEARCH_LIMIT {
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
        if count
            == if filtered {
                SEARCH_TOTAL_LIMIT
            } else {
                RUNS.results
            }
        {
            break;
        }
    }
    crate::page(ctx, "workflow_runs", count, &page)
}

struct Filters<'a> {
    event: Option<&'a str>,
    branch: Option<&'a str>,
    sha: Option<&'a str>,
    status: Option<&'a str>,
    actor: IdFilter,
    suite: IdFilter,
    created: Option<(Bound<Timestamp>, Bound<Timestamp>)>,
}

enum IdFilter {
    Any,
    Known(i64),
    Unknown,
}

impl IdFilter {
    fn matches(&self, id: Option<i64>) -> bool {
        match self {
            IdFilter::Any => true,
            IdFilter::Known(known) => id == Some(*known),
            IdFilter::Unknown => false,
        }
    }
}

impl<'a> Filters<'a> {
    fn new(tx: &Tx, query: &'a Query, docs: &str) -> Result<Self, ApiError> {
        let actor = match query.get("actor") {
            None => IdFilter::Any,
            Some(login) => tx
                .table::<Accounts>()
                .by_login(login)
                .map_or(IdFilter::Unknown, |account| IdFilter::Known(account.id)),
        };
        let suite = match query.get("check_suite_id") {
            None => IdFilter::Any,
            Some(value) if hubdouble_api_checks::leading_integer(value) <= 0 => {
                return Err(
                    ApiError::new(400, format!("invalid check_suite_id `{value}`")).with_docs(docs),
                );
            }
            Some(value) => value.parse().map_or(IdFilter::Unknown, IdFilter::Known),
        };
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
            && self.actor.matches(Some(run.actor_id))
            && self.suite.matches(run.check_suite_id)
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
    let choice = (ctx.query().get("filter") != Some("all")).then_some(run.run_attempt);
    jobs(ctx, run, choice)
}

fn list_attempt_jobs(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let run = attempt(ctx)?;
    jobs(ctx, run, Some(run.run_attempt))
}

fn jobs(ctx: &Ctx<'_>, run: &WorkflowRun, number: Option<i64>) -> Result<Reply, ApiError> {
    let (start, size) = crate::window(ctx)?;
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

const LOG_LINK_SECONDS: i64 = 600;

fn logs(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let id = job(ctx)?
        .log_blob_id
        .as_deref()
        .ok_or_else(ApiError::not_found)?;
    let base = &ctx.tx().config().base_url;
    let expiry = ctx.now().plus_seconds(LOG_LINK_SECONDS);
    let signature = ctx
        .tx()
        .table::<Blobs>()
        .get(id)
        .and_then(|blob| blob.link_signature(id, expiry));
    let link = match signature {
        Some(sig) => {
            let time = |t: Timestamp| t.to_string().replace(':', "%3A");
            let (st, se) = (time(ctx.now().plus_seconds(-5)), time(expiry));
            format!("{base}/_blobs/{id}?rsct=text%2Fplain&se={se}&sig={sig}&sp=r&st={st}")
        }
        None => format!("{base}/_blobs/{id}"),
    };
    Reply::redirect(&link)
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
    rerun_as(ctx, run, Rerun::All)
}

fn rerun_failed(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let run = Arc::clone(for_repo(ctx)?);
    rerun_as(ctx, run, Rerun::Failed)
}

fn rerun_job(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let job = job(ctx)?;
    let (id, run) = (job.id, job.run_id);
    let run = Arc::clone(find(ctx.tx(), run)?);
    rerun_as(ctx, run, Rerun::Job(id))
}

fn rerun_as(ctx: &mut Ctx<'_>, run: Arc<WorkflowRun>, which: Rerun) -> Result<Reply, ApiError> {
    let actor = ctx.actor().account().ok_or_else(ApiError::unauthorized)?.id;
    let now = ctx.now();
    lifecycle::rerun(ctx.tx_mut_and_git()?, run, which, actor, now)?;
    Reply::json(&serde_json::json!({})).map(|reply| reply.status(StatusCode::CREATED))
}

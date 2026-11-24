use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use hubdouble_api_checks::actions as checks;
use hubdouble_api_checks::{AnnotationInput, annotation};
use hubdouble_git::{Git, ObjectId};
use hubdouble_model::{
    Annotation, ApiError, Conclusion, IssueState, JobStep, Repo, RunStatus, Timestamp, WorkflowJob,
    WorkflowRun, node_id,
};
use hubdouble_store::{
    Accounts, Blob, Blobs, Issues, Repos, Tx, TxMut, WorkflowJobs, WorkflowRuns,
};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::configuration::Settings;
use crate::runs::{
    Attempt, Attempts, JobKey, JobKeys, Reusable, RunContext, RunContexts, RunNumber, RunNumbers,
};
use crate::views;
use crate::workflows::{self, Definition, JobDefinition};

pub(crate) struct Dispatch {
    pub workflow_id: i64,
    pub path: String,
    pub name: String,
    pub definition: Arc<Definition>,
    pub actor: i64,
    pub git_ref: String,
    pub sha: ObjectId,
    pub inputs: BTreeMap<String, Value>,
}

pub(crate) fn create(
    tx: &mut TxMut<'_>,
    git: &Git,
    repo: &Repo,
    now: Timestamp,
    info: Dispatch,
) -> Result<Arc<WorkflowRun>, ApiError> {
    let id = tx.next_id();
    let number = tx
        .table::<RunNumbers>()
        .get(info.workflow_id)
        .map_or(1, |row| row.number + 1);
    tx.table_mut::<RunNumbers>().replace(RunNumber {
        workflow: info.workflow_id,
        number,
    })?;
    let head_sha = info.sha.to_string();
    let branch = info
        .git_ref
        .strip_prefix("refs/heads/")
        .or_else(|| info.git_ref.strip_prefix("refs/tags/"));
    let check_suite = tx.next_id();
    let run = WorkflowRun {
        id,
        node_id: node_id::entity(node_id::WORKFLOW_RUN, id),
        repo_id: repo.id,
        workflow_id: info.workflow_id,
        path: info.path,
        display_title: info.name.clone(),
        name: info.name,
        run_number: number,
        run_attempt: 1,
        event: "workflow_dispatch".to_owned(),
        status: RunStatus::Queued,
        conclusion: None,
        head_branch: branch.map(str::to_owned),
        pull_request_numbers: pull_requests(tx, repo, branch, &head_sha),
        head_sha,
        head_repo_id: repo.id,
        actor_id: info.actor,
        triggering_actor_id: info.actor,
        check_suite_id: Some(check_suite),
        inputs: info.inputs,
        created_at: now,
        updated_at: now,
        run_started_at: None,
    };
    let reusable = info
        .definition
        .jobs
        .values()
        .find_map(|job| reusable(tx, git, repo, &run.head_sha, job.uses.as_deref()?));
    let environment = info.definition.jobs.values().find_map(environment);
    checks::open_suite(
        tx,
        checks::Suite {
            id: check_suite,
            repo_id: repo.id,
            head_branch: run.head_branch.clone(),
            head_sha: run.head_sha.clone(),
            created_at: now,
        },
    )?;
    views::dispatch_event(tx, &run, &info.git_ref);
    tx.table_mut::<RunContexts>().insert(RunContext {
        run: id,
        git_ref: info.git_ref,
        reusable,
        environment,
        request_token: Uuid::new_v4().to_string(),
    })?;
    let run = tx.table_mut::<WorkflowRuns>().insert(run)?;
    views::run_event(tx, git, &run, info.actor, "requested")?;
    for (key, job) in &info.definition.jobs {
        let name = job.name.as_ref().unwrap_or(key).clone();
        queue_job(
            (tx, git),
            &run,
            name,
            (key.clone(), job.needs.clone()),
            now,
            info.actor,
        )?;
    }
    if let Some(conclusion) = tx.table::<Settings>().conclusion() {
        return complete(tx, git, &run, now, info.actor, conclusion);
    }
    Ok(run)
}

fn pull_requests(tx: &Tx, repo: &Repo, branch: Option<&str>, sha: &str) -> Vec<i64> {
    tx.table::<Issues>()
        .in_repo(repo.id)
        .filter_map(|issue| {
            let head = &issue.pull.as_ref()?.head;
            (issue.state == IssueState::Open
                && head.repo_id == Some(repo.id)
                && Some(head.ref_name.as_str()) == branch
                && head.sha == sha)
                .then_some(issue.number)
        })
        .collect()
}

fn environment(job: &JobDefinition) -> Option<String> {
    let value = job.environment.as_ref()?;
    value
        .as_str()
        .or_else(|| value.get("name").and_then(Value::as_str))
        .map(str::to_owned)
}

fn reusable(tx: &Tx, git: &Git, repo: &Repo, caller_sha: &str, value: &str) -> Option<Reusable> {
    if let Some(path) = value.strip_prefix("./.github/workflows/") {
        let (caller, _) = workflows::repo_name(tx, repo).ok()?;
        return Some(Reusable {
            workflow_ref: format!("{caller}/.github/workflows/{path}@{caller_sha}"),
            sha: caller_sha.to_owned(),
        });
    }
    let (workflow, git_ref) = value.rsplit_once('@')?;
    let (full_name, _) = workflow.split_once("/.github/workflows/")?;
    let (owner, name) = full_name.split_once('/')?;
    if let Ok(sha) = git_ref.parse::<ObjectId>() {
        return Some(Reusable {
            workflow_ref: value.to_owned(),
            sha: sha.to_string(),
        });
    }
    let account = tx.table::<Accounts>().by_login(owner)?;
    let repo = tx.table::<Repos>().by_name(account.id, name)?;
    let (git_ref, sha) = workflows::resolve(git, tx, repo.id, git_ref)?;
    Some(Reusable {
        workflow_ref: format!("{workflow}@{git_ref}"),
        sha: sha.to_string(),
    })
}

fn new_job(tx: &mut TxMut<'_>, run: &WorkflowRun, name: String, now: Timestamp) -> WorkflowJob {
    let id = tx.next_id();
    WorkflowJob {
        id,
        node_id: node_id::entity(node_id::CHECK_RUN, id),
        repo_id: run.repo_id,
        run_id: run.id,
        run_attempt: run.run_attempt,
        name,
        workflow_name: Some(run.name.clone()),
        head_sha: run.head_sha.clone(),
        head_branch: run.head_branch.clone(),
        status: RunStatus::Queued,
        conclusion: None,
        steps: Vec::new(),
        labels: Vec::new(),
        runner_id: None,
        runner_name: None,
        check_run_id: Some(id),
        log_blob_id: None,
        created_at: now,
        started_at: None,
        completed_at: None,
    }
}

fn queue_job(
    (tx, git): (&mut TxMut<'_>, &Git),
    run: &WorkflowRun,
    name: String,
    (key, needs): (String, Vec<String>),
    now: Timestamp,
    actor: i64,
) -> Result<(), ApiError> {
    let job = new_job(tx, run, name, now);
    tx.table_mut::<JobKeys>().insert(JobKey {
        job: job.id,
        key,
        needs,
    })?;
    save_job((tx, git), job, (actor, Some("queued")), Vec::new())
}

fn carry_job(
    (tx, git): (&mut TxMut<'_>, &Git),
    run: &WorkflowRun,
    old: &WorkflowJob,
    (key, needs): (String, Vec<String>),
    (now, actor): (Timestamp, i64),
) -> Result<(), ApiError> {
    let fresh = new_job(tx, run, old.name.clone(), now);
    let log_blob_id = match old
        .log_blob_id
        .as_deref()
        .and_then(|id| tx.table::<Blobs>().get(id))
    {
        Some(blob) => {
            let blob = Blob::clone(blob);
            let id = Uuid::new_v4().to_string();
            tx.table_mut::<Blobs>().insert(id.clone(), blob);
            Some(id)
        }
        None => None,
    };
    let job = WorkflowJob {
        status: old.status,
        conclusion: old.conclusion,
        steps: old.steps.clone(),
        labels: old.labels.clone(),
        runner_id: old.runner_id,
        runner_name: old.runner_name.clone(),
        log_blob_id,
        started_at: old.started_at,
        completed_at: old.completed_at,
        ..fresh
    };
    tx.table_mut::<JobKeys>().insert(JobKey {
        job: job.id,
        key,
        needs,
    })?;
    save_job((tx, git), job, (actor, None), Vec::new())
}

fn save_job(
    (tx, git): (&mut TxMut<'_>, &Git),
    job: WorkflowJob,
    (actor, action): (i64, Option<&'static str>),
    annotations: Vec<Annotation>,
) -> Result<(), ApiError> {
    if let Some(action) = action {
        views::job_event(tx, &job, actor, action)?;
    }
    let suite = tx
        .table::<WorkflowRuns>()
        .get(job.run_id)
        .and_then(|run| run.check_suite_id);
    if let Some(suite) = suite {
        checks::write_job(tx, git, (&job, suite), annotations, actor)?;
    }
    tx.table_mut::<WorkflowJobs>().replace(job)?;
    Ok(())
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Patch {
    status: Option<RunStatus>,
    conclusion: Option<Conclusion>,
    #[serde(default)]
    jobs: Vec<JobPatch>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JobPatch {
    id: i64,
    status: Option<RunStatus>,
    conclusion: Option<Conclusion>,
    steps: Option<Vec<StepPatch>>,
    logs: Option<String>,
    labels: Option<Vec<String>>,
    runner_id: Option<i64>,
    runner_name: Option<String>,
    #[serde(default)]
    annotations: Vec<AnnotationInput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StepPatch {
    number: i64,
    name: String,
    status: RunStatus,
    conclusion: Option<Conclusion>,
    started_at: Option<Timestamp>,
    completed_at: Option<Timestamp>,
}

pub(crate) fn apply(
    tx: &mut TxMut<'_>,
    git: &Git,
    old: &WorkflowRun,
    now: Timestamp,
    actor: i64,
    patch: Patch,
) -> Result<Arc<WorkflowRun>, ApiError> {
    let (status, conclusion) = state(old.status, old.conclusion, patch.status, patch.conclusion)?;
    for input in patch.jobs {
        apply_job((tx, git), old, now, actor, input)?;
    }
    if status == RunStatus::Completed && old.status != status {
        let conclusion = conclusion.ok_or_else(|| {
            crate::invalid(
                "WorkflowRun",
                "conclusion",
                "A completed run requires a conclusion",
            )
        })?;
        return complete(tx, git, old, now, actor, conclusion);
    }
    let mut run = old.clone();
    if status == RunStatus::InProgress {
        run.run_started_at.get_or_insert(now);
    }
    run.status = status;
    run.conclusion = conclusion;
    run.updated_at = now;
    if old.status != status && matches!(status, RunStatus::InProgress | RunStatus::Requested) {
        views::run_event(tx, git, &run, actor, status.as_str())?;
    }
    store_run((tx, git), run, actor)
}

fn state(
    old: RunStatus,
    old_conclusion: Option<Conclusion>,
    status: Option<RunStatus>,
    conclusion: Option<Conclusion>,
) -> Result<(RunStatus, Option<Conclusion>), ApiError> {
    let status = status.unwrap_or(if conclusion.is_some() {
        RunStatus::Completed
    } else {
        old
    });
    let conclusion = conclusion.or(old_conclusion);
    if (old == RunStatus::Completed || old == RunStatus::InProgress && status == RunStatus::Queued)
        && status != old
    {
        return Err(crate::invalid(
            "Actions",
            "status",
            "Invalid status transition",
        ));
    }
    if (status == RunStatus::Completed) != conclusion.is_some()
        || old == RunStatus::Completed && conclusion != old_conclusion
    {
        return Err(crate::invalid(
            "Actions",
            "conclusion",
            "Invalid conclusion for this status",
        ));
    }
    Ok((status, conclusion))
}

fn apply_job(
    (tx, git): (&mut TxMut<'_>, &Git),
    run: &WorkflowRun,
    now: Timestamp,
    actor: i64,
    patch: JobPatch,
) -> Result<(), ApiError> {
    let old = tx
        .table::<WorkflowJobs>()
        .get(patch.id)
        .filter(|job| job.run_id == run.id && job.run_attempt == run.run_attempt)
        .ok_or_else(ApiError::not_found)?;
    let (status, conclusion) = state(old.status, old.conclusion, patch.status, patch.conclusion)?;
    if !matches!(
        status,
        RunStatus::Queued | RunStatus::Waiting | RunStatus::InProgress | RunStatus::Completed
    ) || conclusion.is_some_and(|c| {
        !matches!(
            c,
            Conclusion::Success
                | Conclusion::Failure
                | Conclusion::Cancelled
                | Conclusion::Skipped
                | Conclusion::TimedOut
        )
    }) {
        return Err(crate::invalid("WorkflowJob", "status", "Invalid job state"));
    }
    let action = (status != old.status).then(|| status.as_str());
    let mut job = WorkflowJob::clone(old);
    if let Some(steps) = patch.steps {
        job.steps = steps
            .into_iter()
            .map(|step| step.into_step(now))
            .collect::<Result<_, _>>()?;
    }
    set_job_state(&mut job, status, conclusion, now);
    if let Some(labels) = patch.labels {
        job.labels = labels;
    }
    job.runner_id = patch.runner_id.or(job.runner_id);
    job.runner_name = patch.runner_name.or(job.runner_name);
    if let Some(logs) = patch.logs {
        let id = Uuid::new_v4().to_string();
        let mut blobs = tx.table_mut::<Blobs>();
        if let Some(old) = &job.log_blob_id {
            blobs.remove(old);
        }
        blobs.insert(
            id.clone(),
            Blob {
                content_type: "text/plain; charset=utf-8".to_owned(),
                data: logs.into(),
                link_key: Some(Uuid::new_v4().to_string().into()),
            },
        );
        job.log_blob_id = Some(id);
    }
    let annotations = patch
        .annotations
        .into_iter()
        .map(annotation)
        .collect::<Result<_, _>>()?;
    save_job((tx, git), job, (actor, action), annotations)
}

impl StepPatch {
    fn into_step(self, now: Timestamp) -> Result<JobStep, ApiError> {
        let (_, conclusion) = state(RunStatus::Queued, None, Some(self.status), self.conclusion)?;
        let (started_at, completed_at) = match self.status {
            RunStatus::Queued => (None, None),
            RunStatus::InProgress => (self.started_at.or(Some(now)), None),
            RunStatus::Completed => (
                self.started_at.or(Some(now)),
                self.completed_at.or(Some(now)),
            ),
            _ => {
                return Err(crate::invalid(
                    "WorkflowJob",
                    "steps",
                    "Invalid step status",
                ));
            }
        };
        Ok(JobStep {
            number: self.number,
            name: self.name,
            status: self.status,
            conclusion,
            started_at,
            completed_at,
        })
    }
}

fn set_job_state(
    job: &mut WorkflowJob,
    status: RunStatus,
    conclusion: Option<Conclusion>,
    now: Timestamp,
) {
    job.status = status;
    job.conclusion = conclusion;
    if matches!(status, RunStatus::InProgress | RunStatus::Completed) {
        job.started_at.get_or_insert(now);
    }
    if job.status == RunStatus::Completed {
        job.completed_at.get_or_insert(now);
        for step in job
            .steps
            .iter_mut()
            .filter(|step| step.status != RunStatus::Completed)
        {
            step.status = RunStatus::Completed;
            step.conclusion = conclusion;
            step.started_at.get_or_insert(now);
            step.completed_at.get_or_insert(now);
        }
    }
}

pub(crate) fn complete(
    tx: &mut TxMut<'_>,
    git: &Git,
    old: &WorkflowRun,
    now: Timestamp,
    actor: i64,
    conclusion: Conclusion,
) -> Result<Arc<WorkflowRun>, ApiError> {
    let unfinished: Vec<_> = tx
        .table::<WorkflowJobs>()
        .children(old.id)
        .filter(|job| job.run_attempt == old.run_attempt && job.status != RunStatus::Completed)
        .map(|job| WorkflowJob::clone(job))
        .collect();
    let job_conclusion = match conclusion {
        Conclusion::Success
        | Conclusion::Failure
        | Conclusion::Cancelled
        | Conclusion::Skipped
        | Conclusion::TimedOut => conclusion,
        _ => Conclusion::Skipped,
    };
    for mut job in unfinished {
        set_job_state(&mut job, RunStatus::Completed, Some(job_conclusion), now);
        save_job((tx, git), job, (actor, Some("completed")), Vec::new())?;
    }
    let mut run = old.clone();
    run.status = RunStatus::Completed;
    run.conclusion = Some(conclusion);
    run.updated_at = now;
    run.run_started_at.get_or_insert(now);
    views::run_event(tx, git, &run, actor, "completed")?;
    store_run((tx, git), run, actor)
}

fn store_run(
    (tx, git): (&mut TxMut<'_>, &Git),
    run: WorkflowRun,
    actor: i64,
) -> Result<Arc<WorkflowRun>, ApiError> {
    let run = Arc::new(run);
    tx.table_mut::<WorkflowRuns>().replace(Arc::clone(&run))?;
    if let Some(suite) = run.check_suite_id {
        checks::follow_run(
            tx,
            git,
            suite,
            (run.status, run.conclusion),
            (actor, run.updated_at),
        )?;
    }
    Ok(run)
}

const RERUN_WINDOW_SECONDS: i64 = 30 * 24 * 60 * 60;
const MAX_RERUNS: i64 = 50;

#[derive(Clone, Copy)]
pub(crate) enum Rerun {
    All,
    Failed,
    Job(i64),
}

pub(crate) fn rerun(
    (tx, git): (&mut TxMut<'_>, &Git),
    old: Arc<WorkflowRun>,
    which: Rerun,
    actor: i64,
    now: Timestamp,
) -> Result<(), ApiError> {
    if now > old.created_at.plus_seconds(RERUN_WINDOW_SECONDS) {
        return Err(ApiError::forbidden(
            "Unable to retry this workflow run because it was created over a month ago",
        )
        .with_docs("https://docs.github.com/rest/actions/workflow-runs#re-run-a-workflow"));
    }
    if old.status != RunStatus::Completed {
        return Err(ApiError::forbidden("The workflow run is not complete"));
    }
    let keys = tx.table::<JobKeys>();
    let key_of = |id: i64| {
        keys.get(id)
            .map_or_default(|row| (row.key.clone(), row.needs.clone()))
    };
    let asked = match which {
        Rerun::Job(id) => Some(key_of(id).0),
        Rerun::All | Rerun::Failed => None,
    };
    let mut jobs: Vec<_> = tx
        .table::<WorkflowJobs>()
        .children(old.id)
        .filter(|job| job.run_attempt == old.run_attempt)
        .map(|job| {
            let (key, needs) = key_of(job.id);
            let chosen = match which {
                Rerun::All => true,
                Rerun::Failed => matches!(
                    job.conclusion,
                    Some(Conclusion::Failure | Conclusion::Cancelled | Conclusion::TimedOut)
                ),
                Rerun::Job(_) => asked.as_ref() == Some(&key),
            };
            (chosen, Arc::clone(job), key, needs)
        })
        .collect();
    if !jobs.iter().any(|(chosen, ..)| *chosen) {
        return Err(match which {
            Rerun::Failed => ApiError::forbidden("The workflow run has no failed jobs"),
            Rerun::All | Rerun::Job(_) => ApiError::not_found(),
        });
    }
    let mut chosen: BTreeSet<String> = jobs
        .iter()
        .filter(|(chosen, ..)| *chosen)
        .map(|(_, _, key, _)| key.clone())
        .collect();
    while let Some(job) = jobs.iter_mut().find(|(failed, _, key, needs)| {
        !*failed && !chosen.contains(key) && needs.iter().any(|need| chosen.contains(need))
    }) {
        job.0 = true;
        chosen.insert(job.2.clone());
    }
    let mut run = WorkflowRun::clone(&old);
    run.run_attempt += 1;
    run.status = RunStatus::Queued;
    run.conclusion = None;
    run.triggering_actor_id = actor;
    run.updated_at = now;
    run.run_started_at = Some(now);
    let archive = Attempt {
        id: tx.next_id(),
        snapshot: old,
    };
    tx.table_mut::<Attempts>().insert(archive)?;
    let run = store_run((tx, git), run, actor)?;
    if run.run_attempt > MAX_RERUNS + 1 {
        complete(tx, git, &run, now, actor, Conclusion::Failure)?;
        return Ok(());
    }
    for (rerun, old, key, needs) in jobs {
        if rerun {
            queue_job((tx, git), &run, old.name.clone(), (key, needs), now, actor)?;
        } else {
            carry_job((tx, git), &run, &old, (key, needs), (now, actor))?;
        }
    }
    if let Some(conclusion) = tx.table::<Settings>().conclusion() {
        complete(tx, git, &run, now, actor, conclusion)?;
    }
    Ok(())
}

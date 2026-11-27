use hubdouble_model as m;
use serde::Serialize;

use crate::{PullRequestMinimal, Repository, SimpleCommit, SimpleUser, Urls};

/// GitHub's `workflow-run`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkflowRun {
    pub id: i64,
    pub name: Option<String>,
    pub node_id: String,
    pub check_suite_id: Option<i64>,
    pub check_suite_node_id: Option<String>,
    pub head_branch: Option<String>,
    pub head_sha: String,
    pub path: String,
    pub run_number: i64,
    pub run_attempt: i64,
    pub referenced_workflows: Vec<serde_json::Value>,
    pub event: String,
    pub status: Option<&'static str>,
    pub conclusion: Option<&'static str>,
    pub workflow_id: i64,
    pub url: String,
    pub html_url: String,
    pub pull_requests: Vec<PullRequestMinimal>,
    pub created_at: m::Timestamp,
    pub updated_at: m::Timestamp,
    pub actor: Option<SimpleUser>,
    pub triggering_actor: Option<SimpleUser>,
    pub run_started_at: Option<m::Timestamp>,
    pub jobs_url: String,
    pub logs_url: String,
    pub check_suite_url: String,
    pub artifacts_url: String,
    pub cancel_url: String,
    pub rerun_url: String,
    pub previous_attempt_url: Option<String>,
    pub workflow_url: String,
    pub head_commit: Option<SimpleCommit>,
    pub repository: Repository,
    pub head_repository: Repository,
    pub head_repository_id: i64,
    pub display_title: String,
}

impl WorkflowRun {
    #[must_use]
    pub fn new(
        urls: &Urls,
        r: &m::WorkflowRun,
        repository: Repository,
        head_repository: Repository,
    ) -> WorkflowRun {
        let base = repository.urls.url.clone();
        let api = format!("{base}/actions/runs/{}", r.id);
        WorkflowRun {
            id: r.id,
            name: Some(r.name.clone()),
            node_id: r.node_id.clone(),
            check_suite_id: r.check_suite_id,
            check_suite_node_id: r
                .check_suite_id
                .map(|id| m::node_id::entity(m::node_id::CHECK_SUITE, id)),
            head_branch: r.head_branch.clone(),
            head_sha: r.head_sha.clone(),
            path: r.path.clone(),
            run_number: r.run_number,
            run_attempt: r.run_attempt,
            referenced_workflows: Vec::new(),
            event: r.event.clone(),
            status: Some(r.status.as_str()),
            conclusion: r.conclusion.map(m::Conclusion::as_str),
            workflow_id: r.workflow_id,
            html_url: format!(
                "{}/{}/actions/runs/{}",
                urls.web, repository.full_name, r.id
            ),
            pull_requests: Vec::new(),
            created_at: r.created_at,
            updated_at: r.updated_at,
            actor: None,
            triggering_actor: None,
            run_started_at: r.run_started_at,
            jobs_url: format!("{api}/jobs"),
            logs_url: format!("{api}/logs"),
            check_suite_url: format!(
                "{base}/check-suites/{}",
                r.check_suite_id.unwrap_or_default()
            ),
            artifacts_url: format!("{api}/artifacts"),
            cancel_url: format!("{api}/cancel"),
            rerun_url: format!("{api}/rerun"),
            previous_attempt_url: (r.run_attempt > 1)
                .then(|| format!("{api}/attempts/{}", r.run_attempt - 1)),
            workflow_url: format!("{base}/actions/workflows/{}", r.workflow_id),
            url: api,
            head_commit: None,
            head_repository_id: head_repository.id,
            repository,
            head_repository,
            display_title: r.display_title.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobStep {
    pub status: &'static str,
    pub conclusion: Option<&'static str>,
    pub name: String,
    pub number: i64,
    pub started_at: Option<m::Timestamp>,
    pub completed_at: Option<m::Timestamp>,
}

/// GitHub's `job`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Job {
    pub id: i64,
    pub run_id: i64,
    pub run_url: String,
    pub run_attempt: i64,
    pub node_id: String,
    pub head_sha: String,
    pub url: String,
    pub html_url: Option<String>,
    pub status: &'static str,
    pub conclusion: Option<&'static str>,
    pub created_at: m::Timestamp,
    pub started_at: m::Timestamp,
    pub completed_at: Option<m::Timestamp>,
    pub name: String,
    pub steps: Vec<JobStep>,
    pub check_run_url: String,
    pub labels: Vec<String>,
    pub runner_id: Option<i64>,
    pub runner_name: Option<String>,
    pub runner_group_id: Option<i64>,
    pub runner_group_name: Option<String>,
    pub workflow_name: Option<String>,
    pub head_branch: Option<String>,
}

impl Job {
    #[must_use]
    pub fn new(urls: &Urls, j: &m::WorkflowJob, full_name: &str) -> Job {
        let repo = format!("{}/repos/{full_name}", urls.api);
        Job {
            id: j.id,
            run_id: j.run_id,
            run_url: format!("{repo}/actions/runs/{}", j.run_id),
            run_attempt: j.run_attempt,
            node_id: j.node_id.clone(),
            head_sha: j.head_sha.clone(),
            url: format!("{repo}/actions/jobs/{}", j.id),
            html_url: Some(format!(
                "{}/{full_name}/actions/runs/{}/job/{}",
                urls.web, j.run_id, j.id
            )),
            status: j.status.as_str(),
            conclusion: j.conclusion.map(m::Conclusion::as_str),
            created_at: j.created_at,
            started_at: j.started_at.unwrap_or(j.created_at),
            completed_at: j.completed_at,
            name: j.name.clone(),
            steps: j
                .steps
                .iter()
                .map(|s| JobStep {
                    status: s.status.as_str(),
                    conclusion: s.conclusion.map(m::Conclusion::as_str),
                    name: s.name.clone(),
                    number: s.number,
                    started_at: s.started_at,
                    completed_at: s.completed_at,
                })
                .collect(),
            check_run_url: format!("{repo}/check-runs/{}", j.check_run_id.unwrap_or(j.id)),
            labels: j.labels.clone(),
            runner_id: j.runner_id,
            runner_name: j.runner_name.clone(),
            runner_group_id: j.runner_group_id,
            runner_group_name: j.runner_group_name.clone(),
            workflow_name: j.workflow_name.clone(),
            head_branch: j.head_branch.clone(),
        }
    }
}

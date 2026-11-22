use hubdouble_model as m;
use serde::Serialize;

use crate::{Integration, PullRequestMinimal, Repository, SimpleUser, Urls};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommitPerson {
    pub name: String,
    pub email: String,
}

/// GitHub's `simple-commit`, the `head_commit` of check suites and workflow runs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SimpleCommit {
    pub id: String,
    pub tree_id: String,
    pub message: String,
    pub timestamp: m::Timestamp,
    pub author: Option<CommitPerson>,
    pub committer: Option<CommitPerson>,
}

/// GitHub's `check-suite`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CheckSuite {
    pub id: i64,
    pub node_id: String,
    pub head_branch: Option<String>,
    pub head_sha: String,
    pub status: Option<&'static str>,
    pub conclusion: Option<&'static str>,
    pub url: Option<String>,
    pub before: Option<String>,
    pub after: Option<String>,
    pub pull_requests: Vec<PullRequestMinimal>,
    pub app: Option<Integration>,
    pub repository: Repository,
    pub created_at: Option<m::Timestamp>,
    pub updated_at: Option<m::Timestamp>,
    pub head_commit: Option<SimpleCommit>,
    pub latest_check_runs_count: i64,
    pub check_runs_url: String,
    pub rerequestable: bool,
    pub runs_rerequestable: bool,
}

impl CheckSuite {
    #[must_use]
    pub fn new(s: &m::CheckSuite, repository: Repository, app: Option<Integration>) -> CheckSuite {
        let api = format!("{}/check-suites/{}", repository.urls.url, s.id);
        CheckSuite {
            id: s.id,
            node_id: s.node_id.clone(),
            head_branch: s.head_branch.clone(),
            head_sha: s.head_sha.clone(),
            status: Some(s.status.as_str()),
            conclusion: s.conclusion.map(m::Conclusion::as_str),
            check_runs_url: format!("{api}/check-runs"),
            url: Some(api),
            before: s.before.clone(),
            after: s.after.clone(),
            pull_requests: Vec::new(),
            app,
            repository,
            created_at: Some(s.created_at),
            updated_at: Some(s.updated_at),
            head_commit: None,
            latest_check_runs_count: 0,
            rerequestable: s.rerequestable,
            runs_rerequestable: s.rerequestable,
        }
    }
}

/// The `check_suite` member of a check run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CheckSuiteRef {
    pub id: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CheckOutput {
    pub title: Option<String>,
    pub summary: Option<String>,
    pub text: Option<String>,
    pub annotations_count: usize,
    pub annotations_url: String,
}

/// GitHub's `check-run`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CheckRun {
    pub id: i64,
    pub head_sha: String,
    pub node_id: String,
    pub external_id: Option<String>,
    pub url: String,
    pub html_url: Option<String>,
    pub details_url: Option<String>,
    pub status: &'static str,
    pub conclusion: Option<&'static str>,
    pub started_at: Option<m::Timestamp>,
    pub completed_at: Option<m::Timestamp>,
    pub output: CheckOutput,
    pub name: String,
    pub check_suite: Option<CheckSuiteRef>,
    pub app: Option<Integration>,
    pub pull_requests: Vec<PullRequestMinimal>,
}

impl CheckRun {
    #[must_use]
    pub fn new(
        urls: &Urls,
        r: &m::CheckRun,
        full_name: &str,
        app: Option<Integration>,
    ) -> CheckRun {
        let api = format!("{}/repos/{full_name}/check-runs/{}", urls.api, r.id);
        CheckRun {
            id: r.id,
            head_sha: r.head_sha.clone(),
            node_id: r.node_id.clone(),
            external_id: r.external_id.clone(),
            html_url: Some(format!("{}/{full_name}/runs/{}", urls.web, r.id)),
            details_url: r.details_url.clone(),
            status: r.status.as_str(),
            conclusion: r.conclusion.map(m::Conclusion::as_str),
            started_at: r.started_at,
            completed_at: r.completed_at,
            output: CheckOutput {
                title: r.output.title.clone(),
                summary: r.output.summary.clone(),
                text: r.output.text.clone(),
                annotations_count: r.output.annotations.len(),
                annotations_url: format!("{api}/annotations"),
            },
            url: api,
            name: r.name.clone(),
            check_suite: Some(CheckSuiteRef {
                id: r.check_suite_id,
            }),
            app,
            pull_requests: Vec::new(),
        }
    }
}

/// GitHub's `status`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Status {
    pub url: String,
    pub avatar_url: Option<String>,
    pub id: i64,
    pub node_id: String,
    pub state: &'static str,
    pub description: Option<String>,
    pub target_url: Option<String>,
    pub context: String,
    pub created_at: m::Timestamp,
    pub updated_at: m::Timestamp,
    pub creator: Option<SimpleUser>,
}

impl Status {
    #[must_use]
    pub fn new(urls: &Urls, s: &m::Status, full_name: &str, creator: Option<SimpleUser>) -> Status {
        Status {
            url: format!("{}/repos/{full_name}/statuses/{}", urls.api, s.sha),
            avatar_url: creator.as_ref().map(|c| c.avatar_url.clone()),
            id: s.id,
            node_id: s.node_id.clone(),
            state: s.state.as_str(),
            description: s.description.clone(),
            target_url: s.target_url.clone(),
            context: s.context.clone(),
            created_at: s.created_at,
            updated_at: s.updated_at,
            creator,
        }
    }
}

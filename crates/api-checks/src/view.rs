use std::collections::BTreeMap;
use std::sync::Arc;

use hubdouble_git::{Git, ObjectData, ObjectId};
use hubdouble_model::{Account, ApiError, CheckRun, CheckSuite, IssueState, Repo, Status};
use hubdouble_render::{
    self as render, CommitPerson, Integration, PullRequestMinimal, PullRequestMinimalRef,
    PullRequestMinimalRepo, Repository, SimpleCommit, SimpleUser, Urls, escape,
};
use hubdouble_store::{Accounts, Apps, CheckRuns, Issues, Repos, Statuses, Tx};

use crate::actions;

pub fn commit(tx: &Tx, git: &Git, repo: i64, reference: &str) -> Option<ObjectId> {
    let object = git.expression(tx, repo, reference).ok()?;
    git.peel_to_commit(tx, repo, object.id).ok()
}

pub fn no_commit(reference: &str) -> ApiError {
    ApiError::unprocessable(format!("No commit found for SHA: {reference}"))
}

/// The newest run per suite and name, as GitHub's `filter=latest` and the rollup count them.
pub fn latest<'t>(runs: impl Iterator<Item = &'t Arc<CheckRun>>) -> Vec<&'t Arc<CheckRun>> {
    let mut newest: BTreeMap<(i64, &str), &Arc<CheckRun>> = BTreeMap::new();
    for run in runs {
        newest
            .entry((run.check_suite_id, run.name.as_str()))
            .and_modify(|held| {
                if run.id > held.id {
                    *held = run;
                }
            })
            .or_insert(run);
    }
    newest.into_values().collect()
}

/// GitHub Actions re-runs a job only through the Actions API, so its suites have no re-runnable
/// check runs.
pub fn runs_rerequestable(suite: &CheckSuite) -> bool {
    suite.rerequestable && suite.app_id != actions::APP_ID
}

pub fn runs_on<'t>(tx: &'t Tx, repo: i64, sha: &'t str) -> impl Iterator<Item = &'t Arc<CheckRun>> {
    tx.table::<CheckRuns>()
        .in_repo(repo)
        .filter(move |run| run.head_sha == sha)
}

/// Every status on `sha`, newest first.
pub fn statuses_on<'t>(tx: &'t Tx, repo: i64, sha: &str) -> Vec<&'t Arc<Status>> {
    let mut statuses: Vec<_> = tx
        .table::<Statuses>()
        .in_repo(repo)
        .filter(|status| status.sha == sha)
        .collect();
    statuses.sort_unstable_by_key(|status| std::cmp::Reverse(status.id));
    statuses
}

/// The newest status per context, in the order GitHub's combined status lists them.
pub fn latest_statuses<'t>(statuses: &[&'t Arc<Status>]) -> Vec<&'t Arc<Status>> {
    let mut seen = std::collections::BTreeSet::new();
    let mut latest: Vec<_> = statuses
        .iter()
        .copied()
        .filter(|status| seen.insert(status.context.as_str()))
        .collect();
    latest.sort_unstable_by_key(|status| status.id);
    latest
}

pub struct View<'a> {
    pub tx: &'a Tx,
    pub git: &'a Git,
    pub urls: &'a Urls,
    pub repo: &'a Repo,
    owner: &'a Account,
    pub name: String,
}

impl<'a> View<'a> {
    pub fn new(tx: &'a Tx, git: &'a Git, urls: &'a Urls, repo: &'a Repo) -> Result<Self, ApiError> {
        let owner = tx
            .table::<Accounts>()
            .get(repo.owner_id)
            .ok_or_else(ApiError::not_found)?;
        Ok(View {
            tx,
            git,
            urls,
            repo,
            owner,
            name: format!("{}/{}", escape(&owner.login), escape(&repo.name)),
        })
    }

    pub fn of(tx: &'a Tx, git: &'a Git, urls: &'a Urls, repo: i64) -> Result<Self, ApiError> {
        let repo = tx
            .table::<Repos>()
            .get(repo)
            .ok_or_else(ApiError::not_found)?;
        View::new(tx, git, urls, repo)
    }

    pub fn repository(&self) -> Repository {
        Repository::new(self.urls, self.repo, self.owner)
    }

    pub fn user(&self, id: i64) -> Option<SimpleUser> {
        self.tx
            .table::<Accounts>()
            .get(id)
            .map(|account| SimpleUser::new(self.urls, account))
    }

    pub fn app(&self, id: i64) -> Option<Integration> {
        let Some(app) = self.tx.table::<Apps>().get(id) else {
            return (id == actions::APP_ID).then(|| actions::app(self.urls));
        };
        let owner = self.tx.table::<Accounts>().get(app.owner_id)?;
        Some(Integration::new(self.urls, app, owner))
    }

    pub fn pulls(&self, sha: &str) -> Vec<PullRequestMinimal> {
        let repo = PullRequestMinimalRepo {
            id: self.repo.id,
            url: format!("{}/repos/{}", self.urls.api, self.name),
            name: self.repo.name.clone(),
        };
        self.tx
            .table::<Issues>()
            .in_repo(self.repo.id)
            .filter(|issue| issue.state == IssueState::Open)
            .filter_map(|issue| Some((issue, issue.pull.as_ref()?)))
            .filter(|(_, pull)| pull.head.sha == sha && pull.head.repo_id == Some(self.repo.id))
            .map(|(issue, pull)| PullRequestMinimal {
                id: pull.id,
                number: issue.number,
                url: format!(
                    "{}/repos/{}/pulls/{}",
                    self.urls.api, self.name, issue.number
                ),
                head: PullRequestMinimalRef {
                    ref_name: pull.head.ref_name.clone(),
                    sha: pull.head.sha.clone(),
                    repo: repo.clone(),
                },
                base: PullRequestMinimalRef {
                    ref_name: pull.base.ref_name.clone(),
                    sha: pull.base.sha.clone(),
                    repo: repo.clone(),
                },
            })
            .collect()
    }

    pub fn run(&self, run: &CheckRun) -> render::CheckRun {
        self.run_with(run, self.app(run.app_id), self.pulls(&run.head_sha))
    }

    /// Renders runs of one head with the App and pull requests already rendered, so a list
    /// renders each once.
    pub fn run_with(
        &self,
        run: &CheckRun,
        app: Option<Integration>,
        pulls: Vec<PullRequestMinimal>,
    ) -> render::CheckRun {
        let mut view = render::CheckRun::new(self.urls, run, &self.name, app);
        if run.app_id == actions::APP_ID {
            view.html_url.clone_from(&run.details_url);
        }
        view.pull_requests = pulls;
        view
    }

    pub fn head_commit(&self, sha: &str) -> Option<SimpleCommit> {
        let id = sha.parse().ok()?;
        let ObjectData::Commit(commit) = self.git.object(self.tx, self.repo.id, id).ok()?.data
        else {
            return None;
        };
        Some(SimpleCommit {
            id: sha.to_owned(),
            tree_id: commit.tree.to_string(),
            message: commit.message.clone(),
            timestamp: commit.committer.time,
            author: Some(CommitPerson {
                name: commit.author.name.clone(),
                email: commit.author.email.clone(),
            }),
            committer: Some(CommitPerson {
                name: commit.committer.name.clone(),
                email: commit.committer.email.clone(),
            }),
        })
    }

    pub fn suite(&self, suite: &CheckSuite) -> render::CheckSuite {
        let mut view = render::CheckSuite::new(suite, self.repository(), self.app(suite.app_id));
        view.pull_requests = self.pulls(&suite.head_sha);
        view.runs_rerequestable = runs_rerequestable(suite);
        view.latest_check_runs_count =
            i64::try_from(latest(self.tx.table::<CheckRuns>().children(suite.id)).len())
                .unwrap_or(i64::MAX);
        view.head_commit = self.head_commit(&suite.head_sha);
        view
    }

    pub fn status(&self, status: &Status) -> render::Status {
        render::Status::new(self.urls, status, &self.name, self.user(status.creator_id))
    }
}

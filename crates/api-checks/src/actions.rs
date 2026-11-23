use std::sync::Arc;

use hubdouble_git::Git;
use hubdouble_model::{
    Access, Account, AccountKind, Annotation, ApiError, CheckOutput, CheckRun, CheckSuite,
    Conclusion, RunStatus, Timestamp, WorkflowJob, node_id,
};
use hubdouble_render::{Integration, SimpleUser, Urls};
use hubdouble_store::{Accounts, CheckRuns, CheckSuites, Tx, TxMut};
use uuid::Uuid;

use crate::events::{self, To};
use crate::pushes;
use crate::view::View;

/// The ID of the GitHub Actions App, which owns the check suite of every workflow run and the
/// check run of every workflow job.
pub const APP_ID: i64 = hubdouble_model::ACTIONS_APP_ID;

const PERMISSIONS: [(&str, Access); 23] = [
    ("actions", Access::Write),
    ("administration", Access::Read),
    ("artifact_metadata", Access::Write),
    ("attestations", Access::Write),
    ("checks", Access::Write),
    ("code_quality", Access::Write),
    ("contents", Access::Write),
    ("copilot_requests", Access::Write),
    ("deployments", Access::Write),
    ("discussions", Access::Write),
    ("drives", Access::Write),
    ("issues", Access::Write),
    ("merge_queues", Access::Write),
    ("metadata", Access::Read),
    ("models", Access::Read),
    ("packages", Access::Write),
    ("pages", Access::Write),
    ("pull_requests", Access::Write),
    ("repository_hooks", Access::Write),
    ("repository_projects", Access::Write),
    ("security_events", Access::Write),
    ("statuses", Access::Write),
    ("vulnerability_alerts", Access::Read),
];

const EVENTS: [&str; 30] = [
    "branch_protection_rule",
    "check_run",
    "check_suite",
    "create",
    "delete",
    "deployment",
    "deployment_status",
    "discussion",
    "discussion_comment",
    "fork",
    "gollum",
    "issues",
    "issue_comment",
    "label",
    "merge_group",
    "milestone",
    "page_build",
    "public",
    "pull_request",
    "pull_request_review",
    "pull_request_review_comment",
    "push",
    "registry_package",
    "release",
    "repository",
    "repository_dispatch",
    "status",
    "watch",
    "workflow_dispatch",
    "workflow_run",
];

/// The `github` organization that owns GitHub Actions, as `GET /users/github` gives it. It is
/// not a row of the store, so a seeded account with its login or ID takes its place there.
#[must_use]
pub fn owner() -> Account {
    Account {
        id: 9919,
        node_id: "MDEyOk9yZ2FuaXphdGlvbjk5MTk=".to_owned(),
        login: "github".to_owned(),
        kind: AccountKind::Organization,
        name: Some("GitHub".to_owned()),
        email: None,
        company: None,
        blog: Some("https://github.com/about".to_owned()),
        location: Some("United States of America".to_owned()),
        bio: Some("How people build software.".to_owned()),
        twitter_username: None,
        description: None,
        site_admin: false,
        verified_email: true,
        sso_enforced: false,
        app_id: None,
        created_at: Timestamp(1_210_480_651),
        updated_at: Timestamp(1_778_109_125),
    }
}

/// The account that owns GitHub Actions in this store: a seeded account with the `github`
/// organization's ID, else one with its login, else that organization. `GET /users/{username}`
/// and `GET /user/{account_id}` serve the same account.
#[must_use]
pub fn owner_in(tx: &Tx) -> Account {
    let builtin = owner();
    let accounts = tx.table::<Accounts>();
    accounts
        .get(builtin.id)
        .or_else(|| accounts.by_login(&builtin.login))
        .map_or(builtin, |seeded| (**seeded).clone())
}

/// GitHub Actions as GitHub renders it in a check run or suite, owned by `owner`.
#[must_use]
pub fn app(urls: &Urls, owner: &Account) -> Integration {
    Integration {
        id: APP_ID,
        slug: hubdouble_model::ACTIONS_APP_SLUG.to_owned(),
        node_id: "MDM6QXBwMTUzNjg=".to_owned(),
        client_id: "Iv1.05c79e9ad1f6bdfa".to_owned(),
        owner: SimpleUser::new(urls, owner),
        name: "GitHub Actions".to_owned(),
        description: Some("Automate your workflow from idea to production".to_owned()),
        external_url: "https://help.github.com/en/actions".to_owned(),
        html_url: format!("{}/apps/{}", urls.web, hubdouble_model::ACTIONS_APP_SLUG),
        created_at: Timestamp(1_532_943_017),
        updated_at: Timestamp(1_781_799_468),
        permissions: PERMISSIONS
            .into_iter()
            .map(|(name, access)| (name.to_owned(), access))
            .collect(),
        events: EVENTS.into_iter().map(str::to_owned).collect(),
        installations_count: None,
    }
}

/// The check suite of a new workflow run.
pub struct Suite {
    pub id: i64,
    pub repo_id: i64,
    pub head_branch: Option<String>,
    pub head_sha: String,
    pub created_at: Timestamp,
}

/// Opens the queued check suite of a new workflow run. Nothing is delivered, because GitHub
/// sends `check_suite.requested` only to the suite's own App.
///
/// # Errors
///
/// Returns a store error when the suite ID is taken.
pub fn open_suite(tx: &mut TxMut<'_>, suite: Suite) -> Result<(), ApiError> {
    let (before, after) = pushes::of_commit(tx, suite.repo_id, &suite.head_sha);
    tx.table_mut::<CheckSuites>().insert(CheckSuite {
        id: suite.id,
        node_id: node_id::entity(node_id::CHECK_SUITE, suite.id),
        repo_id: suite.repo_id,
        app_id: APP_ID,
        head_branch: suite.head_branch,
        head_sha: suite.head_sha,
        before,
        after,
        status: RunStatus::Queued,
        conclusion: None,
        rerequestable: true,
        created_at: suite.created_at,
        updated_at: suite.created_at,
    })?;
    Ok(())
}

/// Gives the suite the status and conclusion of its run's latest attempt, and sends
/// `check_suite.completed` to subscribed Apps when that attempt completes.
///
/// # Errors
///
/// Returns `404` when the suite does not exist.
pub fn follow_run(
    tx: &mut TxMut<'_>,
    git: &Git,
    suite: i64,
    (status, conclusion): (RunStatus, Option<Conclusion>),
    (sender, now): (i64, Timestamp),
) -> Result<(), ApiError> {
    let held = tx
        .table::<CheckSuites>()
        .get(suite)
        .cloned()
        .ok_or_else(ApiError::not_found)?;
    if (held.status, held.conclusion) == (status, conclusion) {
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

/// Writes the check run of `job`, which has the job's ID and node ID, in the suite of the job's
/// run. Its name, head, status, conclusion and times are the job's; its external ID and
/// annotations are kept, and `annotations` are appended. Subscribed Apps get
/// `check_run.created` for a new check run and `check_run.completed` when the job completes.
///
/// # Errors
///
/// Returns `404` when the job's repository is gone, or a store error.
pub fn write_job(
    tx: &mut TxMut<'_>,
    git: &Git,
    (job, suite): (&WorkflowJob, i64),
    annotations: Vec<Annotation>,
    sender: i64,
) -> Result<(), ApiError> {
    let urls = Urls::new(tx.config(), "");
    let name = View::of(tx, git, &urls, job.repo_id)?.name;
    let held = tx.table::<CheckRuns>().get(job.id).cloned();
    let mut output = held
        .as_ref()
        .map_or_else(CheckOutput::default, |run| run.output.clone());
    output.annotations.extend(annotations);
    let run = CheckRun {
        id: job.id,
        node_id: job.node_id.clone(),
        repo_id: job.repo_id,
        check_suite_id: suite,
        app_id: APP_ID,
        head_sha: job.head_sha.clone(),
        name: job.name.clone(),
        external_id: Some(
            held.as_ref()
                .and_then(|run| run.external_id.clone())
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
        ),
        details_url: Some(format!(
            "{}/{name}/actions/runs/{}/job/{}",
            urls.web, job.run_id, job.id
        )),
        status: job.status,
        conclusion: job.conclusion,
        started_at: Some(job.started_at.unwrap_or(job.created_at)),
        completed_at: job.completed_at,
        output,
        actions: Vec::new(),
    };
    let completed = run.status == RunStatus::Completed
        && held
            .as_ref()
            .is_none_or(|held| held.status != RunStatus::Completed);
    let run = if held.is_some() {
        let run = Arc::new(run);
        tx.table_mut::<CheckRuns>().replace(Arc::clone(&run))?;
        run
    } else {
        let run = tx.table_mut::<CheckRuns>().insert(run)?;
        events::check_run(tx, git, &run, ("created", sender, To::Subscribers), None)?;
        run
    };
    if completed {
        events::check_run(tx, git, &run, ("completed", sender, To::Subscribers), None)?;
    }
    Ok(())
}

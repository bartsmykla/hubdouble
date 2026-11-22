use hubdouble_admin::SeedCtx;
use hubdouble_model::ApiError;
use hubdouble_store::{Accounts, Entity, Issues, Repos, Rows, Tx};
use serde::Deserialize;
use serde_json::Value;

/// A pull request linked to an issue by hand, in the issue's Development sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IssueLink {
    pub(crate) id: i64,
    pub(crate) repo_id: i64,
    pub(crate) issue_id: i64,
    pub(crate) pull_issue_id: i64,
}

impl Entity for IssueLink {
    fn id(&self) -> i64 {
        self.id
    }
    fn repo_id(&self) -> Option<i64> {
        Some(self.repo_id)
    }
    fn parent_id(&self) -> Option<i64> {
        Some(self.issue_id)
    }
    fn keys(&self) -> Vec<String> {
        vec![format!("{}:{}", self.issue_id, self.pull_issue_id)]
    }
}

pub(crate) type IssueLinks = Rows<IssueLink>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkSeed {
    repository: String,
    issue_number: i64,
    pull_repository: Option<String>,
    pull_number: i64,
}

fn repo_named(tx: &Tx, full_name: &str) -> Result<i64, ApiError> {
    let unknown = || ApiError::unprocessable("No such repository");
    let (owner, name) = full_name.split_once('/').ok_or_else(unknown)?;
    let owner = tx.table::<Accounts>().by_login(owner).ok_or_else(unknown)?;
    Ok(tx
        .table::<Repos>()
        .by_name(owner.id, name)
        .ok_or_else(unknown)?
        .id)
}

pub(crate) fn seed(ctx: &mut SeedCtx<'_>, body: Value) -> Result<(), ApiError> {
    let seed: LinkSeed =
        serde_json::from_value(body).map_err(|e| ApiError::unprocessable(e.to_string()))?;
    let tx = &mut ctx.tx;
    let repo = repo_named(tx, &seed.repository)?;
    let pull_repo = match &seed.pull_repository {
        Some(name) => repo_named(tx, name)?,
        None => repo,
    };
    let issues = tx.table::<Issues>();
    let issue = issues
        .by_number(repo, seed.issue_number)
        .filter(|i| i.pull.is_none())
        .ok_or_else(|| ApiError::unprocessable("No such issue"))?
        .id;
    let pull = issues
        .by_number(pull_repo, seed.pull_number)
        .filter(|i| i.pull.is_some())
        .ok_or_else(|| ApiError::unprocessable("No such pull request"))?
        .id;
    let id = tx.next_id();
    tx.table_mut::<IssueLinks>().insert(IssueLink {
        id,
        repo_id: repo,
        issue_id: issue,
        pull_issue_id: pull,
    })?;
    Ok(())
}

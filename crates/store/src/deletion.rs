use std::sync::Arc;

use hubdouble_model::{Account, ApiError, Environment, Repo, Team};

use crate::rows::{Entity, Rows};
use crate::store::TxMut;
use crate::tables::{
    Accounts, ActivityEvents, Apps, CheckRuns, CheckSuites, Deliveries, Environments,
    Installations, IssueComments, Issues, Labels, Memberships, Reactions, ReleaseAssets, Releases,
    RepoRedirects, Repos, ReviewComments, ReviewThreads, Reviews, Rulesets, Statuses, Teams,
    Tokens, WorkflowJobs, WorkflowRuns,
};

/// An entity that [`TxMut::delete`] removes.
#[derive(Debug, Clone, PartialEq)]
pub enum Deleted {
    Repo(Arc<Repo>),
    Team(Arc<Team>),
    /// A user or an organization.
    Account(Arc<Account>),
    Environment(Arc<Environment>),
}

/// Removes a crate's own rows of a deleted entity inside the deleting transaction. It runs
/// before the store drops its own rows, so the repository's issues, runs and memberships are
/// still there to find the side rows they key.
pub type DeletionSubscriber = fn(&mut TxMut<'_>, &Deleted) -> Result<(), ApiError>;

pub(crate) fn delete(tx: &mut TxMut<'_>, deleted: &Deleted) -> Result<(), ApiError> {
    if let Deleted::Account(account) = deleted {
        let repos: Vec<_> = tx.table::<Repos>().children(account.id).cloned().collect();
        for repo in repos {
            delete(tx, &Deleted::Repo(repo))?;
        }
        let teams: Vec<_> = tx.table::<Teams>().children(account.id).cloned().collect();
        for team in teams {
            delete(tx, &Deleted::Team(team))?;
        }
    }
    if let Deleted::Repo(repo) = deleted {
        let environments: Vec<_> = tx
            .table::<Environments>()
            .in_repo(repo.id)
            .cloned()
            .collect();
        for environment in environments {
            delete(tx, &Deleted::Environment(environment))?;
        }
    }
    for subscriber in tx.deletion_subscribers().iter() {
        subscriber(tx, deleted)?;
    }
    match deleted {
        Deleted::Repo(repo) => drop_repo(tx, repo.id),
        Deleted::Team(team) => {
            tx.table_mut::<Teams>().remove(team.id);
        }
        Deleted::Account(account) => drop_account(tx, account.id),
        Deleted::Environment(environment) => {
            tx.table_mut::<Environments>().remove(environment.id);
        }
    }
    Ok(())
}

fn replace_each<E: Entity + Clone>(tx: &mut TxMut<'_>, rows: Vec<Arc<E>>, change: impl Fn(&mut E))
where
    Rows<E>: crate::store::Table,
{
    for row in rows {
        let mut changed = (*row).clone();
        change(&mut changed);
        let _ = tx.table_mut::<Rows<E>>().replace(changed);
    }
}

fn drop_repo(tx: &mut TxMut<'_>, repo_id: i64) {
    tx.table_mut::<IssueComments>().remove_in_repo(repo_id);
    tx.table_mut::<Reviews>().remove_in_repo(repo_id);
    tx.table_mut::<ReviewComments>().remove_in_repo(repo_id);
    tx.table_mut::<ReviewThreads>().remove_in_repo(repo_id);
    tx.table_mut::<Reactions>().remove_in_repo(repo_id);
    tx.table_mut::<Labels>().remove_in_repo(repo_id);
    tx.table_mut::<CheckRuns>().remove_in_repo(repo_id);
    tx.table_mut::<CheckSuites>().remove_in_repo(repo_id);
    tx.table_mut::<Statuses>().remove_in_repo(repo_id);
    tx.table_mut::<ReleaseAssets>().remove_in_repo(repo_id);
    tx.table_mut::<Releases>().remove_in_repo(repo_id);
    tx.table_mut::<WorkflowJobs>().remove_in_repo(repo_id);
    tx.table_mut::<WorkflowRuns>().remove_in_repo(repo_id);
    tx.table_mut::<Rulesets>().remove_in_repo(repo_id);
    tx.table_mut::<ActivityEvents>().remove_in_repo(repo_id);
    tx.table_mut::<Issues>().remove_in_repo(repo_id);
    let heads: Vec<_> = tx
        .table::<Issues>()
        .pulls_with_head_repo(repo_id)
        .cloned()
        .collect();
    replace_each(tx, heads, |issue| {
        if let Some(pull) = issue.pull.as_mut() {
            pull.head.repo_id = None;
        }
    });
    let teams: Vec<_> = tx
        .table::<Teams>()
        .iter()
        .filter(|t| t.repos.iter().any(|r| r.repo_id == repo_id))
        .cloned()
        .collect();
    replace_each(tx, teams, |team| {
        team.repos.retain(|r| r.repo_id != repo_id);
    });
    let installations: Vec<_> = tx
        .table::<Installations>()
        .iter()
        .filter(|i| i.repo_ids.contains(&repo_id))
        .cloned()
        .collect();
    replace_each(tx, installations, |i| {
        i.repo_ids.retain(|id| *id != repo_id);
    });
    let tokens: Vec<_> = tx
        .table::<Tokens>()
        .iter()
        .filter(|t| {
            t.repo_ids
                .as_ref()
                .is_some_and(|ids| ids.contains(&repo_id))
        })
        .cloned()
        .collect();
    replace_each(tx, tokens, |t| {
        if let Some(ids) = t.repo_ids.as_mut() {
            ids.retain(|id| *id != repo_id);
        }
    });
    tx.table_mut::<RepoRedirects>().get_mut().forget(repo_id);
    tx.table_mut::<Repos>().remove(repo_id);
}

fn drop_account(tx: &mut TxMut<'_>, account_id: i64) {
    tx.table_mut::<Memberships>()
        .remove_where(|m| m.org_id == account_id || m.user_id == account_id);
    let teams: Vec<_> = tx
        .table::<Teams>()
        .iter()
        .filter(|t| t.members.iter().any(|m| m.user_id == account_id))
        .cloned()
        .collect();
    replace_each(tx, teams, |team| {
        team.members.retain(|m| m.user_id != account_id);
    });
    let repos: Vec<_> = tx
        .table::<Repos>()
        .iter()
        .filter(|r| r.collaborators.iter().any(|c| c.user_id == account_id))
        .cloned()
        .collect();
    replace_each(tx, repos, |repo| {
        repo.collaborators.retain(|c| c.user_id != account_id);
    });
    let apps: Vec<i64> = tx
        .table::<Apps>()
        .iter()
        .filter(|a| a.owner_id == account_id)
        .map(|a| a.id)
        .collect();
    let installations: Vec<i64> = tx
        .table::<Installations>()
        .iter()
        .filter(|i| i.account_id == account_id || apps.contains(&i.app_id))
        .map(|i| i.id)
        .collect();
    tx.table_mut::<Tokens>().remove_where(|t| {
        t.user_id == Some(account_id)
            || t.app_id.is_some_and(|a| apps.contains(&a))
            || t.installation_id
                .is_some_and(|i| installations.contains(&i))
    });
    tx.table_mut::<Installations>().remove_all(installations);
    for app in &apps {
        tx.table_mut::<Deliveries>().remove_children(*app);
    }
    tx.table_mut::<Apps>().remove_all(apps);
    tx.table_mut::<Rulesets>().remove_children(account_id);
    tx.table_mut::<ActivityEvents>().remove_children(account_id);
    tx.table_mut::<Accounts>().remove(account_id);
}

use std::collections::BTreeMap;

use hubdouble_git::{Git, RefChange};
use hubdouble_model::ApiError;
use hubdouble_store::{Entity, Rows, Tx, TxMut};

const ZERO: &str = "0000000000000000000000000000000000000000";

pub(crate) type Pushes = Rows<PushLog>;

pub(crate) struct PushLog {
    repo: i64,
    branches: BTreeMap<String, (String, String)>,
}

impl Entity for PushLog {
    fn id(&self) -> i64 {
        self.repo
    }
}

pub(crate) fn record(tx: &mut TxMut<'_>, _: &Git, change: &RefChange) -> Result<(), ApiError> {
    let Some(branch) = change.name.strip_prefix("refs/heads/") else {
        return Ok(());
    };
    let mut branches = tx
        .table::<Pushes>()
        .get(change.repo_id)
        .map_or_default(|log| log.branches.clone());
    match change.new {
        Some(new) => {
            let before = change
                .old
                .map_or_else(|| ZERO.to_owned(), |old| old.to_string());
            branches.insert(branch.to_owned(), (before, new.to_string()));
        }
        None => {
            branches.remove(branch);
        }
    }
    tx.table_mut::<Pushes>().replace(PushLog {
        repo: change.repo_id,
        branches,
    })?;
    Ok(())
}

pub(crate) fn last(
    tx: &Tx,
    repo: i64,
    branch: Option<&str>,
    sha: &str,
) -> (Option<String>, Option<String>) {
    branch
        .and_then(|branch| {
            tx.table::<Pushes>()
                .get(repo)?
                .branches
                .get(branch)
                .cloned()
        })
        .filter(|(_, after)| after == sha)
        .map_or((None, None), |(before, after)| (Some(before), Some(after)))
}

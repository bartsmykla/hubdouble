use std::collections::HashMap;

use hubdouble_model::{Repo, RepositoryPropertyValues, RulesetRepository};
use hubdouble_store::{Table, Tx, TxMut};

#[derive(Clone, Default)]
pub(crate) struct RepositoryProperties(pub(crate) HashMap<i64, RepositoryPropertyValues>);

impl Table for RepositoryProperties {}

#[must_use]
pub fn ruleset_repository<'a>(tx: &'a Tx, repo: &'a Repo) -> RulesetRepository<'a> {
    RulesetRepository {
        repo,
        custom_properties: tx
            .try_table::<RepositoryProperties>()
            .and_then(|properties| properties.0.get(&repo.id)),
    }
}

pub fn clear_repository_properties(tx: &mut TxMut<'_>, repo_id: i64) {
    if tx
        .try_table::<RepositoryProperties>()
        .is_some_and(|properties| properties.0.contains_key(&repo_id))
    {
        tx.table_mut::<RepositoryProperties>()
            .get_mut()
            .0
            .remove(&repo_id);
    }
}

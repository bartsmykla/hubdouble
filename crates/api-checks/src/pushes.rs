use hubdouble_git::{Git, ObjectId, RefChange};
use hubdouble_model::ApiError;
use hubdouble_store::{Deleted, Entity, Rows, Tx, TxMut};

pub(crate) type FirstPushes = Rows<FirstPush>;

pub(crate) struct FirstPush {
    id: i64,
    key: String,
    before: ObjectId,
}

impl Entity for FirstPush {
    fn id(&self) -> i64 {
        self.id
    }
    fn keys(&self) -> Vec<String> {
        vec![self.key.clone()]
    }
}

fn key(repo: i64, sha: &str) -> String {
    format!("{repo}/{sha}")
}

pub(crate) fn record(tx: &mut TxMut<'_>, git: &Git, change: &RefChange) -> Result<(), ApiError> {
    if !(change.name.starts_with("refs/heads/") || change.name.starts_with("refs/tags/")) {
        return Ok(());
    }
    let peel = |id: ObjectId| git.peel_to_commit(tx, change.repo_id, id).ok();
    let Some(new) = change.new.and_then(peel) else {
        return Ok(());
    };
    let before = change
        .old
        .map_or(ObjectId::NULL, |old| peel(old).unwrap_or(old));
    let key = key(change.repo_id, &new.to_string());
    let pushes = tx.table::<FirstPushes>();
    if pushes.by_key(&key).is_some() {
        return Ok(());
    }
    let id = pushes.iter().next_back().map_or(1, |last| last.id + 1);
    tx.table_mut::<FirstPushes>()
        .insert(FirstPush { id, key, before })?;
    Ok(())
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "a deletion subscriber returns a Result"
)]
pub(crate) fn forget(tx: &mut TxMut<'_>, deleted: &Deleted) -> Result<(), ApiError> {
    if let Deleted::Repo(repo) = deleted {
        let prefix = key(repo.id, "");
        tx.table_mut::<FirstPushes>()
            .remove_where(|push| push.key.starts_with(&prefix));
    }
    Ok(())
}

/// The `before` and `after` of every check suite on `sha`: those of the first push that made
/// `sha` the tip of a branch or tag in the repository, with zeros as `before` for a created
/// ref. Both are `None` when no push did.
#[must_use]
pub fn of_commit(tx: &Tx, repo: i64, sha: &str) -> (Option<String>, Option<String>) {
    tx.table::<FirstPushes>()
        .by_key(&key(repo, sha))
        .map_or((None, None), |push| {
            (Some(push.before.to_string()), Some(sha.to_owned()))
        })
}

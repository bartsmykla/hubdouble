use hubdouble_git::{ObjectId, RefChange};
use hubdouble_model::ApiError;
use hubdouble_store::{Entity, Rows, Tx, TxMut};

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

pub(crate) fn record(tx: &mut TxMut<'_>, change: &RefChange) -> Result<(), ApiError> {
    let Some(new) = change.new else {
        return Ok(());
    };
    let key = key(change.repo_id, &new.to_string());
    let pushes = tx.table::<FirstPushes>();
    if pushes.by_key(&key).is_some() {
        return Ok(());
    }
    let id = i64::try_from(pushes.len()).unwrap_or(i64::MAX - 1) + 1;
    tx.table_mut::<FirstPushes>().insert(FirstPush {
        id,
        key,
        before: change.old.unwrap_or(ObjectId::NULL),
    })?;
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

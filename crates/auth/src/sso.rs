use hubdouble_model::ApiError;
use hubdouble_store::{Deleted, Entity, Rows, StoreBuilder, Tx, TxMut};

/// A user's SSO sign-in for an SSO-enforced org, made by `/orgs/{org}/sso`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsoSession {
    pub id: i64,
    pub user_id: i64,
    pub org_id: i64,
}

impl Entity for SsoSession {
    fn id(&self) -> i64 {
        self.id
    }
    fn parent_id(&self) -> Option<i64> {
        Some(self.org_id)
    }
    fn keys(&self) -> Vec<String> {
        vec![key(self.user_id, self.org_id)]
    }
}

fn key(user_id: i64, org_id: i64) -> String {
    format!("{user_id}/{org_id}")
}

pub type SsoSessions = Rows<SsoSession>;

/// Registers the tables the choke point reads beyond the frozen ones. The server calls it on its
/// `StoreBuilder` next to `GitBuilder::new`.
pub fn register_tables(b: &mut StoreBuilder) {
    b.register_table::<SsoSessions>().on_delete(forget_sso);
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "a deletion subscriber returns a Result"
)]
fn forget_sso(tx: &mut TxMut<'_>, deleted: &Deleted) -> Result<(), ApiError> {
    if let Deleted::Account(account) = deleted {
        let id = account.id;
        tx.table_mut::<SsoSessions>()
            .remove_where(|s| s.org_id == id || s.user_id == id);
    }
    Ok(())
}

#[must_use]
pub fn has_sso(tx: &Tx, user_id: i64, org_id: i64) -> bool {
    tx.table::<SsoSessions>()
        .by_key(&key(user_id, org_id))
        .is_some()
}

/// Records SSO for a user and an org; recording it again changes nothing.
///
/// # Errors
///
/// Only the store's own insert error, which a fresh ID never meets.
pub fn record_sso(tx: &mut TxMut<'_>, user_id: i64, org_id: i64) -> Result<(), ApiError> {
    if has_sso(tx, user_id, org_id) {
        return Ok(());
    }
    let id = tx.next_id();
    tx.table_mut::<SsoSessions>().insert(SsoSession {
        id,
        user_id,
        org_id,
    })?;
    Ok(())
}

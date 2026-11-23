use hubdouble_auth::Actor;
use hubdouble_model::{ApiError, Issue, Timestamp, node_id};
use hubdouble_store::{Entity, Rows, TxMut};

/// A `reopened` issue event of a pull request, from any path, or a `closed` one from
/// `PATCH .../pulls/{n}`. T7 records the other closes itself, and its
/// `GET .../issues/{n}/events` lists these rows with its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateEvent {
    pub id: i64,
    pub node_id: String,
    pub repo_id: i64,
    pub issue_id: i64,
    pub actor_id: i64,
    pub via_app: Option<i64>,
    pub reopened: bool,
    pub created_at: Timestamp,
}

impl Entity for StateEvent {
    fn id(&self) -> i64 {
        self.id
    }
    fn node_id(&self) -> Option<&str> {
        Some(&self.node_id)
    }
    fn repo_id(&self) -> Option<i64> {
        Some(self.repo_id)
    }
    fn parent_id(&self) -> Option<i64> {
        Some(self.issue_id)
    }
}

pub type StateEvents = Rows<StateEvent>;

pub(crate) fn via_app(actor: &Actor) -> Option<i64> {
    match actor {
        Actor::Installation { app, .. } => Some(app.id),
        _ => None,
    }
}

pub(crate) fn record(
    tx: &mut TxMut<'_>,
    issue: &Issue,
    (actor_id, via_app): (i64, Option<i64>),
    reopened: bool,
) -> Result<(), ApiError> {
    let id = tx.next_id();
    let prefix = if reopened { "REE" } else { "CE" };
    tx.table_mut::<StateEvents>().insert(StateEvent {
        id,
        node_id: node_id::entity(prefix, id),
        repo_id: issue.repo_id,
        issue_id: issue.id,
        actor_id,
        via_app,
        reopened,
        created_at: issue.updated_at,
    })?;
    Ok(())
}

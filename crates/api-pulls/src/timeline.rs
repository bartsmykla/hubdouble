use hubdouble_auth::Actor;
use hubdouble_model::{ApiError, Issue, Timestamp, node_id};
use hubdouble_store::{Entity, Rows, TxMut};

/// What a [`StateEvent`] records, as GitHub names the issue event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateKind {
    Closed,
    Reopened,
    Renamed { from: String, to: String },
    BaseRefChanged,
    HeadRefDeleted,
    BaseRefDeleted,
}

impl StateKind {
    /// The REST `event` value.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            StateKind::Closed => "closed",
            StateKind::Reopened => "reopened",
            StateKind::Renamed { .. } => "renamed",
            StateKind::BaseRefChanged => "base_ref_changed",
            StateKind::HeadRefDeleted => "head_ref_deleted",
            StateKind::BaseRefDeleted => "base_ref_deleted",
        }
    }

    fn prefix(&self) -> &'static str {
        match self {
            StateKind::Closed => "CE",
            StateKind::Reopened => "REE",
            StateKind::Renamed { .. } => "RTE",
            StateKind::BaseRefChanged => "BRCE",
            StateKind::HeadRefDeleted => "HRDE",
            StateKind::BaseRefDeleted => "BRDE",
        }
    }
}

/// An issue event that T5 records for a pull request: a `reopened` one from any path, and the
/// `closed`, `renamed`, `base_ref_changed`, `head_ref_deleted` and `base_ref_deleted` ones of
/// T5's own writes and of branch deletions. T7 records the other closes itself, and its
/// `GET .../issues/{n}/events` lists these rows with its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateEvent {
    pub id: i64,
    pub node_id: String,
    pub repo_id: i64,
    pub issue_id: i64,
    pub actor_id: i64,
    pub via_app: Option<i64>,
    pub kind: StateKind,
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
    kind: StateKind,
) -> Result<(), ApiError> {
    let id = tx.next_id();
    tx.table_mut::<StateEvents>().insert(StateEvent {
        id,
        node_id: node_id::entity(kind.prefix(), id),
        repo_id: issue.repo_id,
        issue_id: issue.id,
        actor_id,
        via_app,
        kind,
        created_at: issue.updated_at,
    })?;
    Ok(())
}

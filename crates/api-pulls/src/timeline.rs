use hubdouble_auth::Actor;
use hubdouble_model::{ApiError, Issue, Timestamp, node_id};
use hubdouble_store::{Entity, Rows, TxMut};

use crate::review_requests::Reviewer;

/// What a [`StateEvent`] records, as GitHub names the issue event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateKind {
    Closed,
    Reopened,
    Renamed {
        from: String,
        to: String,
    },
    BaseRefChanged,
    AutomaticBaseChangeSucceeded,
    AutomaticBaseChangeFailed,
    HeadRefDeleted,
    HeadRefRestored,
    BaseRefDeleted,
    /// A review dismissed: `state` is the review's state before, in lowercase, `message` the
    /// dismissal message (`None` for a stale approval), and `commit` the head that made the
    /// approval stale (`None` for a dismissal through the API).
    ReviewDismissed {
        review_id: i64,
        state: &'static str,
        message: Option<String>,
        commit: Option<String>,
    },
    /// A review requested by the actor of the event.
    ReviewRequested(Reviewer),
    /// A review request that the actor of the event removed.
    ReviewRequestRemoved(Reviewer),
    ReadyForReview,
    ConvertToDraft,
    /// A push to the head that the old head is not an ancestor of: `commit` is the new head and
    /// `repo_id` the head repository, `None` for the repository of the pull request.
    HeadRefForcePushed {
        commit: String,
        repo_id: Option<i64>,
    },
    /// The actor was `@` mentioned in the body or a comment.
    Mentioned,
    /// The actor subscribed to notifications, as a mention of the actor or a team does.
    Subscribed,
    /// A pushed commit of the repository `repo_id` names the issue without closing it.
    Referenced {
        commit: String,
        repo_id: i64,
    },
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
            StateKind::AutomaticBaseChangeSucceeded => "automatic_base_change_succeeded",
            StateKind::AutomaticBaseChangeFailed => "automatic_base_change_failed",
            StateKind::HeadRefDeleted => "head_ref_deleted",
            StateKind::HeadRefRestored => "head_ref_restored",
            StateKind::BaseRefDeleted => "base_ref_deleted",
            StateKind::ReviewDismissed { .. } => "review_dismissed",
            StateKind::ReviewRequested(_) => "review_requested",
            StateKind::ReviewRequestRemoved(_) => "review_request_removed",
            StateKind::ReadyForReview => "ready_for_review",
            StateKind::ConvertToDraft => "convert_to_draft",
            StateKind::HeadRefForcePushed { .. } => "head_ref_force_pushed",
            StateKind::Mentioned => "mentioned",
            StateKind::Subscribed => "subscribed",
            StateKind::Referenced { .. } => "referenced",
        }
    }

    fn prefix(&self) -> &'static str {
        match self {
            StateKind::Closed => "CE",
            StateKind::Reopened => "REE",
            StateKind::Renamed { .. } => "RTE",
            StateKind::BaseRefChanged => "BRCE",
            StateKind::AutomaticBaseChangeSucceeded => "ABCSE",
            StateKind::AutomaticBaseChangeFailed => "ABCFE",
            StateKind::HeadRefDeleted => "HRDE",
            StateKind::HeadRefRestored => "HRRE",
            StateKind::BaseRefDeleted => "BRDE",
            StateKind::ReviewDismissed { .. } => "RDE",
            StateKind::ReviewRequested(_) => "RRE",
            StateKind::ReviewRequestRemoved(_) => "RRRE",
            StateKind::ReadyForReview => "RFRE",
            StateKind::ConvertToDraft => "CTDE",
            StateKind::HeadRefForcePushed { .. } => "HRFPE",
            StateKind::Mentioned => "MEE",
            StateKind::Subscribed => "SE",
            StateKind::Referenced { .. } => "REFE",
        }
    }
}

/// Records the `referenced` issue event of a pushed commit of the repository `repo_id` that
/// names `issue`, once per commit.
///
/// # Errors
///
/// A store error.
pub fn referenced(
    tx: &mut TxMut<'_>,
    issue: &Issue,
    actor: (i64, Option<i64>),
    (repo_id, commit): (i64, String),
    at: Timestamp,
) -> Result<(), ApiError> {
    let kind = StateKind::Referenced { commit, repo_id };
    if tx
        .table::<StateEvents>()
        .children(issue.id)
        .any(|e| e.kind == kind)
    {
        return Ok(());
    }
    record_at(tx, issue, actor, kind, at)
}

/// Records the `review_dismissed` issue event of a dismissal by `actor` at `at`.
///
/// # Errors
///
/// A store error.
pub fn review_dismissed(
    tx: &mut TxMut<'_>,
    issue: &Issue,
    actor: (i64, Option<i64>),
    kind: StateKind,
    at: Timestamp,
) -> Result<(), ApiError> {
    record_at(tx, issue, actor, kind, at)
}

/// An issue event of a [`StateKind`] that T5 records for a pull request, from its own writes,
/// from branch pushes, deletions and restores, and a `reopened` one from any path. T7 records
/// the other closes itself, and its
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
    who: (i64, Option<i64>),
    kind: StateKind,
) -> Result<(), ApiError> {
    record_at(tx, issue, who, kind, issue.updated_at)
}

pub(crate) fn record_at(
    tx: &mut TxMut<'_>,
    issue: &Issue,
    (actor_id, via_app): (i64, Option<i64>),
    kind: StateKind,
    created_at: Timestamp,
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
        created_at,
    })?;
    Ok(())
}

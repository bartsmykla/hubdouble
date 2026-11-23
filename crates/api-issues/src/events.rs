use std::sync::Arc;

use hubdouble_git::ObjectId;
use hubdouble_model::{ApiError, Issue, IssueState, StateReason, Timestamp, node_id};
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::{Entity, Issues, Repos, Rows, TxMut};
use serde_json::{Value, json};

use crate::shared::{full_name, integration, user};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum What {
    Closed(Option<StateReason>),
    CommitClosed { repo_id: i64, sha: ObjectId },
    Reopened,
    Labeled { name: String, color: String },
    Unlabeled { name: String, color: String },
    Assigned(i64),
    Unassigned(i64),
    Renamed { from: String, to: String },
    Merged(Option<String>),
}

impl What {
    fn name(&self) -> &'static str {
        match self {
            What::Closed(_) | What::CommitClosed { .. } => "closed",
            What::Reopened => "reopened",
            What::Labeled { .. } => "labeled",
            What::Unlabeled { .. } => "unlabeled",
            What::Assigned(_) => "assigned",
            What::Unassigned(_) => "unassigned",
            What::Renamed { .. } => "renamed",
            What::Merged(_) => "merged",
        }
    }

    fn prefix(&self) -> &'static str {
        match self {
            What::Closed(_) | What::CommitClosed { .. } => "CE",
            What::Reopened => "REE",
            What::Labeled { .. } => "LE",
            What::Unlabeled { .. } => "UNLE",
            What::Assigned(_) => "AE",
            What::Unassigned(_) => "UAE",
            What::Renamed { .. } => "RTE",
            What::Merged(_) => "ME",
        }
    }

    fn state(&self) -> Option<IssueState> {
        match self {
            What::Closed(_) | What::CommitClosed { .. } => Some(IssueState::Closed),
            What::Reopened => Some(IssueState::Open),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IssueEvent {
    pub(crate) id: i64,
    pub(crate) node_id: String,
    pub(crate) repo_id: i64,
    pub(crate) issue_id: i64,
    pub(crate) actor_id: i64,
    pub(crate) via_app: Option<i64>,
    pub(crate) what: What,
    pub(crate) created_at: Timestamp,
}

impl Entity for IssueEvent {
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

pub(crate) type IssueEvents = Rows<IssueEvent>;

pub(crate) fn record(
    tx: &mut TxMut<'_>,
    issue: &Issue,
    actor_id: i64,
    via_app: Option<i64>,
    what: What,
    at: Timestamp,
) -> Result<(), ApiError> {
    let id = tx.next_id();
    tx.table_mut::<IssueEvents>().insert(IssueEvent {
        id,
        node_id: node_id::entity(what.prefix(), id),
        repo_id: issue.repo_id,
        issue_id: issue.id,
        actor_id,
        via_app,
        what,
        created_at: at,
    })?;
    Ok(())
}

fn synthesized(issue: &Issue, recorded: &[IssueEvent]) -> Vec<IssueEvent> {
    let make = |id: i64, actor_id: i64, what: What, created_at: Timestamp| IssueEvent {
        id,
        node_id: node_id::entity(what.prefix(), id),
        repo_id: issue.repo_id,
        issue_id: issue.id,
        actor_id,
        via_app: None,
        what,
        created_at,
    };
    let mut out = Vec::new();
    if let Some(p) = &issue.pull
        && let Some(at) = p.merged_at
        && !recorded.iter().any(|e| matches!(e.what, What::Merged(_)))
    {
        out.push(make(
            p.id,
            p.merged_by.unwrap_or(issue.user_id),
            What::Merged(p.merge_commit_sha.clone()),
            at,
        ));
    }
    let last = recorded
        .iter()
        .rev()
        .find_map(|e| e.what.state())
        .unwrap_or(IssueState::Open);
    if last != issue.state {
        let actor = issue.closed_by.unwrap_or(issue.user_id);
        out.push(match issue.state {
            IssueState::Closed => make(
                issue.id,
                actor,
                What::Closed(issue.state_reason),
                issue.closed_at.unwrap_or(issue.updated_at),
            ),
            IssueState::Open => make(issue.id, actor, What::Reopened, issue.updated_at),
        });
    }
    out
}

pub(crate) fn list(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?.clone();
    let number = ctx.param_i64("issue_number")?;
    let tx = ctx.tx();
    let issue: Arc<Issue> = tx
        .table::<Issues>()
        .by_number(repo.id, number)
        .cloned()
        .ok_or_else(ApiError::not_found)?;
    let mut events: Vec<IssueEvent> = tx
        .table::<IssueEvents>()
        .children(issue.id)
        .map(|e| IssueEvent::clone(e))
        .collect();
    let extra = synthesized(&issue, &events);
    events.extend(extra);
    events.sort_by_key(|e| e.created_at);
    let urls = ctx.urls();
    let full = full_name(tx, &repo);
    ctx.page(&events, |e| {
        let (commit_id, commit_url) = match &e.what {
            What::Merged(Some(sha)) => (
                Some(sha.clone()),
                Some(format!("{}/repos/{full}/commits/{sha}", urls.api)),
            ),
            What::CommitClosed { repo_id, sha } => {
                let url = tx.table::<Repos>().get(*repo_id).map(|source| {
                    let source = full_name(tx, source);
                    format!("{}/repos/{source}/commits/{sha}", urls.api)
                });
                (Some(sha.to_string()), url)
            }
            _ => (None, None),
        };
        let mut v = json!({
            "id": e.id,
            "node_id": e.node_id,
            "url": format!("{}/repos/{full}/issues/events/{}", urls.api, e.id),
            "actor": user(tx, urls, e.actor_id),
            "event": e.what.name(),
            "commit_url": commit_url,
            "commit_id": commit_id,
            "created_at": e.created_at,
            "performed_via_github_app": integration(tx, urls, e.via_app),
        });
        let extra = match &e.what {
            What::Labeled { name, color } | What::Unlabeled { name, color } => vec![
                ("label", json!({"name": name, "color": color})),
                ("intent", Value::Null),
            ],
            What::Assigned(id) => vec![
                ("assignee", json!(user(tx, urls, *id))),
                ("assigner", json!(user(tx, urls, e.actor_id))),
                ("intent", Value::Null),
            ],
            What::Unassigned(id) => vec![
                ("assignee", json!(user(tx, urls, *id))),
                ("assigner", json!(user(tx, urls, e.actor_id))),
            ],
            What::Renamed { from, to } => vec![("rename", json!({"from": from, "to": to}))],
            What::Closed(reason) => vec![
                (
                    "state_reason",
                    json!(
                        reason
                            .filter(|r| *r != StateReason::Completed)
                            .map(StateReason::as_str)
                    ),
                ),
                ("intent", Value::Null),
            ],
            What::CommitClosed { .. } => {
                vec![("state_reason", Value::Null), ("intent", Value::Null)]
            }
            What::Reopened => vec![("state_reason", json!("reopened"))],
            What::Merged(_) => vec![],
        };
        for (k, value) in extra {
            v[k] = value;
        }
        v
    })
}

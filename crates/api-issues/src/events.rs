use std::sync::Arc;

use hubdouble_api_pulls::{Reviewer, StateEvent, StateEvents, StateKind};
use hubdouble_api_repos::DeletedRepos;
use hubdouble_auth::{Actor, Target};
use hubdouble_git::ObjectId;
use hubdouble_model::{
    ApiError, Grant, Issue, IssueState, Requirement, Scope, StateReason, Timestamp, node_id,
};
use hubdouble_render::{self as r, Urls};
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::{Accounts, Entity, Issues, Repos, Rows, Teams, Tx, TxMut};
use serde_json::{Value, json};

use crate::shared::{author, full_name, integration};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum What {
    Closed(Option<StateReason>),
    CommitClosed {
        repo_id: i64,
        sha: ObjectId,
    },
    Reopened,
    Labeled {
        name: String,
        color: String,
    },
    Unlabeled {
        name: String,
        color: String,
    },
    Assigned(i64),
    Unassigned(i64),
    Renamed {
        from: String,
        to: String,
    },
    Merged(Option<String>),
    PullRef(&'static str),
    ReviewDismissed {
        review_id: i64,
        state: &'static str,
        message: Option<String>,
        commit: Option<String>,
    },
    Locked(Option<String>),
    Unlocked,
    ReviewRequest {
        name: &'static str,
        reviewer: Reviewer,
    },
    AtCommit {
        name: &'static str,
        repo_id: Option<i64>,
        sha: String,
    },
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
            What::PullRef(name)
            | What::ReviewRequest { name, .. }
            | What::AtCommit { name, .. } => name,
            What::ReviewDismissed { .. } => "review_dismissed",
            What::Locked(_) => "locked",
            What::Unlocked => "unlocked",
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
            What::PullRef(_) | What::ReviewRequest { .. } | What::AtCommit { .. } => "",
            What::ReviewDismissed { .. } => "RDE",
            What::Locked(_) => "LOE",
            What::Unlocked => "UNLOE",
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

fn pull_event(e: &Arc<StateEvent>) -> IssueEvent {
    IssueEvent {
        id: e.id,
        node_id: e.node_id.clone(),
        repo_id: e.repo_id,
        issue_id: e.issue_id,
        actor_id: e.actor_id,
        via_app: e.via_app,
        what: match &e.kind {
            StateKind::Reopened => What::Reopened,
            StateKind::Closed => What::Closed(None),
            StateKind::Renamed { from, to } => What::Renamed {
                from: from.clone(),
                to: to.clone(),
            },
            StateKind::ReviewDismissed {
                review_id,
                state,
                message,
                commit,
            } => What::ReviewDismissed {
                review_id: *review_id,
                state,
                message: message.clone(),
                commit: commit.clone(),
            },
            StateKind::ReviewRequested(reviewer) | StateKind::ReviewRequestRemoved(reviewer) => {
                What::ReviewRequest {
                    name: e.kind.name(),
                    reviewer: *reviewer,
                }
            }
            StateKind::HeadRefForcePushed { commit, repo_id } => What::AtCommit {
                name: e.kind.name(),
                repo_id: *repo_id,
                sha: commit.clone(),
            },
            StateKind::Referenced { commit, repo_id } => What::AtCommit {
                name: e.kind.name(),
                repo_id: Some(*repo_id),
                sha: commit.clone(),
            },
            other => What::PullRef(other.name()),
        },
        created_at: e.created_at,
    }
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
        .chain(tx.table::<StateEvents>().children(issue.id).map(pull_event))
        .collect();
    events.sort_by_key(|e| (e.created_at, e.id));
    let extra = synthesized(&issue, &events);
    events.extend(extra);
    events.sort_by_key(|e| e.created_at);
    let urls = ctx.urls();
    let actor = ctx.actor();
    let full = full_name(tx, &repo);
    ctx.page(&events, |e| {
        let (commit_id, commit_url) = match &e.what {
            What::Merged(Some(sha))
            | What::AtCommit {
                repo_id: None, sha, ..
            } => commit_in(tx, urls, actor, repo.id, sha),
            What::AtCommit {
                repo_id: Some(repo_id),
                sha,
                ..
            } => commit_in(tx, urls, actor, *repo_id, sha),
            What::CommitClosed { repo_id, sha } => commit_in(tx, urls, actor, *repo_id, sha),
            _ => (None, None),
        };
        let mut v = json!({
            "id": e.id,
            "node_id": e.node_id,
            "url": format!("{}/repos/{full}/issues/events/{}", urls.api, e.id),
            "actor": author(tx, urls,e.actor_id),
            "event": e.what.name(),
            "commit_url": commit_url,
            "commit_id": commit_id,
            "created_at": e.created_at,
            "performed_via_github_app": integration(tx, urls, e.via_app),
        });
        for (k, value) in fields_of(tx, urls, e) {
            v[k] = value;
        }
        v
    })
}

fn fields_of(tx: &Tx, urls: &Urls, e: &IssueEvent) -> Vec<(&'static str, Value)> {
    match &e.what {
        What::Labeled { name, color } | What::Unlabeled { name, color } => vec![
            ("label", json!({"name": name, "color": color})),
            ("intent", Value::Null),
        ],
        What::Assigned(id) => vec![
            ("assignee", json!(author(tx, urls, *id))),
            ("assigner", json!(author(tx, urls, e.actor_id))),
            ("intent", Value::Null),
        ],
        What::Unassigned(id) => vec![
            ("assignee", json!(author(tx, urls, *id))),
            ("assigner", json!(author(tx, urls, e.actor_id))),
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
        What::Locked(reason) => vec![("lock_reason", json!(reason))],
        What::Merged(_) | What::PullRef(_) | What::Unlocked | What::AtCommit { .. } => vec![],
        What::ReviewRequest { reviewer, .. } => vec![
            ("review_requester", json!(author(tx, urls, e.actor_id))),
            match reviewer {
                Reviewer::User(id) => ("requested_reviewer", json!(author(tx, urls, *id))),
                Reviewer::Team(id) => ("requested_team", json!(team(tx, urls, *id))),
            },
        ],
        What::ReviewDismissed {
            review_id,
            state,
            message,
            commit,
        } => {
            let mut review = json!({
                "state": state,
                "review_id": review_id,
                "dismissal_message": message,
            });
            if let Some(commit) = commit {
                review["dismissal_commit_id"] = json!(commit);
            }
            vec![("dismissed_review", review)]
        }
    }
}

fn commit_in(
    tx: &Tx,
    urls: &Urls,
    actor: &Actor,
    repo_id: i64,
    sha: impl std::fmt::Display,
) -> (Option<String>, Option<String>) {
    let full = match tx.table::<Repos>().get(repo_id) {
        Some(repo)
            if hubdouble_auth::authorize(tx, actor, &READ_COMMIT, Target::Repo(repo)).is_ok() =>
        {
            full_name(tx, repo)
        }
        Some(_) => return (None, None),
        None => match tx.table::<DeletedRepos>().name(repo_id) {
            Some(name) => name.to_owned(),
            None => return (None, None),
        },
    };
    (
        Some(sha.to_string()),
        Some(format!("{}/repos/{full}/commits/{sha}", urls.api)),
    )
}

const READ_COMMIT: Requirement = Requirement {
    any_of: &[Grant::read("contents")],
    all_of: &[],
    scope: Scope::Repo,
    scopes: &[],
};

fn team(tx: &Tx, urls: &Urls, id: i64) -> Option<r::Team> {
    let teams = tx.table::<Teams>();
    let team = teams.get(id)?;
    let org = tx.table::<Accounts>().get(team.org_id)?;
    let parent = team.parent_id.and_then(|id| teams.get(id)).map(Arc::as_ref);
    Some(r::Team::new(urls, team, org, parent))
}

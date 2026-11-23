use std::sync::Arc;

use http::StatusCode;
use http::header::LINK;
use hubdouble_git::Git;
use hubdouble_model::{ApiError, Repo, Status, StatusState, Timestamp, node_id};
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::{Statuses, TxMut};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::view::{self, View, latest_statuses, no_commit, statuses_on};
use crate::{events, invalid, missing, window};

#[derive(Deserialize)]
pub struct StatusInput {
    state: Option<String>,
    target_url: Option<String>,
    description: Option<String>,
    context: Option<String>,
}

/// Records a status on the commit `reference` names and emits `status`.
pub fn insert(
    tx: &mut TxMut<'_>,
    git: &Git,
    (repo, creator): (&Repo, i64),
    reference: &str,
    input: StatusInput,
    (id, now): (Option<i64>, Timestamp),
) -> Result<Arc<Status>, ApiError> {
    let state = input.state.ok_or_else(|| missing("state"))?;
    let state = StatusState::from_rest(&state).ok_or_else(|| invalid("state"))?;
    let sha = view::commit(tx, git, repo.id, reference)
        .ok_or_else(|| no_commit(reference))?
        .to_string();
    let id = id.unwrap_or_else(|| tx.next_id());
    let status = tx.table_mut::<Statuses>().insert(Status {
        id,
        node_id: node_id::entity(node_id::STATUS, id),
        repo_id: repo.id,
        sha,
        state,
        context: input.context.unwrap_or_else(|| "default".to_owned()),
        description: input.description,
        target_url: input.target_url,
        creator_id: creator,
        created_at: now,
        updated_at: now,
    })?;
    events::status(tx, git, &status)?;
    Ok(status)
}

fn rest_view<'c>(ctx: &'c Ctx<'_>) -> Result<View<'c>, ApiError> {
    View::new(ctx.tx(), ctx.git(), ctx.urls(), ctx.repo()?)
}

pub fn create(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let creator = ctx
        .actor()
        .account()
        .map(|a| a.id)
        .ok_or_else(ApiError::unauthorized)?;
    let sha = ctx.param("sha")?.to_owned();
    if sha.parse::<hubdouble_git::ObjectId>().is_err() {
        return Err(no_commit(&sha));
    }
    let input: StatusInput = ctx.body()?;
    let repo = ctx.repo()?.clone();
    let now = ctx.now();
    let (tx, git) = ctx.tx_mut_and_git()?;
    let status = insert(tx, git, (&repo, creator), &sha, input, (None, now))?;
    let rendered = rest_view(ctx)?.status(&status);
    Reply::json(&rendered)?
        .status(StatusCode::CREATED)
        .header(http::header::LOCATION, &rendered.url)
}

fn resolved(ctx: &Ctx<'_>) -> Result<String, ApiError> {
    let reference = ctx.param("ref")?;
    view::commit(ctx.tx(), ctx.git(), ctx.repo()?.id, reference)
        .map(|id| id.to_string())
        .ok_or_else(ApiError::not_found)
}

pub fn list(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let sha = resolved(ctx)?;
    let statuses = statuses_on(ctx.tx(), ctx.repo()?.id, &sha);
    let view = rest_view(ctx)?;
    ctx.page(&statuses, |status| view.status(status))
}

/// GitHub's combined state: `failure` when any context failed or errored, `pending` with no
/// statuses or any pending one, else `success`.
pub fn combined_state(statuses: &[&Arc<Status>]) -> &'static str {
    if statuses
        .iter()
        .any(|s| matches!(s.state, StatusState::Error | StatusState::Failure))
    {
        "failure"
    } else if statuses.is_empty() || statuses.iter().any(|s| s.state == StatusState::Pending) {
        "pending"
    } else {
        "success"
    }
}

pub fn combined(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let sha = resolved(ctx)?;
    let all = statuses_on(ctx.tx(), ctx.repo()?.id, &sha);
    let latest = latest_statuses(&all);
    let view = rest_view(ctx)?;
    let (shown, link) = window(ctx, &latest)?;
    let statuses = shown
        .iter()
        .map(|status| {
            let mut value = events::object(&view.status(status))?;
            if let Some(map) = value.as_object_mut() {
                map.remove("creator");
            }
            Ok(value)
        })
        .collect::<Result<Vec<Value>, ApiError>>()?;
    let commit = format!("{}/repos/{}/commits/{sha}", ctx.urls().api, view.name);
    let body = json!({
        "state": combined_state(&latest),
        "statuses": statuses,
        "sha": sha,
        "total_count": latest.len(),
        "repository": view.repository(),
        "url": format!("{commit}/status"),
        "commit_url": commit,
    });
    let reply = Reply::json(&body)?;
    match link {
        Some(link) => reply.header(LINK, &link),
        None => Ok(reply),
    }
}

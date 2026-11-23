use std::sync::Arc;

use http::StatusCode;
use hubdouble_model::{ApiError, Reaction, ReactionContent, ReactionSubjectKind, node_id};
use hubdouble_render as r;
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::Reactions;
use serde::Deserialize;

use crate::comments::comment_and_issue;
use crate::shared::{invalid, user, who};

fn content(text: Option<&str>) -> Result<Option<ReactionContent>, ApiError> {
    text.map(|t| {
        ReactionContent::from_rest(t).ok_or_else(|| invalid("Reaction", "content", "invalid"))
    })
    .transpose()
}

pub(crate) fn list(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?.clone();
    let wanted = ctx
        .query()
        .get("content")
        .and_then(ReactionContent::from_rest);
    let tx = ctx.tx();
    let (comment, _) = comment_and_issue(tx, &repo, ctx.param_i64("comment_id")?)?;
    let reactions: Vec<&Arc<Reaction>> = tx
        .table::<Reactions>()
        .children(comment.id)
        .filter(|r| r.subject_kind == ReactionSubjectKind::IssueComment)
        .filter(|r| wanted.is_none_or(|w| r.content == w))
        .collect();
    let urls = ctx.urls();
    ctx.page(&reactions, |r| {
        r::Reaction::new(r, user(tx, urls, r.user_id))
    })
}

#[derive(Deserialize)]
struct NewReaction {
    content: Option<String>,
}

pub(crate) fn create(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let input: NewReaction = ctx.body()?;
    let wanted = content(input.content.as_deref())?
        .ok_or_else(|| invalid("Reaction", "content", "missing_field"))?;
    let repo = ctx.repo()?.clone();
    let id = ctx.param_i64("comment_id")?;
    let who = who(ctx, &repo)?;
    let urls = ctx.urls().clone();
    let now = ctx.now();
    let tx = ctx.tx_mut()?;
    let (comment, _) = comment_and_issue(tx, &repo, id)?;
    let existing = tx
        .table::<Reactions>()
        .children(comment.id)
        .find(|r| {
            r.subject_kind == ReactionSubjectKind::IssueComment
                && r.user_id == who.account.id
                && r.content == wanted
        })
        .cloned();
    let (reaction, status) = if let Some(r) = existing {
        (r, StatusCode::OK)
    } else {
        let rid = tx.next_id();
        let reaction = tx.table_mut::<Reactions>().insert(Reaction {
            id: rid,
            node_id: node_id::entity(node_id::REACTION, rid),
            repo_id: repo.id,
            subject_kind: ReactionSubjectKind::IssueComment,
            subject_id: comment.id,
            user_id: who.account.id,
            content: wanted,
            created_at: now,
        })?;
        (reaction, StatusCode::CREATED)
    };
    let view = r::Reaction::new(&reaction, user(tx, &urls, reaction.user_id));
    Ok(Reply::json(&view)?.status(status))
}

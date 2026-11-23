use std::sync::Arc;

use http::StatusCode;
use hubdouble_auth::Actor;
use hubdouble_model::{
    ApiError, Issue, Reaction, ReactionContent, ReactionSubjectKind, RepoRole, ReviewComment,
    ReviewState, ReviewThread, Timestamp, node_id,
};
use hubdouble_rest::{Ctx, Registry, Reply};
use hubdouble_store::{Issues, Reactions, ReviewComments, ReviewThreads, Reviews, Tx, TxMut};
use serde::Deserialize;

use crate::anchors::{self, Anchor, Placement};
use crate::{READ, WRITE, events, invalid, reviews, view};

pub fn register(registry: &mut Registry) {
    for (id, grant, handler) in [
        (
            "pulls/list-review-comments",
            READ,
            list as fn(&mut Ctx<'_>) -> _,
        ),
        ("pulls/list-review-comments-for-repo", READ, list),
        ("pulls/get-review-comment", READ, get),
        ("pulls/create-review-comment", WRITE, create),
        ("pulls/create-reply-for-review-comment", WRITE, create_reply),
        ("pulls/update-review-comment", WRITE, edit),
        ("pulls/delete-review-comment", WRITE, delete),
        (
            "reactions/list-for-pull-request-review-comment",
            READ,
            reactions,
        ),
        (
            "reactions/create-for-pull-request-review-comment",
            WRITE,
            react,
        ),
    ] {
        registry.handle(id, grant, handler);
    }
}

#[derive(Deserialize)]
pub struct Draft {
    pub body: String,
    #[serde(flatten)]
    pub anchor: Anchor,
}

pub struct Post<'a> {
    pub issue: &'a Issue,
    pub sender: i64,
    pub app: Option<i64>,
    pub now: Timestamp,
}

pub fn open(
    tx: &mut TxMut<'_>,
    post: &Post<'_>,
    review_id: i64,
    sha: &str,
    body: String,
    place: Placement,
) -> Result<(Arc<ReviewThread>, Arc<ReviewComment>), ApiError> {
    let ids = (tx.next_id(), tx.next_id());
    open_with_ids(tx, post, review_id, sha, body, place, ids)
}

pub fn open_with_ids(
    tx: &mut TxMut<'_>,
    post: &Post<'_>,
    review_id: i64,
    sha: &str,
    body: String,
    place: Placement,
    (thread_id, id): (i64, i64),
) -> Result<(Arc<ReviewThread>, Arc<ReviewComment>), ApiError> {
    validate_body(&body)?;
    let thread = ReviewThread {
        id: thread_id,
        node_id: node_id::entity(node_id::REVIEW_THREAD, thread_id),
        repo_id: post.issue.repo_id,
        issue_id: post.issue.id,
        path: place.path.clone(),
        line: place.line,
        original_line: place.line,
        start_line: place.start_line,
        original_start_line: place.start_line,
        diff_side: place.side,
        start_diff_side: place.start_side,
        subject_type: place.subject_type,
        is_resolved: false,
        resolved_by: None,
        is_outdated: false,
    };
    let comment = ReviewComment {
        id,
        node_id: node_id::entity(node_id::REVIEW_COMMENT, id),
        repo_id: post.issue.repo_id,
        issue_id: post.issue.id,
        user_id: post.sender,
        review_id: Some(review_id),
        thread_id,
        in_reply_to_id: None,
        body,
        path: place.path,
        diff_hunk: place.hunk,
        commit_id: sha.to_owned(),
        original_commit_id: sha.to_owned(),
        line: place.line,
        original_line: place.line,
        start_line: place.start_line,
        original_start_line: place.start_line,
        side: Some(place.side),
        start_side: place.start_side,
        position: place.position,
        original_position: place.position,
        subject_type: place.subject_type,
        via_app_id: post.app,
        created_at: post.now,
        updated_at: post.now,
        last_edited_at: None,
    };
    let thread = tx
        .table_mut::<ReviewThreads>()
        .insert(thread)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let comment = tx
        .table_mut::<ReviewComments>()
        .insert(comment)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok((thread, comment))
}

pub fn visible(tx: &Tx, viewer: Option<i64>, comment: &ReviewComment) -> bool {
    comment
        .review_id
        .and_then(|id| tx.table::<Reviews>().get(id))
        .is_none_or(|review| reviews::visible(viewer, review))
}

fn published(tx: &Tx, comment: &ReviewComment) -> bool {
    visible(tx, None, comment)
}

fn validate_body(body: &str) -> Result<(), ApiError> {
    if body.trim().is_empty() {
        return Err(invalid(
            "PullRequestReviewComment",
            "body",
            "The body is required",
        ));
    }
    Ok(())
}

pub fn remove(tx: &mut TxMut<'_>, comment: &ReviewComment) {
    let reactions: Vec<_> = tx
        .table::<Reactions>()
        .children(comment.id)
        .map(|r| r.id)
        .collect();
    for id in reactions {
        tx.table_mut::<Reactions>().remove(id);
    }
    tx.table_mut::<ReviewComments>().remove(comment.id);
    let has_comments = tx
        .table::<ReviewComments>()
        .children(comment.issue_id)
        .any(|other| other.thread_id == comment.thread_id);
    if !has_comments {
        tx.table_mut::<ReviewThreads>().remove(comment.thread_id);
    }
}

fn find(ctx: &Ctx<'_>, id: i64) -> Result<Arc<ReviewComment>, ApiError> {
    let repo = ctx.repo()?.id;
    let viewer = ctx.actor().account().map(|a| a.id);
    ctx.tx()
        .table::<ReviewComments>()
        .get(id)
        .filter(|c| c.repo_id == repo && visible(ctx.tx(), viewer, c))
        .cloned()
        .ok_or_else(ApiError::not_found)
}

fn target(ctx: &Ctx<'_>) -> Result<Arc<ReviewComment>, ApiError> {
    find(ctx, ctx.param_i64("comment_id")?)
}

fn issue(tx: &Tx, comment: &ReviewComment) -> Result<Arc<Issue>, ApiError> {
    tx.table::<Issues>()
        .get(comment.issue_id)
        .cloned()
        .ok_or_else(ApiError::not_found)
}

fn answer(ctx: &Ctx<'_>, comment: &ReviewComment) -> Result<Reply, ApiError> {
    let issue = issue(ctx.tx(), comment)?;
    Reply::json(&view::rest_view(ctx)?.comment(comment, issue.number))
}

fn created(ctx: &Ctx<'_>, comment: &ReviewComment) -> Result<Reply, ApiError> {
    let issue = issue(ctx.tx(), comment)?;
    let rendered = view::rest_view(ctx)?.comment(comment, issue.number);
    Reply::json(&rendered)?
        .status(StatusCode::CREATED)
        .header(http::header::LOCATION, &rendered.url)
}

fn list(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?;
    let viewer = ctx.actor().account().map(|a| a.id);
    let table = ctx.tx().table::<ReviewComments>();
    let pull = ctx
        .params()
        .get("pull_number")
        .map(|_| view::pull(ctx))
        .transpose()?;
    let comments: Vec<_> = if let Some(pull) = &pull {
        table.children(pull.id).collect()
    } else {
        table.in_repo(repo.id).collect()
    };
    let docs = if pull.is_some() {
        "https://docs.github.com/rest/pulls/comments#list-review-comments-on-a-pull-request"
    } else {
        "https://docs.github.com/rest/pulls/comments#list-review-comments-in-a-repository"
    };
    let since = ctx
        .query()
        .get("since")
        .filter(|s| !s.is_empty())
        .map(|s| match Timestamp::parse_query(s, ctx.now()) {
            Ok(t) if t.is_datetime() => Ok(t),
            Ok(_) => Err(ApiError::unprocessable(
                "Time values must be in the range 1000-01-01T00:00:00Z to 9999-12-31T23:59:59Z",
            )
            .with_docs(docs)),
            Err(_) => Err(ApiError::unprocessable(
                "Time values must be in ISO 8601 format: YYYY-MM-DDTHH:MM:SSZ",
            )
            .with_docs(docs)),
        })
        .transpose()?;
    let mut comments: Vec<_> = comments
        .into_iter()
        .filter(|c| since.is_none_or(|since| c.updated_at > since) && visible(ctx.tx(), viewer, c))
        .filter_map(|c| Some((c, ctx.tx().table::<Issues>().get(c.issue_id)?.number)))
        .collect();
    if ctx.query().get("sort") == Some("updated") {
        comments.sort_unstable_by_key(|(c, _)| (c.updated_at, c.id));
    } else {
        comments.sort_unstable_by_key(|(c, _)| (c.created_at, c.id));
    }
    if ctx.query().get("direction") == Some("desc")
        || (ctx.query().get("direction").is_none() && ctx.query().get("sort") == Some("updated"))
    {
        comments.reverse();
    }
    let view = view::rest_view(ctx)?;
    ctx.page(&comments, |(comment, number)| {
        view.comment(comment, *number)
    })
}

fn get(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let comment = target(ctx)?;
    answer(ctx, &comment)
}

#[derive(Deserialize)]
struct Create {
    body: String,
    in_reply_to: Option<i64>,
    commit_id: Option<String>,
    #[serde(flatten)]
    anchor: Option<Anchor>,
}

fn new_post<'a>(ctx: &Ctx<'_>, issue: &'a Issue) -> Result<Post<'a>, ApiError> {
    Ok(Post {
        issue,
        sender: view::author(ctx.actor())?,
        app: view::app(ctx.actor()),
        now: ctx.now(),
    })
}

fn create(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let issue = view::pull(ctx)?;
    let request: Create = ctx.body()?;
    if let Some(parent) = request.in_reply_to {
        return reply(ctx, &issue, parent, request.body);
    }
    let commit_id = request.commit_id.as_deref().ok_or_else(|| {
        invalid(
            "PullRequestReviewComment",
            "commit_id",
            "The commit ID is required",
        )
    })?;
    let sha = anchors::commit(ctx.tx(), ctx.git(), &issue, Some(commit_id))?;
    let files = anchors::files(ctx.tx(), ctx.git(), &issue, sha)?;
    let sha = sha.to_string();
    let anchor = request
        .anchor
        .ok_or_else(|| invalid("PullRequestReviewComment", "path", "The path is required"))?;
    let place = anchor.locate(&files)?;
    let post = new_post(ctx, &issue)?;
    let tx = ctx.tx_mut()?;
    let review_id = comment_review(tx, &post, sha.clone())?;
    let (_, comment) = open(tx, &post, review_id, &sha, request.body, place)?;
    events::comment(tx, &issue, post.sender, &comment, "created", None)?;
    created(ctx, &comment)
}

fn comment_review(tx: &mut TxMut<'_>, post: &Post<'_>, sha: String) -> Result<i64, ApiError> {
    let id = tx.next_id();
    let mut review = reviews::new_review(id, post.issue, post.sender, sha, String::new(), post.now);
    review.state = ReviewState::Commented;
    review.submitted_at = Some(post.now);
    tx.table_mut::<Reviews>()
        .insert(review)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(id)
}

#[derive(Deserialize)]
struct Body {
    body: String,
}

fn create_reply(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let issue = view::pull(ctx)?;
    let parent = ctx.param_i64("comment_id")?;
    let Body { body } = ctx.body()?;
    reply(ctx, &issue, parent, body)
}

fn reply(ctx: &mut Ctx<'_>, issue: &Issue, parent: i64, body: String) -> Result<Reply, ApiError> {
    validate_body(&body)?;
    let parent = find(ctx, parent)?;
    if parent.issue_id != issue.id {
        return Err(ApiError::not_found());
    }
    if parent.in_reply_to_id.is_some() || !published(ctx.tx(), &parent) {
        return Err(invalid(
            "PullRequestReviewComment",
            "in_reply_to",
            "Only a published top-level comment can receive replies",
        ));
    }
    let post = new_post(ctx, issue)?;
    let tx = ctx.tx_mut()?;
    let review = comment_review(tx, &post, parent.commit_id.clone())?;
    let id = tx.next_id();
    let comment = ReviewComment {
        id,
        node_id: node_id::entity(node_id::REVIEW_COMMENT, id),
        review_id: Some(review),
        in_reply_to_id: Some(parent.id),
        user_id: post.sender,
        body,
        via_app_id: post.app,
        created_at: post.now,
        updated_at: post.now,
        last_edited_at: None,
        ..(*parent).clone()
    };
    let comment = tx
        .table_mut::<ReviewComments>()
        .insert(comment)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    events::comment(tx, issue, post.sender, &comment, "created", None)?;
    created(ctx, &comment)
}

fn may_change(ctx: &Ctx<'_>, comment: &ReviewComment) -> Result<i64, ApiError> {
    let sender = view::author(ctx.actor())?;
    let pushes = match ctx.actor() {
        Actor::Installation { .. } => true,
        Actor::User { user, .. } => {
            hubdouble_auth::repo_role(ctx.tx(), user.id, ctx.repo()?) >= Some(RepoRole::Write)
        }
        Actor::Anonymous | Actor::App { .. } => false,
    };
    if sender == comment.user_id || pushes {
        Ok(sender)
    } else {
        Err(ApiError::forbidden("Must have push access to repository"))
    }
}

fn edit(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let comment = target(ctx)?;
    let issue = issue(ctx.tx(), &comment)?;
    let sender = may_change(ctx, &comment)?;
    let Body { body } = ctx.body()?;
    validate_body(&body)?;
    if body == comment.body {
        return answer(ctx, &comment);
    }
    let mut edited = (*comment).clone();
    edited.body = body;
    edited.updated_at = ctx.now();
    edited.last_edited_at = Some(edited.updated_at);
    let tx = ctx.tx_mut()?;
    tx.table_mut::<ReviewComments>()
        .replace(edited.clone())
        .map_err(|e| ApiError::internal(e.to_string()))?;
    if published(tx, &edited) {
        events::comment(tx, &issue, sender, &edited, "edited", Some(&comment.body))?;
    }
    answer(ctx, &edited)
}

fn delete(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let comment = target(ctx)?;
    let issue = issue(ctx.tx(), &comment)?;
    let sender = may_change(ctx, &comment)?;
    let tx = ctx.tx_mut()?;
    if published(tx, &comment) {
        events::comment(tx, &issue, sender, &comment, "deleted", None)?;
    }
    remove(tx, &comment);
    Ok(Reply::empty())
}

fn content(text: &str) -> Result<ReactionContent, ApiError> {
    ReactionContent::from_rest(text)
        .ok_or_else(|| invalid("Reaction", "content", "Invalid reaction content"))
}

fn reactions(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let comment = target(ctx)?;
    let content = ctx
        .query()
        .get("content")
        .and_then(ReactionContent::from_rest);
    let rows: Vec<_> = ctx
        .tx()
        .table::<Reactions>()
        .children(comment.id)
        .filter(|r| content.is_none_or(|content| r.content == content))
        .collect();
    let view = view::rest_view(ctx)?;
    ctx.page(&rows, |r| {
        hubdouble_render::Reaction::new(r, view.user(r.user_id))
    })
}

#[derive(Deserialize)]
struct React {
    content: String,
}

fn react(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let comment = target(ctx)?;
    let sender = view::author(ctx.actor())?;
    let request: React = ctx.body()?;
    let content = content(&request.content)?;
    let now = ctx.now();
    let existing = ctx
        .tx()
        .table::<Reactions>()
        .children(comment.id)
        .find(|r| r.user_id == sender && r.content == content)
        .cloned();
    let (reaction, status) = if let Some(reaction) = existing {
        (reaction, StatusCode::OK)
    } else {
        let tx = ctx.tx_mut()?;
        let id = tx.next_id();
        let reaction = Reaction {
            id,
            node_id: node_id::entity(node_id::REACTION, id),
            repo_id: comment.repo_id,
            subject_kind: ReactionSubjectKind::ReviewComment,
            subject_id: comment.id,
            user_id: sender,
            content,
            created_at: now,
        };
        let reaction = tx
            .table_mut::<Reactions>()
            .insert(reaction)
            .map_err(|e| ApiError::internal(e.to_string()))?;
        (reaction, StatusCode::CREATED)
    };
    let response = hubdouble_render::Reaction::new(&reaction, view::rest_view(ctx)?.user(sender));
    Ok(Reply::json(&response)?.status(status))
}

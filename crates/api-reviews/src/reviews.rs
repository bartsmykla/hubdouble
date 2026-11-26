use std::sync::Arc;

use hubdouble_api_pulls::{StateKind, review_dismissed, via_app};
use hubdouble_model::{ApiError, FieldError, Issue, Review, ReviewState, Timestamp, node_id};
use hubdouble_rest::{Ctx, Registry, Reply};
use hubdouble_store::{Issues, ReviewComments, Reviews, TxMut};
use serde::Deserialize;

use crate::{READ, WRITE, anchors, comments, events, invalid, view};

pub fn register(registry: &mut Registry) {
    for (id, grant, handler) in [
        ("pulls/list-reviews", READ, list as fn(&mut Ctx<'_>) -> _),
        ("pulls/create-review", WRITE, |ctx| simple(create(ctx))),
        ("pulls/get-review", READ, get),
        ("pulls/delete-pending-review", WRITE, |ctx| {
            simple(delete(ctx))
        }),
        ("pulls/submit-review", WRITE, |ctx| simple(submit(ctx))),
        ("pulls/update-review", WRITE, |ctx| simple(edit(ctx))),
        ("pulls/dismiss-review", WRITE, |ctx| simple(dismiss(ctx))),
    ] {
        registry.handle(id, grant, handler);
    }
}

fn simple(answer: Result<Reply, ApiError>) -> Result<Reply, ApiError> {
    answer.map_err(|mut e| {
        if e.status == 422 {
            let text = std::mem::replace(&mut e.message, "Unprocessable Entity".to_owned());
            e.errors = vec![FieldError {
                resource: String::new(),
                field: String::new(),
                code: String::new(),
                message: Some(text),
                value: None,
            }];
        }
        e
    })
}

pub fn visible(viewer: Option<i64>, review: &Review) -> bool {
    review.state != ReviewState::Pending || viewer == Some(review.user_id)
}

pub fn event_state(event: &str) -> Result<ReviewState, ApiError> {
    match event {
        "APPROVE" => Ok(ReviewState::Approved),
        "REQUEST_CHANGES" => Ok(ReviewState::ChangesRequested),
        "COMMENT" => Ok(ReviewState::Commented),
        _ => Err(invalid(
            "PullRequestReview",
            "event",
            "Invalid review event",
        )),
    }
}

fn list(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let issue = view::pull(ctx)?;
    let viewer = ctx.actor().account().map(|a| a.id);
    let view = view::rest_view(ctx)?;
    let mut reviews: Vec<_> = ctx
        .tx()
        .table::<Reviews>()
        .children(issue.id)
        .filter(|review| visible(viewer, review))
        .collect();
    reviews.sort_unstable_by_key(|r| (r.submitted_at.unwrap_or(r.created_at), r.id));
    ctx.page(&reviews, |r| view.review(r, issue.number))
}

fn find(ctx: &Ctx<'_>, issue: &Issue) -> Result<Arc<Review>, ApiError> {
    let id = ctx.param_i64("review_id")?;
    let viewer = ctx.actor().account().map(|a| a.id);
    ctx.tx()
        .table::<Reviews>()
        .get(id)
        .filter(|review| review.issue_id == issue.id && visible(viewer, review))
        .cloned()
        .ok_or_else(ApiError::not_found)
}

fn get(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let issue = view::pull(ctx)?;
    let review = find(ctx, &issue)?;
    Reply::json(&view::rest_view(ctx)?.review(&review, issue.number))
}

#[derive(Deserialize)]
struct Create {
    commit_id: Option<String>,
    #[serde(default)]
    body: String,
    event: Option<String>,
    #[serde(default)]
    comments: Vec<comments::Draft>,
}

pub fn new_review(
    id: i64,
    issue: &Issue,
    sender: i64,
    sha: String,
    body: String,
    now: Timestamp,
) -> Review {
    Review {
        id,
        node_id: node_id::entity(node_id::REVIEW, id),
        repo_id: issue.repo_id,
        issue_id: issue.id,
        user_id: sender,
        body,
        state: ReviewState::Pending,
        commit_id: Some(sha),
        submitted_at: None,
        created_at: now,
        last_edited_at: None,
    }
}

pub fn no_pending(tx: &hubdouble_store::Tx, issue: &Issue, sender: i64) -> Result<(), ApiError> {
    if tx
        .table::<Reviews>()
        .children(issue.id)
        .any(|r| r.user_id == sender && r.state == ReviewState::Pending)
    {
        return Err(invalid(
            "PullRequestReview",
            "user",
            "You can only have one pending review per pull request",
        ));
    }
    Ok(())
}

fn create(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let issue = view::pull(ctx)?;
    let request: Create = ctx.body()?;
    if !request.body.is_empty() || !request.comments.is_empty() {
        comments::refuse_locked(ctx, &issue)?;
    }
    let sender = view::author(ctx.actor())?;
    let app = view::app(ctx.actor());
    let now = ctx.now();
    let state = request.event.as_deref().map(event_state).transpose()?;
    no_pending(ctx.tx(), &issue, sender)?;
    let sha = anchors::commit(ctx.tx(), ctx.git(), &issue, request.commit_id.as_deref())?;
    let files = if request.comments.is_empty() {
        Vec::new()
    } else {
        anchors::files(ctx.tx(), ctx.git(), &issue, sha)?
    };
    let sha = sha.to_string();
    let tx = ctx.tx_mut()?;
    let id = tx.next_id();
    let review = tx
        .table_mut::<Reviews>()
        .insert(new_review(
            id,
            &issue,
            sender,
            sha.clone(),
            request.body,
            now,
        ))
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let post = comments::Post {
        issue: &issue,
        sender,
        app,
        now,
    };
    for draft in request.comments {
        let placement = draft.anchor.locate(&files)?;
        comments::open(tx, &post, id, &sha, draft.body, placement)?;
    }
    let review = if let Some(state) = state {
        publish(tx, &issue, &review, state, None, now)?
    } else {
        review
    };
    Reply::json(&view::rest_view(ctx)?.review(&review, issue.number))
}

#[derive(Deserialize)]
struct Submit {
    body: Option<String>,
    event: String,
}

fn submit(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let issue = view::pull(ctx)?;
    let review = find(ctx, &issue)?;
    let request: Submit = ctx.body()?;
    if request.body.as_deref().is_some_and(|b| !b.is_empty()) {
        comments::refuse_locked(ctx, &issue)?;
    }
    let state = event_state(&request.event)?;
    let now = ctx.now();
    let review = publish(ctx.tx_mut()?, &issue, &review, state, request.body, now)?;
    Reply::json(&view::rest_view(ctx)?.review(&review, issue.number))
}

pub fn publish(
    tx: &mut TxMut<'_>,
    issue: &Arc<Issue>,
    review: &Review,
    state: ReviewState,
    body: Option<String>,
    now: Timestamp,
) -> Result<Arc<Review>, ApiError> {
    if review.state != ReviewState::Pending {
        return Err(invalid(
            "PullRequestReview",
            "event",
            "Only a pending review can be submitted",
        ));
    }
    let mut submitted = review.clone();
    if let Some(body) = body {
        submitted.body = body;
    }
    let comments: Vec<_> = tx
        .table::<ReviewComments>()
        .children(issue.id)
        .filter(|comment| comment.review_id == Some(review.id))
        .cloned()
        .collect();
    if (state == ReviewState::ChangesRequested
        || (state == ReviewState::Commented && comments.is_empty()))
        && submitted.body.trim().is_empty()
    {
        return Err(invalid(
            "PullRequestReview",
            "body",
            "A body is required for this review",
        ));
    }
    if state != ReviewState::Commented && issue.user_id == review.user_id {
        return Err(invalid(
            "PullRequestReview",
            "event",
            "You cannot approve or request changes on your own pull request",
        ));
    }
    submitted.state = state;
    submitted.submitted_at = Some(now);
    let submitted = Arc::new(submitted);
    tx.table_mut::<Reviews>()
        .replace(submitted.clone())
        .map_err(|e| ApiError::internal(e.to_string()))?;
    hubdouble_api_pulls::review_submitted(tx, submitted.id);
    let issue = remove_requested_user(tx, issue, review.user_id)?;
    events::review(tx, &issue, review.user_id, &submitted, "submitted", None)?;
    for comment in comments {
        let mut comment = (*comment).clone();
        comment.updated_at = now;
        let comment = Arc::new(comment);
        tx.table_mut::<ReviewComments>()
            .replace(comment.clone())
            .map_err(|e| ApiError::internal(e.to_string()))?;
        events::comment(tx, &issue, review.user_id, &comment, "created", None)?;
    }
    Ok(submitted)
}

fn remove_requested_user(
    tx: &mut TxMut<'_>,
    issue: &Arc<Issue>,
    user: i64,
) -> Result<Arc<Issue>, ApiError> {
    let pull = view::pull_fields(issue)?;
    if !pull.requested_reviewers.contains(&user) {
        return Ok(issue.clone());
    }
    let mut pull = pull.clone();
    pull.requested_reviewers.retain(|&id| id != user);
    let issue = Arc::new(Issue {
        pull: Some(pull),
        ..(**issue).clone()
    });
    tx.table_mut::<Issues>()
        .replace(issue.clone())
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(issue)
}

fn delete(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let issue = view::pull(ctx)?;
    let review = find(ctx, &issue)?;
    if review.state != ReviewState::Pending {
        return Err(invalid(
            "PullRequestReview",
            "state",
            "Only a pending review can be deleted",
        ));
    }
    let tx = ctx.tx_mut()?;
    let comments: Vec<_> = tx
        .table::<ReviewComments>()
        .children(issue.id)
        .filter(|comment| comment.review_id == Some(review.id))
        .cloned()
        .collect();
    for comment in comments {
        comments::remove(tx, &comment);
    }
    tx.table_mut::<Reviews>().remove(review.id);
    Reply::json(&view::rest_view(ctx)?.review(&review, issue.number))
}

#[derive(Deserialize)]
struct Body {
    body: String,
}

fn edit(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let issue = view::pull(ctx)?;
    let review = find(ctx, &issue)?;
    let sender = view::author(ctx.actor())?;
    if sender != review.user_id {
        return Err(ApiError::forbidden("Only the review author can edit it"));
    }
    let Body { body } = ctx.body()?;
    if body == review.body {
        return Reply::json(&view::rest_view(ctx)?.review(&review, issue.number));
    }
    let mut edited = (*review).clone();
    edited.body = body;
    edited.last_edited_at = Some(ctx.now());
    let tx = ctx.tx_mut()?;
    tx.table_mut::<Reviews>()
        .replace(edited.clone())
        .map_err(|e| ApiError::internal(e.to_string()))?;
    if edited.state != ReviewState::Pending {
        events::review(tx, &issue, sender, &edited, "edited", Some(&review.body))?;
    }
    Reply::json(&view::rest_view(ctx)?.review(&edited, issue.number))
}

#[derive(Deserialize)]
struct Dismiss {
    message: String,
}

fn dismiss(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let issue = view::pull(ctx)?;
    let review = find(ctx, &issue)?;
    let sender = view::author(ctx.actor())?;
    let Dismiss { message } = ctx.body()?;
    if message.trim().is_empty()
        || !matches!(
            review.state,
            ReviewState::Approved | ReviewState::ChangesRequested
        )
    {
        return Err(invalid(
            "PullRequestReview",
            "message",
            "A dismissal requires a review decision and a message",
        ));
    }
    let mut dismissed = (*review).clone();
    dismissed.state = ReviewState::Dismissed;
    let kind = StateKind::ReviewDismissed {
        review_id: review.id,
        state: if review.state == ReviewState::Approved {
            "approved"
        } else {
            "changes_requested"
        },
        message: Some(message),
        commit: None,
    };
    let (app, now) = (via_app(ctx.actor()), ctx.now());
    let tx = ctx.tx_mut()?;
    tx.table_mut::<Reviews>()
        .replace(dismissed.clone())
        .map_err(|e| ApiError::internal(e.to_string()))?;
    review_dismissed(tx, &issue, (sender, app), kind, now)?;
    events::review(tx, &issue, sender, &dismissed, "dismissed", None)?;
    Reply::json(&view::rest_view(ctx)?.review(&dismissed, issue.number))
}

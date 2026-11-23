use std::any::Any;
use std::collections::HashSet;
use std::sync::Arc;

use hubdouble_auth::Target as AuthTarget;
use hubdouble_git::{CommitNode, ObjectData};
use hubdouble_gql::{
    Args, Ctx, ErrorType, GqlError, MutCtx, Obj, Registry, Resolved, Target, connection,
};
use hubdouble_model::{
    AccountKind, ApiError, Issue, Repo, Review, ReviewComment, ReviewState, ReviewThread, node_id,
};
use hubdouble_store::{Accounts, Issues, Repos, ReviewComments, ReviewThreads, Reviews};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{READ, WRITE, anchors, comments, events, reviews, view};

const REVIEW: &str = "PullRequestReview";
const COMMENT: &str = "PullRequestReviewComment";
const THREAD: &str = "PullRequestReviewThread";
const ADD: &str = "AddPullRequestReviewThreadPayload";
const RESOLVE: &str = "ResolveReviewThreadPayload";
const UNRESOLVE: &str = "UnresolveReviewThreadPayload";

pub fn register(r: &mut Registry) {
    r.object::<Review>(REVIEW);
    r.object::<ReviewComment>(COMMENT);
    r.object::<ReviewThread>(THREAD);
    r.node(node_id::REVIEW, REVIEW, review_node);
    r.node(node_id::REVIEW_COMMENT, COMMENT, comment_node);
    r.node(node_id::REVIEW_THREAD, THREAD, thread_node);
    r.field("PullRequest", "reviews", pull_reviews);
    r.field("PullRequest", "reviewThreads", pull_threads);
    review_fields(r);
    comment_fields(r);
    thread_fields(r);
    r.mutation("addPullRequestReviewThread", add_thread);
    r.mutation("resolveReviewThread", |ctx, args| resolve(ctx, args, true));
    r.mutation("unresolveReviewThread", |ctx, args| {
        resolve(ctx, args, false)
    });
    for name in [ADD, RESOLVE, UNRESOLVE] {
        r.object::<ThreadPayload>(name);
        r.field(name, "thread", |_, p, _| {
            Ok(object(THREAD, &p.get::<ThreadPayload>()?.thread))
        });
    }
}

fn review_fields(r: &mut Registry) {
    r.field(REVIEW, "author", |ctx, p, _| {
        Ok(account(ctx, p.get::<Review>()?.user_id))
    });
    r.field(REVIEW, "databaseId", |_, p, _| leaf(p.get::<Review>()?.id));
    r.field(REVIEW, "body", |_, p, _| leaf(&p.get::<Review>()?.body));
    r.field(REVIEW, "lastEditedAt", |_, p, _| {
        leaf(p.get::<Review>()?.last_edited_at)
    });
    r.field(REVIEW, "submittedAt", |_, p, _| {
        leaf(p.get::<Review>()?.submitted_at)
    });
    r.field(REVIEW, "authorAssociation", |ctx, p, _| {
        let review = p.get::<Review>()?;
        association(ctx, review.repo_id, review.user_id)
    });
}

fn association(ctx: &Ctx<'_>, repo_id: i64, user_id: i64) -> Result<Resolved, GqlError> {
    let repo = ctx
        .tx
        .table::<Repos>()
        .get(repo_id)
        .ok_or_else(ApiError::not_found)?;
    leaf(hubdouble_api_pulls::association(ctx.tx, repo, user_id))
}

fn comment_fields(r: &mut Registry) {
    r.field(COMMENT, "author", |ctx, p, _| {
        Ok(account(ctx, p.get::<ReviewComment>()?.user_id))
    });
    r.field(COMMENT, "authorAssociation", |ctx, p, _| {
        let comment = p.get::<ReviewComment>()?;
        association(ctx, comment.repo_id, comment.user_id)
    });
    r.field(COMMENT, "body", |_, p, _| {
        leaf(&p.get::<ReviewComment>()?.body)
    });
    r.field(COMMENT, "createdAt", |_, p, _| {
        leaf(p.get::<ReviewComment>()?.created_at)
    });
    r.field(COMMENT, "databaseId", |_, p, _| {
        leaf(p.get::<ReviewComment>()?.id)
    });
    r.field(COMMENT, "id", |_, p, _| {
        leaf(&p.get::<ReviewComment>()?.node_id)
    });
    r.field(COMMENT, "lastEditedAt", |_, p, _| {
        leaf(p.get::<ReviewComment>()?.last_edited_at)
    });
    r.field(COMMENT, "originalCommit", original_commit);
    r.field(COMMENT, "pullRequest", |ctx, p, _| {
        pull(ctx, p.get::<ReviewComment>()?.issue_id)
    });
    r.field(COMMENT, "pullRequestReview", |ctx, p, _| {
        let review = p.get::<ReviewComment>()?.review_id;
        Ok(review
            .and_then(|id| ctx.tx.table::<Reviews>().get(id))
            .map_or(Resolved::Null, |r| object(REVIEW, r)))
    });
    r.field(COMMENT, "replyTo", |ctx, p, _| {
        let parent = p.get::<ReviewComment>()?.in_reply_to_id;
        Ok(parent
            .and_then(|id| ctx.tx.table::<ReviewComments>().get(id))
            .map_or(Resolved::Null, |c| object(COMMENT, c)))
    });
    r.field(COMMENT, "url", comment_url);
}

fn thread_fields(r: &mut Registry) {
    r.field(THREAD, "comments", thread_comments);
    r.field(THREAD, "diffSide", |_, p, _| {
        leaf(p.get::<ReviewThread>()?.diff_side)
    });
    r.field(THREAD, "id", |_, p, _| {
        leaf(&p.get::<ReviewThread>()?.node_id)
    });
    r.field(THREAD, "isOutdated", |_, p, _| {
        leaf(p.get::<ReviewThread>()?.is_outdated)
    });
    r.field(THREAD, "isResolved", |_, p, _| {
        leaf(p.get::<ReviewThread>()?.is_resolved)
    });
    r.field(THREAD, "line", |_, p, _| {
        leaf(p.get::<ReviewThread>()?.line)
    });
    r.field(THREAD, "originalLine", |_, p, _| {
        leaf(p.get::<ReviewThread>()?.original_line)
    });
    r.field(THREAD, "path", |_, p, _| {
        leaf(&p.get::<ReviewThread>()?.path)
    });
    r.field(THREAD, "pullRequest", |ctx, p, _| {
        pull(ctx, p.get::<ReviewThread>()?.issue_id)
    });
    r.field(THREAD, "subjectType", |_, p, _| {
        leaf(p.get::<ReviewThread>()?.subject_type)
    });
}

fn leaf(value: impl Serialize) -> Result<Resolved, GqlError> {
    match events::object(&value)? {
        Value::Null => Ok(Resolved::Null),
        value => Ok(Resolved::Leaf(value)),
    }
}

fn object<T: Any + Send + Sync>(type_name: &'static str, value: &Arc<T>) -> Resolved {
    Resolved::Object {
        type_name,
        value: value.clone(),
    }
}

fn not_found() -> GqlError {
    ApiError::not_found().into()
}

fn read(ctx: &Ctx<'_>, id: i64) -> Result<(), GqlError> {
    let repo = ctx.tx.table::<Repos>().get(id).ok_or_else(not_found)?;
    hubdouble_auth::authorize(ctx.tx, ctx.actor, &READ, AuthTarget::Repo(repo))?;
    Ok(())
}

fn viewer(ctx: &Ctx<'_>) -> Option<i64> {
    ctx.actor.account().map(|account| account.id)
}

fn account(ctx: &Ctx<'_>, id: i64) -> Resolved {
    ctx.tx
        .table::<Accounts>()
        .get(id)
        .map_or(Resolved::Null, |account| {
            object(
                match account.kind {
                    AccountKind::User => "User",
                    AccountKind::Bot => "Bot",
                    AccountKind::Organization => "Organization",
                },
                account,
            )
        })
}

fn pull(ctx: &Ctx<'_>, id: i64) -> Result<Resolved, GqlError> {
    let issue = ctx
        .tx
        .table::<Issues>()
        .get(id)
        .filter(|i| i.pull.is_some())
        .ok_or_else(not_found)?;
    Ok(object("PullRequest", issue))
}

fn review_node(ctx: &Ctx<'_>, id: i64) -> Result<Resolved, GqlError> {
    let review = ctx
        .tx
        .table::<Reviews>()
        .get(id)
        .filter(|r| reviews::visible(viewer(ctx), r))
        .ok_or_else(not_found)?;
    read(ctx, review.repo_id)?;
    Ok(object(REVIEW, review))
}

fn comment_node(ctx: &Ctx<'_>, id: i64) -> Result<Resolved, GqlError> {
    let comment = ctx
        .tx
        .table::<ReviewComments>()
        .get(id)
        .filter(|comment| comments::visible(ctx.tx, viewer(ctx), comment))
        .ok_or_else(not_found)?;
    read(ctx, comment.repo_id)?;
    Ok(object(COMMENT, comment))
}

fn thread_visible(ctx: &Ctx<'_>, thread: &ReviewThread) -> bool {
    let viewer = viewer(ctx);
    ctx.tx
        .table::<ReviewComments>()
        .children(thread.issue_id)
        .any(|comment| comment.thread_id == thread.id && comments::visible(ctx.tx, viewer, comment))
}

fn thread_node(ctx: &Ctx<'_>, id: i64) -> Result<Resolved, GqlError> {
    let thread = ctx
        .tx
        .table::<ReviewThreads>()
        .get(id)
        .filter(|thread| thread_visible(ctx, thread))
        .ok_or_else(not_found)?;
    read(ctx, thread.repo_id)?;
    Ok(object(THREAD, thread))
}

fn pull_reviews(ctx: &Ctx<'_>, parent: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let issue = parent.get::<Issue>()?;
    read(ctx, issue.repo_id)?;
    let viewer = viewer(ctx);
    let reviews: Vec<_> = ctx
        .tx
        .table::<Reviews>()
        .children(issue.id)
        .filter(|review| reviews::visible(viewer, review))
        .collect();
    connection(
        "PullRequestReviewConnection",
        &reviews,
        args,
        |r| r.node_id.clone(),
        |r| object(REVIEW, r),
    )
}

fn pull_threads(ctx: &Ctx<'_>, parent: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let issue = parent.get::<Issue>()?;
    read(ctx, issue.repo_id)?;
    let viewer = viewer(ctx);
    let visible: HashSet<_> = ctx
        .tx
        .table::<ReviewComments>()
        .children(issue.id)
        .filter(|c| comments::visible(ctx.tx, viewer, c))
        .map(|c| c.thread_id)
        .collect();
    let threads: Vec<_> = ctx
        .tx
        .table::<ReviewThreads>()
        .children(issue.id)
        .filter(|thread| visible.contains(&thread.id))
        .collect();
    connection(
        "PullRequestReviewThreadConnection",
        &threads,
        args,
        |t| t.node_id.clone(),
        |t| object(THREAD, t),
    )
}

fn thread_comments(ctx: &Ctx<'_>, parent: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let thread = parent.get::<ReviewThread>()?;
    let viewer = viewer(ctx);
    let comments: Vec<_> = ctx
        .tx
        .table::<ReviewComments>()
        .children(thread.issue_id)
        .filter(|c| c.thread_id == thread.id && comments::visible(ctx.tx, viewer, c))
        .collect();
    connection(
        "PullRequestReviewCommentConnection",
        &comments,
        args,
        |c| c.node_id.clone(),
        |c| object(COMMENT, c),
    )
}

fn original_commit(ctx: &Ctx<'_>, parent: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let comment = parent.get::<ReviewComment>()?;
    let issue = ctx
        .tx
        .table::<Issues>()
        .get(comment.issue_id)
        .ok_or_else(not_found)?;
    let repo_id = view::pull_fields(issue)?
        .head
        .repo_id
        .unwrap_or(issue.repo_id);
    let Ok(id) = comment.original_commit_id.parse() else {
        return Ok(Resolved::Null);
    };
    let Ok(found) = ctx.git.object(ctx.tx, repo_id, id) else {
        return Ok(Resolved::Null);
    };
    let ObjectData::Commit(commit) = found.data else {
        return Ok(Resolved::Null);
    };
    Ok(object(
        "Commit",
        &Arc::new(CommitNode {
            repo_id,
            id,
            commit,
        }),
    ))
}

fn comment_url(ctx: &Ctx<'_>, parent: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let comment = parent.get::<ReviewComment>()?;
    let repo = ctx
        .tx
        .table::<Repos>()
        .get(comment.repo_id)
        .ok_or_else(not_found)?;
    let issue = ctx
        .tx
        .table::<Issues>()
        .get(comment.issue_id)
        .ok_or_else(not_found)?;
    let view = view::View::new(ctx.tx, ctx.urls, repo)?;
    leaf(format!(
        "{}/{}/pull/{}#discussion_r{}",
        ctx.urls.web, view.name, issue.number, comment.id
    ))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreadPayload {
    #[serde(skip)]
    thread: Arc<ReviewThread>,
    client_mutation_id: Option<String>,
}

fn payload(
    name: &'static str,
    thread: Arc<ReviewThread>,
    client_mutation_id: Option<String>,
) -> Resolved {
    object(
        name,
        &Arc::new(ThreadPayload {
            thread,
            client_mutation_id,
        }),
    )
}

fn input<T: serde::de::DeserializeOwned>(args: &Args) -> Result<T, GqlError> {
    args.get("input")?
        .ok_or_else(|| GqlError::new(ErrorType::Unprocessable, "An input is required"))
}

fn get_node<T: Any + Send + Sync>(
    ctx: &MutCtx<'_>,
    id: &str,
    expected: &str,
) -> Result<Arc<T>, GqlError> {
    match ctx.load(Target::Node(id))? {
        Resolved::Object { type_name, value } if type_name == expected => {
            value.downcast().map_err(|_| not_found())
        }
        _ => Err(not_found()),
    }
}

fn writable(ctx: &MutCtx<'_>, id: i64) -> Result<Arc<Repo>, GqlError> {
    let repo = ctx.tx.table::<Repos>().get(id).ok_or_else(not_found)?;
    ctx.authorize(&WRITE, repo)?;
    Ok(repo.clone())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddThread {
    pull_request_review_id: Option<String>,
    pull_request_id: Option<String>,
    client_mutation_id: Option<String>,
    body: String,
    path: String,
    line: Option<i64>,
    start_line: Option<i64>,
    side: Option<hubdouble_model::Side>,
    start_side: Option<hubdouble_model::Side>,
    subject_type: Option<String>,
}

fn add_thread(ctx: &mut MutCtx<'_>, args: &Args) -> Result<Resolved, GqlError> {
    let input: AddThread = input(args)?;
    let sender = view::author(ctx.actor)?;
    let review = input
        .pull_request_review_id
        .as_deref()
        .map(|id| get_node::<Review>(ctx, id, REVIEW))
        .transpose()?;
    let issue = if let Some(review) = &review {
        if review.state != ReviewState::Pending || review.user_id != sender {
            return Err(GqlError::new(
                ErrorType::Unprocessable,
                "The review must be pending and belong to the viewer",
            ));
        }
        ctx.tx
            .table::<Issues>()
            .get(review.issue_id)
            .cloned()
            .ok_or_else(not_found)?
    } else if let Some(id) = &input.pull_request_id {
        get_node(ctx, id, "PullRequest")?
    } else {
        return Err(GqlError::new(
            ErrorType::Unprocessable,
            "A pull request or review is required",
        ));
    };
    writable(ctx, issue.repo_id)?;
    let now = ctx.clock.now();
    let commit_id = review.as_ref().and_then(|r| r.commit_id.as_deref());
    let sha = anchors::commit(&ctx.tx, ctx.git, &issue, commit_id)?;
    let files = anchors::files(&ctx.tx, ctx.git, &issue, sha)?;
    let sha = sha.to_string();
    let place = anchors::Anchor {
        path: input.path,
        line: input.line,
        start_line: input.start_line,
        side: input.side,
        start_side: input.start_side,
        position: None,
        subject_type: input.subject_type.as_deref().map(|s| {
            if s == "FILE" {
                hubdouble_model::SubjectType::File
            } else {
                hubdouble_model::SubjectType::Line
            }
        }),
    }
    .locate(&files)?;
    let id = if let Some(review) = review {
        review.id
    } else {
        reviews::no_pending(&ctx.tx, &issue, sender)?;
        let id = ctx.tx.next_id();
        let row = reviews::new_review(id, &issue, sender, sha.clone(), String::new(), now);
        ctx.tx
            .table_mut::<Reviews>()
            .insert(row)
            .map_err(|e| ApiError::internal(e.to_string()))?;
        id
    };
    let post = comments::Post {
        issue: &issue,
        sender,
        app: view::app(ctx.actor),
        now,
    };
    let (thread, _) = comments::open(&mut ctx.tx, &post, id, &sha, input.body, place)?;
    Ok(payload(ADD, thread, input.client_mutation_id))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Resolve {
    thread_id: String,
    client_mutation_id: Option<String>,
}

fn resolve(ctx: &mut MutCtx<'_>, args: &Args, resolve: bool) -> Result<Resolved, GqlError> {
    let input: Resolve = input(args)?;
    let thread = get_node::<ReviewThread>(ctx, &input.thread_id, THREAD)?;
    writable(ctx, thread.repo_id)?;
    let name = if resolve { RESOLVE } else { UNRESOLVE };
    if resolve == thread.is_resolved {
        return Ok(payload(name, thread, input.client_mutation_id));
    }
    let issue = ctx
        .tx
        .table::<Issues>()
        .get(thread.issue_id)
        .cloned()
        .ok_or_else(not_found)?;
    let sender = view::author(ctx.actor)?;
    let updated = Arc::new(ReviewThread {
        is_resolved: resolve,
        resolved_by: resolve.then_some(sender),
        ..(*thread).clone()
    });
    ctx.tx
        .table_mut::<ReviewThreads>()
        .replace(updated.clone())
        .map_err(|e| ApiError::internal(e.to_string()))?;
    events::thread(&mut ctx.tx, &issue, sender, &updated)?;
    Ok(payload(name, updated, input.client_mutation_id))
}

use std::sync::Arc;

use hubdouble_model::{
    ApiError, Issue, IssueComment, ReactionSubjectKind, Repo, RepoRole, node_id,
};
use hubdouble_rest::{Ctx, PageLimit, Reply};
use hubdouble_store::{IssueComments, Issues, Reactions, Tx, TxMut};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::shared::{
    Who, changed_from, comment_view, created, emit, full_name, invalid, issue_view, payload_urls,
    to_value, who,
};

pub(crate) fn comment_and_issue(
    tx: &Tx,
    repo: &Repo,
    id: i64,
) -> Result<(Arc<IssueComment>, Arc<Issue>), ApiError> {
    let comment = tx
        .table::<IssueComments>()
        .get(id)
        .filter(|c| c.repo_id == repo.id)
        .cloned()
        .ok_or_else(ApiError::not_found)?;
    let issue = tx
        .table::<Issues>()
        .get(comment.issue_id)
        .cloned()
        .ok_or_else(ApiError::not_found)?;
    Ok((comment, issue))
}

fn comment_event(
    tx: &mut TxMut<'_>,
    repo: &Repo,
    issue: &Issue,
    comment: &IssueComment,
    action: &'static str,
    sender: i64,
    changes: Option<Map<String, Value>>,
) -> Result<(), ApiError> {
    let urls = payload_urls(tx);
    let full = full_name(tx, repo);
    let mut p = Map::new();
    p.insert(
        "issue".to_owned(),
        to_value(&issue_view(tx, &urls, repo, &full, issue))?,
    );
    p.insert(
        "comment".to_owned(),
        to_value(&comment_view(tx, &urls, repo, &full, issue, comment))?,
    );
    if let Some(c) = changes {
        p.insert("changes".to_owned(), Value::Object(c));
    }
    emit(tx, "issue_comment", action, repo, sender, p);
    Ok(())
}

fn may_change(who: &Who, comment: &IssueComment) -> Result<(), ApiError> {
    if comment.user_id == who.account.id || who.can(RepoRole::Write) {
        Ok(())
    } else {
        Err(ApiError::forbidden("Must have push access to repository"))
    }
}

pub(crate) fn list(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?.clone();
    let number = ctx.param_i64("issue_number")?;
    let since = ctx.since("https://docs.github.com/rest/issues/comments#list-issue-comments")?;
    let tx = ctx.tx();
    let issue = tx
        .table::<Issues>()
        .by_number(repo.id, number)
        .cloned()
        .ok_or_else(ApiError::not_found)?;
    let comments: Vec<&Arc<IssueComment>> = tx
        .table::<IssueComments>()
        .children(issue.id)
        .filter(|c| since.is_none_or(|s| s.admits(c.updated_at)))
        .collect();
    let urls = ctx.urls();
    let full = full_name(tx, &repo);
    ctx.page(&comments, |c| {
        comment_view(tx, urls, &repo, &full, &issue, c)
    })
}

pub(crate) fn list_for_repo(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?.clone();
    let since = ctx.since(
        "https://docs.github.com/rest/issues/comments#list-issue-comments-for-a-repository",
    )?;
    let q = ctx.query();
    let tx = ctx.tx();
    let mut comments: Vec<&Arc<IssueComment>> = tx
        .table::<IssueComments>()
        .in_repo(repo.id)
        .filter(|c| since.is_none_or(|s| s.admits(c.updated_at)))
        .collect();
    if q.get("sort") == Some("updated") {
        comments.sort_by_key(|c| (c.updated_at, c.id));
    } else {
        comments.sort_by_key(|c| (c.created_at, c.id));
    }
    if q.get("direction") == Some("desc") {
        comments.reverse();
    }
    let issues = tx.table::<Issues>();
    let urls = ctx.urls();
    let full = full_name(tx, &repo);
    let rendered: Vec<_> = comments
        .iter()
        .filter_map(|c| {
            let issue = issues.get(c.issue_id)?;
            Some((Arc::clone(c), Arc::clone(issue)))
        })
        .collect();
    ctx.page_within(&REPOSITORY_COMMENTS, &rendered, |(c, issue)| {
        comment_view(tx, urls, &repo, &full, issue, c)
    })
}

const REPOSITORY_COMMENTS: PageLimit = PageLimit {
    results: 30_000,
    lookahead: 0,
    message: "In order to keep the API fast for everyone, pagination is limited for this resource.",
    documentation_url: "https://docs.github.com/v3/#pagination",
    before_overflow: true,
};

pub(crate) fn get(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?.clone();
    let tx = ctx.tx();
    let (comment, issue) = comment_and_issue(tx, &repo, ctx.param_i64("comment_id")?)?;
    Reply::json(&comment_view(
        tx,
        ctx.urls(),
        &repo,
        &full_name(tx, &repo),
        &issue,
        &comment,
    ))
}

#[derive(Deserialize)]
struct CommentBody {
    body: Option<String>,
}

fn body_of(ctx: &Ctx<'_>) -> Result<String, ApiError> {
    let input: CommentBody = ctx.body()?;
    input
        .body
        .ok_or_else(|| invalid("IssueComment", "body", "missing_field"))
}

pub(crate) fn create(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let body = body_of(ctx)?;
    let repo = ctx.repo()?.clone();
    let number = ctx.param_i64("issue_number")?;
    let who = who(ctx, &repo)?;
    let urls = ctx.urls().clone();
    let now = ctx.now();
    let issue = ctx
        .tx()
        .table::<Issues>()
        .by_number(repo.id, number)
        .cloned()
        .ok_or_else(ApiError::not_found)?;
    hubdouble_auth::refuse_locked(ctx.tx(), ctx.actor(), &repo, issue.locked)?;
    let tx = ctx.tx_mut()?;
    let id = tx.next_id();
    let comment = tx.table_mut::<IssueComments>().insert(IssueComment {
        id,
        node_id: node_id::entity(node_id::ISSUE_COMMENT, id),
        repo_id: repo.id,
        issue_id: issue.id,
        user_id: who.account.id,
        body,
        via_app_id: who.via_app,
        created_at: now,
        updated_at: now,
        last_edited_at: None,
    })?;
    comment_event(tx, &repo, &issue, &comment, "created", who.account.id, None)?;
    let author = who.account.id;
    hubdouble_api_pulls::record_mentions(tx, &issue, author, (None, Some(&comment.body)), now)?;
    let view = comment_view(tx, &urls, &repo, &full_name(tx, &repo), &issue, &comment);
    created(&view, &view.url)
}

pub(crate) fn update(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let body = body_of(ctx)?;
    let repo = ctx.repo()?.clone();
    let id = ctx.param_i64("comment_id")?;
    let who = who(ctx, &repo)?;
    let urls = ctx.urls().clone();
    let now = ctx.now();
    let tx = ctx.tx_mut()?;
    let (old, issue) = comment_and_issue(tx, &repo, id)?;
    may_change(&who, &old)?;
    let comment = if body == old.body {
        old
    } else {
        let comment = Arc::new(IssueComment {
            body,
            updated_at: now,
            last_edited_at: Some(now),
            ..IssueComment::clone(&old)
        });
        tx.table_mut::<IssueComments>().replace(comment.clone())?;
        let changes = changed_from("body", Value::from(old.body.clone()));
        comment_event(
            tx,
            &repo,
            &issue,
            &comment,
            "edited",
            who.account.id,
            Some(changes),
        )?;
        hubdouble_api_pulls::record_mentions(
            tx,
            &issue,
            who.account.id,
            (Some(&old.body), Some(&comment.body)),
            now,
        )?;
        comment
    };
    Reply::json(&comment_view(
        tx,
        &urls,
        &repo,
        &full_name(tx, &repo),
        &issue,
        &comment,
    ))
}

pub(crate) fn delete(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?.clone();
    let id = ctx.param_i64("comment_id")?;
    let who = who(ctx, &repo)?;
    let tx = ctx.tx_mut()?;
    let (comment, issue) = comment_and_issue(tx, &repo, id)?;
    may_change(&who, &comment)?;
    comment_event(tx, &repo, &issue, &comment, "deleted", who.account.id, None)?;
    let reactions: Vec<i64> = tx
        .table::<Reactions>()
        .children(comment.id)
        .filter(|r| r.subject_kind == ReactionSubjectKind::IssueComment)
        .map(|r| r.id)
        .collect();
    let mut table = tx.table_mut::<Reactions>();
    for r in reactions {
        table.remove(r);
    }
    tx.table_mut::<IssueComments>().remove(comment.id);
    Ok(Reply::empty())
}

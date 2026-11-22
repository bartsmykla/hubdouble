use std::sync::Arc;

use hubdouble_admin::{Admin, SeedCtx};
use hubdouble_model::{AccountKind, ApiError, Issue, ReviewState};
use hubdouble_store::{Accounts, Issues, Repos, Reviews, Tx};
use serde::Deserialize;
use serde_json::Value;

use crate::{anchors, comments, events, reviews};

pub fn register(admin: &mut Admin) {
    admin.kind("review", review);
    admin.kind("review_thread", thread);
}

#[derive(Deserialize)]
struct PullKey {
    repository: String,
    pull_number: i64,
}

impl PullKey {
    fn find(&self, tx: &Tx) -> Result<Arc<Issue>, ApiError> {
        let unknown = || ApiError::unprocessable("No such pull request");
        let (owner, name) = self.repository.split_once('/').ok_or_else(unknown)?;
        let owner = tx.table::<Accounts>().by_login(owner).ok_or_else(unknown)?;
        let repo = tx
            .table::<Repos>()
            .by_name(owner.id, name)
            .ok_or_else(unknown)?;
        tx.table::<Issues>()
            .by_number(repo.id, self.pull_number)
            .filter(|i| i.pull.is_some())
            .cloned()
            .ok_or_else(unknown)
    }
}

#[derive(Deserialize)]
struct ReviewSeed {
    id: Option<i64>,
    #[serde(flatten)]
    pull: PullKey,
    author: String,
    #[serde(default)]
    body: String,
    event: Option<String>,
    commit_id: Option<String>,
}

fn review(ctx: &mut SeedCtx<'_>, body: Value) -> Result<(), ApiError> {
    let seed: ReviewSeed =
        serde_json::from_value(body).map_err(|e| ApiError::unprocessable(e.to_string()))?;
    let issue = seed.pull.find(&ctx.tx)?;
    let sender = ctx
        .tx
        .table::<Accounts>()
        .by_login(&seed.author)
        .filter(|a| a.kind != AccountKind::Organization)
        .ok_or_else(|| ApiError::unprocessable("No such review author"))?
        .id;
    reviews::no_pending(&ctx.tx, &issue, sender)?;
    let sha = anchors::commit(&ctx.tx, ctx.git, &issue, seed.commit_id.as_deref())?.to_string();
    let id = seed.id.unwrap_or_else(|| ctx.tx.next_id());
    let now = ctx.clock.now();
    let review = ctx
        .tx
        .table_mut::<Reviews>()
        .insert(reviews::new_review(id, &issue, sender, sha, seed.body, now))
        .map_err(|e| ApiError::unprocessable(e.to_string()))?;
    if let Some(event) = seed.event {
        reviews::publish(
            &mut ctx.tx,
            &issue,
            &review,
            reviews::event_state(&event)?,
            None,
            now,
        )?;
    }
    Ok(())
}

#[derive(Deserialize)]
struct ThreadSeed {
    id: Option<i64>,
    comment: CommentSeed,
    review_id: i64,
}

#[derive(Deserialize)]
struct CommentSeed {
    id: Option<i64>,
    #[serde(flatten)]
    draft: comments::Draft,
}

fn thread(ctx: &mut SeedCtx<'_>, body: Value) -> Result<(), ApiError> {
    let seed: ThreadSeed =
        serde_json::from_value(body).map_err(|e| ApiError::unprocessable(e.to_string()))?;
    let review = ctx
        .tx
        .table::<Reviews>()
        .get(seed.review_id)
        .cloned()
        .ok_or_else(|| ApiError::unprocessable("No such review"))?;
    let issue = ctx
        .tx
        .table::<Issues>()
        .get(review.issue_id)
        .cloned()
        .ok_or_else(ApiError::not_found)?;
    let sha = anchors::commit(&ctx.tx, ctx.git, &issue, review.commit_id.as_deref())?;
    let files = anchors::files(&ctx.tx, ctx.git, &issue, sha)?;
    let sha = sha.to_string();
    let placement = seed.comment.draft.anchor.locate(&files)?;
    let ids = (
        seed.id.unwrap_or_else(|| ctx.tx.next_id()),
        seed.comment.id.unwrap_or_else(|| ctx.tx.next_id()),
    );
    let post = comments::Post {
        issue: &issue,
        sender: review.user_id,
        app: None,
        now: ctx.clock.now(),
    };
    let (_, comment) = comments::open_with_ids(
        &mut ctx.tx,
        &post,
        review.id,
        &sha,
        seed.comment.draft.body,
        placement,
        ids,
    )?;
    if review.state != ReviewState::Pending {
        events::comment(
            &mut ctx.tx,
            &issue,
            review.user_id,
            &comment,
            "created",
            None,
        )?;
    }
    Ok(())
}

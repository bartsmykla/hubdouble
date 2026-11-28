use std::collections::BTreeSet;

use http::header::LINK;
use hubdouble_model::ApiError;
use hubdouble_render::Repository;
use hubdouble_store::{Accounts, Repos};
use serde::Serialize;

use crate::{Ctx, Page, Reply};

#[derive(Serialize)]
struct Selected {
    total_count: usize,
    repositories: Vec<Repository>,
}

/// One page of the repositories that `ids` names and that still exist, in ID order, as the
/// `GET .../repositories` of a selected list answers it: `{"total_count", "repositories"}` with
/// GitHub's `Link` header.
///
/// # Errors
///
/// As [`Ctx::page_params`], and `500` when the page cannot be serialized.
pub fn selected_repositories(ctx: &Ctx<'_>, ids: &BTreeSet<i64>) -> Result<Reply, ApiError> {
    let Page { page, per_page } = ctx.page_params()?;
    let tx = ctx.tx();
    let (repos, accounts) = (tx.table::<Repos>(), tx.table::<Accounts>());
    let found: Vec<_> = ids
        .iter()
        .filter_map(|id| {
            let repo = repos.get(*id)?;
            Some((repo, accounts.get(repo.owner_id)?))
        })
        .collect();
    let repositories = found
        .iter()
        .skip((page - 1).saturating_mul(per_page))
        .take(per_page)
        .map(|(repo, owner)| Repository::new(ctx.urls(), repo, owner))
        .collect();
    let reply = Reply::json(&Selected {
        total_count: found.len(),
        repositories,
    })?;
    match ctx.link_header(page, found.len().div_ceil(per_page).max(1)) {
        Some(link) => reply.header(LINK, &link),
        None => Ok(reply),
    }
}

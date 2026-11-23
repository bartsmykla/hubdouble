use std::sync::Arc;

use http::header::LINK;
use hubdouble_git::{Commit, ObjectData, ObjectId};
use hubdouble_model::ApiError;
use hubdouble_rest::{Ctx, Page, Reply};
use serde::Serialize;

use super::{COMMIT_FILES, DiffEntry, RepoCommit, View, explicit_page, slice};
use crate::common::{commit_at, network_repo};

const COMPARE_COMMITS: usize = 250;

#[derive(Serialize)]
struct Comparison {
    url: String,
    html_url: String,
    permalink_url: String,
    diff_url: String,
    patch_url: String,
    base_commit: RepoCommit,
    merge_base_commit: RepoCommit,
    status: &'static str,
    ahead_by: u64,
    behind_by: u64,
    total_commits: u64,
    commits: Vec<RepoCommit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    files: Option<Vec<DiffEntry>>,
}

fn side(view: &View<'_>, text: &str) -> Option<ObjectId> {
    let (repo, rev) = match text.split_once(':') {
        Some((owner, rest)) => {
            let rest = rest.split_once(':').map_or(rest, |(_, r)| r);
            (network_repo(view.tx, view.repo, owner)?.id, rest)
        }
        None => (view.repo.id, text),
    };
    commit_at(view.git, view.tx, repo, rev).map(|(id, _)| id)
}

pub(crate) fn compare(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let basehead = ctx.param("basehead")?.to_owned();
    let (base, head) = basehead.split_once("...").ok_or_else(ApiError::not_found)?;
    let view = View::of(ctx)?;
    let base_id = side(&view, base).ok_or_else(ApiError::not_found)?;
    let head_id = side(&view, head).ok_or_else(ApiError::not_found)?;
    let cmp = view.git.compare(view.tx, view.repo.id, base_id, head_id)?;
    let merge_base = cmp.merge_base.ok_or_else(|| {
        ApiError::new(
            404,
            format!("No common ancestor between {base} and {head}."),
        )
    })?;
    let load = |id: ObjectId| -> Result<Arc<Commit>, ApiError> {
        match view.git.object(view.tx, view.repo.id, id)?.data {
            ObjectData::Commit(c) => Ok(c),
            _ => Err(ApiError::not_found()),
        }
    };
    let base_commit = load(base_id)?;
    let status = match (cmp.ahead_by, cmp.behind_by) {
        (0, 0) => "identical",
        (_, 0) => "ahead",
        (0, _) => "behind",
        _ => "diverged",
    };
    let page = explicit_page(ctx)?;
    let commits = match page {
        Some(p) => slice(&cmp.commits, p),
        None => &cmp.commits[cmp.commits.len().saturating_sub(COMPARE_COMMITS)..],
    };
    let files = page.is_none_or(|p| p.page == 1).then(|| {
        let shown = &cmp.files[..cmp.files.len().min(COMMIT_FILES)];
        shown
            .iter()
            .map(|f| DiffEntry::new(&view.links, head_id, f))
            .collect()
    });
    let links = &view.links;
    let body = Comparison {
        url: format!("{}/compare/{basehead}", links.api),
        html_url: format!("{}/compare/{basehead}", links.web),
        permalink_url: format!("{}/compare/{base_id}...{head_id}", links.web),
        diff_url: format!("{}/compare/{basehead}.diff", links.web),
        patch_url: format!("{}/compare/{basehead}.patch", links.web),
        base_commit: view.commit(base_id, &base_commit),
        merge_base_commit: view.commit(merge_base, &*load(merge_base)?),
        status,
        ahead_by: cmp.ahead_by,
        behind_by: cmp.behind_by,
        total_commits: cmp.ahead_by,
        commits: commits.iter().map(|(id, c)| view.commit(*id, c)).collect(),
        files,
    };
    let reply = Reply::json(&body)?;
    let Some(Page { page, per_page }) = page else {
        return Ok(reply);
    };
    let last = cmp.commits.len().div_ceil(per_page).max(1);
    if last <= 1 && page <= 1 {
        return Ok(reply);
    }
    let mut rels = vec![(last, "last")];
    if page < last {
        rels.push((page + 1, "next"));
    }
    rels.push((1, "first"));
    if page > 1 {
        rels.push((page - 1, "prev"));
    }
    reply.header(LINK, &ctx.links(&rels))
}

use std::cmp::Ordering;
use std::sync::Arc;

use http::header::LINK;
use hubdouble_git::ObjectId;
use hubdouble_model::{ApiError, Issue, IssueState};
use hubdouble_rest::{Ctx, Page, Reply};
use hubdouble_store::{IssueComments, Issues};
use serde::Serialize;

use crate::{render, state};

pub(crate) fn issue<'a>(ctx: &'a Ctx<'_>) -> Result<&'a Arc<Issue>, ApiError> {
    state::numbered(ctx.tx(), ctx.repo()?.id, ctx.param_i64("pull_number")?)
}

pub(crate) fn get(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let issue = issue(ctx)?;
    Reply::json(&render::detail(ctx.git(), ctx.tx(), ctx.urls(), issue)?)
}

pub(crate) fn list(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?;
    let filter = ListFilter::parse(ctx)?;
    let mut pulls: Vec<_> = ctx
        .tx()
        .table::<Issues>()
        .in_repo(repo.id)
        .filter(|issue| filter.matches(ctx, issue))
        .collect();
    pulls.sort_unstable_by(|a, b| filter.compare(ctx, a, b));
    pull_page(ctx, &pulls)
}

struct ListFilter<'a> {
    state: &'a str,
    head: Option<(&'a str, &'a str)>,
    base: Option<&'a str>,
    sort: &'a str,
    ascending: bool,
}

impl<'a> ListFilter<'a> {
    fn parse(ctx: &'a Ctx<'_>) -> Result<Self, ApiError> {
        let query = ctx.query();
        let state = query
            .get("state")
            .filter(|&state| matches!(state, "closed" | "all"))
            .unwrap_or("open");
        let sort = query.get("sort").filter(|sort| !sort.is_empty());
        let direction =
            query
                .get("direction")
                .unwrap_or(if sort.is_some() { "asc" } else { "desc" });
        let head = query
            .get("head")
            .and_then(|value| value.split_once(':'))
            .filter(|(_, branch)| !branch.is_empty());
        if !matches!(direction, "asc" | "desc") {
            return Err(ApiError::unprocessable("Invalid pull request filter"));
        }
        Ok(Self {
            state,
            head,
            base: query.get("base"),
            sort: sort.unwrap_or("created"),
            ascending: direction == "asc",
        })
    }

    fn matches(&self, ctx: &Ctx<'_>, issue: &Issue) -> bool {
        let Some(p) = &issue.pull else { return false };
        if self.state != "all" && issue.state.as_str() != self.state {
            return false;
        }
        if self.base.is_some_and(|base| p.base.ref_name != base) {
            return false;
        }
        if let Some((login, name)) = self.head {
            let tx = ctx.tx();
            let owner = p
                .head
                .repo_id
                .and_then(|id| tx.table::<hubdouble_store::Repos>().get(id))
                .and_then(|repo| tx.table::<hubdouble_store::Accounts>().get(repo.owner_id));
            if p.head.ref_name != name
                || !owner.is_some_and(|a| a.login.eq_ignore_ascii_case(login))
            {
                return false;
            }
        }
        if self.sort == "long-running" {
            let month_ago = ctx.now().plus_seconds(-30 * 24 * 60 * 60);
            return issue.state == IssueState::Open
                && issue.created_at < month_ago
                && issue.updated_at >= month_ago;
        }
        true
    }

    fn compare(&self, ctx: &Ctx<'_>, a: &Issue, b: &Issue) -> Ordering {
        let count = |i: &Issue| ctx.tx().table::<IssueComments>().children(i.id).count();
        let order = match self.sort {
            "updated" => a.updated_at.cmp(&b.updated_at),
            "popularity" => count(a).cmp(&count(b)),
            _ => a.created_at.cmp(&b.created_at),
        }
        .then_with(|| a.number.cmp(&b.number));
        if self.ascending {
            order
        } else {
            order.reverse()
        }
    }
}

fn pull_page(ctx: &Ctx<'_>, pulls: &[&Arc<Issue>]) -> Result<Reply, ApiError> {
    let name = render::full_name(ctx.tx(), ctx.repo()?.id)?;
    page(ctx, pulls, |issue| {
        render::pull(ctx.tx(), ctx.urls(), issue, &name)
    })
}

fn page<'a, T, R: Serialize>(
    ctx: &Ctx<'_>,
    items: &'a [T],
    render: impl FnMut(&'a T) -> Result<R, ApiError>,
) -> Result<Reply, ApiError> {
    let Page { page, per_page } = ctx.page_params();
    let start = (page - 1).saturating_mul(per_page).min(items.len());
    let end = start.saturating_add(per_page).min(items.len());
    let answer: Vec<_> = items[start..end]
        .iter()
        .map(render)
        .collect::<Result<_, _>>()?;
    let reply = Reply::json(&answer)?;
    match ctx.link_header(page, items.len().div_ceil(per_page).max(1)) {
        Some(link) => reply.header(LINK, &link),
        None => Ok(reply),
    }
}

pub(crate) fn files(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let issue = issue(ctx)?;
    let pull = state::pull(issue)?;
    let comparison = state::compare(ctx.git(), ctx.tx(), issue)?;
    let full = render::full_name(ctx.tx(), issue.repo_id)?;
    let base = comparison
        .merge_base
        .map_or_else(|| pull.base.sha.clone(), |id| id.to_string());
    page(
        ctx,
        &comparison.files[..comparison.files.len().min(3_000)],
        |file| Ok(render::file(ctx.urls(), &full, &base, &pull.head.sha, file)),
    )
}

pub(crate) fn commits(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let issue = issue(ctx)?;
    let comparison = state::compare(ctx.git(), ctx.tx(), issue)?;
    let full = render::full_name(ctx.tx(), issue.repo_id)?;
    page(
        ctx,
        &comparison.commits[..comparison.commits.len().min(250)],
        |(id, commit)| {
            Ok(render::commit(
                ctx.git(),
                ctx.tx(),
                ctx.urls(),
                &full,
                issue.repo_id,
                *id,
                commit,
            ))
        },
    )
}

pub(crate) fn for_commit(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?;
    let git = ctx.git();
    let tx = ctx.tx();
    let rev = ctx.param("commit_sha")?;
    let oid = if let Ok(id) = rev.parse::<ObjectId>() {
        git.peel_to_commit(tx, repo.id, id)?
    } else {
        let id = state::branch(git, tx, repo.id, rev)?;
        git.peel_to_commit(tx, repo.id, id)?
    };
    let in_default = match state::branch(git, tx, repo.id, &repo.default_branch) {
        Ok(tip) => git.compare(tx, repo.id, tip, oid)?.ahead_by == 0,
        Err(hubdouble_git::GitError::NotFound) => false,
        Err(e) => return Err(e.into()),
    };
    let sha = oid.to_string();
    let mut found = Vec::new();
    for issue in tx.table::<Issues>().in_repo(repo.id) {
        let Some(p) = &issue.pull else { continue };
        if p.merged_at.is_none() && (in_default || issue.state == IssueState::Closed) {
            continue;
        }
        if p.merge_commit_sha.as_ref() == Some(&sha)
            || p.head.sha == sha
            || state::compare(git, tx, issue)?
                .commits
                .iter()
                .any(|(id, _)| *id == oid)
        {
            found.push(issue);
        }
    }
    if in_default {
        found.sort_unstable_by_key(|i| {
            i.pull.as_ref().map(|p| {
                (
                    p.base.ref_name != repo.default_branch,
                    p.merged_at,
                    i.number,
                )
            })
        });
        found.truncate(1);
    } else {
        found.sort_unstable_by_key(|i| std::cmp::Reverse(i.number));
    }
    pull_page(ctx, &found)
}

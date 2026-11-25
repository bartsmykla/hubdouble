use http::{StatusCode, header::LINK};
use hubdouble_api_pulls::Reviewer;
use hubdouble_model::{AccountKind, ApiError, RepoRole};
use hubdouble_render::{SimpleUser, Team};
use hubdouble_rest::{Ctx, Registry, Reply};
use hubdouble_store::{Accounts, Teams};
use serde::{Deserialize, Serialize};

use crate::{READ, WRITE, invalid, view};

pub fn register(registry: &mut Registry) {
    registry.handle("pulls/list-requested-reviewers", READ, list);
    registry.handle("pulls/request-reviewers", WRITE, add);
    registry.handle("pulls/remove-requested-reviewers", WRITE, remove);
}

#[derive(Default, Serialize)]
struct Reviewers {
    users: Vec<SimpleUser>,
    teams: Vec<Team>,
}

fn list(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let issue = view::pull(ctx)?;
    let pull = view::pull_fields(&issue)?;
    let view = view::rest_view(ctx)?;
    let page = ctx.page_params()?;
    let skip = (page.page - 1).saturating_mul(page.per_page);
    let reviewers = pull
        .requested_reviewers
        .iter()
        .map(|&id| (false, id))
        .chain(pull.requested_teams.iter().map(|&id| (true, id)));
    let mut response = Reviewers::default();
    for (team, id) in reviewers.skip(skip).take(page.per_page) {
        if team {
            response.teams.extend(view.full_team(id));
        } else {
            response.users.extend(view.user(id));
        }
    }
    let reply = Reply::json(&response)?;
    let count = pull.requested_reviewers.len() + pull.requested_teams.len();
    if let Some(link) = ctx.link_header(page.page, count.div_ceil(page.per_page).max(1)) {
        reply.header(LINK, &link)
    } else {
        Ok(reply)
    }
}

#[derive(Deserialize)]
struct Request {
    reviewers: Option<Vec<String>>,
    #[serde(default)]
    team_reviewers: Vec<String>,
}

fn add(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    change(ctx, true).map(|reply| reply.status(StatusCode::CREATED))
}

fn remove(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    change(ctx, false)
}

fn change(ctx: &mut Ctx<'_>, add: bool) -> Result<Reply, ApiError> {
    let issue = view::pull(ctx)?;
    let request: Request = ctx.body()?;
    if !add && request.reviewers.is_none() {
        return Err(invalid(
            "PullRequest",
            "reviewers",
            "\"reviewers\" wasn't supplied",
        ));
    }
    let repo = ctx.repo()?;
    let mut ids = Vec::new();
    for login in request.reviewers.unwrap_or_default() {
        let user = ctx
            .tx()
            .table::<Accounts>()
            .by_login(&login)
            .filter(|user| {
                user.kind != AccountKind::Organization
                    && (!add
                        || (user.id != issue.user_id
                            && hubdouble_auth::repo_role(ctx.tx(), user.id, repo)
                                >= Some(RepoRole::Write)))
            })
            .ok_or_else(|| {
                invalid(
                    "PullRequest",
                    "reviewers",
                    "A reviewer cannot access this pull request",
                )
            })?;
        ids.push(Reviewer::User(user.id));
    }
    for slug in request.team_reviewers {
        let team = ctx
            .tx()
            .table::<Teams>()
            .by_slug(repo.owner_id, &slug)
            .ok_or_else(|| {
                invalid(
                    "PullRequest",
                    "team_reviewers",
                    "A team does not belong to this organization",
                )
            })?;
        ids.push(Reviewer::Team(team.id));
    }
    let who = (view::author(ctx.actor())?, view::app(ctx.actor()));
    let (tx, git) = ctx.tx_mut_and_git()?;
    let issue = hubdouble_api_pulls::change_reviewers(tx, git, &issue, who, &ids, add)?;
    Reply::json(&view::rest_view(ctx)?.pull(&issue)?)
}

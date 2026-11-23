use http::{StatusCode, header::LINK};
use hubdouble_model::{AccountKind, ApiError, RepoRole};
use hubdouble_render::{SimpleUser, Team};
use hubdouble_rest::{Ctx, Registry, Reply};
use hubdouble_store::{Accounts, Issues, Teams};
use serde::{Deserialize, Serialize};
use serde_json::Map;

use crate::{READ, WRITE, events, invalid, view};

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
    let mut issue = (*view::pull(ctx)?).clone();
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
        ids.push((false, user.id));
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
        ids.push((true, team.id));
    }
    let sender = view::author(ctx.actor())?;
    let pull = issue.pull.as_mut().ok_or_else(ApiError::not_found)?;
    ids.retain(|&(team, id)| {
        let requested = if team {
            &mut pull.requested_teams
        } else {
            &mut pull.requested_reviewers
        };
        match (add, requested.iter().position(|&value| value == id)) {
            (true, None) => {
                requested.push(id);
                true
            }
            (false, Some(index)) => {
                requested.remove(index);
                true
            }
            _ => false,
        }
    });
    if !ids.is_empty() {
        let tx = ctx.tx_mut()?;
        tx.table_mut::<Issues>()
            .replace(issue.clone())
            .map_err(|e| ApiError::internal(e.to_string()))?;
        let action = if add {
            "review_requested"
        } else {
            "review_request_removed"
        };
        for (team, id) in ids {
            events::send(tx, &issue, sender, "pull_request", action, |view| {
                let (key, reviewer) = if team {
                    let mut team = events::object(&view.full_team(id))?;
                    events::webhook_team(&mut team);
                    ("requested_team", team)
                } else {
                    ("requested_reviewer", events::object(&view.user(id))?)
                };
                Ok(Map::from_iter([(key.to_owned(), reviewer)]))
            })?;
        }
    }
    Reply::json(&view::rest_view(ctx)?.pull(&issue)?)
}

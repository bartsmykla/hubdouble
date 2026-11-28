use std::sync::Arc;

use hubdouble_auth::Actor;
use hubdouble_model::{ApiError, Issue, Pull, PullRef, Repo, Review, ReviewComment};
use hubdouble_render::{
    PullBranch, PullRequest, ReactionRollup, Repository, SimpleUser, Team, Urls, escape,
};
use hubdouble_rest::Ctx;
use hubdouble_store::{
    Accounts, IssueComments, Issues, Reactions, Repos, ReviewComments, Teams, Tx,
};

pub fn pull(ctx: &Ctx<'_>) -> Result<Arc<Issue>, ApiError> {
    ctx.tx()
        .table::<Issues>()
        .by_number(ctx.repo()?.id, ctx.param_i64("pull_number")?)
        .filter(|issue| issue.pull.is_some())
        .cloned()
        .ok_or_else(ApiError::not_found)
}

pub fn pull_fields(issue: &Issue) -> Result<&Pull, ApiError> {
    issue.pull.as_ref().ok_or_else(ApiError::not_found)
}

pub fn author(actor: &Actor) -> Result<i64, ApiError> {
    actor
        .account()
        .map(|a| a.id)
        .ok_or_else(ApiError::unauthorized)
}

pub fn app(actor: &Actor) -> Option<i64> {
    match actor {
        Actor::Installation { app, .. } => Some(app.id),
        _ => None,
    }
}

pub struct View<'a> {
    pub tx: &'a Tx,
    pub urls: &'a Urls,
    pub repo: &'a Repo,
    pub name: String,
}

impl<'a> View<'a> {
    pub fn new(tx: &'a Tx, urls: &'a Urls, repo: &'a Repo) -> Result<Self, ApiError> {
        let owner = tx
            .table::<Accounts>()
            .get(repo.owner_id)
            .ok_or_else(ApiError::not_found)?;
        Ok(Self {
            tx,
            urls,
            repo,
            name: format!("{}/{}", escape(&owner.login), escape(&repo.name)),
        })
    }

    pub fn user(&self, id: i64) -> Option<SimpleUser> {
        self.tx
            .table::<Accounts>()
            .get(id)
            .map(|user| SimpleUser::new(self.urls, user))
    }

    pub fn author(&self, id: i64) -> SimpleUser {
        SimpleUser::new(self.urls, self.tx.table::<Accounts>().author(id))
    }

    pub fn full_team(&self, id: i64) -> Option<Team> {
        let teams = self.tx.table::<Teams>();
        let team = teams.get(id)?;
        let org = self.tx.table::<Accounts>().get(team.org_id)?;
        let parent = team.parent_id.and_then(|id| teams.get(id)).map(Arc::as_ref);
        Some(Team::new(self.urls, team, org, parent))
    }

    pub fn review(&self, review: &Review, number: i64) -> hubdouble_render::Review {
        let mut view = hubdouble_render::Review::new(
            self.urls,
            review,
            &self.name,
            number,
            Some(self.author(review.user_id)),
        );
        view.author_association =
            hubdouble_api_pulls::association(self.tx, self.repo, review.user_id);
        view
    }

    pub fn comment(&self, comment: &ReviewComment, number: i64) -> hubdouble_render::ReviewComment {
        let mut view = hubdouble_render::ReviewComment::new(
            self.urls,
            comment,
            &self.name,
            number,
            Some(self.author(comment.user_id)),
        );
        view.author_association =
            hubdouble_api_pulls::association(self.tx, self.repo, comment.user_id);
        let mut reactions = self.tx.table::<Reactions>().children(comment.id);
        if let Some(first) = reactions.next() {
            view.reactions = ReactionRollup::new(
                format!("{}/reactions", view.url),
                std::iter::once(first).chain(reactions).map(Arc::as_ref),
            );
        }
        view
    }

    fn branch(&self, reference: &PullRef) -> Result<PullBranch, ApiError> {
        let repo = reference
            .repo_id
            .and_then(|id| self.tx.table::<Repos>().get(id));
        let owner = self
            .tx
            .table::<Accounts>()
            .get(repo.map_or(self.repo.owner_id, |r| r.owner_id))
            .ok_or_else(ApiError::not_found)?;
        let repo = repo.map(|repo| Repository::new(self.urls, repo, owner));
        Ok(PullBranch::new(self.urls, reference, owner, repo))
    }

    pub fn pull(&self, issue: &Issue) -> Result<serde_json::Value, ApiError> {
        let pull = pull_fields(issue)?;
        let mut response = PullRequest::new(
            self.urls,
            issue,
            &self.name,
            Some(self.author(issue.user_id)),
            self.branch(&pull.head)?,
            self.branch(&pull.base)?,
        )
        .ok_or_else(ApiError::not_found)?;
        response.requested_reviewers = pull
            .requested_reviewers
            .iter()
            .filter_map(|&id| self.user(id))
            .collect();
        response.merged_by = pull.merged_by.map(|id| self.author(id));
        response.author_association =
            hubdouble_api_pulls::association(self.tx, self.repo, issue.user_id);
        response.mergeable_state = hubdouble_api_pulls::mergeable_state(self.tx, issue);
        response.comments =
            i64::try_from(self.tx.table::<IssueComments>().children(issue.id).count())
                .unwrap_or(i64::MAX);
        response.review_comments = i64::try_from(
            self.tx
                .table::<ReviewComments>()
                .children(issue.id)
                .filter(|comment| crate::comments::visible(self.tx, None, comment))
                .count(),
        )
        .unwrap_or(i64::MAX);
        let mut response = crate::events::object(&response)?;
        if !pull.requested_teams.is_empty() {
            let teams: Vec<_> = pull
                .requested_teams
                .iter()
                .filter_map(|&id| self.full_team(id))
                .collect();
            response["requested_teams"] = crate::events::object(&teams)?;
        }
        Ok(response)
    }
}

pub fn rest_view<'a>(ctx: &'a Ctx<'_>) -> Result<View<'a>, ApiError> {
    View::new(ctx.tx(), ctx.urls(), ctx.repo()?)
}

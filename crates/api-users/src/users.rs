use hubdouble_auth::Actor;
use hubdouble_model::{Account, ApiError, Requirement, Scope, Timestamp, Visibility};
use hubdouble_render::{SimpleUser, Urls};
use hubdouble_rest::{Ctx, Registry, Reply};
use hubdouble_store::{Accounts, Repos, Tx};
use serde::Serialize;

const SIGNED_IN: Requirement = Requirement {
    scope: Scope::User,
    any_of: &[],
    scopes: &[],
};

pub(crate) fn register(registry: &mut Registry) {
    registry.handle("users/get-authenticated", SIGNED_IN, authenticated);
    registry.handle("users/get-by-username", Requirement::public(), by_username);
    registry.handle("users/get-by-id", Requirement::public(), by_id);
}

#[derive(Serialize)]
struct Plan {
    name: &'static str,
    space: i64,
    collaborators: i64,
    private_repos: i64,
}

#[derive(Serialize)]
struct PrivateFields {
    private_gists: i64,
    total_private_repos: usize,
    owned_private_repos: usize,
    disk_usage: i64,
    collaborators: i64,
    two_factor_authentication: bool,
    plan: Plan,
}

#[derive(Serialize)]
pub(crate) struct User {
    #[serde(flatten)]
    simple: SimpleUser,
    name: Option<String>,
    company: Option<String>,
    blog: Option<String>,
    location: Option<String>,
    email: Option<String>,
    hireable: Option<bool>,
    bio: Option<String>,
    twitter_username: Option<String>,
    public_repos: usize,
    public_gists: i64,
    followers: i64,
    following: i64,
    created_at: Timestamp,
    updated_at: Timestamp,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    private: Option<PrivateFields>,
}

pub(crate) fn render(tx: &Tx, urls: &Urls, a: &Account, own: bool) -> User {
    let (public, private) =
        tx.table::<Repos>()
            .children(a.id)
            .fold((0, 0), |(public, private), r| {
                if r.visibility == Visibility::Public {
                    (public + 1, private)
                } else {
                    (public, private + 1)
                }
            });
    let mut simple = SimpleUser::new(urls, a);
    simple.user_view_type = if own { "private" } else { "public" };
    User {
        simple,
        name: a.name.clone(),
        company: a.company.clone(),
        blog: a.blog.clone(),
        location: a.location.clone(),
        email: a.email.clone(),
        hireable: None,
        bio: a.bio.clone(),
        twitter_username: a.twitter_username.clone(),
        public_repos: public,
        public_gists: 0,
        followers: 0,
        following: 0,
        created_at: a.created_at,
        updated_at: a.updated_at,
        private: own.then_some(PrivateFields {
            private_gists: 0,
            total_private_repos: private,
            owned_private_repos: private,
            disk_usage: 0,
            collaborators: 0,
            two_factor_authentication: false,
            plan: Plan {
                name: "free",
                space: 976_562_499,
                collaborators: 0,
                private_repos: 10_000,
            },
        }),
    }
}

fn own(ctx: &Ctx<'_>, a: &Account) -> bool {
    matches!(ctx.actor(), Actor::User { user, .. } if user.id == a.id)
}

fn authenticated(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let Actor::User { user, .. } = ctx.actor() else {
        return Err(ApiError::unauthorized());
    };
    Reply::json(&render(ctx.tx(), ctx.urls(), user, true))
}

fn by_username(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let account = ctx
        .tx()
        .table::<Accounts>()
        .by_login(ctx.param("username")?)
        .ok_or_else(ApiError::not_found)?;
    Reply::json(&render(ctx.tx(), ctx.urls(), account, own(ctx, account)))
}

fn by_id(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let account = ctx
        .tx()
        .table::<Accounts>()
        .get(ctx.param_i64("account_id")?)
        .ok_or_else(ApiError::not_found)?;
    Reply::json(&render(ctx.tx(), ctx.urls(), account, own(ctx, account)))
}

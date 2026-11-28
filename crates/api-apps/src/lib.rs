mod admin;
mod deliveries;
mod events;
mod graphql;
mod installations;

use hubdouble_admin::Registrar;
use hubdouble_auth::Actor;
use hubdouble_model::{ApiError, App, Requirement, Scope};
use hubdouble_render::Integration;
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::{Accounts, Apps, Installations};

pub use deliveries::{DeliveryLog, get_delivery, list_deliveries, redeliver_delivery};

const APP: Requirement = Requirement {
    scope: Scope::App,
    any_of: &[],
    all_of: &[],
    scopes: &[],
};

pub fn register(r: &mut Registrar<'_>) {
    r.rest.handle("apps/get-authenticated", APP, authenticated);
    r.rest
        .handle("apps/get-by-slug", Requirement::public(), by_slug);
    installations::register(r.rest);
    deliveries::register(r.rest);
    graphql::register(r.gql);
    admin::register(r.admin);
}

fn app<'a>(ctx: &'a Ctx<'_>) -> Result<&'a App, ApiError> {
    match ctx.actor() {
        Actor::App { app } => Ok(app),
        _ => Err(ApiError::unauthorized()),
    }
}

fn authenticated(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let app = app(ctx)?;
    let mut answer = integration(ctx, app)?;
    let count = ctx.tx().table::<Installations>().children(app.id).count();
    answer.installations_count = i64::try_from(count).ok();
    Reply::json(&answer)
}

fn by_slug(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let app = ctx
        .tx()
        .table::<Apps>()
        .by_slug(ctx.param("app_slug")?)
        .ok_or_else(ApiError::not_found)?;
    Reply::json(&integration(ctx, app)?)
}

fn integration(ctx: &Ctx<'_>, app: &App) -> Result<Integration, ApiError> {
    let owner = ctx
        .tx()
        .table::<Accounts>()
        .get(app.owner_id)
        .ok_or_else(ApiError::not_found)?;
    Ok(Integration::new(ctx.urls(), app, owner))
}

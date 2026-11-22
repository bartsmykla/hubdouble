use hubdouble_gql::{Args, Ctx, ErrorType, GqlError, Obj, Registry, Resolved};
use hubdouble_model::{Account, AccountKind, App, node_id};
use hubdouble_store::{Accounts, Apps};
use serde_json::json;

pub(crate) fn register(registry: &mut Registry) {
    registry.object::<App>("App");
    registry.node(node_id::APP, "App", app_node);
    registry.field("App", "id", app_id);
    registry.field("App", "databaseId", app_database_id);
    registry.field("App", "url", app_url);
    registry.object::<Account>("Bot");
    registry.node(node_id::BOT, "Bot", bot_node);
    registry.field("Bot", "id", bot_id);
    registry.field("Bot", "databaseId", bot_database_id);
}

fn app_node(ctx: &Ctx<'_>, id: i64) -> Result<Resolved, GqlError> {
    let app = ctx.tx.table::<Apps>().get(id).ok_or_else(not_found)?;
    Ok(Resolved::Object {
        type_name: "App",
        value: app.clone(),
    })
}

fn bot_node(ctx: &Ctx<'_>, id: i64) -> Result<Resolved, GqlError> {
    let account = ctx
        .tx
        .table::<Accounts>()
        .get(id)
        .filter(|account| account.kind == AccountKind::Bot)
        .ok_or_else(not_found)?;
    Ok(Resolved::Object {
        type_name: "Bot",
        value: account.clone(),
    })
}

fn not_found() -> GqlError {
    GqlError::new(ErrorType::NotFound, "No such node")
}

fn app_id(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(Resolved::Leaf(json!(obj.get::<App>()?.node_id)))
}

fn app_database_id(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(Resolved::Leaf(json!(obj.get::<App>()?.id)))
}

fn app_url(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(Resolved::Leaf(json!(obj.get::<App>()?.external_url)))
}

fn bot_id(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(Resolved::Leaf(json!(obj.get::<Account>()?.node_id)))
}

fn bot_database_id(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(Resolved::Leaf(json!(obj.get::<Account>()?.id)))
}

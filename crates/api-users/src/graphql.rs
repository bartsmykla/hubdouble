use hubdouble_auth::Actor;
use hubdouble_gql::{Args, Ctx, ErrorType, GqlError, Obj, Registry, Resolved};
use hubdouble_model::{Account, AccountKind, node_id};
use hubdouble_store::Accounts;
use serde_json::json;

pub(crate) fn register(registry: &mut Registry) {
    registry.object::<Account>("User");
    registry.node(node_id::USER, "User", user_node);
    registry.field("User", "id", id);
    registry.field("User", "databaseId", database_id);
    registry.field("User", "login", user_login);
    registry.field("Bot", "login", bot_login);
    registry.field("Query", "viewer", viewer);
}

fn user_node(ctx: &Ctx<'_>, id: i64) -> Result<Resolved, GqlError> {
    let account = ctx
        .tx
        .table::<Accounts>()
        .get(id)
        .filter(|a| a.kind == AccountKind::User)
        .ok_or_else(|| GqlError::new(ErrorType::NotFound, "No such node"))?;
    Ok(Resolved::Object {
        type_name: "User",
        value: account.clone(),
    })
}

fn id(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(Resolved::Leaf(json!(obj.get::<Account>()?.node_id)))
}

fn database_id(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(Resolved::Leaf(json!(obj.get::<Account>()?.id)))
}

fn user_login(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(Resolved::Leaf(json!(obj.get::<Account>()?.login)))
}

fn bot_login(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let login = &obj.get::<Account>()?.login;
    Ok(Resolved::Leaf(json!(
        login.strip_suffix("[bot]").unwrap_or(login)
    )))
}

fn viewer(ctx: &Ctx<'_>, _: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let account = match ctx.actor {
        Actor::User { user, .. } => user,
        Actor::Installation { bot, .. } => bot,
        Actor::Anonymous | Actor::App { .. } => {
            return Err(GqlError::new(
                ErrorType::Forbidden,
                "This endpoint requires you to be authenticated.",
            ));
        }
    };
    Ok(Resolved::Object {
        type_name: "User",
        value: account.clone(),
    })
}

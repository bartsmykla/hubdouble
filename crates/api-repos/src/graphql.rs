use hubdouble_admin::Registrar;
use hubdouble_gql::{Args, Ctx, ErrorType, GqlError, Obj, Resolved, Target};
use hubdouble_model::{Repo, node_id};
use serde_json::Value;

pub(crate) fn register(r: &mut Registrar<'_>) {
    r.gql.object::<Repo>("Repository");
    r.gql.node(node_id::REPOSITORY, "Repository", node);
    r.gql.field("Query", "repository", repository);
    r.gql.field("Repository", "id", id);
    r.gql.field("Repository", "nameWithOwner", name_with_owner);
}

fn node(ctx: &Ctx<'_>, id: i64) -> Result<Resolved, GqlError> {
    ctx.load(Target::RepoId(id))
}

fn required(args: &Args, name: &str) -> Result<String, GqlError> {
    args.get(name)?.ok_or_else(|| {
        GqlError::new(
            ErrorType::Unprocessable,
            format!("argument {name} is required"),
        )
    })
}

fn repository(ctx: &Ctx<'_>, _: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let owner = required(args, "owner")?;
    let name = required(args, "name")?;
    ctx.load(Target::Repo {
        owner: &owner,
        name: &name,
    })
}

fn id(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(Resolved::Leaf(Value::String(
        obj.get::<Repo>()?.node_id.clone(),
    )))
}

fn name_with_owner(ctx: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let repo = obj.get::<Repo>()?;
    Ok(Resolved::Leaf(Value::String(crate::full_name(
        ctx.tx, repo,
    )?)))
}

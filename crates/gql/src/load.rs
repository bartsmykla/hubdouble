use std::sync::Arc;

use hubdouble_model::node_id::{self, Part};
use hubdouble_model::{Account, AccountKind, Repo};
use hubdouble_store::{Accounts, Repos};

use crate::contract::{Ctx, ErrorType, GqlError, NodeType, Resolved, Target};

pub(crate) fn load(cx: &Ctx<'_>, t: Target<'_>) -> Result<Resolved, GqlError> {
    match t {
        Target::Repo { owner, name } => {
            let not_found = || {
                GqlError::new(
                    ErrorType::NotFound,
                    format!("Could not resolve to a Repository with the name '{owner}/{name}'."),
                )
            };
            let owner_id = cx
                .tx
                .table::<Accounts>()
                .by_login(owner)
                .ok_or_else(not_found)?
                .id;
            let repo = cx.tx.table::<Repos>().by_name(owner_id, name);
            visible(cx, repo).ok_or_else(not_found)
        }
        Target::RepoId(id) => visible(cx, cx.tx.table::<Repos>().get(id)).ok_or_else(|| {
            GqlError::new(
                ErrorType::NotFound,
                format!("Could not resolve to a Repository with the id of '{id}'."),
            )
        }),
        Target::Org(login) => cx
            .tx
            .table::<Accounts>()
            .by_login(login)
            .filter(|a| a.kind == AccountKind::Organization)
            .map(|a| object("Organization", a))
            .ok_or_else(|| {
                GqlError::new(
                    ErrorType::NotFound,
                    format!("Could not resolve to an Organization with the login of '{login}'."),
                )
            }),
        Target::Node(id) => node(cx, id).map(|(r, _)| r),
    }
}

fn visible(cx: &Ctx<'_>, repo: Option<&Arc<Repo>>) -> Option<Resolved> {
    let repo = repo.filter(|r| hubdouble_auth::can_see(cx.tx, cx.actor, r))?;
    Some(Resolved::Object {
        type_name: "Repository",
        value: repo.clone(),
    })
}

fn object(type_name: &'static str, a: &Arc<Account>) -> Resolved {
    Resolved::Object {
        type_name,
        value: a.clone(),
    }
}

pub(crate) fn node(cx: &Ctx<'_>, id: &str) -> Result<(Resolved, Option<String>), GqlError> {
    let not_found = || {
        GqlError::new(
            ErrorType::NotFound,
            format!("Could not resolve to a node with the global id of '{id}'."),
        )
    };
    let nodes = &cx.registry.nodes;
    let (entry, entity, next) = if let Some(n) = node_id::decode(id) {
        let [Part::Int(0), Part::Int(entity)] = n.parts[..] else {
            return Err(not_found());
        };
        let entry = nodes.iter().find(|t| t.prefix == n.prefix);
        (entry, entity, None)
    } else if let Some((type_name, entity)) = node_id::decode_legacy(id) {
        let entry: Option<&NodeType> = nodes.iter().find(|t| t.type_name == type_name);
        let next = entry.map(|t| node_id::entity(t.prefix, entity));
        (entry, entity, next)
    } else {
        return Err(not_found());
    };
    let entry = entry.ok_or_else(not_found)?;
    match (entry.load)(cx, entity) {
        Ok(Resolved::Null) => Err(not_found()),
        Ok(r) => Ok((r, next)),
        Err(e) if e.kind == ErrorType::NotFound => Err(not_found()),
        Err(e) => Err(e),
    }
}

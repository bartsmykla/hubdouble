use std::sync::Arc;

use hubdouble_auth::{self as auth, Target as AuthTarget};
use hubdouble_gql::{
    Args, Conn, Ctx, ErrorType, GqlError, Obj, Registry, Resolved, Target, connection,
};
use hubdouble_model::{Account, AccountKind, Repo, RepoRole, Team, node_id};
use hubdouble_store::{Accounts, Teams};
use serde_json::{Value, json};

use crate::hierarchy::TeamViewer;
use crate::{MEMBERS, hierarchy};

const TEAM_CONNECTION: &str = "TeamConnection";
const COLLABORATORS: &str = "RepositoryCollaboratorConnection";
const COLLABORATOR_EDGE: &str = "RepositoryCollaboratorEdge";

pub fn register(gql: &mut Registry) {
    gql.object::<Account>("Organization");
    gql.node(node_id::ORGANIZATION, "Organization", org_node);
    gql.field("Query", "organization", organization);
    gql.field("Organization", "id", org_id);
    gql.field("Organization", "databaseId", org_database_id);
    gql.field("Organization", "teams", org_teams);
    gql.object::<Team>("Team");
    gql.node(node_id::TEAM, "Team", team_node);
    gql.field("Team", "id", team_id);
    gql.field("Team", "databaseId", team_database_id);
    gql.field("Team", "ancestors", team_ancestors);
    gql.object_resolved::<Conn>(COLLABORATORS);
    gql.object_resolved::<Collaborator>(COLLABORATOR_EDGE);
    gql.field("Repository", "collaborators", collaborators);
    gql.field(COLLABORATORS, "edges", collaborator_edges);
    gql.field(COLLABORATORS, "nodes", collaborator_nodes);
    gql.field(COLLABORATOR_EDGE, "node", collaborator_node);
    gql.field(COLLABORATOR_EDGE, "permission", collaborator_permission);
    gql.field(COLLABORATOR_EDGE, "cursor", collaborator_cursor);
}

fn organization(ctx: &Ctx<'_>, _: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    ctx.load(Target::Org(arg_text(args, "login").unwrap_or_default()))
}

fn org_node(ctx: &Ctx<'_>, id: i64) -> Result<Resolved, GqlError> {
    let org = ctx.tx.table::<Accounts>().get(id).ok_or_else(not_found)?;
    ctx.load(Target::Org(&org.login))
}

fn org_id(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(Resolved::Leaf(json!(obj.get::<Account>()?.node_id)))
}

fn org_database_id(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(Resolved::Leaf(json!(obj.get::<Account>()?.id)))
}

fn team_node(ctx: &Ctx<'_>, id: i64) -> Result<Resolved, GqlError> {
    let team = ctx.tx.table::<Teams>().get(id).ok_or_else(not_found)?;
    authorize_teams(ctx, team.org_id)?;
    if !TeamViewer::new(ctx.tx, ctx.actor, team.org_id).can_see(team) {
        return Err(not_found());
    }
    Ok(team_object(team))
}

fn authorize_teams(ctx: &Ctx<'_>, org_id: i64) -> Result<(), GqlError> {
    let org = ctx
        .tx
        .table::<Accounts>()
        .get(org_id)
        .ok_or_else(not_found)?;
    auth::authorize(ctx.tx, ctx.actor, &MEMBERS, AuthTarget::Org(org)).map_err(GqlError::from)
}

fn org_teams(ctx: &Ctx<'_>, obj: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let org = obj.get::<Account>()?;
    authorize_teams(ctx, org.id)?;
    let viewer = TeamViewer::new(ctx.tx, ctx.actor, org.id);
    let logins = args.get::<Vec<String>>("userLogins")?;
    let accounts = ctx.tx.table::<Accounts>();
    let matches: Option<Vec<i64>> = logins.map(|logins| {
        logins
            .iter()
            .filter_map(|login| accounts.by_login(login))
            .map(|user| user.id)
            .collect()
    });
    let teams: Vec<_> = ctx
        .tx
        .table::<Teams>()
        .children(org.id)
        .filter(|t| viewer.can_see(t))
        .filter(|t| {
            matches
                .as_ref()
                .is_none_or(|ids| t.members.iter().any(|member| ids.contains(&member.user_id)))
        })
        .collect();
    team_page(&teams, args)
}

fn team_id(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(Resolved::Leaf(json!(obj.get::<Team>()?.node_id)))
}

fn team_database_id(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(Resolved::Leaf(json!(obj.get::<Team>()?.id)))
}

fn team_ancestors(ctx: &Ctx<'_>, obj: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let team = obj.get::<Team>()?;
    let viewer = TeamViewer::new(ctx.tx, ctx.actor, team.org_id);
    let parents: Vec<_> = hierarchy::ancestors(ctx.tx.table::<Teams>(), team)
        .filter(|p| viewer.can_see(p))
        .collect();
    team_page(&parents, args)
}

fn team_page(teams: &[&Arc<Team>], args: &Args) -> Result<Resolved, GqlError> {
    connection(
        TEAM_CONNECTION,
        teams,
        args,
        |t| t.id.to_string(),
        |t| team_object(t),
    )
}

fn team_object(team: &Arc<Team>) -> Resolved {
    Resolved::Object {
        type_name: "Team",
        value: team.clone(),
    }
}

struct Collaborator {
    user: Arc<Account>,
    role: RepoRole,
}

fn collaborators(ctx: &Ctx<'_>, obj: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let repo = obj.get::<Repo>()?;
    let accounts = ctx.tx.table::<Accounts>();
    let mut matches = Vec::new();
    let affiliation = arg_text(args, "affiliation");
    let query = arg_text(args, "query").map(str::to_ascii_lowercase);
    let mut add = |user: &Arc<Account>| {
        if user.kind == AccountKind::User
            && matches_affiliation(ctx, repo, user.id, affiliation)
            && query.as_ref().is_none_or(|q| {
                user.login.to_ascii_lowercase().contains(q)
                    || user
                        .name
                        .as_ref()
                        .is_some_and(|name| name.to_ascii_lowercase().contains(q))
            })
            && let Some(role) = auth::repo_role(ctx.tx, user.id, repo)
        {
            matches.push((user.clone(), role));
        }
    };
    if let Some(login) = arg_text(args, "login") {
        if let Some(user) = accounts.by_login(login) {
            add(user);
        }
    } else {
        accounts.iter().for_each(add);
    }
    connection(
        COLLABORATORS,
        &matches,
        args,
        |(user, _)| user.id.to_string(),
        |(user, role)| Resolved::Object {
            type_name: COLLABORATOR_EDGE,
            value: Arc::new(Collaborator {
                user: user.clone(),
                role: *role,
            }),
        },
    )
}

fn matches_affiliation(
    ctx: &Ctx<'_>,
    repo: &Repo,
    user_id: i64,
    affiliation: Option<&str>,
) -> bool {
    match affiliation {
        Some("DIRECT") => {
            repo.owner_id == user_id || repo.collaborators.iter().any(|c| c.user_id == user_id)
        }
        Some("OUTSIDE") => {
            repo.owner_id != user_id
                && hierarchy::org_role(ctx.tx, repo.owner_id, user_id).is_none()
        }
        _ => true,
    }
}

fn collaborator_edges(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(Resolved::List(
        obj.get::<Conn>()?
            .edges
            .iter()
            .map(|edge| edge.node.clone())
            .collect(),
    ))
}

fn collaborator_nodes(ctx: &Ctx<'_>, obj: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let nodes = obj
        .get::<Conn>()?
        .edges
        .iter()
        .map(|edge| match &edge.node {
            Resolved::Object { type_name, value } => collaborator_node(
                ctx,
                &Obj {
                    type_name,
                    value: value.clone(),
                },
                args,
            ),
            _ => Err(GqlError::new(
                ErrorType::Internal,
                "Missing collaborator edge",
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Resolved::List(nodes))
}

fn collaborator_node(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(Resolved::Object {
        type_name: "User",
        value: obj.get::<Collaborator>()?.user.clone(),
    })
}

fn collaborator_permission(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let role = obj.get::<Collaborator>()?.role;
    let value = match role {
        RepoRole::Read => "READ",
        RepoRole::Triage => "TRIAGE",
        RepoRole::Write => "WRITE",
        RepoRole::Maintain => "MAINTAIN",
        RepoRole::Admin => "ADMIN",
    };
    Ok(Resolved::Leaf(json!(value)))
}

fn collaborator_cursor(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(Resolved::Leaf(json!(
        obj.get::<Collaborator>()?.user.id.to_string()
    )))
}

fn arg_text<'a>(args: &'a Args, name: &str) -> Option<&'a str> {
    args.0.get(name).and_then(Value::as_str)
}

fn not_found() -> GqlError {
    GqlError::new(ErrorType::NotFound, "Could not resolve the node")
}

use std::sync::Arc;

use http::{Method, StatusCode};
use hubdouble_admin::{
    Admin, AdminHandler, AdminResponse, SeedCtx, error_response, json_response, read_json,
};
use hubdouble_model::{
    Account, ApiError, Collaborator, FieldError, Membership, MembershipState, OrgRole, Repo,
    RepoRole, Timestamp,
};
use hubdouble_rest::Params;
use hubdouble_store::{Accounts, Deleted, Memberships, Repos, Teams, Tx, TxMut};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;

use crate::{PublicMemberships, admin_teams, events, rest};

pub fn register(admin: &mut Admin) {
    admin.kind("org_membership", seed_membership);
    admin_teams::register(admin);
    let member = "/org/{org}/members/{login}";
    admin.route(Method::GET, member, read(read_membership));
    admin.route(Method::PUT, member, write(put_membership));
    admin.route(Method::DELETE, member, write(remove_membership));
    let collaborator = "/repository/{owner}/{repo}/collaborators/{login}";
    admin.route(Method::PUT, collaborator, write(put_collaborator));
    admin.route(Method::DELETE, collaborator, write(remove_collaborator));
    admin.route(Method::DELETE, "/org/{org}", write(delete_org));
    admin.route(Method::DELETE, "/user/{login}", write(delete_user));
}

fn delete_org(tx: &mut TxMut<'_>, params: &Params, _: Timestamp, _: Value) -> Result<(), ApiError> {
    let org = org(tx, params)?.clone();
    tx.delete(&Deleted::Account(org))
}

fn delete_user(
    tx: &mut TxMut<'_>,
    params: &Params,
    _: Timestamp,
    _: Value,
) -> Result<(), ApiError> {
    let user = rest::user(tx, param(params, "login")?)?.clone();
    tx.delete(&Deleted::Account(user))
}

type Read = fn(&Tx, &Params) -> Result<Value, ApiError>;
type Write = fn(&mut TxMut<'_>, &Params, Timestamp, Value) -> Result<(), ApiError>;

pub fn read(handler: Read) -> AdminHandler {
    Arc::new(move |request| {
        Box::pin(async move {
            match request
                .services
                .store
                .view(|tx| handler(tx, &request.params))
            {
                Ok(value) => json_response(StatusCode::OK, &value),
                Err(error) => error_response(&error),
            }
        })
    })
}

pub fn write(handler: Write) -> AdminHandler {
    Arc::new(move |request| {
        Box::pin(async move {
            let services = request.services;
            let body = match read_json(request.request, services.config.max_body_bytes).await {
                Ok(body) => body,
                Err(error) => return error_response(&error),
            };
            match services
                .store
                .update(|tx| handler(tx, &request.params, services.clock.now(), body))
            {
                Ok(()) => {
                    let mut response = AdminResponse::default();
                    *response.status_mut() = StatusCode::NO_CONTENT;
                    response
                }
                Err(error) => error_response(&error),
            }
        })
    })
}

pub fn decode<T: DeserializeOwned>(value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value).map_err(|e| invalid("body", &e.to_string()))
}

pub fn invalid(field: &str, message: &str) -> ApiError {
    ApiError::unprocessable(message).with_error(FieldError {
        resource: "Organization".to_owned(),
        field: field.to_owned(),
        code: "invalid".to_owned(),
        message: None,
        value: None,
    })
}

pub fn param<'a>(params: &'a Params, name: &str) -> Result<&'a str, ApiError> {
    params.get(name).ok_or_else(ApiError::not_found)
}

pub fn org<'a>(tx: &'a Tx, params: &Params) -> Result<&'a Arc<Account>, ApiError> {
    rest::organization(tx, param(params, "org")?)
}

pub fn sender(tx: &Tx, org: &Account, login: Option<&str>) -> Result<i64, ApiError> {
    if let Some(login) = login {
        return rest::user(tx, login).map(|user| user.id);
    }
    Ok(tx
        .table::<Memberships>()
        .children(org.id)
        .find(|m| m.role == OrgRole::Admin && m.state == MembershipState::Active)
        .map_or(org.id, |m| m.user_id))
}

pub fn repo(tx: &Tx, owner: &str, name: &str) -> Result<Arc<Repo>, ApiError> {
    tx.table::<Accounts>()
        .by_login(owner)
        .and_then(|owner| tx.table::<Repos>().by_name(owner.id, name))
        .cloned()
        .ok_or_else(ApiError::not_found)
}

#[derive(Deserialize)]
struct MembershipSeed {
    org: String,
    login: String,
    #[serde(flatten)]
    membership: MembershipInput,
}

#[derive(Deserialize)]
struct MembershipInput {
    role: Option<OrgRole>,
    state: Option<MembershipState>,
    public: Option<bool>,
    sender: Option<String>,
}

fn seed_membership(ctx: &mut SeedCtx<'_>, body: Value) -> Result<(), ApiError> {
    let seed: MembershipSeed = decode(body)?;
    let now = ctx.clock.now();
    set_membership(&mut ctx.tx, &seed.org, &seed.login, now, &seed.membership)
}

fn put_membership(
    tx: &mut TxMut<'_>,
    params: &Params,
    now: Timestamp,
    body: Value,
) -> Result<(), ApiError> {
    set_membership(
        tx,
        param(params, "org")?,
        param(params, "login")?,
        now,
        &decode(body)?,
    )
}

fn set_membership(
    tx: &mut TxMut<'_>,
    org: &str,
    login: &str,
    now: Timestamp,
    input: &MembershipInput,
) -> Result<(), ApiError> {
    let org = rest::organization(tx, org)?.clone();
    let user = rest::user(tx, login)?.clone();
    let source = events::Source {
        org: &org,
        sender: sender(tx, &org, input.sender.as_deref())?,
    };
    let previous = tx.table::<Memberships>().of(org.id, user.id).cloned();
    let state = input
        .state
        .or_else(|| previous.as_ref().map(|m| m.state))
        .unwrap_or(MembershipState::Active);
    let role = input
        .role
        .or_else(|| previous.as_ref().map(|m| m.role))
        .unwrap_or(OrgRole::Member);
    let old_state = previous.as_ref().map(|m| m.state);
    if old_state == Some(MembershipState::Active) && state == MembershipState::Pending {
        return Err(invalid("state", "An active member cannot be invited again"));
    }
    let id = previous.as_ref().map_or_else(|| tx.next_id(), |m| m.id);
    let membership = Membership {
        id,
        user_id: user.id,
        org_id: org.id,
        role,
        state,
    };
    tx.table_mut::<Memberships>().replace(membership.clone())?;
    if let Some(public) = input.public {
        let mut table = tx.table_mut::<PublicMemberships>();
        let ids = &mut table.get_mut().0;
        if public {
            ids.insert(id);
        } else {
            ids.remove(&id);
        }
    }
    match (old_state, state) {
        (None, MembershipState::Pending) => {
            events::invitation(tx, &source, &membership, &user, now);
        }
        (None | Some(MembershipState::Pending), MembershipState::Active) => {
            events::organization(tx, &source, "member_added", &membership, &user);
        }
        _ => {}
    }
    Ok(())
}

fn read_membership(tx: &Tx, params: &Params) -> Result<Value, ApiError> {
    let org = org(tx, params)?;
    let user = rest::user(tx, param(params, "login")?)?;
    let membership = tx
        .table::<Memberships>()
        .of(org.id, user.id)
        .ok_or_else(ApiError::not_found)?;
    let urls = hubdouble_render::Urls::new(tx.config(), "");
    serde_json::to_value(hubdouble_render::OrgMembership::new(
        &urls, membership, org, user,
    ))
    .map_err(|e| ApiError::internal(e.to_string()))
}

#[derive(Deserialize)]
pub struct Sender {
    pub sender: Option<String>,
}

fn remove_membership(
    tx: &mut TxMut<'_>,
    params: &Params,
    now: Timestamp,
    body: Value,
) -> Result<(), ApiError> {
    let org = org(tx, params)?.clone();
    let user = rest::user(tx, param(params, "login")?)?.clone();
    let input: Sender = decode(body)?;
    let source = events::Source {
        org: &org,
        sender: sender(tx, &org, input.sender.as_deref())?,
    };
    let id = tx
        .table::<Memberships>()
        .of(org.id, user.id)
        .ok_or_else(ApiError::not_found)?
        .id;
    let membership = tx
        .table_mut::<Memberships>()
        .remove(id)
        .ok_or_else(ApiError::not_found)?;
    if tx.table::<PublicMemberships>().0.contains(&id) {
        tx.table_mut::<PublicMemberships>().get_mut().0.remove(&id);
    }
    if membership.state == MembershipState::Active {
        events::organization(tx, &source, "member_removed", &membership, &user);
        let teams: Vec<_> = tx
            .table::<Teams>()
            .children(org.id)
            .filter(|t| t.members.iter().any(|m| m.user_id == user.id))
            .cloned()
            .collect();
        for team in teams {
            admin_teams::change_member(tx, &source, &team, &user, now, None)?;
        }
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct Permission {
    pub permission: RepoRole,
    pub sender: Option<String>,
}

fn put_collaborator(
    tx: &mut TxMut<'_>,
    params: &Params,
    now: Timestamp,
    body: Value,
) -> Result<(), ApiError> {
    let input: Permission = decode(body)?;
    set_collaborator(
        tx,
        params,
        now,
        input.sender.as_deref(),
        Some(input.permission),
    )
}

fn remove_collaborator(
    tx: &mut TxMut<'_>,
    params: &Params,
    now: Timestamp,
    body: Value,
) -> Result<(), ApiError> {
    let input: Sender = decode(body)?;
    set_collaborator(tx, params, now, input.sender.as_deref(), None)
}

fn set_collaborator(
    tx: &mut TxMut<'_>,
    params: &Params,
    now: Timestamp,
    author: Option<&str>,
    role: Option<RepoRole>,
) -> Result<(), ApiError> {
    let repo = repo(tx, param(params, "owner")?, param(params, "repo")?)?;
    let owner = tx
        .table::<Accounts>()
        .get(repo.owner_id)
        .cloned()
        .ok_or_else(ApiError::not_found)?;
    let user = rest::user(tx, param(params, "login")?)?.clone();
    if owner.id == user.id {
        return Err(invalid("login", "The repository owner already has access"));
    }
    let source = events::Source {
        org: &owner,
        sender: sender(tx, &owner, author)?,
    };
    let mut updated = (*repo).clone();
    let old = updated
        .collaborators
        .iter()
        .find(|c| c.user_id == user.id)
        .map(|c| c.role);
    if old == role {
        return Ok(());
    }
    updated.collaborators.retain(|c| c.user_id != user.id);
    if let Some(role) = role {
        updated.collaborators.push(Collaborator {
            user_id: user.id,
            role,
        });
    }
    updated.updated_at = now;
    tx.table_mut::<Repos>().replace(updated)?;
    events::collaborator(tx, &source, repo.id, &user, old, role);
    Ok(())
}

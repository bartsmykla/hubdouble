use hubdouble_model::{Account, Event, Membership, RepoRole, Team, Timestamp, node_id};
use hubdouble_render::{OrgMembership, SimpleUser, TeamSimple, Urls};
use hubdouble_store::{Accounts, Teams, Tx, TxMut};
use serde_json::{Map, Value, json};

pub struct Source<'a> {
    pub org: &'a Account,
    pub sender: i64,
}

impl Source<'_> {
    fn send(
        &self,
        tx: &mut TxMut<'_>,
        event: &'static str,
        action: &'static str,
        repo_id: Option<i64>,
        payload: Value,
    ) {
        let Value::Object(payload) = payload else {
            return;
        };
        hubdouble_hooks::emit(
            tx,
            Event {
                name: event,
                action: Some(action),
                repo_id,
                org_id: Some(self.org.id),
                sender_id: self.sender,
                app_id: None,
                installation_id: None,
                payload,
            },
        );
    }
}

pub fn organization(
    tx: &mut TxMut<'_>,
    source: &Source<'_>,
    action: &'static str,
    membership: &Membership,
    user: &Account,
) {
    let urls = Urls::new(tx.config(), "");
    let mut body = json!(OrgMembership::new(&urls, membership, source.org, user));
    if let Some(fields) = body.as_object_mut() {
        fields.remove("organization");
    }
    source.send(
        tx,
        "organization",
        action,
        None,
        json!({ "membership": body }),
    );
}

pub fn invitation(
    tx: &mut TxMut<'_>,
    source: &Source<'_>,
    membership: &Membership,
    user: &Account,
    now: Timestamp,
) {
    let urls = Urls::new(tx.config(), "");
    let inviter = tx
        .table::<Accounts>()
        .get(source.sender)
        .map_or(source.org, AsRef::as_ref);
    let id = membership.id;
    let org = &source.org.login;
    let role = match membership.role {
        hubdouble_model::OrgRole::Admin => "admin",
        hubdouble_model::OrgRole::Member => "direct_member",
    };
    source.send(
        tx,
        "organization",
        "member_invited",
        None,
        json!({
            "invitation": {
                "id": id, "node_id": node_id::entity("OI", id), "login": user.login,
                "email": user.email, "role": role, "created_at": now,
                "failed_at": null, "failed_reason": null,
                "inviter": SimpleUser::new(&urls, inviter), "team_count": 0,
                "invitation_teams_url": format!("{}/orgs/{org}/invitations/{id}/teams", urls.api),
            },
            "user": SimpleUser::new(&urls, user),
        }),
    );
}

pub fn membership(
    tx: &mut TxMut<'_>,
    source: &Source<'_>,
    action: &'static str,
    team: &Team,
    user: &Account,
) {
    let urls = Urls::new(tx.config(), "");
    let body = team_payload(tx, &urls, team, source.org);
    source.send(
        tx,
        "membership",
        action,
        None,
        json!({
            "scope": "team",
            "team": body,
            "member": SimpleUser::new(&urls, user),
        }),
    );
}

pub fn collaborator(
    tx: &mut TxMut<'_>,
    source: &Source<'_>,
    repo_id: i64,
    user: &Account,
    before: Option<RepoRole>,
    after: Option<RepoRole>,
) {
    let action = match (before, after) {
        (None, Some(_)) => "added",
        (Some(_), None) => "removed",
        (Some(old), Some(new)) if old != new => "edited",
        _ => return,
    };
    let urls = Urls::new(tx.config(), "");
    let mut body = json!({ "member": SimpleUser::new(&urls, user) });
    if action == "edited" {
        body["changes"] = json!({ "old_permission": { "from": legacy_role(before) } });
    }
    source.send(tx, "member", action, Some(repo_id), body);
}

fn legacy_role(role: Option<RepoRole>) -> &'static str {
    match role {
        Some(RepoRole::Read) => "pull",
        Some(RepoRole::Write) => "push",
        Some(role) => role.as_str(),
        None => "none",
    }
}

pub fn team(
    tx: &mut TxMut<'_>,
    source: &Source<'_>,
    action: &'static str,
    team: &Team,
    repo_id: Option<i64>,
    changes: Option<Map<String, Value>>,
) {
    let urls = Urls::new(tx.config(), "");
    let mut body = json!({ "team": team_payload(tx, &urls, team, source.org) });
    if let Some(changes) = changes {
        body["changes"] = Value::Object(changes);
    }
    source.send(tx, "team", action, repo_id, body);
}

fn team_payload(tx: &Tx, urls: &Urls, team: &Team, org: &Account) -> Value {
    let mut body = simple_team(urls, team, org);
    if let Some(parent) = team.parent_id.and_then(|id| tx.table::<Teams>().get(id)) {
        body["parent"] = simple_team(urls, parent, org);
    }
    body
}

fn simple_team(urls: &Urls, team: &Team, org: &Account) -> Value {
    let mut body = json!(TeamSimple::new(urls, team, org));
    if let Some(fields) = body.as_object_mut() {
        fields.remove("type");
        fields.remove("organization_id");
    }
    body
}

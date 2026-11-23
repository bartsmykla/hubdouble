use std::sync::Arc;

use bytes::Bytes;
use hubdouble_model::{Access, AccountKind, App, Event, Installation, RepositorySelection};
use hubdouble_render::{OrganizationSimple, Repository, SimpleUser, Urls};
use hubdouble_store::{Accounts, Apps, Deliveries, Installations, OutboxEntry, Repos, Tx, TxMut};
use serde::ser::SerializeMap as _;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value, json};

use crate::HooksError;

const ALWAYS_DELIVERED: [&str; 4] = [
    "installation",
    "installation_repositories",
    "installation_target",
    "github_app_authorization",
];

const ZEN: &str = "Keep it logically awesome.";

fn payload_urls(tx: &Tx) -> Urls {
    Urls::new(tx.config(), "")
}

fn hook_active(app: &App) -> bool {
    app.webhook.as_ref().is_some_and(|w| w.active)
}

fn subscribes(app: &App, event: &str) -> bool {
    ALWAYS_DELIVERED.contains(&event)
        || app.events.iter().any(|e| e == event)
        || matches!(event, "check_run" | "check_suite")
            && app.permissions.get("checks") >= Some(&Access::Write)
}

enum Need {
    Nothing,
    Permission(&'static str, Access),
}

fn needs(event: &str, action: Option<&str>) -> Option<Need> {
    let level = |write: &[&str]| {
        if action.is_some_and(|a| write.contains(&a)) {
            Access::Write
        } else {
            Access::Read
        }
    };
    Some(match event {
        _ if ALWAYS_DELIVERED.contains(&event) => Need::Nothing,
        "repository" | "label" => Need::Nothing,
        "push" | "create" | "delete" | "release" => Need::Permission("contents", Access::Read),
        "issues" | "issue_comment" => Need::Permission("issues", Access::Read),
        "pull_request"
        | "pull_request_review"
        | "pull_request_review_comment"
        | "pull_request_review_thread" => Need::Permission("pull_requests", Access::Read),
        "check_run" => Need::Permission("checks", level(&["rerequested", "requested_action"])),
        "check_suite" => Need::Permission("checks", level(&["requested", "rerequested"])),
        "status" => Need::Permission("statuses", Access::Read),
        "workflow_run" | "workflow_job" => Need::Permission("actions", Access::Read),
        "organization" | "membership" | "member" | "team" => {
            Need::Permission("members", Access::Read)
        }
        _ => return None,
    })
}

fn granted(i: &Installation, e: &Event) -> bool {
    match needs(e.name, e.action) {
        Some(Need::Nothing) => true,
        Some(Need::Permission(permission, at_least)) => {
            i.permissions.get(permission) >= Some(&at_least)
        }
        None => false,
    }
}

fn covers(i: &Installation, repo_id: i64) -> bool {
    i.repository_selection == RepositorySelection::All || i.repo_ids.contains(&repo_id)
}

pub(crate) fn recipients(tx: &Tx, e: &Event) -> Vec<(i64, Option<Arc<Installation>>)> {
    let apps = tx.table::<Apps>();
    let wants = |app_id: i64| {
        apps.get(app_id)
            .is_some_and(|a| hook_active(a) && subscribes(a, e.name))
    };
    if let Some(id) = e.installation_id {
        return tx
            .table::<Installations>()
            .get(id)
            .filter(|i| {
                e.app_id.is_none_or(|a| a == i.app_id)
                    && (ALWAYS_DELIVERED.contains(&e.name)
                        || i.suspended_at.is_none() && e.repo_id.is_none_or(|r| covers(i, r)))
                    && granted(i, e)
                    && wants(i.app_id)
            })
            .map(|i| (i.app_id, Some(i.clone())))
            .into_iter()
            .collect();
    }
    if let Some(app_id) = e.app_id {
        return if wants(app_id) {
            vec![(app_id, None)]
        } else {
            Vec::new()
        };
    }
    let repo = e.repo_id.and_then(|id| tx.table::<Repos>().get(id));
    if e.repo_id.is_some() && repo.is_none() {
        return Vec::new();
    }
    let Some(account) = repo.map(|r| r.owner_id).or(e.org_id) else {
        return Vec::new();
    };
    tx.table::<Installations>()
        .iter()
        .filter(|i| {
            i.account_id == account
                && i.suspended_at.is_none()
                && repo.is_none_or(|r| covers(i, r.id))
                && granted(i, e)
                && wants(i.app_id)
        })
        .map(|i| (i.app_id, Some(i.clone())))
        .collect()
}

enum Rendered {
    Repository(Box<Repository>),
    Organization(OrganizationSimple),
    Sender(SimpleUser),
}

#[derive(Serialize)]
struct InstallationRef<'a> {
    id: i64,
    node_id: &'a str,
}

enum Entry<'a> {
    Value(&'a Value),
    Rendered(&'a Rendered),
    Installation(InstallationRef<'a>),
}

impl Serialize for Entry<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Entry::Value(v) => v.serialize(s),
            Entry::Rendered(Rendered::Repository(r)) => r.serialize(s),
            Entry::Rendered(Rendered::Organization(o)) => o.serialize(s),
            Entry::Rendered(Rendered::Sender(u)) => u.serialize(s),
            Entry::Installation(i) => i.serialize(s),
        }
    }
}

struct Body<'a>(Vec<(&'a str, Entry<'a>)>);

impl Serialize for Body<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(self.0.len()))?;
        for (key, value) in &self.0 {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

fn rendered(tx: &Tx, e: &Event, p: &Map<String, Value>) -> Vec<(&'static str, Rendered)> {
    let urls = payload_urls(tx);
    let accounts = tx.table::<Accounts>();
    let repo = e.repo_id.and_then(|id| tx.table::<Repos>().get(id));
    let absent = |key: &str| !p.contains_key(key);
    let mut out = Vec::with_capacity(3);
    if absent("repository")
        && let Some(r) = repo
        && let Some(owner) = accounts.get(r.owner_id)
    {
        out.push((
            "repository",
            Rendered::Repository(Box::new(Repository::new(&urls, r, owner))),
        ));
    }
    if absent("organization")
        && let Some(org) = e
            .org_id
            .or(repo.map(|r| r.owner_id))
            .and_then(|id| accounts.get(id))
        && org.kind == AccountKind::Organization
    {
        out.push((
            "organization",
            Rendered::Organization(OrganizationSimple::new(&urls, org)),
        ));
    }
    if absent("sender")
        && let Some(sender) = accounts.get(e.sender_id)
    {
        out.push(("sender", Rendered::Sender(SimpleUser::new(&urls, sender))));
    }
    out
}

fn payload_body(
    p: &Map<String, Value>,
    rendered: &[(&'static str, Rendered)],
    installation: Option<&Installation>,
) -> Bytes {
    let mut entries: Vec<(&str, Entry<'_>)> = p
        .iter()
        .map(|(k, v)| (k.as_str(), Entry::Value(v)))
        .chain(rendered.iter().map(|(k, r)| (*k, Entry::Rendered(r))))
        .collect();
    if let Some(i) = installation.filter(|_| !p.contains_key("installation")) {
        entries.push((
            "installation",
            Entry::Installation(InstallationRef {
                id: i.id,
                node_id: &i.node_id,
            }),
        ));
    }
    entries.sort_unstable_by_key(|(k, _)| *k);
    body(&Body(entries))
}

fn webhook_label(l: &mut Value) {
    if let Some(l) = l.as_object_mut() {
        l.remove("archived_at");
        l.remove("archived_by");
    }
}

fn webhook_form(p: &mut Map<String, Value>) {
    if let Some(l) = p.get_mut("label") {
        webhook_label(l);
    }
    for holder in ["issue", "pull_request"] {
        let Some(h) = p.get_mut(holder).and_then(Value::as_object_mut) else {
            continue;
        };
        if holder == "issue" {
            h.remove("closed_by");
        }
        if let Some(labels) = h.get_mut("labels").and_then(Value::as_array_mut) {
            labels.iter_mut().for_each(webhook_label);
        }
    }
}

pub(crate) fn emit(tx: &mut TxMut<'_>, mut e: Event) {
    let recipients = recipients(tx, &e);
    if recipients.is_empty() {
        return;
    }
    let mut payload = std::mem::take(&mut e.payload);
    webhook_form(&mut payload);
    if let Some(action) = e.action.filter(|_| !payload.contains_key("action")) {
        payload.insert("action".to_owned(), Value::from(action));
    }
    let rendered = rendered(tx, &e, &payload);
    for (app_id, installation) in recipients {
        let body = payload_body(&payload, &rendered, installation.as_deref());
        tx.push_outbox(OutboxEntry {
            app_id,
            installation_id: installation.map(|i| i.id),
            repo_id: e.repo_id,
            event: e.name.into(),
            action: e.action.map(Into::into),
            payload: body,
            redelivery_of: None,
        });
    }
}

pub(crate) fn ping(tx: &mut TxMut<'_>, app_id: i64) {
    let Some(app) = tx.table::<Apps>().get(app_id).cloned() else {
        return;
    };
    let Some(hook) = app.webhook.as_ref().filter(|w| w.active) else {
        return;
    };
    let mut config = json!({ "content_type": "json", "insecure_ssl": "0", "url": hook.url });
    if hook.secret.is_some() {
        config["secret"] = json!("********");
    }
    let api = payload_urls(tx).api;
    let payload = json!({
        "zen": ZEN,
        "hook_id": app.id,
        "hook": {
            "type": "App",
            "id": app.id,
            "name": "web",
            "active": hook.active,
            "events": app.events,
            "config": config,
            "updated_at": app.updated_at,
            "created_at": app.created_at,
            "app_id": app.id,
            "url": format!("{api}/app/hook/config"),
            "ping_url": format!("{api}/app/hook/pings"),
            "deliveries_url": format!("{api}/app/hook/deliveries"),
        },
    });
    tx.push_outbox(OutboxEntry {
        app_id,
        installation_id: None,
        repo_id: None,
        event: "ping".into(),
        action: None,
        payload: body(&payload),
        redelivery_of: None,
    });
}

fn body(payload: &impl Serialize) -> Bytes {
    Bytes::copy_from_slice(&serde_json::to_vec(payload).unwrap_or_default())
}

pub(crate) fn redeliver(
    tx: &mut TxMut<'_>,
    app_id: i64,
    delivery_id: i64,
) -> Result<(), HooksError> {
    let d = tx
        .table::<Deliveries>()
        .get(delivery_id)
        .filter(|d| d.app_id == app_id && !d.request.payload.is_empty())
        .cloned()
        .ok_or(HooksError::UnknownDelivery(delivery_id))?;
    tx.push_outbox(OutboxEntry {
        app_id,
        installation_id: d.installation_id,
        repo_id: d.repo_id,
        event: d.event.clone().into(),
        action: d.action.clone().map(Into::into),
        payload: d.request.payload.clone(),
        redelivery_of: Some(d.guid.clone()),
    });
    Ok(())
}

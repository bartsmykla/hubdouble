use std::sync::Arc;

use bytes::Bytes;
use hubdouble_model::{Access, AccountKind, App, Event, Installation, RepositorySelection};
use hubdouble_render::{OrganizationSimple, Repository, SimpleUser, Urls};
use hubdouble_store::{Accounts, Apps, Deliveries, Installations, OutboxEntry, Repos, Tx, TxMut};
use serde::Serialize;
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
    ALWAYS_DELIVERED.contains(&event) || app.events.iter().any(|e| e == event)
}

fn needs(event: &str, action: Option<&str>) -> Option<(&'static str, Access)> {
    let level = |write: &[&str]| {
        if action.is_some_and(|a| write.contains(&a)) {
            Access::Write
        } else {
            Access::Read
        }
    };
    Some(match event {
        "push" | "create" | "delete" | "release" => ("contents", Access::Read),
        "issues" | "issue_comment" => ("issues", Access::Read),
        "pull_request"
        | "pull_request_review"
        | "pull_request_review_comment"
        | "pull_request_review_thread" => ("pull_requests", Access::Read),
        "check_run" => ("checks", level(&["rerequested", "requested_action"])),
        "check_suite" => ("checks", level(&["requested", "rerequested"])),
        "status" => ("statuses", Access::Read),
        "workflow_run" | "workflow_job" => ("actions", Access::Read),
        "organization" | "membership" | "member" | "team" => ("members", Access::Read),
        _ => return None,
    })
}

fn granted(i: &Installation, e: &Event) -> bool {
    needs(e.name, e.action)
        .is_none_or(|(permission, at_least)| i.permissions.get(permission) >= Some(&at_least))
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
            .filter(|i| e.app_id.is_none_or(|a| a == i.app_id) && granted(i, e) && wants(i.app_id))
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

fn insert_absent(p: &mut Map<String, Value>, key: &str, value: impl FnOnce() -> Option<Value>) {
    if !p.contains_key(key)
        && let Some(v) = value()
    {
        p.insert(key.to_owned(), v);
    }
}

fn to_value(v: &impl Serialize) -> Option<Value> {
    serde_json::to_value(v).ok()
}

fn base_payload(tx: &Tx, e: &Event, mut p: Map<String, Value>) -> Map<String, Value> {
    let urls = payload_urls(tx);
    let accounts = tx.table::<Accounts>();
    let repo = e.repo_id.and_then(|id| tx.table::<Repos>().get(id));
    insert_absent(&mut p, "action", || e.action.map(Value::from));
    insert_absent(&mut p, "repository", || {
        let r = repo?;
        to_value(&Repository::new(&urls, r, accounts.get(r.owner_id)?))
    });
    insert_absent(&mut p, "organization", || {
        let org = accounts.get(e.org_id.or(repo.map(|r| r.owner_id))?)?;
        (org.kind == AccountKind::Organization)
            .then(|| to_value(&OrganizationSimple::new(&urls, org)))
            .flatten()
    });
    insert_absent(&mut p, "sender", || {
        to_value(&SimpleUser::new(&urls, accounts.get(e.sender_id)?))
    });
    p
}

fn webhook_label(l: &mut Value) {
    if let Some(l) = l.as_object_mut() {
        l.remove("archived_at");
        l.remove("archived_by");
    }
}

/// GitHub's webhook `label` has no `archived_at` or `archived_by` and its webhook `issue` has no
/// `closed_by`, which the REST forms that tracks render carry.
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
    let mut payload = base_payload(tx, &e, payload);
    let mut recipients = recipients.into_iter().peekable();
    while let Some((app_id, installation)) = recipients.next() {
        let mut p = if recipients.peek().is_some() {
            payload.clone()
        } else {
            std::mem::take(&mut payload)
        };
        insert_absent(&mut p, "installation", || {
            installation
                .as_ref()
                .map(|i| json!({ "id": i.id, "node_id": i.node_id }))
        });
        tx.push_outbox(OutboxEntry {
            app_id,
            installation_id: installation.map(|i| i.id),
            repo_id: e.repo_id,
            event: e.name,
            action: e.action,
            payload: body(&Value::Object(p)),
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
        event: "ping",
        action: None,
        payload: body(&payload),
        redelivery_of: None,
    });
}

fn body(payload: &Value) -> Bytes {
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
        event: "",
        action: None,
        payload: d.request.payload.clone(),
        redelivery_of: Some(d.guid.clone()),
    });
    Ok(())
}

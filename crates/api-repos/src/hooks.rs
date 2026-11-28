use std::sync::Arc;

use http::StatusCode;
use http::header::LOCATION;
use hubdouble_admin::Registrar;
use hubdouble_api_apps::{DeliveryLog, get_delivery, list_deliveries, redeliver_delivery};
use hubdouble_git::{ObjectData, RefChange};
use hubdouble_hooks::{ContentType, Hook, HookOwner, Hooks, LatestPushes, is_event};
use hubdouble_model::{AccountKind, ApiError, FieldError, Grant, Requirement, Scope};
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::{Accounts, Deleted, Deliveries, Tx, TxMut};
use serde::Deserialize;
use serde_json::Value;

const REPO_READ: Requirement = Requirement {
    any_of: &[Grant::read("repository_hooks")],
    all_of: &[],
    scope: Scope::Repo,
    scopes: &["read:repo_hook", "repo"],
};
const REPO_WRITE: Requirement = Requirement {
    any_of: &[Grant::write("repository_hooks")],
    all_of: &[],
    scope: Scope::Repo,
    scopes: &["write:repo_hook", "repo"],
};
const REPO_DELETE: Requirement = Requirement {
    any_of: &[Grant::write("repository_hooks")],
    all_of: &[],
    scope: Scope::Repo,
    scopes: &["admin:repo_hook", "repo"],
};
const ORG_READ: Requirement = Requirement {
    any_of: &[Grant::read("organization_hooks")],
    all_of: &[],
    scope: Scope::Org,
    scopes: &["admin:org_hook"],
};
const ORG_WRITE: Requirement = Requirement {
    any_of: &[Grant::write("organization_hooks")],
    all_of: &[],
    scope: Scope::Org,
    scopes: &["admin:org_hook"],
};

pub(crate) fn register(r: &mut Registrar<'_>) {
    r.store
        .register_table::<Hooks>()
        .register_table::<LatestPushes>()
        .on_delete(forget);
    let rest = &mut *r.rest;
    rest.handle("repos/list-webhooks", REPO_READ, list);
    rest.handle("repos/create-webhook", REPO_WRITE, create);
    rest.handle("repos/get-webhook", REPO_READ, get);
    rest.handle("repos/update-webhook", REPO_WRITE, update);
    rest.handle("repos/delete-webhook", REPO_DELETE, delete);
    rest.handle("repos/get-webhook-config-for-repo", REPO_READ, config);
    rest.handle(
        "repos/update-webhook-config-for-repo",
        REPO_WRITE,
        update_config,
    );
    rest.handle("repos/list-webhook-deliveries", REPO_READ, deliveries);
    rest.handle("repos/get-webhook-delivery", REPO_READ, delivery);
    rest.handle("repos/redeliver-webhook-delivery", REPO_WRITE, redeliver);
    rest.handle("repos/ping-webhook", REPO_READ, ping);
    rest.handle("repos/test-push-webhook", REPO_READ, test_push);
    rest.handle("orgs/list-webhooks", ORG_READ, list);
    rest.handle("orgs/create-webhook", ORG_WRITE, create);
    rest.handle("orgs/get-webhook", ORG_READ, get);
    rest.handle("orgs/update-webhook", ORG_WRITE, update);
    rest.handle("orgs/delete-webhook", ORG_WRITE, delete);
    rest.handle("orgs/get-webhook-config-for-org", ORG_READ, config);
    rest.handle(
        "orgs/update-webhook-config-for-org",
        ORG_WRITE,
        update_config,
    );
    rest.handle("orgs/list-webhook-deliveries", ORG_READ, deliveries);
    rest.handle("orgs/get-webhook-delivery", ORG_READ, delivery);
    rest.handle("orgs/redeliver-webhook-delivery", ORG_WRITE, redeliver);
    rest.handle("orgs/ping-webhook", ORG_WRITE, ping);
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "a deletion subscriber returns a Result"
)]
fn forget(tx: &mut TxMut<'_>, deleted: &Deleted) -> Result<(), ApiError> {
    let owner = match deleted {
        Deleted::Repo(repo) => {
            tx.table_mut::<LatestPushes>().remove(repo.id);
            HookOwner::Repo(repo.id)
        }
        Deleted::Account(account) => HookOwner::Org(account.id),
        Deleted::Team(_) => return Ok(()),
    };
    let hooks: Vec<i64> = tx
        .table::<Hooks>()
        .iter()
        .filter(|h| h.owner == owner)
        .map(|h| h.id)
        .collect();
    for hook in hooks {
        tx.table_mut::<Deliveries>().remove_children(hook);
    }
    tx.table_mut::<Hooks>().remove_where(|h| h.owner == owner);
    Ok(())
}

fn owner(ctx: &Ctx<'_>) -> Result<HookOwner, ApiError> {
    if ctx.operation_id().starts_with("repos/") {
        return Ok(HookOwner::Repo(ctx.repo()?.id));
    }
    ctx.tx()
        .table::<Accounts>()
        .by_login(ctx.param("org")?)
        .filter(|a| a.kind == AccountKind::Organization)
        .map(|org| HookOwner::Org(org.id))
        .ok_or_else(ApiError::not_found)
}

fn hook(ctx: &Ctx<'_>) -> Result<Arc<Hook>, ApiError> {
    let owner = owner(ctx)?;
    ctx.tx()
        .table::<Hooks>()
        .get(ctx.param_i64("hook_id")?)
        .filter(|h| h.owner == owner)
        .cloned()
        .ok_or_else(ApiError::not_found)
}

fn rendered(tx: &Tx, hook: &Hook) -> Result<Value, ApiError> {
    hook.render(tx)
        .ok_or_else(|| ApiError::internal("the webhook's owner is gone"))
}

fn sender(ctx: &Ctx<'_>) -> Option<i64> {
    ctx.actor().account().map(|a| a.id)
}

#[derive(Deserialize)]
struct HookBody {
    name: Option<String>,
    config: Option<ConfigBody>,
    events: Option<Vec<String>>,
    add_events: Option<Vec<String>>,
    remove_events: Option<Vec<String>>,
    active: Option<bool>,
}

#[derive(Deserialize, Default)]
struct ConfigBody {
    url: Option<String>,
    content_type: Option<String>,
    secret: Option<String>,
    insecure_ssl: Option<Value>,
}

fn invalid(message: &str) -> ApiError {
    ApiError::unprocessable("Validation Failed").with_error(FieldError {
        resource: "Hook".to_owned(),
        field: String::new(),
        code: "custom".to_owned(),
        message: Some(message.to_owned()),
        value: None,
    })
}

fn web_url(url: &str) -> bool {
    url.parse::<http::Uri>().is_ok_and(|u| {
        matches!(u.scheme_str(), Some("http" | "https")) && u.host().is_some_and(|h| !h.is_empty())
    })
}

fn apply_config(hook: &mut Hook, c: ConfigBody) -> Result<(), ApiError> {
    if let Some(url) = c.url {
        if !web_url(&url) {
            return Err(invalid("Config url is not a valid URL"));
        }
        hook.url = url;
    }
    if let Some(content_type) = c.content_type {
        hook.content_type = ContentType::parse(&content_type)
            .ok_or_else(|| invalid("Config content_type must be json or form"))?;
    }
    if let Some(secret) = c.secret {
        hook.secret = Some(secret).filter(|s| !s.is_empty());
    }
    if let Some(ssl) = c.insecure_ssl {
        hook.insecure_ssl = match ssl {
            Value::String(s) if s == "0" => false,
            Value::String(s) if s == "1" => true,
            Value::Number(n) if n.as_i64() == Some(0) => false,
            Value::Number(n) if n.as_i64() == Some(1) => true,
            _ => return Err(invalid("Config insecure_ssl must be 0 or 1")),
        };
    }
    Ok(())
}

fn check_events(events: Option<&[String]>) -> Result<(), ApiError> {
    match events.into_iter().flatten().find(|e| !is_event(e)) {
        Some(unknown) => Err(invalid(&format!("\"{unknown}\" is not a valid event name"))),
        None => Ok(()),
    }
}

fn check_wildcard(events: &[String]) -> Result<(), ApiError> {
    if events.len() > 1 && events.iter().any(|e| e == "*") {
        return Err(invalid("\"*\" must be the only event"));
    }
    Ok(())
}

fn check_name(name: Option<&str>, required: bool) -> Result<(), ApiError> {
    match name {
        Some("web") => Ok(()),
        None if !required => Ok(()),
        _ => Err(invalid("Name must be \"web\"")),
    }
}

fn list(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let owner = owner(ctx)?;
    let tx = ctx.tx();
    let hooks: Vec<Value> = tx
        .table::<Hooks>()
        .children(owner.id())
        .map(|h| rendered(tx, h))
        .collect::<Result<_, _>>()?;
    ctx.page(&hooks, Clone::clone)
}

fn create(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let owner = owner(ctx)?;
    let body: HookBody = ctx.body()?;
    check_name(body.name.as_deref(), matches!(owner, HookOwner::Org(_)))?;
    check_events(body.events.as_deref())?;
    let config = body.config.unwrap_or_default();
    if config.url.as_deref().is_none_or(str::is_empty) {
        return Err(invalid("Config must contain URL for webhooks"));
    }
    let now = ctx.now();
    let mut hook = Hook {
        id: 0,
        owner,
        url: String::new(),
        content_type: ContentType::Form,
        secret: None,
        insecure_ssl: false,
        events: body.events.unwrap_or_else(|| vec!["push".to_owned()]),
        active: body.active.unwrap_or(true),
        created_at: now,
        updated_at: now,
    };
    check_wildcard(&hook.events)?;
    apply_config(&mut hook, config)?;
    let taken = ctx
        .tx()
        .table::<Hooks>()
        .children(owner.id())
        .any(|h| h.url == hook.url);
    if taken {
        return Err(invalid(match owner {
            HookOwner::Repo(_) => "Hook already exists on this repository",
            HookOwner::Org(_) => "Hook already exists on this organization",
        }));
    }
    let sender = sender(ctx);
    let tx = ctx.tx_mut()?;
    hook.id = tx.next_id();
    let id = hook.id;
    tx.table_mut::<Hooks>().insert(hook)?;
    hubdouble_hooks::ping_hook(tx, id, sender)?;
    let hook = hook_by_id(tx, id)?;
    let answer = rendered(tx, &hook)?;
    let location = answer["url"].as_str().unwrap_or_default().to_owned();
    Reply::json(&answer)?
        .status(StatusCode::CREATED)
        .header(LOCATION, &location)
}

fn hook_by_id(tx: &Tx, id: i64) -> Result<Arc<Hook>, ApiError> {
    tx.table::<Hooks>()
        .get(id)
        .cloned()
        .ok_or_else(ApiError::not_found)
}

fn get(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let hook = hook(ctx)?;
    Reply::json(&rendered(ctx.tx(), &hook)?)
}

fn save(ctx: &mut Ctx<'_>, mut hook: Hook) -> Result<Arc<Hook>, ApiError> {
    hook.updated_at = ctx.now();
    let tx = ctx.tx_mut()?;
    let id = hook.id;
    tx.table_mut::<Hooks>().replace(hook)?;
    hook_by_id(tx, id)
}

fn update(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let mut hook = Hook::clone(&*hook(ctx)?);
    let body: HookBody = ctx.body()?;
    if matches!(hook.owner, HookOwner::Org(_)) {
        check_name(body.name.as_deref(), false)?;
    }
    check_events(body.events.as_deref())?;
    check_events(body.add_events.as_deref())?;
    if let Some(config) = body.config {
        apply_config(&mut hook, config)?;
    }
    if let Some(events) = body.events {
        hook.events = events;
    }
    for event in body.add_events.unwrap_or_default() {
        if !hook.events.contains(&event) {
            hook.events.push(event);
        }
    }
    let removed = body.remove_events.unwrap_or_default();
    hook.events.retain(|e| !removed.contains(e));
    check_wildcard(&hook.events)?;
    if let Some(active) = body.active {
        hook.active = active;
    }
    let hook = save(ctx, hook)?;
    Reply::json(&rendered(ctx.tx(), &hook)?)
}

fn delete(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let id = hook(ctx)?.id;
    ctx.tx_mut()?.table_mut::<Hooks>().remove(id);
    Ok(Reply::empty())
}

fn config(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    Reply::json(&hook(ctx)?.config())
}

fn update_config(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let mut hook = Hook::clone(&*hook(ctx)?);
    let body: ConfigBody = ctx.body()?;
    apply_config(&mut hook, body)?;
    let hook = save(ctx, hook)?;
    Reply::json(&hook.config())
}

fn log_key(hook: &Hook) -> String {
    format!("hubdouble:hook:{}:{}", hook.id, hook.created_at)
}

fn deliveries(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let hook = hook(ctx)?;
    let path = hook.path(ctx.tx()).ok_or_else(ApiError::not_found)?;
    let key = log_key(&hook);
    list_deliveries(
        ctx,
        &DeliveryLog {
            hook_id: hook.id,
            key: key.as_bytes(),
            path: format!("{path}/deliveries"),
        },
    )
}

fn delivery(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let id = hook(ctx)?.id;
    get_delivery(ctx, id)
}

fn redeliver(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let id = hook(ctx)?.id;
    redeliver_delivery(ctx, id)
}

fn ping(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let id = hook(ctx)?.id;
    let sender = sender(ctx);
    hubdouble_hooks::ping_hook(ctx.tx_mut()?, id, sender)?;
    Ok(Reply::empty())
}

fn test_push(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let hook = hook(ctx)?;
    if !hook.subscribes("push") {
        return Ok(Reply::empty());
    }
    let repo = ctx.repo()?.clone();
    let latest = ctx.tx().table::<LatestPushes>().get(repo.id).cloned();
    if let Some(push) = latest {
        let tx = ctx.tx_mut()?;
        hubdouble_hooks::deliver_to_hook(tx, hook.id, "push", Some(push.sender_id), &push.payload)?;
        return Ok(Reply::empty());
    }
    let sender = sender(ctx).unwrap_or(repo.owner_id);
    let (tx, git) = ctx.tx_mut_and_git()?;
    let name = format!("refs/heads/{}", repo.default_branch);
    let Ok(head) = git.resolve_ref(tx, repo.id, &name) else {
        return Ok(Reply::empty());
    };
    let parent = match git.object(tx, repo.id, head).map(|o| o.data) {
        Ok(ObjectData::Commit(c)) => c.parents.first().copied(),
        _ => None,
    };
    let change = RefChange {
        repo_id: repo.id,
        sender,
        name,
        old: parent,
        new: Some(head),
        forced: false,
    };
    if let Some(event) = git.push_event(tx, &change)? {
        hubdouble_hooks::deliver_to_hook(tx, hook.id, "push", Some(sender), &event.payload)?;
    }
    Ok(Reply::empty())
}

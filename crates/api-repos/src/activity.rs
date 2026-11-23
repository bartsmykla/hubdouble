use std::fmt::Write as _;
use std::sync::Arc;

use http::header::LINK;
use hubdouble_admin::Registrar;
use hubdouble_git::{Git, ObjectId, RefChange};
use hubdouble_model::{ApiError, Timestamp, node_id};
use hubdouble_render::{SimpleUser, escape};
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::{Accounts, Entity, Issues, Rows, Tx, TxMut};
use serde::Serialize;

use crate::CONTENTS;

const NODE_PREFIX: &str = "RAC";
const HEADS: &str = "refs/heads/";
const KINDS: [&str; 6] = [
    "push",
    "force_push",
    "branch_creation",
    "branch_deletion",
    "pr_merge",
    "merge_queue_merge",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Activity {
    pub(crate) id: i64,
    pub(crate) node_id: String,
    pub(crate) repo_id: i64,
    pub(crate) ref_name: String,
    pub(crate) before: ObjectId,
    pub(crate) after: ObjectId,
    pub(crate) at: Timestamp,
    pub(crate) kind: &'static str,
    pub(crate) actor_id: i64,
}

impl Entity for Activity {
    fn id(&self) -> i64 {
        self.id
    }
    fn node_id(&self) -> Option<&str> {
        Some(&self.node_id)
    }
    fn repo_id(&self) -> Option<i64> {
        Some(self.repo_id)
    }
}

pub(crate) type Activities = Rows<Activity>;

pub(crate) fn register(r: &mut Registrar<'_>) {
    r.store.register_table::<Activities>();
    r.git.on_ref_update(record);
    r.rest.handle("repos/list-activities", CONTENTS, list);
}

fn merged_pull(tx: &Tx, c: &RefChange, branch: &str, new: ObjectId) -> bool {
    let sha = new.to_string();
    tx.table::<Issues>().in_repo(c.repo_id).any(|i| {
        i.pull.as_ref().is_some_and(|p| {
            p.base.ref_name == branch && p.merge_commit_sha.as_deref() == Some(sha.as_str())
        })
    })
}

pub(crate) fn kind_of(tx: &Tx, c: &RefChange) -> Option<&'static str> {
    let branch = c.name.strip_prefix(HEADS)?;
    Some(match (c.old, c.new) {
        (None, _) => "branch_creation",
        (_, None) => "branch_deletion",
        _ if c.forced => "force_push",
        (_, Some(new)) if merged_pull(tx, c, branch, new) => "pr_merge",
        _ => "push",
    })
}

fn record(tx: &mut TxMut<'_>, git: &Git, c: &RefChange) -> Result<(), ApiError> {
    let Some(kind) = kind_of(tx, c) else {
        return Ok(());
    };
    let id = tx.next_id();
    tx.table_mut::<Activities>().insert(Activity {
        id,
        node_id: node_id::entity(NODE_PREFIX, id),
        repo_id: c.repo_id,
        ref_name: c.name.clone(),
        before: c.old.unwrap_or(ObjectId::NULL),
        after: c.new.unwrap_or(ObjectId::NULL),
        at: git.now(),
        kind,
        actor_id: c.sender,
    })?;
    Ok(())
}

#[derive(Serialize)]
struct ActivityOut<'a> {
    id: i64,
    node_id: &'a str,
    before: ObjectId,
    after: ObjectId,
    #[serde(rename = "ref")]
    ref_name: &'a str,
    timestamp: Timestamp,
    activity_type: &'static str,
    actor: Option<SimpleUser>,
}

fn invalid(field: &str) -> ApiError {
    ApiError::unprocessable(format!("Validation Failed: {field} is not valid"))
}

fn period_seconds(period: &str) -> Option<i64> {
    let days = match period {
        "day" => 1,
        "week" => 7,
        "month" => 30,
        "quarter" => 90,
        "year" => 365,
        _ => return None,
    };
    Some(days * 86_400)
}

fn cursor_position(items: &[&Arc<Activity>], cursor: &str) -> Result<usize, ApiError> {
    let id: i64 = cursor.parse().map_err(|_| invalid("cursor"))?;
    items
        .iter()
        .position(|a| a.id == id)
        .ok_or_else(|| invalid("cursor"))
}

fn link(ctx: &Ctx<'_>, rel: &str, name: &str, cursor: i64) -> Result<String, ApiError> {
    let mut url = format!(
        "{}/repositories/{}/activity?",
        ctx.urls().api,
        ctx.repo()?.id
    );
    for (k, v) in ctx
        .query()
        .iter()
        .filter(|(k, _)| !matches!(*k, "before" | "after"))
    {
        let _ = write!(url, "{}={}&", escape(k), escape(v));
    }
    Ok(format!("<{url}{name}={cursor}>; rel=\"{rel}\""))
}

fn list(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?;
    let tx = ctx.tx();
    let query = ctx.query();
    let descending = match query.get("direction") {
        None | Some("desc") => true,
        Some("asc") => false,
        Some(_) => return Err(invalid("direction")),
    };
    let kind = match query.get("activity_type") {
        Some(k) if !KINDS.contains(&k) => return Err(invalid("activity_type")),
        k => k,
    };
    let since = match query.get("time_period") {
        Some(p) => Some(
            ctx.now()
                .plus_seconds(-period_seconds(p).ok_or_else(|| invalid("time_period"))?),
        ),
        None => None,
    };
    let ref_name = query.get("ref").map(|r| {
        if r.starts_with("refs/") {
            r.to_owned()
        } else {
            format!("{HEADS}{r}")
        }
    });
    let accounts = tx.table::<Accounts>();
    let actor = query
        .get("actor")
        .map(|login| accounts.by_login(login).map(|a| a.id));
    let mut items: Vec<&Arc<Activity>> = tx
        .table::<Activities>()
        .in_repo(repo.id)
        .filter(|a| ref_name.as_ref().is_none_or(|r| a.ref_name == *r))
        .filter(|a| kind.is_none_or(|k| a.kind == k))
        .filter(|a| since.is_none_or(|s| a.at >= s))
        .filter(|a| actor.is_none_or(|id| id == Some(a.actor_id)))
        .collect();
    if descending {
        items.reverse();
    }
    let per_page = ctx.page_params().per_page;
    let (start, end) = match (query.get("after"), query.get("before")) {
        (Some(after), _) => {
            let start = cursor_position(&items, after)? + 1;
            (start, (start + per_page).min(items.len()))
        }
        (None, Some(before)) => {
            let end = cursor_position(&items, before)?;
            (end.saturating_sub(per_page), end)
        }
        (None, None) => (0, per_page.min(items.len())),
    };
    let page = &items[start..end];
    let rendered: Vec<ActivityOut<'_>> = page
        .iter()
        .map(|a| ActivityOut {
            id: a.id,
            node_id: &a.node_id,
            before: a.before,
            after: a.after,
            ref_name: &a.ref_name,
            timestamp: a.at,
            activity_type: a.kind,
            actor: accounts
                .get(a.actor_id)
                .map(|u| SimpleUser::new(ctx.urls(), u)),
        })
        .collect();
    let mut links = Vec::with_capacity(2);
    if let (true, Some(last)) = (end < items.len(), page.last()) {
        links.push(link(ctx, "next", "after", last.id)?);
    }
    if let (true, Some(first)) = (start > 0, page.first()) {
        links.push(link(ctx, "prev", "before", first.id)?);
    }
    let reply = Reply::json(&rendered)?;
    if links.is_empty() {
        Ok(reply)
    } else {
        reply.header(LINK, &links.join(", "))
    }
}

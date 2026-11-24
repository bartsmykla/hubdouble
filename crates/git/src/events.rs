use std::collections::HashSet;
use std::sync::Arc;

use hubdouble_model::{Account, AccountKind, Event, Repo, Timestamp};
use hubdouble_render::Urls;
use hubdouble_store::{Accounts, Repos, Tx};
use serde_json::{Map, Value, json};

use crate::contract::{Commit, GitError, ObjectId, RefChange, Signature};
use crate::odb::Odb;
use crate::{diff, history, refs};

const MAX_PUSH_COMMITS: usize = 2048;

struct Scope {
    repo: Arc<Repo>,
    full_name: String,
    org_id: Option<i64>,
    sender: Option<Arc<Account>>,
}

fn object(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

fn short(id: ObjectId) -> String {
    let mut s = id.to_string();
    s.truncate(12);
    s
}

fn account_by_email<'a>(tx: &'a Tx, email: &str) -> Option<&'a Arc<Account>> {
    tx.table::<Accounts>().iter().find(|a| {
        a.kind != AccountKind::Organization
            && a.email
                .as_deref()
                .is_some_and(|e| e.eq_ignore_ascii_case(email))
    })
}

fn local_time(s: &Signature) -> String {
    let local = Timestamp(s.time.unix() + i64::from(s.offset_minutes) * 60).to_string();
    if s.offset_minutes == 0 {
        return local;
    }
    let sign = if s.offset_minutes < 0 { '-' } else { '+' };
    let m = s.offset_minutes.unsigned_abs();
    format!(
        "{}{sign}{:02}:{:02}",
        local.trim_end_matches('Z'),
        m / 60,
        m % 60
    )
}

fn scope(tx: &Tx, change: &RefChange) -> Option<Scope> {
    let accounts = tx.table::<Accounts>();
    let repo = tx.table::<Repos>().get(change.repo_id)?.clone();
    let owner = accounts.get(repo.owner_id)?;
    Some(Scope {
        full_name: format!("{}/{}", owner.login, repo.name),
        org_id: (owner.kind == AccountKind::Organization).then_some(owner.id),
        sender: accounts.get(change.sender).cloned(),
        repo,
    })
}

fn person(tx: &Tx, s: &Signature) -> Value {
    let mut p = json!({"name": s.name, "email": s.email});
    if let (Some(a), Value::Object(m)) = (account_by_email(tx, &s.email), &mut p) {
        m.insert("username".to_owned(), Value::String(a.login.clone()));
    }
    p
}

fn commit_json(
    tx: &Tx,
    odb: &Odb,
    web: &str,
    scope: &Scope,
    id: ObjectId,
    commit: &Commit,
) -> Result<Value, GitError> {
    let parent_tree = match commit.parents.first() {
        Some(p) => Some(odb.commit(*p)?.tree),
        None => None,
    };
    let (mut added, mut removed, mut modified) = (Vec::new(), Vec::new(), Vec::new());
    for c in diff::tree_changes(odb, parent_tree, Some(commit.tree))? {
        match (c.old, c.new) {
            (None, _) => added.push(c.path),
            (_, None) => removed.push(c.path),
            _ => modified.push(c.path),
        }
    }
    Ok(json!({
        "id": id,
        "tree_id": commit.tree,
        "distinct": true,
        "message": commit.message,
        "timestamp": local_time(&commit.committer),
        "url": format!("{web}/{}/commit/{id}", scope.full_name),
        "author": person(tx, &commit.author),
        "committer": person(tx, &commit.committer),
        "added": added,
        "removed": removed,
        "modified": modified,
    }))
}

fn pushed(name: &str) -> Option<(&str, &'static str)> {
    name.strip_prefix("refs/heads/")
        .map(|b| (b, "branch"))
        .or_else(|| name.strip_prefix("refs/tags/").map(|t| (t, "tag")))
}

fn new_commits(
    tx: &Tx,
    odb: &Odb,
    change: &RefChange,
    head: ObjectId,
) -> Result<Vec<(ObjectId, Arc<Commit>)>, GitError> {
    let mut known = HashSet::new();
    let others: Vec<ObjectId> = match change.old {
        Some(old) => vec![old],
        None => refs::list(tx, change.repo_id, "")
            .into_iter()
            .filter(|r| r.name != change.name)
            .map(|r| r.target)
            .collect(),
    };
    for id in others {
        if let Ok((c, _)) = odb.peel_commit(id) {
            known.extend(history::ancestors(odb, c)?);
        }
    }
    let mut list = history::topo(odb, head, &known)?;
    if list.len() > MAX_PUSH_COMMITS {
        list.drain(..list.len() - MAX_PUSH_COMMITS);
    }
    Ok(list)
}

pub(crate) fn push(tx: &Tx, odb: &Odb, change: &RefChange) -> Result<Option<Event>, GitError> {
    let Some((short_name, _)) = pushed(&change.name) else {
        return Ok(None);
    };
    let head = change.new.and_then(|n| odb.peel_commit(n).ok());
    let Some(scope) = scope(tx, change) else {
        return Ok(None);
    };
    let urls = Urls::new(tx.config(), "");
    let web = urls.web.as_str();
    let commits = match head {
        Some((id, _)) => new_commits(tx, odb, change, id)?,
        None => Vec::new(),
    };
    let commits = commits
        .iter()
        .map(|(id, c)| commit_json(tx, odb, web, &scope, *id, c))
        .collect::<Result<Vec<_>, _>>()?;
    let head_commit = match &head {
        Some((id, c)) => commit_json(tx, odb, web, &scope, *id, c)?,
        None => Value::Null,
    };
    let before = change.old.unwrap_or(ObjectId::NULL);
    let after = change.new.unwrap_or(ObjectId::NULL);
    let compare = match change.old {
        None => format!("{web}/{}/compare/{short_name}", scope.full_name),
        Some(_) => format!(
            "{web}/{}/compare/{}...{}",
            scope.full_name,
            short(before),
            short(after)
        ),
    };
    let pusher = scope.sender.as_ref().map_or_else(
        || json!({"name": scope.full_name, "email": null}),
        |a| json!({"name": a.login, "email": a.email}),
    );
    Ok(Some(Event {
        name: "push",
        action: None,
        repo_id: Some(scope.repo.id),
        org_id: scope.org_id,
        sender_id: change.sender,
        app_id: None,
        installation_id: None,
        payload: object(json!({
            "ref": change.name,
            "before": before,
            "after": after,
            "created": change.old.is_none(),
            "deleted": change.new.is_none(),
            "forced": change.forced,
            "base_ref": null,
            "compare": compare,
            "commits": commits,
            "head_commit": head_commit,
            "pusher": pusher,
        })),
    }))
}

pub(crate) fn create(tx: &Tx, change: &RefChange) -> Option<Event> {
    if change.old.is_some() {
        return None;
    }
    let (short_name, ref_type) = pushed(&change.name)?;
    let scope = scope(tx, change)?;
    Some(Event {
        name: "create",
        action: None,
        repo_id: Some(scope.repo.id),
        org_id: scope.org_id,
        sender_id: change.sender,
        app_id: None,
        installation_id: None,
        payload: object(json!({
            "ref": short_name,
            "ref_type": ref_type,
            "master_branch": scope.repo.default_branch,
            "description": scope.repo.description,
            "pusher_type": "user",
        })),
    })
}

pub(crate) fn delete(tx: &Tx, change: &RefChange) -> Option<Event> {
    if change.old.is_none() || change.new.is_some() {
        return None;
    }
    let (short_name, ref_type) = pushed(&change.name)?;
    let scope = scope(tx, change)?;
    Some(Event {
        name: "delete",
        action: None,
        repo_id: Some(scope.repo.id),
        org_id: scope.org_id,
        sender_id: change.sender,
        app_id: None,
        installation_id: None,
        payload: object(json!({
            "ref": short_name,
            "ref_type": ref_type,
            "pusher_type": "user",
        })),
    })
}

pub(crate) fn for_change(tx: &Tx, odb: &Odb, change: &RefChange) -> Result<Vec<Event>, GitError> {
    Ok(push(tx, odb, change)?
        .into_iter()
        .chain(create(tx, change))
        .chain(delete(tx, change))
        .collect())
}

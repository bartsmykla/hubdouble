use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use hubdouble_store::{Repos, Table, Tx, TxMut};

use crate::contract::{ObjectId, Ref};

const MAX_FORK_DEPTH: usize = 64;

#[derive(Clone, Default)]
pub(crate) struct Refs {
    by_repo: HashMap<i64, Arc<BTreeMap<String, ObjectId>>>,
}

impl Table for Refs {}

pub(crate) fn network_root(tx: &Tx, repo: i64) -> i64 {
    let repos = tx.table::<Repos>();
    let mut id = repo;
    for _ in 0..MAX_FORK_DEPTH {
        match repos.get(id).and_then(|r| r.fork_of) {
            Some(parent) if parent != id => id = parent,
            _ => break,
        }
    }
    id
}

pub(crate) fn get(tx: &Tx, repo: i64, name: &str) -> Option<ObjectId> {
    tx.table::<Refs>()
        .by_repo
        .get(&repo)
        .and_then(|m| m.get(name))
        .copied()
}

pub(crate) fn set(tx: &mut TxMut<'_>, repo: i64, name: &str, target: Option<ObjectId>) {
    let mut table = tx.table_mut::<Refs>();
    let map = Arc::make_mut(table.get_mut().by_repo.entry(repo).or_default());
    match target {
        Some(id) => {
            map.insert(name.to_owned(), id);
        }
        None => {
            map.remove(name);
        }
    }
}

pub(crate) fn branch_ref(branch: &str) -> String {
    if branch.starts_with("refs/") {
        branch.to_owned()
    } else {
        format!("refs/heads/{branch}")
    }
}

pub(crate) fn default_branch(tx: &Tx, repo: i64) -> String {
    let name = tx
        .table::<Repos>()
        .get(repo)
        .map_or("main", |r| r.default_branch.as_str());
    format!("refs/heads/{name}")
}

pub(crate) fn resolve(tx: &Tx, repo: i64, name: &str) -> Option<ObjectId> {
    let map = tx.table::<Refs>().by_repo.get(&repo)?;
    if name == "HEAD" {
        return map.get(&default_branch(tx, repo)).copied();
    }
    if name.starts_with("refs/") {
        return map.get(name).copied();
    }
    ["refs/", "refs/tags/", "refs/heads/"]
        .iter()
        .find_map(|prefix| map.get(&format!("{prefix}{name}")).copied())
}

pub(crate) fn list(tx: &Tx, repo: i64, prefix: &str) -> Vec<Ref> {
    let Some(map) = tx.table::<Refs>().by_repo.get(&repo) else {
        return Vec::new();
    };
    let full = if prefix.is_empty() || prefix.starts_with("refs/") {
        prefix.to_owned()
    } else {
        format!("refs/{prefix}")
    };
    map.range::<str, _>((
        std::ops::Bound::Included(full.as_str()),
        std::ops::Bound::Unbounded,
    ))
    .take_while(|(name, _)| name.starts_with(&full))
    .map(|(name, target)| Ref {
        name: name.clone(),
        target: *target,
    })
    .collect()
}

pub(crate) fn valid_name(name: &str) -> bool {
    name.strip_prefix("refs/")
        .is_some_and(|rest| !rest.is_empty())
        && !name.as_bytes().ends_with(b".lock")
        && !name.ends_with('.')
        && !name.contains("..")
        && !name.contains("@{")
        && name
            .bytes()
            .all(|b| b > 0x20 && b != 0x7f && !b"~^:?*[\\".contains(&b))
        && name
            .split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.'))
}

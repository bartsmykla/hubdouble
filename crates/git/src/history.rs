use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};
use std::sync::Arc;

use crate::contract::{Commit, GitError, ObjectData, ObjectId};
use crate::odb::Odb;

pub(crate) fn ancestors(odb: &Odb, start: ObjectId) -> Result<HashSet<ObjectId>, GitError> {
    let mut seen = HashSet::from([start]);
    let mut stack = vec![start];
    while let Some(id) = stack.pop() {
        for parent in &odb.commit(id)?.parents {
            if seen.insert(*parent) {
                stack.push(*parent);
            }
        }
    }
    Ok(seen)
}

pub(crate) fn is_ancestor(odb: &Odb, ancestor: ObjectId, of: ObjectId) -> Result<bool, GitError> {
    if ancestor == of {
        return Ok(true);
    }
    let mut seen = HashSet::from([of]);
    let mut stack = vec![of];
    while let Some(id) = stack.pop() {
        for parent in &odb.commit(id)?.parents {
            if *parent == ancestor {
                return Ok(true);
            }
            if seen.insert(*parent) {
                stack.push(*parent);
            }
        }
    }
    Ok(false)
}

pub(crate) fn fast_forward(odb: &Odb, old: ObjectId, new: ObjectId) -> Result<bool, GitError> {
    match (odb.get(old)?, odb.get(new)?) {
        (ObjectData::Commit(_), ObjectData::Commit(_)) => is_ancestor(odb, old, new),
        _ => Ok(false),
    }
}

pub(crate) fn best_common(
    odb: &Odb,
    a: &HashSet<ObjectId>,
    b: &HashSet<ObjectId>,
) -> Result<Option<ObjectId>, GitError> {
    let common: Vec<ObjectId> = a.intersection(b).copied().collect();
    let mut covered = HashSet::new();
    for id in &common {
        covered.extend(odb.commit(*id)?.parents.iter().copied());
    }
    let mut best = None;
    for id in common.into_iter().filter(|id| !covered.contains(id)) {
        let time = odb.commit(id)?.committer.time;
        if best.is_none_or(|(t, b)| (time, id) > (t, b)) {
            best = Some((time, id));
        }
    }
    Ok(best.map(|(_, id)| id))
}

pub(crate) fn merge_base(
    odb: &Odb,
    a: ObjectId,
    b: ObjectId,
) -> Result<Option<ObjectId>, GitError> {
    best_common(odb, &ancestors(odb, a)?, &ancestors(odb, b)?)
}

pub(crate) fn topo(
    odb: &Odb,
    head: ObjectId,
    exclude: &HashSet<ObjectId>,
) -> Result<Vec<(ObjectId, Arc<Commit>)>, GitError> {
    let mut out = Vec::new();
    if exclude.contains(&head) {
        return Ok(out);
    }
    let mut seen = HashSet::from([head]);
    let mut stack = vec![(head, odb.commit(head)?, 0usize)];
    while let Some((id, commit, next)) = stack.pop() {
        match commit.parents.get(next).copied() {
            Some(parent) => {
                stack.push((id, commit, next + 1));
                if !exclude.contains(&parent) && seen.insert(parent) {
                    stack.push((parent, odb.commit(parent)?, 0));
                }
            }
            None => out.push((id, commit)),
        }
    }
    Ok(out)
}

pub(crate) fn compare_order(
    odb: &Odb,
    head: ObjectId,
    exclude: &HashSet<ObjectId>,
) -> Result<Vec<(ObjectId, Arc<Commit>)>, GitError> {
    if exclude.contains(&head) {
        return Ok(Vec::new());
    }
    let mut seen = HashSet::from([head]);
    let mut queue = BinaryHeap::from([(odb.commit(head)?.committer.time, Reverse(1), head)]);
    let mut commits = Vec::new();
    while let Some((_, _, id)) = queue.pop() {
        let commit = odb.commit(id)?;
        for &parent in &commit.parents {
            if !exclude.contains(&parent) && seen.insert(parent) {
                let time = odb.commit(parent)?.committer.time;
                queue.push((time, Reverse(seen.len()), parent));
            }
        }
        commits.push((id, commit));
    }
    commits.reverse();
    Ok(commits)
}

pub(crate) fn log(
    odb: &Odb,
    head: ObjectId,
    first_parent: bool,
    limit: usize,
) -> Result<Vec<(ObjectId, Arc<Commit>)>, GitError> {
    let mut out = Vec::new();
    if first_parent {
        let mut next = Some(head);
        while let Some(id) = next.filter(|_| out.len() < limit) {
            let commit = odb.commit(id)?;
            next = commit.parents.first().copied();
            out.push((id, commit));
        }
        return Ok(out);
    }
    let first = odb.commit(head)?;
    let mut seen = HashSet::from([head]);
    let mut queue = BinaryHeap::from([(first.committer.time, head)]);
    while let Some((_, id)) = queue.pop() {
        if out.len() >= limit {
            break;
        }
        let commit = odb.commit(id)?;
        for parent in &commit.parents {
            if seen.insert(*parent) {
                queue.push((odb.commit(*parent)?.committer.time, *parent));
            }
        }
        out.push((id, commit));
    }
    Ok(out)
}

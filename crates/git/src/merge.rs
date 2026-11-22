use std::collections::BTreeMap;

use crate::contract::{EntryMode, GitError, ObjectId, TreeEntry};
use crate::odb::Odb;
use crate::tree;

type Entry = (EntryMode, ObjectId);

pub(crate) fn trees(
    odb: &Odb,
    origin: Option<ObjectId>,
    ours: Option<ObjectId>,
    theirs: Option<ObjectId>,
    write: bool,
) -> Result<Option<ObjectId>, GitError> {
    let mut conflicts = Vec::new();
    let tree = merge(odb, origin, ours, theirs, "", write, &mut conflicts)?;
    if conflicts.is_empty() {
        Ok(tree)
    } else {
        conflicts.sort();
        Err(GitError::Conflict { paths: conflicts })
    }
}

fn entries(odb: &Odb, id: Option<ObjectId>) -> Result<Vec<TreeEntry>, GitError> {
    Ok(match id {
        Some(id) => odb.tree(id)?.to_vec(),
        None => Vec::new(),
    })
}

fn merge(
    odb: &Odb,
    origin: Option<ObjectId>,
    ours: Option<ObjectId>,
    theirs: Option<ObjectId>,
    prefix: &str,
    write: bool,
    conflicts: &mut Vec<String>,
) -> Result<Option<ObjectId>, GitError> {
    if ours == theirs || theirs == origin {
        return Ok(ours);
    }
    if ours == origin {
        return Ok(theirs);
    }
    let mut joined: BTreeMap<String, [Option<Entry>; 3]> = BTreeMap::new();
    for (side, id) in [origin, ours, theirs].into_iter().enumerate() {
        for e in entries(odb, id)? {
            joined.entry(e.name).or_default()[side] = Some((e.mode, e.id));
        }
    }
    let mut out = Vec::with_capacity(joined.len());
    for (name, [o, a, b]) in joined {
        let picked = if a == b || b == o {
            a
        } else if a == o {
            b
        } else {
            let is_dir = |e: Option<Entry>| e.is_none_or(|(m, _)| m == EntryMode::Tree);
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if is_dir(a) && is_dir(b) {
                let dir =
                    |e: Option<Entry>| e.filter(|(m, _)| *m == EntryMode::Tree).map(|(_, id)| id);
                merge(odb, dir(o), dir(a), dir(b), &path, write, conflicts)?
                    .map(|id| (EntryMode::Tree, id))
            } else {
                conflicts.push(path);
                None
            }
        };
        if let Some((mode, id)) = picked {
            out.push(TreeEntry { name, mode, id });
        }
    }
    if out.is_empty() {
        return Ok(None);
    }
    if !write || !conflicts.is_empty() {
        return Ok(Some(ObjectId::NULL));
    }
    tree::put_sorted(odb, out).map(Some)
}

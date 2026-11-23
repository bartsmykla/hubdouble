use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::contract::{EntryMode, FileChange, GitError, ObjectData, ObjectId, TreeEntry};
use crate::odb::Odb;

/// Maximum number of directories below the root of a stored tree.
pub const MAX_TREE_DEPTH: usize = 50;

/// # Errors
/// Returns [`GitError::Invalid`] when `depth` exceeds [`MAX_TREE_DEPTH`].
pub fn check_tree_depth(depth: usize) -> Result<(), GitError> {
    if depth > MAX_TREE_DEPTH {
        return Err(GitError::Invalid(format!(
            "tree exceeds {MAX_TREE_DEPTH} directory levels"
        )));
    }
    Ok(())
}

enum Slot {
    Leaf(EntryMode, ObjectId),
    Dir(BTreeMap<String, Slot>),
}

fn git_order(a: &TreeEntry, b: &TreeEntry) -> Ordering {
    fn key(e: &TreeEntry) -> impl Iterator<Item = u8> + '_ {
        e.name
            .bytes()
            .chain((e.mode == EntryMode::Tree).then_some(b'/'))
    }
    key(a).cmp(key(b))
}

fn valid_entry_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && name != ".git" && !name.contains(['/', '\0'])
}

fn invalid(what: &str, path: &str) -> GitError {
    GitError::Invalid(format!("{what}: {path}"))
}

pub(crate) fn empty(odb: &Odb) -> Result<ObjectId, GitError> {
    odb.put(ObjectData::Tree(Arc::from([])))
}

pub(crate) fn write_entries(odb: &Odb, mut entries: Vec<TreeEntry>) -> Result<ObjectId, GitError> {
    for e in &entries {
        if !valid_entry_name(&e.name) {
            return Err(invalid("invalid tree entry name", &e.name));
        }
        let data = match e.mode {
            EntryMode::Submodule => continue,
            _ => odb.get(e.id)?,
        };
        let fits = match data {
            ObjectData::Blob(_) => e.mode != EntryMode::Tree,
            ObjectData::Tree(_) => e.mode == EntryMode::Tree,
            _ => false,
        };
        if !fits {
            return Err(GitError::WrongKind);
        }
    }
    entries.sort_by(git_order);
    if entries.windows(2).any(|w| w[0].name == w[1].name) {
        return Err(GitError::Invalid("duplicate tree entry".to_owned()));
    }
    odb.put(ObjectData::Tree(entries.into()))
}

pub(crate) fn split(path: &str) -> Result<Vec<&str>, GitError> {
    let parts: Vec<&str> = path.split('/').collect();
    check_tree_depth(parts.len().saturating_sub(1))?;
    if parts.iter().all(|p| valid_entry_name(p)) {
        Ok(parts)
    } else {
        Err(invalid("invalid path", path))
    }
}

fn load(odb: &Odb, id: ObjectId) -> Result<BTreeMap<String, Slot>, GitError> {
    Ok(odb
        .tree(id)?
        .iter()
        .map(|e| (e.name.clone(), Slot::Leaf(e.mode, e.id)))
        .collect())
}

pub(crate) fn apply(
    odb: &Odb,
    base: Option<ObjectId>,
    changes: &[FileChange],
) -> Result<ObjectId, GitError> {
    let mut root = match base {
        Some(id) => load(odb, id)?,
        None => BTreeMap::new(),
    };
    for change in changes {
        let (path, value) = match change {
            FileChange::Write { path, data, mode } => {
                if matches!(mode, EntryMode::Tree | EntryMode::Submodule) {
                    return Err(invalid("a file change cannot write a tree", path));
                }
                (
                    path,
                    Some((*mode, odb.put(ObjectData::Blob(data.clone()))?)),
                )
            }
            FileChange::Delete { path } => (path, None),
        };
        let parts = split(path)?;
        let Some((last, dirs)) = parts.split_last() else {
            return Err(invalid("invalid path", path));
        };
        let mut dir = &mut root;
        for name in dirs {
            if value.is_none() && !dir.contains_key(*name) {
                return Err(invalid("no such path", path));
            }
            let slot = dir
                .entry((*name).to_owned())
                .or_insert_with(|| Slot::Dir(BTreeMap::new()));
            if let Slot::Leaf(EntryMode::Tree, id) = *slot {
                *slot = Slot::Dir(load(odb, id)?);
            }
            dir = match slot {
                Slot::Dir(d) => d,
                Slot::Leaf(..) => return Err(invalid("a parent of the path is a file", path)),
            };
        }
        match value {
            Some((mode, id)) => {
                if matches!(
                    dir.get(*last),
                    Some(Slot::Dir(_) | Slot::Leaf(EntryMode::Tree, _))
                ) {
                    return Err(invalid("the path is a directory", path));
                }
                dir.insert((*last).to_owned(), Slot::Leaf(mode, id));
            }
            None => match dir.remove(*last) {
                Some(Slot::Leaf(mode, _)) if mode != EntryMode::Tree => {}
                _ => return Err(invalid("no such file", path)),
            },
        }
    }
    match write_dir(odb, root)? {
        Some(id) => Ok(id),
        None => empty(odb),
    }
}

fn write_dir(odb: &Odb, dir: BTreeMap<String, Slot>) -> Result<Option<ObjectId>, GitError> {
    let mut entries = Vec::with_capacity(dir.len());
    for (name, slot) in dir {
        match slot {
            Slot::Leaf(mode, id) => entries.push(TreeEntry { name, mode, id }),
            Slot::Dir(d) => {
                if let Some(id) = write_dir(odb, d)? {
                    entries.push(TreeEntry {
                        name,
                        mode: EntryMode::Tree,
                        id,
                    });
                }
            }
        }
    }
    if entries.is_empty() {
        return Ok(None);
    }
    entries.sort_by(git_order);
    odb.put(ObjectData::Tree(entries.into())).map(Some)
}

pub(crate) fn put_sorted(odb: &Odb, mut entries: Vec<TreeEntry>) -> Result<ObjectId, GitError> {
    entries.sort_by(git_order);
    odb.put(ObjectData::Tree(entries.into()))
}

pub(crate) fn lookup(odb: &Odb, root: ObjectId, path: &str) -> Result<ObjectId, GitError> {
    let mut id = root;
    for name in path.split('/').filter(|p| !p.is_empty()) {
        id = odb
            .tree(id)
            .map_err(|_| GitError::NotFound)?
            .iter()
            .find(|e| e.name == name)
            .ok_or(GitError::NotFound)?
            .id;
    }
    Ok(id)
}

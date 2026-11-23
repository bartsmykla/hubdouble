use std::collections::BTreeMap;

use hubdouble_git::{EntryMode, Git, GitError, ObjectData, ObjectId, TreeEntry, check_tree_depth};
use hubdouble_store::Tx;

pub(crate) type Leaves = BTreeMap<String, (EntryMode, ObjectId)>;

pub(crate) fn flatten(
    git: &Git,
    tx: &Tx,
    repo: i64,
    prefix: &str,
    depth: usize,
    tree: ObjectId,
    out: &mut Leaves,
) -> Result<(), GitError> {
    check_tree_depth(depth)?;
    let ObjectData::Tree(entries) = git.object(tx, repo, tree)?.data else {
        return Err(GitError::WrongKind);
    };
    for e in entries.iter() {
        let path = if prefix.is_empty() {
            e.name.clone()
        } else {
            format!("{prefix}/{}", e.name)
        };
        if e.mode == EntryMode::Tree {
            flatten(git, tx, repo, &path, depth + 1, e.id, out)?;
        } else {
            out.insert(path, (e.mode, e.id));
        }
    }
    Ok(())
}

pub(crate) fn build(git: &Git, tx: &Tx, repo: i64, leaves: &Leaves) -> Result<ObjectId, GitError> {
    let mut entries = Vec::new();
    let mut dirs: BTreeMap<&str, Leaves> = BTreeMap::new();
    for (path, (mode, id)) in leaves {
        match path.split_once('/') {
            Some((dir, rest)) => {
                dirs.entry(dir)
                    .or_default()
                    .insert(rest.to_owned(), (*mode, *id));
            }
            None => entries.push(TreeEntry {
                name: path.clone(),
                mode: *mode,
                id: *id,
            }),
        }
    }
    for (dir, inner) in dirs {
        entries.push(TreeEntry {
            name: dir.to_owned(),
            mode: EntryMode::Tree,
            id: build(git, tx, repo, &inner)?,
        });
    }
    git.write_tree(tx, repo, &entries)
}

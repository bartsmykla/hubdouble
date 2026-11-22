use std::io::{self, Write};
use std::sync::Arc;

use hubdouble_git::{EntryMode, Git, ObjectData, ObjectId, ObjectKind, TreeEntry};
use hubdouble_model::ApiError;
use hubdouble_rest::Ctx;
use hubdouble_store::Tx;
use serde::Serialize;

use crate::common::{Links, kind_name};

#[derive(Serialize)]
struct TreeItem {
    path: String,
    mode: &'static str,
    #[serde(rename = "type")]
    kind: &'static str,
    sha: ObjectId,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct GitTree {
    sha: ObjectId,
    pub(crate) url: String,
    tree: Vec<TreeItem>,
    truncated: bool,
}

pub(crate) fn entry_kind(mode: EntryMode) -> ObjectKind {
    match mode {
        EntryMode::Tree => ObjectKind::Tree,
        EntryMode::Submodule => ObjectKind::Commit,
        EntryMode::File | EntryMode::Executable | EntryMode::Symlink => ObjectKind::Blob,
    }
}

#[derive(Clone, Copy)]
struct TreeLimits {
    entries: usize,
    bytes: usize,
}

const RECURSIVE_LIMITS: TreeLimits = TreeLimits {
    entries: 100_000,
    bytes: 7_000_000,
};

struct TreeWalk<'a> {
    git: &'a Git,
    tx: &'a Tx,
    repo: i64,
    links: &'a Links,
    limits: Option<TreeLimits>,
    bytes: usize,
    items: Vec<TreeItem>,
}

#[derive(Default)]
struct ByteCount(usize);

impl Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl TreeWalk<'_> {
    fn visit(&mut self, entries: Arc<[TreeEntry]>) -> Result<bool, ApiError> {
        let mut pending = vec![(String::new(), entries, 0)];
        while let Some((prefix, entries, next)) = pending.last_mut() {
            let Some(entry) = entries.get(*next) else {
                pending.pop();
                continue;
            };
            *next += 1;
            if self
                .limits
                .is_some_and(|max| self.items.len() >= max.entries)
            {
                return Ok(true);
            }
            let path = if prefix.is_empty() {
                entry.name.clone()
            } else {
                format!("{prefix}/{}", entry.name)
            };
            let kind = entry_kind(entry.mode);
            let size = if kind == ObjectKind::Blob {
                match self.git.object(self.tx, self.repo, entry.id)?.data {
                    ObjectData::Blob(data) => Some(data.len()),
                    _ => None,
                }
            } else {
                None
            };
            let descend = self.limits.is_some() && kind == ObjectKind::Tree;
            let child_path = descend.then(|| path.clone());
            let item = TreeItem {
                path,
                mode: entry.mode.as_str(),
                kind: kind_name(kind),
                sha: entry.id,
                size,
                url: (kind != ObjectKind::Commit).then(|| self.links.object(kind, entry.id)),
            };
            if let Some(max) = self.limits {
                let mut size = ByteCount::default();
                serde_json::to_writer(&mut size, &item)
                    .map_err(|error| ApiError::internal(error.to_string()))?;
                self.bytes += size.0 + usize::from(!self.items.is_empty());
                if self.bytes > max.bytes {
                    return Ok(true);
                }
            }
            self.items.push(item);
            if let Some(path) = child_path
                && let ObjectData::Tree(children) =
                    self.git.object(self.tx, self.repo, entry.id)?.data
            {
                pending.push((path, children, 0));
            }
        }
        Ok(false)
    }
}

pub(crate) fn render_tree(
    ctx: &Ctx<'_>,
    id: ObjectId,
    entries: Arc<[TreeEntry]>,
    recursive: bool,
) -> Result<GitTree, ApiError> {
    let links = Links::of(ctx)?;
    let mut walk = TreeWalk {
        git: ctx.git(),
        tx: ctx.tx(),
        repo: ctx.repo()?.id,
        links: &links,
        limits: recursive.then_some(RECURSIVE_LIMITS),
        bytes: 2,
        items: Vec::with_capacity(if recursive {
            entries.len().min(RECURSIVE_LIMITS.entries)
        } else {
            entries.len()
        }),
    };
    let truncated = walk.visit(entries)?;
    Ok(GitTree {
        sha: id,
        url: links.object(ObjectKind::Tree, id),
        tree: walk.items,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use hubdouble_git::GitBuilder;
    use hubdouble_store::StoreBuilder;

    use super::*;

    #[test]
    fn recursive_tree_stops_at_both_limits() {
        let mut builder = StoreBuilder::new();
        let git = GitBuilder::new(&mut builder).build();
        builder
            .build()
            .view(|tx| {
                let repo = 1;
                let blob = git.write_blob(tx, repo, b"data")?;
                let entry = |name: &str, mode, id| TreeEntry {
                    name: name.into(),
                    mode,
                    id,
                };
                let sub = git.write_tree(tx, repo, &[entry("abcdef", EntryMode::File, blob)])?;
                let entries: Arc<[TreeEntry]> = [
                    entry("folder", EntryMode::Tree, sub),
                    entry("top", EntryMode::File, blob),
                ]
                .into();
                let links = Links {
                    api: "http://test/repos/org/repo".into(),
                    web: String::new(),
                };
                let walk = |limits| {
                    let mut walk = TreeWalk {
                        git: &git,
                        tx,
                        repo,
                        links: &links,
                        limits,
                        bytes: 2,
                        items: Vec::new(),
                    };
                    let truncated = walk.visit(entries.clone())?;
                    Ok::<_, ApiError>((walk.items, truncated))
                };

                let (all, truncated) = walk(Some(RECURSIVE_LIMITS))?;
                assert!(!truncated);
                let paths: Vec<_> = all.iter().map(|item| item.path.as_str()).collect();
                assert_eq!(paths, ["folder", "folder/abcdef", "top"]);
                let bytes = serde_json::to_vec(&all[..1]).unwrap().len();
                for limits in [
                    TreeLimits {
                        entries: 1,
                        bytes: usize::MAX,
                    },
                    TreeLimits {
                        entries: usize::MAX,
                        bytes,
                    },
                ] {
                    let (items, truncated) = walk(Some(limits))?;
                    assert!(truncated);
                    assert_eq!(items.len(), 1);
                    assert_eq!(items[0].path, "folder");
                }
                let (items, truncated) = walk(None)?;
                assert!(!truncated);
                assert_eq!(items.len(), 2);
                assert_eq!(items[1].path, "top");
                Ok(())
            })
            .unwrap();
    }
}

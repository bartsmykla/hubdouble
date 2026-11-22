use std::collections::BTreeMap;
use std::sync::Arc;

use hubdouble_git::{
    Commit, EntryMode, Git, GitError, Object, ObjectData, ObjectId, ObjectKind, Ref, Signature,
    Tag, TreeEntry,
};
use hubdouble_model::{ApiError, Timestamp};
use hubdouble_store::{Accounts, Repos, Tx, TxMut};
use serde::Deserialize;

use crate::template;

pub(crate) const SEED_NAME: &str = "hubdouble";
pub(crate) const SEED_EMAIL: &str = "seed@hubdouble.local";

pub(crate) trait Vcs {
    fn resolve(&self, tx: &Tx, repo: i64, name: &str) -> Result<ObjectId, GitError>;
    fn refs(&self, tx: &Tx, repo: i64, prefix: &str) -> Vec<Ref>;
    fn update_ref(
        &self,
        tx: &mut TxMut<'_>,
        repo: i64,
        name: &str,
        old: Option<ObjectId>,
        new: ObjectId,
    ) -> Result<(), GitError>;
    fn object(&self, tx: &Tx, repo: i64, id: ObjectId) -> Result<Object, GitError>;
    fn write_blob(&self, tx: &Tx, repo: i64, data: &[u8]) -> Result<ObjectId, GitError>;
    fn write_tree(&self, tx: &Tx, repo: i64, entries: &[TreeEntry]) -> Result<ObjectId, GitError>;
    fn write_commit(&self, tx: &Tx, repo: i64, commit: &Commit) -> Result<ObjectId, GitError>;
    fn write_tag(&self, tx: &Tx, repo: i64, tag: &Tag) -> Result<ObjectId, GitError>;
}

impl Vcs for Git {
    fn resolve(&self, tx: &Tx, repo: i64, name: &str) -> Result<ObjectId, GitError> {
        self.resolve_ref(tx, repo, name)
    }

    fn refs(&self, tx: &Tx, repo: i64, prefix: &str) -> Vec<Ref> {
        Git::refs(self, tx, repo, prefix)
    }

    fn update_ref(
        &self,
        tx: &mut TxMut<'_>,
        repo: i64,
        name: &str,
        old: Option<ObjectId>,
        new: ObjectId,
    ) -> Result<(), GitError> {
        let sender = tx
            .table::<Repos>()
            .get(repo)
            .ok_or(GitError::NotFound)?
            .owner_id;
        Git::update_ref(self, tx, repo, sender, name, old, Some(new), false)
    }

    fn object(&self, tx: &Tx, repo: i64, id: ObjectId) -> Result<Object, GitError> {
        Git::object(self, tx, repo, id)
    }

    fn write_blob(&self, tx: &Tx, repo: i64, data: &[u8]) -> Result<ObjectId, GitError> {
        Git::write_blob(self, tx, repo, data)
    }

    fn write_tree(&self, tx: &Tx, repo: i64, entries: &[TreeEntry]) -> Result<ObjectId, GitError> {
        Git::write_tree(self, tx, repo, entries)
    }

    fn write_commit(&self, tx: &Tx, repo: i64, commit: &Commit) -> Result<ObjectId, GitError> {
        Git::write_commit(self, tx, repo, commit)
    }

    fn write_tag(&self, tx: &Tx, repo: i64, tag: &Tag) -> Result<ObjectId, GitError> {
        Git::write_tag(self, tx, repo, tag)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersonSeed {
    name: String,
    email: String,
    #[serde(default)]
    date: Option<Timestamp>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TagSeed {
    name: String,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommitSeed {
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    files: BTreeMap<String, Option<String>>,
    #[serde(default)]
    author: Option<PersonSeed>,
    #[serde(default)]
    tags: Vec<TagSeed>,
}

#[derive(Debug, Default)]
pub(crate) struct History {
    pub files: BTreeMap<String, Arc<[u8]>>,
    pub tags: Vec<TagSeed>,
    pub branches: Vec<String>,
    pub commits: Vec<CommitSeed>,
}

pub(crate) fn seed_signature() -> Signature {
    Signature {
        name: SEED_NAME.to_owned(),
        email: SEED_EMAIL.to_owned(),
        time: Timestamp::SEED,
        offset_minutes: 0,
    }
}

fn signature(p: Option<&PersonSeed>) -> Signature {
    p.map_or_else(seed_signature, |p| Signature {
        name: p.name.clone(),
        email: p.email.clone(),
        time: p.date.unwrap_or(Timestamp::SEED),
        offset_minutes: 0,
    })
}

fn git_error(what: &str, e: GitError) -> ApiError {
    let mut api = ApiError::from(e);
    api.message = format!("{what}: {}", api.message);
    api
}

fn heads(branch: &str) -> String {
    format!("refs/heads/{branch}")
}

pub(crate) fn copy_refs(
    vcs: &impl Vcs,
    tx: &mut TxMut<'_>,
    parent: i64,
    fork: i64,
) -> Result<(), ApiError> {
    for prefix in ["refs/heads/", "refs/tags/"] {
        for r in vcs.refs(tx, parent, prefix) {
            vcs.update_ref(tx, fork, &r.name, None, r.target)
                .map_err(|e| git_error(&r.name, e))?;
        }
    }
    Ok(())
}

pub(crate) fn write(
    vcs: &impl Vcs,
    tx: &mut TxMut<'_>,
    repo: i64,
    default_branch: &str,
    history: History,
) -> Result<bool, ApiError> {
    let mut committed = false;
    if history.files.is_empty() {
        if !history.tags.is_empty() {
            return Err(ApiError::unprocessable(
                "tags: a repository without files has no commit to tag",
            ));
        }
    } else {
        let changes = history
            .files
            .into_iter()
            .map(|(path, data)| (path, Some(data)))
            .collect();
        let id = commit(
            vcs,
            tx,
            repo,
            default_branch,
            None,
            changes,
            &seed_signature(),
            "Initial commit",
        )
        .map_err(|e| git_error("files", e))?;
        tag(vcs, tx, repo, &history.tags, id)?;
        committed = true;
    }
    for branch in &history.branches {
        let head = vcs
            .resolve(tx, repo, &heads(default_branch))
            .map_err(|e| git_error(branch, e))?;
        vcs.update_ref(tx, repo, &heads(branch), None, head)
            .map_err(|e| git_error(branch, e))?;
    }
    for (i, c) in history.commits.into_iter().enumerate() {
        let what = format!("commits[{i}]");
        let branch = c.branch.as_deref().unwrap_or(default_branch);
        let parent = match vcs.resolve(tx, repo, &heads(branch)) {
            Ok(id) => Some(id),
            Err(GitError::NotFound) => vcs.resolve(tx, repo, &heads(default_branch)).ok(),
            Err(e) => return Err(git_error(&what, e)),
        };
        let mut changes = BTreeMap::new();
        for (path, content) in c.files {
            let data = match content {
                Some(text) => Some(expand_sha(vcs, tx, &text)?.into_bytes().into()),
                None => None,
            };
            changes.insert(path, data);
        }
        let message = c.message.as_deref().unwrap_or("Update");
        let id = commit(
            vcs,
            tx,
            repo,
            branch,
            parent,
            changes,
            &signature(c.author.as_ref()),
            message,
        )
        .map_err(|e| git_error(&what, e))?;
        tag(vcs, tx, repo, &c.tags, id)?;
        committed = true;
    }
    Ok(committed)
}

type Files = BTreeMap<String, (EntryMode, ObjectId)>;

#[expect(clippy::too_many_arguments, reason = "one seeded commit and its place")]
fn commit(
    vcs: &impl Vcs,
    tx: &mut TxMut<'_>,
    repo: i64,
    branch: &str,
    parent: Option<ObjectId>,
    changes: BTreeMap<String, Option<Arc<[u8]>>>,
    who: &Signature,
    message: &str,
) -> Result<ObjectId, GitError> {
    let mut files = Files::new();
    if let Some(p) = parent {
        let ObjectData::Commit(c) = vcs.object(tx, repo, p)?.data else {
            return Err(GitError::WrongKind);
        };
        read_tree(vcs, tx, repo, c.tree, "", &mut files)?;
    }
    for (path, data) in changes {
        match data {
            Some(data) => {
                let id = vcs.write_blob(tx, repo, &data)?;
                files.insert(path, (EntryMode::File, id));
            }
            None => {
                files.remove(&path);
            }
        }
    }
    let tree = write_tree(vcs, tx, repo, &files)?;
    let id = vcs.write_commit(
        tx,
        repo,
        &Commit {
            tree,
            parents: parent.into_iter().collect(),
            author: who.clone(),
            committer: who.clone(),
            message: message.to_owned(),
            gpg_signature: None,
        },
    )?;
    let old = vcs.resolve(tx, repo, &heads(branch)).ok();
    vcs.update_ref(tx, repo, &heads(branch), old, id)?;
    Ok(id)
}

fn read_tree(
    vcs: &impl Vcs,
    tx: &Tx,
    repo: i64,
    tree: ObjectId,
    prefix: &str,
    out: &mut Files,
) -> Result<(), GitError> {
    let ObjectData::Tree(entries) = vcs.object(tx, repo, tree)?.data else {
        return Err(GitError::WrongKind);
    };
    for e in entries.iter() {
        let path = format!("{prefix}{}", e.name);
        if e.mode == EntryMode::Tree {
            read_tree(vcs, tx, repo, e.id, &format!("{path}/"), out)?;
        } else {
            out.insert(path, (e.mode, e.id));
        }
    }
    Ok(())
}

enum Node {
    File(EntryMode, ObjectId),
    Dir(BTreeMap<String, Node>),
}

fn write_tree(vcs: &impl Vcs, tx: &Tx, repo: i64, files: &Files) -> Result<ObjectId, GitError> {
    let mut root = BTreeMap::new();
    for (path, (mode, id)) in files {
        let mut dir = &mut root;
        let mut parts = path.split('/').filter(|p| !p.is_empty()).peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                dir.insert(part.to_owned(), Node::File(*mode, *id));
                break;
            }
            let next = dir
                .entry(part.to_owned())
                .or_insert_with(|| Node::Dir(BTreeMap::new()));
            let Node::Dir(d) = next else {
                return Err(GitError::Invalid(format!("{path} is under a file")));
            };
            dir = d;
        }
    }
    write_dir(vcs, tx, repo, &root)
}

fn write_dir(
    vcs: &impl Vcs,
    tx: &Tx,
    repo: i64,
    dir: &BTreeMap<String, Node>,
) -> Result<ObjectId, GitError> {
    let mut entries = Vec::with_capacity(dir.len());
    for (name, node) in dir {
        let (mode, id) = match node {
            Node::File(mode, id) => (*mode, *id),
            Node::Dir(d) => (EntryMode::Tree, write_dir(vcs, tx, repo, d)?),
        };
        entries.push(TreeEntry {
            name: name.clone(),
            mode,
            id,
        });
    }
    entries.sort_by_key(git_order);
    vcs.write_tree(tx, repo, &entries)
}

fn git_order(e: &TreeEntry) -> Vec<u8> {
    let mut key = e.name.as_bytes().to_vec();
    if e.mode == EntryMode::Tree {
        key.push(b'/');
    }
    key
}

fn tag(
    vcs: &impl Vcs,
    tx: &mut TxMut<'_>,
    repo: i64,
    tags: &[TagSeed],
    commit: ObjectId,
) -> Result<(), ApiError> {
    for t in tags {
        let target = match &t.message {
            None => commit,
            Some(message) => vcs
                .write_tag(
                    tx,
                    repo,
                    &Tag {
                        target: commit,
                        target_kind: ObjectKind::Commit,
                        name: t.name.clone(),
                        tagger: Some(seed_signature()),
                        message: message.clone(),
                        gpg_signature: None,
                    },
                )
                .map_err(|e| git_error(&t.name, e))?,
        };
        vcs.update_ref(tx, repo, &format!("refs/tags/{}", t.name), None, target)
            .map_err(|e| git_error(&t.name, e))?;
    }
    Ok(())
}

fn peel(vcs: &impl Vcs, tx: &Tx, repo: i64, mut id: ObjectId) -> Result<ObjectId, GitError> {
    loop {
        match vcs.object(tx, repo, id)?.data {
            ObjectData::Tag(tag) => id = tag.target,
            _ => return Ok(id),
        }
    }
}

pub(crate) fn expand_sha(vcs: &impl Vcs, tx: &Tx, text: &str) -> Result<String, ApiError> {
    template::expand(text, |call, args| {
        let ("sha", [full, name]) = (call, args) else {
            return Ok(None);
        };
        let unknown = || ApiError::unprocessable(format!("sha: {full} has no ref {name} yet"));
        let (owner, repo) = full.split_once('/').ok_or_else(unknown)?;
        let repo = tx
            .table::<Accounts>()
            .by_login(owner)
            .and_then(|o| tx.table::<Repos>().by_name(o.id, repo))
            .ok_or_else(unknown)?
            .id;
        let candidates = if name.starts_with("refs/") {
            vec![name.clone()]
        } else {
            vec![heads(name), format!("refs/tags/{name}")]
        };
        let id = candidates
            .iter()
            .find_map(|c| vcs.resolve(tx, repo, c).ok())
            .ok_or_else(unknown)?;
        let id = peel(vcs, tx, repo, id).map_err(|_| unknown())?;
        Ok(Some(id.to_string()))
    })
}

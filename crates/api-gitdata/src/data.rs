use std::collections::BTreeMap;
use std::sync::Arc;

use http::StatusCode;
use http::header::LOCATION;
use hubdouble_auth::Actor;
use hubdouble_git::{
    Commit, EntryMode, Git, GitError, ObjectData, ObjectId, ObjectKind, Ref, Tag, TreeEntry,
};
use hubdouble_model::ApiError;
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::Tx;
use serde::{Deserialize, Deserializer, Serialize};

use crate::common::{
    GitPerson, Links, ObjectLink, ParentLink, Verification, accepts, decode_base64, kind_name,
    object_node_id, parse_oid, ref_node_id, sender, signature_of, unprocessable, wrapped_base64,
};

pub(crate) const RAW: &str = "application/vnd.github.raw; charset=utf-8";

#[derive(Deserialize)]
struct NewBlob {
    content: String,
    #[serde(default)]
    encoding: Option<String>,
}

#[derive(Serialize)]
struct ShortBlob {
    url: String,
    sha: ObjectId,
}

pub(crate) fn create_blob(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let body: NewBlob = ctx.body()?;
    let data = match body.encoding.as_deref().unwrap_or("utf-8") {
        "utf-8" | "utf8" => body.content.into_bytes(),
        "base64" => decode_base64(&body.content)
            .ok_or_else(|| unprocessable("Blob", "content", "content is not valid Base64"))?,
        _ => {
            return Err(unprocessable(
                "Blob",
                "encoding",
                "encoding must be utf-8 or base64",
            ));
        }
    };
    let links = Links::of(ctx)?;
    let repo = ctx.repo()?.id;
    let sha = ctx.git().write_blob(ctx.tx(), repo, &data)?;
    let url = links.object(ObjectKind::Blob, sha);
    Reply::json(&ShortBlob {
        url: url.clone(),
        sha,
    })?
    .status(StatusCode::CREATED)
    .header(LOCATION, &url)
}

#[derive(Serialize)]
struct Blob {
    sha: ObjectId,
    node_id: String,
    size: usize,
    url: String,
    content: String,
    encoding: &'static str,
}

fn object_param(ctx: &Ctx<'_>, name: &str) -> Result<(ObjectId, ObjectData), ApiError> {
    let id = parse_oid(ctx.param(name)?).ok_or_else(ApiError::not_found)?;
    let object = ctx.git().object(ctx.tx(), ctx.repo()?.id, id)?;
    Ok((id, object.data))
}

pub(crate) fn get_blob(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let (sha, ObjectData::Blob(data)) = object_param(ctx, "file_sha")? else {
        return Err(ApiError::not_found());
    };
    if accepts(ctx, "raw") {
        return Ok(Reply::raw(RAW, data.to_vec()));
    }
    let repo = ctx.repo()?.id;
    Reply::json(&Blob {
        sha,
        node_id: object_node_id(ObjectKind::Blob, repo, sha),
        size: data.len(),
        url: Links::of(ctx)?.object(ObjectKind::Blob, sha),
        content: wrapped_base64(&data),
        encoding: "base64",
    })
}

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
struct GitTree {
    sha: ObjectId,
    url: String,
    tree: Vec<TreeItem>,
    truncated: bool,
}

fn entry_kind(mode: EntryMode) -> ObjectKind {
    match mode {
        EntryMode::Tree => ObjectKind::Tree,
        EntryMode::Submodule => ObjectKind::Commit,
        EntryMode::File | EntryMode::Executable | EntryMode::Symlink => ObjectKind::Blob,
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "one recursive walk step needs the store, the repository and where it is"
)]
fn tree_items(
    git: &Git,
    tx: &Tx,
    repo: i64,
    links: &Links,
    prefix: &str,
    entries: &[TreeEntry],
    recursive: bool,
    out: &mut Vec<TreeItem>,
) -> Result<(), ApiError> {
    for e in entries {
        let path = if prefix.is_empty() {
            e.name.clone()
        } else {
            format!("{prefix}/{}", e.name)
        };
        let kind = entry_kind(e.mode);
        let (size, url, children) = match kind {
            ObjectKind::Blob => match git.object(tx, repo, e.id)?.data {
                ObjectData::Blob(data) => (Some(data.len()), Some(links.object(kind, e.id)), None),
                _ => (None, Some(links.object(kind, e.id)), None),
            },
            ObjectKind::Tree => {
                let children = match recursive.then(|| git.object(tx, repo, e.id)) {
                    Some(Ok(o)) => match o.data {
                        ObjectData::Tree(t) => Some(t),
                        _ => None,
                    },
                    Some(Err(err)) => return Err(err.into()),
                    None => None,
                };
                (None, Some(links.object(kind, e.id)), children)
            }
            ObjectKind::Commit | ObjectKind::Tag => (None, None, None),
        };
        out.push(TreeItem {
            path: path.clone(),
            mode: e.mode.as_str(),
            kind: kind_name(kind),
            sha: e.id,
            size,
            url,
        });
        if let Some(children) = children {
            tree_items(git, tx, repo, links, &path, &children, true, out)?;
        }
    }
    Ok(())
}

fn render_tree(
    ctx: &Ctx<'_>,
    id: ObjectId,
    entries: &[TreeEntry],
    recursive: bool,
) -> Result<GitTree, ApiError> {
    let links = Links::of(ctx)?;
    let mut tree = Vec::with_capacity(entries.len());
    tree_items(
        ctx.git(),
        ctx.tx(),
        ctx.repo()?.id,
        &links,
        "",
        entries,
        recursive,
        &mut tree,
    )?;
    Ok(GitTree {
        sha: id,
        url: links.object(ObjectKind::Tree, id),
        tree,
        truncated: false,
    })
}

pub(crate) fn tree_of(
    git: &Git,
    tx: &Tx,
    repo: i64,
    rev: &str,
) -> Result<(ObjectId, Arc<[TreeEntry]>), GitError> {
    let object = git.expression(tx, repo, rev)?;
    if let ObjectData::Tree(entries) = object.data {
        return Ok((object.id, entries));
    }
    let root = git.expression(tx, repo, &format!("{rev}:"))?;
    match root.data {
        ObjectData::Tree(entries) => Ok((root.id, entries)),
        _ => Err(GitError::WrongKind),
    }
}

pub(crate) fn get_tree(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let rev = ctx.param("tree_sha")?;
    let (id, entries) =
        tree_of(ctx.git(), ctx.tx(), ctx.repo()?.id, rev).map_err(|_| ApiError::not_found())?;
    let recursive = ctx.query().get("recursive").is_some();
    Reply::json(&render_tree(ctx, id, &entries, recursive)?)
}

fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

#[derive(Deserialize)]
#[expect(
    clippy::option_option,
    reason = "an absent sha, a null sha (delete the path) and a sha are three requests"
)]
struct NewTreeEntry {
    path: String,
    mode: String,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default, deserialize_with = "present")]
    sha: Option<Option<String>>,
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct NewTree {
    tree: Vec<NewTreeEntry>,
    #[serde(default)]
    base_tree: Option<String>,
}

type Leaves = BTreeMap<String, (EntryMode, ObjectId)>;

fn parse_mode(mode: &str) -> Option<EntryMode> {
    Some(match mode {
        "100644" => EntryMode::File,
        "100755" => EntryMode::Executable,
        "120000" => EntryMode::Symlink,
        "040000" | "40000" => EntryMode::Tree,
        "160000" => EntryMode::Submodule,
        _ => return None,
    })
}

fn flatten(
    git: &Git,
    tx: &Tx,
    repo: i64,
    prefix: &str,
    tree: ObjectId,
    out: &mut Leaves,
) -> Result<(), GitError> {
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
            flatten(git, tx, repo, &path, e.id, out)?;
        } else {
            out.insert(path, (e.mode, e.id));
        }
    }
    Ok(())
}

fn clear(leaves: &mut Leaves, path: &str) {
    let below = format!("{path}/");
    leaves.retain(|p, _| p != path && !p.starts_with(&below));
    let mut at = path;
    while let Some((parent, _)) = at.rsplit_once('/') {
        leaves.remove(parent);
        at = parent;
    }
}

fn build(git: &Git, tx: &Tx, repo: i64, leaves: &Leaves) -> Result<ObjectId, GitError> {
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

fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != "..")
}

pub(crate) fn create_tree(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let body: NewTree = ctx.body()?;
    let repo = ctx.repo()?.id;
    let (git, tx) = (ctx.git(), ctx.tx());
    let mut leaves = Leaves::new();
    if let Some(base) = &body.base_tree {
        let base = parse_oid(base)
            .ok_or_else(|| unprocessable("Tree", "base_tree", "base_tree is not a valid tree"))?;
        flatten(git, tx, repo, "", base, &mut leaves)
            .map_err(|_| unprocessable("Tree", "base_tree", "base_tree is not a valid tree"))?;
    }
    for e in body.tree {
        let bad = |field: &str| unprocessable("Tree", field, &format!("tree.{field} is invalid"));
        if !valid_path(&e.path) {
            return Err(bad("path"));
        }
        let mode = parse_mode(&e.mode).ok_or_else(|| bad("mode"))?;
        if e.kind
            .as_deref()
            .is_some_and(|k| k != kind_name(entry_kind(mode)))
        {
            return Err(bad("type"));
        }
        match (e.sha, e.content) {
            (Some(Some(_)), Some(_)) => return Err(bad("content")),
            (Some(None), _) => clear(&mut leaves, &e.path),
            (None, Some(content)) => {
                let id = git.write_blob(tx, repo, content.as_bytes())?;
                clear(&mut leaves, &e.path);
                leaves.insert(e.path, (mode, id));
            }
            (Some(Some(sha)), None) => {
                let id = parse_oid(&sha).ok_or_else(|| bad("sha"))?;
                clear(&mut leaves, &e.path);
                if mode == EntryMode::Submodule {
                    leaves.insert(e.path, (mode, id));
                    continue;
                }
                let kind = git
                    .object(tx, repo, id)
                    .map_err(|_| bad("sha"))?
                    .data
                    .kind();
                match (kind, mode) {
                    (ObjectKind::Tree, EntryMode::Tree) => {
                        flatten(git, tx, repo, &e.path, id, &mut leaves)?;
                    }
                    (ObjectKind::Blob, m) if m != EntryMode::Tree => {
                        leaves.insert(e.path, (mode, id));
                    }
                    _ => return Err(bad("sha")),
                }
            }
            (None, None) => return Err(bad("sha")),
        }
    }
    let id = build(git, tx, repo, &leaves)?;
    let ObjectData::Tree(entries) = git.object(tx, repo, id)?.data else {
        return Err(ApiError::internal("a written tree is not a tree"));
    };
    let tree = render_tree(ctx, id, &entries, false)?;
    Reply::json(&tree)?
        .status(StatusCode::CREATED)
        .header(LOCATION, &tree.url)
}

#[derive(Serialize)]
pub(crate) struct GitCommit {
    sha: ObjectId,
    node_id: String,
    url: String,
    html_url: String,
    author: GitPerson,
    committer: GitPerson,
    message: String,
    tree: ObjectLink,
    parents: Vec<ParentLink>,
    verification: Verification,
}

impl GitCommit {
    pub fn new(git: &Git, repo: i64, links: &Links, id: ObjectId, c: &Commit) -> GitCommit {
        GitCommit {
            sha: id,
            node_id: object_node_id(ObjectKind::Commit, repo, id),
            url: links.object(ObjectKind::Commit, id),
            html_url: format!("{}/commit/{id}", links.web),
            author: (&c.author).into(),
            committer: (&c.committer).into(),
            message: c.message.clone(),
            tree: ObjectLink {
                sha: c.tree,
                url: links.object(ObjectKind::Tree, c.tree),
            },
            parents: c
                .parents
                .iter()
                .map(|p| ParentLink {
                    sha: *p,
                    url: links.object(ObjectKind::Commit, *p),
                    html_url: format!("{}/commit/{p}", links.web),
                })
                .collect(),
            verification: Verification::of_commit(git, repo, id, c),
        }
    }
}

pub(crate) fn get_git_commit(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let (id, ObjectData::Commit(c)) = object_param(ctx, "commit_sha")? else {
        return Err(ApiError::not_found());
    };
    let links = Links::of(ctx)?;
    Reply::json(&GitCommit::new(ctx.git(), ctx.repo()?.id, &links, id, &c))
}

#[derive(Deserialize)]
struct NewPerson {
    name: String,
    email: String,
    #[serde(default)]
    date: Option<String>,
}

#[derive(Deserialize)]
struct NewCommit {
    message: String,
    tree: String,
    #[serde(default)]
    parents: Vec<String>,
    #[serde(default)]
    author: Option<NewPerson>,
    #[serde(default)]
    committer: Option<NewPerson>,
    #[serde(default)]
    signature: Option<String>,
}

pub(crate) fn create_git_commit(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let body: NewCommit = ctx.body()?;
    let repo = ctx.repo()?.id;
    let (git, tx) = (ctx.git(), ctx.tx());
    let kind_of = |sha: &str| {
        parse_oid(sha).and_then(|id| Some((id, git.object(tx, repo, id).ok()?.data.kind())))
    };
    let Some((tree, ObjectKind::Tree)) = kind_of(&body.tree) else {
        return Err(unprocessable("Commit", "tree", "Tree SHA does not exist"));
    };
    let mut parents = Vec::with_capacity(body.parents.len());
    for p in &body.parents {
        match kind_of(p) {
            Some((id, ObjectKind::Commit)) => parents.push(id),
            _ => {
                return Err(unprocessable(
                    "Commit",
                    "parents",
                    "Parent SHA does not exist or is not a commit object",
                ));
            }
        }
    }
    let now = ctx.now();
    let person = |p: Option<&NewPerson>,
                  field: &str|
     -> Result<Option<hubdouble_git::Signature>, ApiError> {
        match p {
            Some(p) => {
                let time = match &p.date {
                    Some(d) => hubdouble_model::Timestamp::parse(d).map_err(|_| {
                        unprocessable(
                            "Commit",
                            field,
                            &format!("{field}.date is not a valid date"),
                        )
                    })?,
                    None => now,
                };
                Ok(Some(hubdouble_git::Signature {
                    name: p.name.clone(),
                    email: p.email.clone(),
                    time,
                    offset_minutes: 0,
                }))
            }
            None => Ok(None),
        }
    };
    let given_author = person(body.author.as_ref(), "author")?;
    let given_committer = person(body.committer.as_ref(), "committer")?;
    let signature = body.signature.filter(|s| !s.is_empty());
    let web_flow = matches!(ctx.actor(), Actor::Installation { .. })
        && given_author.is_none()
        && given_committer.is_none()
        && signature.is_none();
    let author = match given_author {
        Some(a) => a,
        None => signature_of(sender(ctx.actor())?, now),
    };
    let committer = given_committer.unwrap_or_else(|| author.clone());
    let mut commit = Commit {
        tree,
        parents,
        author,
        committer,
        message: body.message,
        gpg_signature: signature,
    };
    if web_flow {
        git.sign(&mut commit)?;
    }
    let id = git.write_commit(tx, repo, &commit)?;
    let links = Links::of(ctx)?;
    let commit = GitCommit::new(git, repo, &links, id, &commit);
    Reply::json(&commit)?
        .status(StatusCode::CREATED)
        .header(LOCATION, &commit.url)
}

#[derive(Serialize)]
struct RefObject {
    #[serde(rename = "type")]
    kind: &'static str,
    sha: ObjectId,
    url: String,
}

#[derive(Serialize)]
struct GitRef {
    #[serde(rename = "ref")]
    name: String,
    node_id: String,
    url: String,
    object: RefObject,
}

fn render_ref(ctx: &Ctx<'_>, links: &Links, r: &Ref) -> Result<GitRef, ApiError> {
    let repo = ctx.repo()?.id;
    let kind = ctx.git().object(ctx.tx(), repo, r.target)?.data.kind();
    Ok(GitRef {
        node_id: ref_node_id(repo, &r.name),
        url: format!(
            "{}/git/{}",
            links.api,
            r.name.strip_prefix("refs/").unwrap_or(&r.name)
        ),
        object: RefObject {
            kind: kind_name(kind),
            sha: r.target,
            url: links.object(kind, r.target),
        },
        name: r.name.clone(),
    })
}

fn full_ref(name: &str) -> String {
    format!("refs/{name}")
}

pub(crate) fn get_ref(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let name = full_ref(ctx.param("ref")?);
    let target = exact_ref(ctx, &name).ok_or_else(ApiError::not_found)?;
    let links = Links::of(ctx)?;
    Reply::json(&render_ref(ctx, &links, &Ref { name, target })?)
}

fn exact_ref(ctx: &Ctx<'_>, name: &str) -> Option<ObjectId> {
    let repo = ctx.repo().ok()?.id;
    ctx.git()
        .refs(ctx.tx(), repo, name)
        .into_iter()
        .find(|r| r.name == name)
        .map(|r| r.target)
}

pub(crate) fn list_matching_refs(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let prefix = full_ref(ctx.param("ref")?);
    let links = Links::of(ctx)?;
    let refs = ctx.git().refs(ctx.tx(), ctx.repo()?.id, &prefix);
    let rendered = refs
        .iter()
        .map(|r| render_ref(ctx, &links, r))
        .collect::<Result<Vec<_>, _>>()?;
    Reply::json(&rendered)
}

#[derive(Deserialize)]
struct NewRef {
    #[serde(rename = "ref")]
    name: String,
    sha: String,
}

pub(crate) fn create_ref(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let body: NewRef = ctx.body()?;
    if !body.name.starts_with("refs/") || body.name.matches('/').count() < 2 {
        return Err(unprocessable(
            "Reference",
            "ref",
            "Reference name must be fully qualified and contain at least two slashes",
        ));
    }
    let sha = parse_oid(&body.sha)
        .ok_or_else(|| unprocessable("Reference", "sha", "Object does not exist"))?;
    let repo = ctx.repo()?.id;
    let account = sender(ctx.actor())?.id;
    if ctx.git().object(ctx.tx(), repo, sha).is_err() {
        return Err(unprocessable("Reference", "sha", "Object does not exist"));
    }
    let (tx, git) = ctx.tx_mut_and_git()?;
    git.update_ref(tx, repo, account, &body.name, None, Some(sha), false)?;
    let links = Links::of(ctx)?;
    let r = Ref {
        name: body.name,
        target: sha,
    };
    let created = render_ref(ctx, &links, &r)?;
    Reply::json(&created)?
        .status(StatusCode::CREATED)
        .header(LOCATION, &created.url)
}

pub(crate) fn delete_ref(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let name = full_ref(ctx.param("ref")?);
    let current =
        exact_ref(ctx, &name).ok_or_else(|| ApiError::unprocessable("Reference does not exist"))?;
    let repo = ctx.repo()?.id;
    let account = sender(ctx.actor())?.id;
    let (tx, git) = ctx.tx_mut_and_git()?;
    git.update_ref(tx, repo, account, &name, Some(current), None, true)?;
    Ok(Reply::empty())
}

#[derive(Serialize)]
struct GitTag {
    node_id: String,
    tag: String,
    sha: ObjectId,
    url: String,
    message: String,
    tagger: GitPerson,
    object: RefObject,
    verification: Verification,
}

fn render_tag(ctx: &Ctx<'_>, id: ObjectId, t: &Tag) -> Result<GitTag, ApiError> {
    let links = Links::of(ctx)?;
    let repo = ctx.repo()?.id;
    let tagger = t
        .tagger
        .clone()
        .unwrap_or_else(hubdouble_git::Signature::seed);
    Ok(GitTag {
        node_id: object_node_id(ObjectKind::Tag, repo, id),
        tag: t.name.clone(),
        sha: id,
        url: links.object(ObjectKind::Tag, id),
        message: t.message.clone(),
        tagger: (&tagger).into(),
        object: RefObject {
            kind: kind_name(t.target_kind),
            sha: t.target,
            url: links.object(t.target_kind, t.target),
        },
        verification: match &t.gpg_signature {
            None => Verification::unsigned(),
            Some(s) => Verification {
                verified: false,
                reason: hubdouble_git::SignatureState::UnknownKey.as_str(),
                signature: Some(s.clone()),
                payload: None,
                verified_at: None,
            },
        },
    })
}

pub(crate) fn get_tag(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let (id, ObjectData::Tag(t)) = object_param(ctx, "tag_sha")? else {
        return Err(ApiError::not_found());
    };
    Reply::json(&render_tag(ctx, id, &t)?)
}

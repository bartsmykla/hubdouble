use std::sync::Arc;

use http::StatusCode;
use http::header::LOCATION;
use hubdouble_auth::Actor;
use hubdouble_git::{
    Commit, EntryMode, Git, GitError, ObjectData, ObjectId, ObjectKind, Tag, TreeEntry,
    check_tree_depth,
};
use hubdouble_model::ApiError;
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::Tx;
use serde::{Deserialize, Deserializer, Serialize};

use crate::common::{
    GitPerson, Links, ObjectLink, ParentLink, Verification, accepts, decode_base64, kind_name,
    object_node_id, parse_oid, sender, signature_of, unprocessable, wrapped_base64,
};
use crate::refs::RefObject;
use crate::tree_edit::{Leaves, build, flatten};
use crate::tree_view::{entry_kind, render_tree};

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
    let text = ctx.param("file_sha")?;
    if text.len() != 40 || !text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(ApiError::unprocessable(
            "The sha parameter must be exactly 40 characters and contain only [0-9a-f].",
        ));
    }
    let (sha, ObjectData::Blob(data)) = object_param(ctx, "file_sha")? else {
        return Err(ApiError::not_found());
    };
    if accepts(ctx, "raw") {
        let binary = data.iter().take(8000).any(|&b| b == 0);
        let media = if binary {
            "application/octet-stream"
        } else {
            "text/plain; charset=utf-8"
        };
        return Ok(Reply::raw(media, data.to_vec()));
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
    if rev.contains(':') {
        return Err(GitError::WrongKind);
    }
    let root = git.expression(tx, repo, &format!("{rev}:"))?;
    match root.data {
        ObjectData::Tree(entries) => Ok((root.id, entries)),
        _ => Err(GitError::WrongKind),
    }
}

pub(crate) fn get_tree(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let rev = ctx.param("tree_sha")?;
    let (id, entries) = tree_of(ctx.git(), ctx.tx(), ctx.repo()?.id, rev).map_err(|e| match e {
        GitError::WrongKind => ApiError::unprocessable(
            "Invalid object requested. SHA must identify a commit or a tree.",
        ),
        _ => ApiError::not_found(),
    })?;
    let recursive = ctx.query().get("recursive").is_some();
    Reply::json(&render_tree(ctx, id, entries, recursive)?)
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

fn holds(leaves: &Leaves, path: &str) -> bool {
    let below = format!("{path}/");
    leaves.contains_key(path)
        || leaves
            .range(below.clone()..)
            .next()
            .is_some_and(|(p, _)| p.starts_with(&below))
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

fn path_directories(path: &str) -> Option<usize> {
    let mut components = 0;
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return None;
        }
        components += 1;
    }
    Some(components - 1)
}

pub(crate) fn create_tree(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let body: NewTree = ctx.body()?;
    let repo = ctx.repo()?.id;
    let (git, tx) = (ctx.git(), ctx.tx());
    let mut leaves = Leaves::new();
    if let Some(base) = &body.base_tree {
        let base = parse_oid(base)
            .ok_or_else(|| unprocessable("Tree", "base_tree", "base_tree is not a valid tree"))?;
        flatten(git, tx, repo, "", 0, base, &mut leaves)
            .map_err(|_| unprocessable("Tree", "base_tree", "base_tree is not a valid tree"))?;
    }
    for e in body.tree {
        let bad = |field: &str| unprocessable("Tree", field, &format!("tree.{field} is invalid"));
        let directories = path_directories(&e.path).ok_or_else(|| bad("path"))?;
        let mode = parse_mode(&e.mode).ok_or_else(|| bad("mode"))?;
        let depth = directories + usize::from(mode == EntryMode::Tree);
        check_tree_depth(depth).map_err(|_| bad("path"))?;
        if e.kind
            .as_deref()
            .is_some_and(|k| k != kind_name(entry_kind(mode)))
        {
            return Err(bad("type"));
        }
        match (e.sha, e.content) {
            (Some(Some(_)), Some(_)) => return Err(bad("content")),
            (Some(None), _) => {
                if !holds(&leaves, &e.path) {
                    return Err(bad("sha"));
                }
                clear(&mut leaves, &e.path);
            }
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
                        flatten(git, tx, repo, &e.path, depth, id, &mut leaves)
                            .map_err(|_| bad("path"))?;
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
    let tree = render_tree(ctx, id, entries, false)?;
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

use std::cmp::Ordering;
use std::sync::Arc;

use hubdouble_git::{
    BlobNode, CommitNode, EntryMode, FileChange, GitError, Object, ObjectData, ObjectId, RefNode,
    SignatureNode, TagNode, TreeEntryNode, TreeNode,
};
use hubdouble_gql::{Args, Ctx, ErrorType, GqlError, MutCtx, Obj, Resolved, Target, connection};
use hubdouble_model::{Repo, node_id};
use serde::Deserialize;
use serde_json::json;

use hubdouble_auth::{Target as AuthTarget, authorize};

use crate::common::{decode_base64, signature_of};
use crate::refs::{denied_workflow, workflow_refusal};
use crate::{READ_CONTENTS, WRITE_CONTENTS};

const REF_CONNECTION: &str = "RefConnection";

fn required<T: serde::de::DeserializeOwned>(args: &Args, name: &str) -> Result<T, GqlError> {
    args.get(name)?.ok_or_else(|| {
        GqlError::new(
            ErrorType::Unprocessable,
            format!("Argument '{name}' is required"),
        )
    })
}

fn git_error(e: GitError) -> GqlError {
    match e {
        GitError::NotFound => GqlError::new(ErrorType::NotFound, e.to_string()),
        GitError::NotImplemented(what) => GqlError::new(ErrorType::NotImplemented, what),
        GitError::Rejected(api) => api.into(),
        other => GqlError::new(ErrorType::Unprocessable, other.to_string()),
    }
}

fn oid(id: ObjectId) -> Resolved {
    Resolved::Leaf(json!(id.to_string()))
}

pub(crate) fn object_resolved(repo_id: i64, object: Object) -> Resolved {
    let id = object.id;
    match object.data {
        ObjectData::Blob(data) => Resolved::Object {
            type_name: "Blob",
            value: Arc::new(BlobNode { repo_id, id, data }),
        },
        ObjectData::Tree(entries) => Resolved::Object {
            type_name: "Tree",
            value: Arc::new(TreeNode {
                repo_id,
                id,
                entries,
            }),
        },
        ObjectData::Commit(commit) => Resolved::Object {
            type_name: "Commit",
            value: Arc::new(CommitNode {
                repo_id,
                id,
                commit,
            }),
        },
        ObjectData::Tag(tag) => Resolved::Object {
            type_name: "Tag",
            value: Arc::new(TagNode { repo_id, id, tag }),
        },
    }
}

fn load_object(ctx: &Ctx<'_>, repo_id: i64, id: ObjectId) -> Result<Resolved, GqlError> {
    match ctx.git.object(ctx.tx, repo_id, id) {
        Ok(o) => Ok(object_resolved(repo_id, o)),
        Err(GitError::NotFound) => Ok(Resolved::Null),
        Err(e) => Err(git_error(e)),
    }
}

fn standard_prefix(name: &str) -> &'static str {
    ["refs/heads/", "refs/tags/", "refs/"]
        .into_iter()
        .find(|p| name.starts_with(p))
        .unwrap_or("")
}

fn ref_resolved(repo_id: i64, name: String, prefix: String, target: ObjectId) -> Resolved {
    Resolved::Object {
        type_name: "Ref",
        value: Arc::new(RefNode {
            repo_id,
            name,
            prefix,
            target,
        }),
    }
}

fn qualified_ref(repo_id: i64, name: String, target: ObjectId) -> Resolved {
    let prefix = standard_prefix(&name).to_owned();
    ref_resolved(repo_id, name, prefix, target)
}

fn readable_repo<'o>(ctx: &Ctx<'_>, obj: &'o Obj) -> Result<&'o Repo, GqlError> {
    let repo = Arc::clone(&obj.value)
        .downcast::<Repo>()
        .map_err(|_| GqlError::new(ErrorType::Internal, "the parent is not a Repository"))?;
    authorize(ctx.tx, ctx.actor, &READ_CONTENTS, AuthTarget::Repo(&repo))?;
    obj.get::<Repo>()
}

pub(crate) fn repository_object(
    ctx: &Ctx<'_>,
    obj: &Obj,
    args: &Args,
) -> Result<Resolved, GqlError> {
    let repo = readable_repo(ctx, obj)?;
    let found = if let Some(text) = args.get::<String>("oid")? {
        let id: ObjectId = text
            .parse()
            .map_err(|e: GitError| GqlError::new(ErrorType::Unprocessable, e.to_string()))?;
        ctx.git.object(ctx.tx, repo.id, id)
    } else {
        let expression: String = required(args, "expression")?;
        ctx.git.expression(ctx.tx, repo.id, &expression)
    };
    match found {
        Ok(o) => Ok(object_resolved(repo.id, o)),
        Err(GitError::NotFound | GitError::WrongKind) => Ok(Resolved::Null),
        Err(e) => Err(git_error(e)),
    }
}

pub(crate) fn repository_ref(ctx: &Ctx<'_>, obj: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let repo = readable_repo(ctx, obj)?;
    let name: String = required(args, "qualifiedName")?;
    let candidates = if name.starts_with("refs/") {
        vec![name]
    } else {
        vec![
            format!("refs/{name}"),
            format!("refs/tags/{name}"),
            format!("refs/heads/{name}"),
        ]
    };
    for full in candidates {
        if let Ok(target) = ctx.git.resolve_ref(ctx.tx, repo.id, &full) {
            return Ok(qualified_ref(repo.id, full, target));
        }
    }
    Ok(Resolved::Null)
}

pub(crate) fn repository_default_branch_ref(
    ctx: &Ctx<'_>,
    obj: &Obj,
    _: &Args,
) -> Result<Resolved, GqlError> {
    let repo = readable_repo(ctx, obj)?;
    let full = format!("refs/heads/{}", repo.default_branch);
    Ok(match ctx.git.resolve_ref(ctx.tx, repo.id, &full) {
        Ok(target) => qualified_ref(repo.id, full, target),
        Err(_) => Resolved::Null,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum OrderField {
    Alphabetical,
    TagCommitDate,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum Direction {
    Asc,
    Desc,
}

#[derive(Deserialize)]
struct RefOrder {
    field: OrderField,
    direction: Direction,
}

pub(crate) fn repository_refs(ctx: &Ctx<'_>, obj: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let repo = readable_repo(ctx, obj)?;
    let prefix: String = required(args, "refPrefix")?;
    if !prefix.starts_with("refs/") {
        return Err(GqlError::new(
            ErrorType::InvalidFieldInput,
            "`refPrefix` must start with `refs/`.",
        ));
    }
    if !prefix.ends_with('/') {
        return Err(GqlError::new(
            ErrorType::InvalidFieldInput,
            "`refPrefix` must end with a `/`.",
        ));
    }
    let query = args.get::<String>("query")?.unwrap_or_default();
    let mut refs: Vec<_> = ctx
        .git
        .refs(ctx.tx, repo.id, &prefix)
        .into_iter()
        .filter(|r| {
            r.name
                .strip_prefix(prefix.as_str())
                .is_some_and(|rest| rest.contains(query.as_str()))
        })
        .collect();
    let order = args.get::<RefOrder>("orderBy")?;
    if let Some(RefOrder {
        field: OrderField::TagCommitDate,
        ..
    }) = &order
    {
        let date = |id: ObjectId| {
            ctx.git
                .peel_to_commit(ctx.tx, repo.id, id)
                .and_then(|c| ctx.git.object(ctx.tx, repo.id, c))
                .ok()
                .and_then(|o| match o.data {
                    ObjectData::Commit(c) => Some(c.committer.time),
                    _ => None,
                })
        };
        refs.sort_by(|a, b| match date(a.target).cmp(&date(b.target)) {
            Ordering::Equal => a.name.cmp(&b.name),
            other => other,
        });
    }
    let descending = order.map_or(
        args.get::<Direction>("direction")? == Some(Direction::Desc),
        |o| o.direction == Direction::Desc,
    );
    if descending {
        refs.reverse();
    }
    connection(
        REF_CONNECTION,
        &refs,
        args,
        |r| r.name.clone(),
        |r| ref_resolved(repo.id, r.name.clone(), prefix.clone(), r.target),
    )
}

pub(crate) fn ref_name(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let r = obj.get::<RefNode>()?;
    let short = r.name.strip_prefix(r.prefix.as_str()).unwrap_or(&r.name);
    Ok(Resolved::Leaf(json!(short)))
}

pub(crate) fn ref_target(ctx: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let r = obj.get::<RefNode>()?;
    load_object(ctx, r.repo_id, r.target)
}

pub(crate) fn commit_oid(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(oid(obj.get::<CommitNode>()?.id))
}

pub(crate) fn tree_oid(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(oid(obj.get::<TreeNode>()?.id))
}

pub(crate) fn blob_oid(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(oid(obj.get::<BlobNode>()?.id))
}

pub(crate) fn tag_oid(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(oid(obj.get::<TagNode>()?.id))
}

pub(crate) fn commit_tree(ctx: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let c = obj.get::<CommitNode>()?;
    load_object(ctx, c.repo_id, c.commit.tree)
}

pub(crate) fn commit_signature(ctx: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let c = obj.get::<CommitNode>()?;
    Ok(match ctx.git.signature(c.repo_id, c.id, &c.commit) {
        Some(node) => Resolved::Object {
            type_name: "GpgSignature",
            value: Arc::new(node),
        },
        None => Resolved::Null,
    })
}

pub(crate) fn signature_state(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let s = obj.get::<SignatureNode>()?;
    Ok(Resolved::Leaf(json!(s.state)))
}

pub(crate) fn tag_target(ctx: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let t = obj.get::<TagNode>()?;
    load_object(ctx, t.repo_id, t.tag.target)
}

pub(crate) fn tree_entries(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let t = obj.get::<TreeNode>()?;
    Ok(Resolved::List(
        t.entries
            .iter()
            .map(|e| Resolved::Object {
                type_name: "TreeEntry",
                value: Arc::new(TreeEntryNode {
                    repo_id: t.repo_id,
                    path: e.name.clone(),
                    entry: e.clone(),
                }),
            })
            .collect(),
    ))
}

pub(crate) fn tree_entry_name(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(Resolved::Leaf(json!(
        obj.get::<TreeEntryNode>()?.entry.name
    )))
}

pub(crate) fn tree_entry_mode(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let mode = obj.get::<TreeEntryNode>()?.entry.mode;
    let value = i64::from_str_radix(mode.as_str(), 8)
        .map_err(|e| GqlError::new(ErrorType::Internal, e.to_string()))?;
    Ok(Resolved::Leaf(json!(value)))
}

pub(crate) fn blob_text(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let b = obj.get::<BlobNode>()?;
    Ok(match std::str::from_utf8(&b.data) {
        Ok(text) if !b.data.contains(&0) => Resolved::Leaf(json!(text)),
        _ => Resolved::Null,
    })
}

pub(crate) fn blob_is_truncated(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    obj.get::<BlobNode>()?;
    Ok(Resolved::Leaf(json!(false)))
}

pub(crate) struct CommitPayload {
    pub client_mutation_id: Option<String>,
    pub commit: CommitNode,
}

pub(crate) struct RefsPayload {
    pub client_mutation_id: Option<String>,
}

pub(crate) fn payload_commit(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let p = obj.get::<CommitPayload>()?;
    Ok(Resolved::Object {
        type_name: "Commit",
        value: Arc::new(p.commit.clone()),
    })
}

pub(crate) fn commit_payload_client_mutation_id(
    _: &Ctx<'_>,
    obj: &Obj,
    _: &Args,
) -> Result<Resolved, GqlError> {
    Ok(Resolved::Leaf(json!(
        obj.get::<CommitPayload>()?.client_mutation_id
    )))
}

pub(crate) fn payload_client_mutation_id(
    _: &Ctx<'_>,
    obj: &Obj,
    _: &Args,
) -> Result<Resolved, GqlError> {
    Ok(Resolved::Leaf(json!(
        obj.get::<RefsPayload>()?.client_mutation_id
    )))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommittableBranch {
    repository_name_with_owner: Option<String>,
    branch_name: Option<String>,
}

#[derive(Deserialize)]
struct CommitMessage {
    headline: String,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Deserialize)]
struct FileAddition {
    path: String,
    contents: String,
}

#[derive(Deserialize)]
struct FileDeletion {
    path: String,
}

#[derive(Deserialize, Default)]
struct FileChanges {
    #[serde(default)]
    additions: Vec<FileAddition>,
    #[serde(default)]
    deletions: Vec<FileDeletion>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateCommitInput {
    branch: CommittableBranch,
    client_mutation_id: Option<String>,
    expected_head_oid: String,
    #[serde(default)]
    file_changes: Option<FileChanges>,
    message: CommitMessage,
}

fn unprocessable(message: impl Into<String>) -> GqlError {
    GqlError::new(ErrorType::Unprocessable, message)
}

fn loaded_repo(r: Resolved) -> Result<Arc<Repo>, GqlError> {
    match r {
        Resolved::Object { value, .. } => value
            .downcast::<Repo>()
            .map_err(|_| GqlError::new(ErrorType::NotFound, "Could not resolve the repository")),
        _ => Err(GqlError::new(
            ErrorType::NotFound,
            "Could not resolve the repository",
        )),
    }
}

pub(crate) fn create_commit_on_branch(
    m: &mut MutCtx<'_>,
    args: &Args,
) -> Result<Resolved, GqlError> {
    let input: CreateCommitInput = required(args, "input")?;
    let (Some(with_owner), Some(branch)) = (
        input.branch.repository_name_with_owner,
        input.branch.branch_name,
    ) else {
        return Err(unprocessable(
            "branch needs repositoryNameWithOwner and branchName",
        ));
    };
    let (owner, name) = with_owner
        .split_once('/')
        .ok_or_else(|| unprocessable("repositoryNameWithOwner must be owner/name"))?;
    let repo = loaded_repo(m.load(Target::Repo { owner, name })?)?;
    m.authorize(&WRITE_CONTENTS, &repo)?;
    let account = m
        .actor
        .account()
        .cloned()
        .ok_or_else(|| GqlError::new(ErrorType::Forbidden, "Resource not accessible"))?;
    let expected: ObjectId = input
        .expected_head_oid
        .parse()
        .map_err(|_| unprocessable("expectedHeadOid is not an object ID"))?;
    let full = format!("refs/heads/{branch}");
    let head = m.git.resolve_ref(&m.tx, repo.id, &full).map_err(|_| {
        GqlError::new(
            ErrorType::NotFound,
            format!("A ref named \"{full}\" does not exist."),
        )
    })?;
    if head != expected {
        return Err(unprocessable(format!(
            "Expected branch to point to \"{expected}\" but it did not. Pull and try again."
        )));
    }
    let changes = input.file_changes.unwrap_or_default();
    if changes
        .additions
        .iter()
        .map(|file| &file.path)
        .chain(changes.deletions.iter().map(|file| &file.path))
        .any(|path| path.starts_with(".github/workflows/"))
    {
        m.authorize(&crate::WRITE_WORKFLOWS, &repo)?;
    }
    let mut writes = Vec::with_capacity(changes.additions.len() + changes.deletions.len());
    for d in changes.deletions {
        writes.push(FileChange::Delete { path: d.path });
    }
    for a in changes.additions {
        let data = decode_base64(&a.contents)
            .ok_or_else(|| unprocessable(format!("contents of {} is not Base64", a.path)))?;
        writes.push(FileChange::Write {
            path: a.path,
            data: data.into(),
            mode: EntryMode::File,
        });
    }
    let message = match input.message.body.filter(|b| !b.is_empty()) {
        Some(body) => format!("{}\n\n{body}", input.message.headline),
        None => input.message.headline,
    };
    let now = m.clock.now();
    let author = signature_of(&account, now);
    let committer = hubdouble_git::Signature::web_flow(now);
    let id = m
        .git
        .commit(
            &mut m.tx,
            repo.id,
            account.id,
            &full,
            Some(expected),
            &writes,
            &message,
            &author,
            &committer,
            true,
        )
        .map_err(git_error)?;
    let ObjectData::Commit(commit) = m.git.object(&m.tx, repo.id, id).map_err(git_error)?.data
    else {
        return Err(GqlError::new(ErrorType::Internal, "the commit is missing"));
    };
    Ok(Resolved::Object {
        type_name: "CreateCommitOnBranchPayload",
        value: Arc::new(CommitPayload {
            client_mutation_id: input.client_mutation_id,
            commit: CommitNode {
                repo_id: repo.id,
                id,
                commit,
            },
        }),
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RefUpdate {
    name: String,
    after_oid: String,
    #[serde(default)]
    before_oid: Option<String>,
    #[serde(default)]
    force: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateRefsInput {
    client_mutation_id: Option<String>,
    ref_updates: Vec<RefUpdate>,
    repository_id: String,
}

fn optional_oid(text: &str) -> Result<Option<ObjectId>, GqlError> {
    let id: ObjectId = text
        .parse()
        .map_err(|_| unprocessable(format!("{text} is not an object ID")))?;
    Ok((id != ObjectId::NULL).then_some(id))
}

pub(crate) fn update_refs(m: &mut MutCtx<'_>, args: &Args) -> Result<Resolved, GqlError> {
    let input: UpdateRefsInput = required(args, "input")?;
    let repo_id = node_id::entity_id(node_id::REPOSITORY, &input.repository_id)
        .or_else(|| match node_id::decode_legacy(&input.repository_id) {
            Some((kind, id)) if kind == "Repository" => Some(id),
            _ => None,
        })
        .ok_or_else(|| {
            GqlError::new(
                ErrorType::NotFound,
                format!(
                    "Could not resolve to a node with the global id of '{}'.",
                    input.repository_id
                ),
            )
        })?;
    let repo = loaded_repo(m.load(Target::RepoId(repo_id))?)?;
    m.authorize(&WRITE_CONTENTS, &repo)?;
    let sender = m
        .actor
        .account()
        .map(|a| a.id)
        .ok_or_else(|| GqlError::new(ErrorType::Forbidden, "Resource not accessible"))?;
    for u in input.ref_updates {
        if !u.name.starts_with("refs/") {
            return Err(unprocessable(format!(
                "{} is not a qualified ref name",
                u.name
            )));
        }
        let after = optional_oid(&u.after_oid)?;
        let before = match &u.before_oid {
            Some(b) => optional_oid(b)?,
            None => m.git.resolve_ref(&m.tx, repo.id, &u.name).ok(),
        };
        if let Some(target) = after {
            let base = m
                .git
                .resolve_ref(&m.tx, repo.id, &u.name)
                .or_else(|_| m.git.resolve_ref(&m.tx, repo.id, "HEAD"))
                .ok();
            if let Some(path) = denied_workflow(&m.tx, m.git, m.actor, &repo, base, target)? {
                return Err(unprocessable(workflow_refusal(&path)));
            }
        }
        m.git
            .update_ref(
                &mut m.tx,
                repo.id,
                sender,
                &u.name,
                before,
                after,
                u.force.unwrap_or(false),
            )
            .map_err(git_error)?;
    }
    Ok(Resolved::Object {
        type_name: "UpdateRefsPayload",
        value: Arc::new(RefsPayload {
            client_mutation_id: input.client_mutation_id,
        }),
    })
}

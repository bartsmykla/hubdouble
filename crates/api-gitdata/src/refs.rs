use std::sync::Arc;

use http::StatusCode;
use http::header::LOCATION;
use hubdouble_auth::{Actor, Target, authorize};
use hubdouble_git::{EntryMode, GitError, ObjectData, ObjectId, Ref, TreeEntry};
use hubdouble_model::ApiError;
use hubdouble_rest::{Ctx, Reply};
use serde::{Deserialize, Serialize};

use crate::WRITE_WORKFLOWS;
use crate::common::{Links, kind_name, parse_oid, ref_node_id, sender, unprocessable};

const DOCS: &str = "https://docs.github.com/rest/git/refs";
const WORKFLOWS: &str = ".github/workflows";

#[derive(Serialize)]
pub(crate) struct RefObject {
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) sha: ObjectId,
    pub(crate) url: String,
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
    let sha = ref_target(ctx, &body.sha)?;
    let repo = ctx.repo()?.id;
    if exact_ref(ctx, &body.name).is_some() {
        return Err(GitError::RefExists.into());
    }
    let base = ctx.git().resolve_ref(ctx.tx(), repo, "HEAD").ok();
    check_workflows(ctx, base, sha, "create-a-reference")?;
    let created = write_ref(ctx, body.name, None, sha, false)?;
    Reply::json(&created)?
        .status(StatusCode::CREATED)
        .header(LOCATION, &created.url)
}

#[derive(Deserialize)]
struct UpdateRef {
    sha: String,
    #[serde(default)]
    force: bool,
}

pub(crate) fn update_ref(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let name = full_ref(ctx.param("ref")?);
    let current =
        exact_ref(ctx, &name).ok_or_else(|| ApiError::unprocessable("Reference does not exist"))?;
    let body: UpdateRef = ctx.body()?;
    let sha = ref_target(ctx, &body.sha)?;
    check_workflows(ctx, Some(current), sha, "update-a-reference")?;
    Reply::json(&write_ref(ctx, name, Some(current), sha, body.force)?)
}

fn ref_target(ctx: &Ctx<'_>, sha: &str) -> Result<ObjectId, ApiError> {
    let not_found = || unprocessable("Reference", "sha", "Object does not exist");
    let sha = parse_oid(sha).ok_or_else(not_found)?;
    ctx.git()
        .object(ctx.tx(), ctx.repo()?.id, sha)
        .map_err(|_| not_found())?;
    Ok(sha)
}

fn write_ref(
    ctx: &mut Ctx<'_>,
    name: String,
    current: Option<ObjectId>,
    sha: ObjectId,
    force: bool,
) -> Result<GitRef, ApiError> {
    let repo = ctx.repo()?.id;
    let account = sender(ctx.actor())?.id;
    let (tx, git) = ctx.tx_mut_and_git()?;
    git.update_ref(tx, repo, account, &name, current, Some(sha), force)?;
    let links = Links::of(ctx)?;
    render_ref(ctx, &links, &Ref { name, target: sha })
}

fn check_workflows(
    ctx: &Ctx<'_>,
    base: Option<ObjectId>,
    target: ObjectId,
    operation: &str,
) -> Result<(), ApiError> {
    if base == Some(target) || !matches!(ctx.actor(), Actor::Installation { .. }) {
        return Ok(());
    }
    let before = workflow_tree(ctx, base)?;
    let after = workflow_tree(ctx, Some(target))?;
    if before == after {
        return Ok(());
    }
    match authorize(
        ctx.tx(),
        ctx.actor(),
        &WRITE_WORKFLOWS,
        Target::Repo(ctx.repo()?),
    ) {
        Ok(()) => return Ok(()),
        Err(error) if error.status == 403 => (),
        Err(error) => return Err(error),
    }
    if let Some(path) = changed_path(ctx, before, after, WORKFLOWS)? {
        return Err(ApiError::unprocessable(format!(
            "refusing to allow a GitHub App to create or update workflow `{path}` without `workflows` permission"
        )).with_docs(format!("{DOCS}#{operation}")));
    }
    Ok(())
}

fn workflow_tree(ctx: &Ctx<'_>, target: Option<ObjectId>) -> Result<Option<ObjectId>, ApiError> {
    let Some(target) = target else {
        return Ok(None);
    };
    let repo = ctx.repo()?.id;
    match ctx
        .git()
        .expression(ctx.tx(), repo, &format!("{target}:{WORKFLOWS}"))
    {
        Ok(object) if matches!(object.data, ObjectData::Tree(_)) => Ok(Some(object.id)),
        Ok(_) | Err(GitError::NotFound | GitError::WrongKind) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn tree_entries(ctx: &Ctx<'_>, id: Option<ObjectId>) -> Result<Arc<[TreeEntry]>, ApiError> {
    let Some(id) = id else {
        return Ok(Arc::from([]));
    };
    match ctx.git().object(ctx.tx(), ctx.repo()?.id, id)?.data {
        ObjectData::Tree(entries) => Ok(entries),
        _ => Err(GitError::WrongKind.into()),
    }
}

fn changed_path(
    ctx: &Ctx<'_>,
    before: Option<ObjectId>,
    after: Option<ObjectId>,
    prefix: &str,
) -> Result<Option<String>, ApiError> {
    if before == after {
        return Ok(None);
    }
    let before = tree_entries(ctx, before)?;
    let after = tree_entries(ctx, after)?;
    let prior = before
        .iter()
        .map(|entry| (entry, after.iter().find(|next| next.name == entry.name)));
    let added = after
        .iter()
        .filter(|next| before.iter().all(|entry| entry.name != next.name))
        .map(|entry| (entry, None));
    for (entry, other) in prior.chain(added) {
        if Some(entry) == other {
            continue;
        }
        let path = format!("{prefix}/{}", entry.name);
        if entry.mode != EntryMode::Tree || other.is_some_and(|item| item.mode != EntryMode::Tree) {
            return Ok(Some(path));
        }
        if let Some(path) = changed_path(ctx, Some(entry.id), other.map(|item| item.id), &path)? {
            return Ok(Some(path));
        }
    }
    Ok(None)
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

use std::sync::Arc;

use http::StatusCode;
use http::header::LOCATION;
use hubdouble_auth::{Actor, Target, authorize};
use hubdouble_git::{EntryMode, Git, GitError, ObjectData, ObjectId, Ref, TreeEntry};
use hubdouble_model::{ApiError, Repo, TokenKind};
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::Tx;
use serde::{Deserialize, Serialize};

use crate::WRITE_WORKFLOWS;
use crate::branch_rules::{Guard, Via, refusal};
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
        url: format!("{}/git/{}", links.api, r.name),
        object: RefObject {
            kind: kind_name(kind),
            sha: r.target,
            url: links.object(kind, r.target),
        },
        name: r.name.clone(),
    })
}

pub(crate) fn writable_ref(name: &str, operation: &str) -> Result<(), ApiError> {
    if name.starts_with("refs/pull/") {
        return Err(ApiError::unprocessable("Reference update failed")
            .with_docs(format!("{DOCS}#{operation}")));
    }
    Ok(())
}

fn full_ref(name: &str) -> String {
    format!("refs/{name}")
}

fn check_rules(
    ctx: &Ctx<'_>,
    name: &str,
    moves: (Option<ObjectId>, Option<ObjectId>),
    operation: &str,
) -> Result<(), ApiError> {
    let guard = Guard {
        tx: ctx.tx(),
        git: ctx.git(),
        actor: ctx.actor(),
        repo: ctx.repo()?,
    };
    let docs = format!("{DOCS}#{operation}");
    let violations = guard
        .check(name, moves, Via::Push)
        .map_err(|e| e.with_docs(docs.clone()))?;
    if violations.is_empty() {
        return Ok(());
    }
    Err(refusal(422, &violations, &docs))
}

pub(crate) fn get_ref(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let name = full_ref(ctx.param("ref")?);
    let target = exact_ref(ctx, &name).ok_or_else(ApiError::not_found)?;
    let links = Links::of(ctx)?;
    let updated = ctx.repo()?.updated_at;
    Ok(Reply::json(&render_ref(ctx, &links, &Ref { name, target })?)?.modified(updated))
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
    writable_ref(&body.name, "create-a-reference")?;
    let sha = ref_target(ctx, &body.sha)?;
    let repo = ctx.repo()?.id;
    if exact_ref(ctx, &body.name).is_some() {
        return Err(GitError::RefExists.into());
    }
    let base = ctx.git().resolve_ref(ctx.tx(), repo, "HEAD").ok();
    check_workflows(ctx, (&body.name, base), sha, "create-a-reference")?;
    check_rules(ctx, &body.name, (None, Some(sha)), "create-a-reference")?;
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
    writable_ref(&name, "update-a-reference")?;
    let current =
        exact_ref(ctx, &name).ok_or_else(|| ApiError::unprocessable("Reference does not exist"))?;
    let body: UpdateRef = ctx.body()?;
    let sha = ref_target(ctx, &body.sha)?;
    check_workflows(ctx, (&name, Some(current)), sha, "update-a-reference")?;
    let updated = write_ref(ctx, name.clone(), Some(current), sha, body.force)?;
    if current != sha {
        check_rules(ctx, &name, (Some(current), Some(sha)), "update-a-reference")?;
    }
    Reply::json(&updated)
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
    (written, base): (&str, Option<ObjectId>),
    target: ObjectId,
    operation: &str,
) -> Result<(), ApiError> {
    let repo = ctx.repo()?;
    match denied_workflow(ctx.tx(), ctx.git(), ctx.actor(), (repo, written), base, target)? {
        Some(path) => Err(
            ApiError::unprocessable(workflow_refusal(ctx.actor(), &path))
                .with_docs(format!("{DOCS}#{operation}")),
        ),
        None => Ok(()),
    }
}

/// GitHub's refusal of a write that changes the workflow file `path` without the right to,
/// worded for the kind of token that `actor` holds.
#[must_use]
pub fn workflow_refusal(actor: &Actor, path: &str) -> String {
    let (who, right) = match actor {
        Actor::User { token, .. } if token.kind == TokenKind::Personal => {
            ("a Personal Access Token", "`workflow` scope")
        }
        Actor::User { token, .. } if token.app_id.is_none() => ("an OAuth App", "`workflow` scope"),
        _ => ("a GitHub App", "`workflows` permission"),
    };
    format!("refusing to allow {who} to create or update workflow `{path}` without {right}")
}

/// The first workflow file that moving the ref `written` from `base` to `target` changes when
/// `actor` may not write workflows: an App without the `workflows` permission, or a classic or
/// OAuth token without the `workflow` scope. For such a token, as the OAuth scopes docs say, a
/// file that another branch already holds at the same path with the same contents does not
/// count; `written` itself never counts, so a caller may check before or after the ref moves.
/// An App gets no such exception, as live merges by an App without the permission are refused.
///
/// # Errors
///
/// A git error while it reads the trees, or an authorization error other than a refusal.
pub fn denied_workflow(
    tx: &Tx,
    git: &Git,
    actor: &Actor,
    (repo, written): (&Arc<Repo>, &str),
    base: Option<ObjectId>,
    target: ObjectId,
) -> Result<Option<String>, ApiError> {
    if base == Some(target) {
        return Ok(None);
    }
    let trees = Trees {
        tx,
        git,
        repo: repo.id,
    };
    let before = trees.workflows(base)?;
    let after = trees.workflows(Some(target))?;
    if before == after {
        return Ok(None);
    }
    match authorize(tx, actor, &WRITE_WORKFLOWS, Target::Repo(repo)) {
        Ok(()) => return Ok(None),
        Err(error) if error.status == 403 => (),
        Err(error) => return Err(error),
    }
    let scoped = matches!(actor, Actor::User { token, .. }
        if token.kind == TokenKind::Personal || token.app_id.is_none());
    trees.changed_path(before, after, WORKFLOWS, scoped.then_some(written))
}

struct Trees<'a> {
    tx: &'a Tx,
    git: &'a Git,
    repo: i64,
}

impl Trees<'_> {
    fn workflows(&self, target: Option<ObjectId>) -> Result<Option<ObjectId>, ApiError> {
        let Some(target) = target else {
            return Ok(None);
        };
        match self
            .git
            .expression(self.tx, self.repo, &format!("{target}:{WORKFLOWS}"))
        {
            Ok(object) if matches!(object.data, ObjectData::Tree(_)) => Ok(Some(object.id)),
            Ok(_) | Err(GitError::NotFound | GitError::WrongKind) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn entries(&self, id: Option<ObjectId>) -> Result<Arc<[TreeEntry]>, ApiError> {
        let Some(id) = id else {
            return Ok(Arc::from([]));
        };
        match self.git.object(self.tx, self.repo, id)?.data {
            ObjectData::Tree(entries) => Ok(entries),
            _ => Err(GitError::WrongKind.into()),
        }
    }

    /// The first changed path under `prefix`, leaving out, when `elsewhere` names the written
    /// ref, a file whose new contents another branch already holds at that path.
    fn changed_path(
        &self,
        before: Option<ObjectId>,
        after: Option<ObjectId>,
        prefix: &str,
        elsewhere: Option<&str>,
    ) -> Result<Option<String>, ApiError> {
        let mut changed = Vec::new();
        self.changes(before, after, prefix, &mut changed)?;
        let tips: Vec<ObjectId> = match elsewhere {
            Some(written) => self
                .git
                .refs(self.tx, self.repo, "refs/heads/")
                .into_iter()
                .filter(|r| r.name != written)
                .map(|r| r.target)
                .collect(),
            None => Vec::new(),
        };
        for (path, new) in changed {
            if let Some(new) = new
                && self.held_elsewhere(&tips, &path, new)?
            {
                continue;
            }
            return Ok(Some(path));
        }
        Ok(None)
    }

    fn held_elsewhere(
        &self,
        tips: &[ObjectId],
        path: &str,
        blob: ObjectId,
    ) -> Result<bool, ApiError> {
        for tip in tips {
            match self
                .git
                .expression(self.tx, self.repo, &format!("{tip}:{path}"))
            {
                Ok(object) if object.id == blob => return Ok(true),
                Ok(_) | Err(GitError::NotFound | GitError::WrongKind) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(false)
    }

    /// Every path under `prefix` that differs between the trees `before` and `after`, with the
    /// file it holds after the change, `None` for a deletion.
    fn changes(
        &self,
        before: Option<ObjectId>,
        after: Option<ObjectId>,
        prefix: &str,
        out: &mut Vec<(String, Option<ObjectId>)>,
    ) -> Result<(), ApiError> {
        if before == after {
            return Ok(());
        }
        let before = self.entries(before)?;
        let after = self.entries(after)?;
        let prior = before.iter().map(|entry| {
            (
                Some(entry),
                after.iter().find(|next| next.name == entry.name),
            )
        });
        let added = after
            .iter()
            .filter(|next| before.iter().all(|entry| entry.name != next.name))
            .map(|entry| (None, Some(entry)));
        for (old, new) in prior.chain(added) {
            if old == new {
                continue;
            }
            let Some(name) = old.or(new).map(|e| &e.name) else {
                continue;
            };
            let path = format!("{prefix}/{name}");
            let tree =
                |e: Option<&TreeEntry>| e.filter(|e| e.mode == EntryMode::Tree).map(|e| e.id);
            let file = new.filter(|e| e.mode != EntryMode::Tree);
            if old.is_some_and(|e| e.mode != EntryMode::Tree) || file.is_some() {
                out.push((path.clone(), file.map(|e| e.id)));
            }
            if tree(old).is_some() || tree(new).is_some() {
                self.changes(tree(old), tree(new), &path, out)?;
            }
        }
        Ok(())
    }
}

pub(crate) fn delete_ref(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let name = full_ref(ctx.param("ref")?);
    writable_ref(&name, "delete-a-reference")?;
    let current =
        exact_ref(ctx, &name).ok_or_else(|| ApiError::unprocessable("Reference does not exist"))?;
    check_rules(ctx, &name, (Some(current), None), "delete-a-reference")?;
    let repo = ctx.repo()?.id;
    let account = sender(ctx.actor())?.id;
    let (tx, git) = ctx.tx_mut_and_git()?;
    git.update_ref(tx, repo, account, &name, Some(current), None, true)?;
    Ok(Reply::empty())
}

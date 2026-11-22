use std::sync::Arc;

use http::StatusCode;
use hubdouble_admin::SeedCtx;
use hubdouble_auth::{Actor, repo_role};
use hubdouble_git::{Git, GitError};
use hubdouble_model::{
    ApiError, FieldError, Issue, IssueState, Pull, PullRef, Repo, RepoRole, Timestamp, node_id,
};
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::{Accounts, Counters, Issues, Repos, Sequence, Tx, TxMut};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{events, read, render, state};

#[derive(Deserialize)]
struct Create {
    title: String,
    head: String,
    #[serde(default)]
    head_repo: Option<String>,
    base: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default = "allow_maintainer")]
    maintainer_can_modify: bool,
}

const fn allow_maintainer() -> bool {
    true
}

#[derive(Deserialize)]
struct Seed {
    #[serde(default)]
    id: Option<i64>,
    #[serde(alias = "repo")]
    repository: String,
    #[serde(alias = "author")]
    user: String,
    #[serde(flatten)]
    input: Create,
}

pub(crate) fn seed(ctx: &mut SeedCtx<'_>, value: Value) -> Result<(), ApiError> {
    let Seed {
        id,
        repository,
        user,
        input,
    } = serde_json::from_value(value).map_err(|e| ApiError::unprocessable(e.to_string()))?;
    let tx = &mut ctx.tx;
    let (owner, name) = repository.split_once('/').ok_or_else(ApiError::not_found)?;
    let repo = repository_named(tx, owner, name)
        .ok_or_else(ApiError::not_found)?
        .clone();
    let sender = tx
        .table::<Accounts>()
        .by_login(&user)
        .ok_or_else(ApiError::not_found)?
        .id;
    let head = resolve_head(tx, &repo, &input)?;
    if let Some(id) = id {
        if id <= 0 || tx.table::<Issues>().by_alt_id(id).is_some() {
            return Err(validation("id", "invalid"));
        }
        tx.table_mut::<Counters>().bump(Sequence::Id, id);
    }
    open(
        tx,
        ctx.git,
        &repo,
        &head,
        input,
        (sender, ctx.clock.now()),
        id,
    )?;
    Ok(())
}

pub(crate) fn create(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let input: Create = ctx.body()?;
    let repo = ctx.repo()?.clone();
    let head = resolve_head(ctx.tx(), &repo, &input)?;
    if !hubdouble_auth::can_see(ctx.tx(), ctx.actor(), &head) {
        return Err(validation("head", "invalid"));
    }
    let actor = (sender(ctx.actor())?, ctx.now());
    let (tx, git) = ctx.tx_mut_and_git()?;
    let created = open(tx, git, &repo, &head, input, actor, None)?;
    let rendered = render::detail(ctx.git(), ctx.tx(), ctx.urls(), &created)?;
    Reply::json(&rendered)?
        .status(StatusCode::CREATED)
        .header(http::header::LOCATION, &rendered.url)
}

fn repository_named<'a>(tx: &'a Tx, owner: &str, name: &str) -> Option<&'a Arc<Repo>> {
    let owner = tx.table::<Accounts>().by_login(owner)?;
    tx.table::<Repos>().by_name(owner.id, name)
}

fn resolve_head(tx: &Tx, base: &Arc<Repo>, input: &Create) -> Result<Arc<Repo>, ApiError> {
    let (owner, _) = input.head.split_once(':').unzip();
    let base_owner = tx
        .table::<Accounts>()
        .get(base.owner_id)
        .ok_or_else(ApiError::not_found)?;
    let owner = owner.unwrap_or(&base_owner.login);
    let invalid = || validation("head", "invalid");
    let head = if let Some(name) = &input.head_repo {
        let (owner, name) = name.split_once('/').unwrap_or((owner, name));
        repository_named(tx, owner, name).ok_or_else(invalid)?
    } else if owner.eq_ignore_ascii_case(&base_owner.login) {
        base
    } else {
        let account = tx.table::<Accounts>().by_login(owner).ok_or_else(invalid)?;
        let repos = tx.table::<Repos>();
        let root = network_root(tx, base);
        repos
            .by_name(account.id, &base.name)
            .filter(|candidate| network_root(tx, candidate) == root)
            .or_else(|| {
                repos
                    .children(account.id)
                    .find(|candidate| network_root(tx, candidate) == root)
            })
            .ok_or_else(invalid)?
    };
    if network_root(tx, head) != network_root(tx, base) {
        return Err(invalid());
    }
    Ok(head.clone())
}

fn network_root(tx: &Tx, repo: &Repo) -> i64 {
    let repos = tx.table::<Repos>();
    let mut root = repo;
    for _ in 0..repos.len() {
        let Some(parent) = root.fork_of.and_then(|parent| repos.get(parent)) else {
            break;
        };
        root = parent;
    }
    root.id
}

fn open(
    tx: &mut TxMut<'_>,
    git: &Git,
    base: &Repo,
    head: &Repo,
    input: Create,
    (sender, now): (i64, Timestamp),
    id: Option<i64>,
) -> Result<Arc<Issue>, ApiError> {
    let head_name = input
        .head
        .split_once(':')
        .map_or(input.head.as_str(), |(_, name)| name);
    let head_oid =
        state::branch(git, tx, head.id, head_name).map_err(|_| validation("head", "invalid"))?;
    let base_oid =
        state::branch(git, tx, base.id, &input.base).map_err(|_| validation("base", "invalid"))?;
    if input.title.trim().is_empty() {
        return Err(validation("title", "missing_field"));
    }
    ensure_unique(tx, base.id, &input.base, head.id, head_name, None)?;
    let diff = git.compare(tx, base.id, base_oid, head_oid)?;
    if diff.ahead_by == 0 || diff.merge_base.is_none() {
        return Err(validation("head", "invalid"));
    }
    let issue_id = tx.next_id();
    let pull_id = id.unwrap_or_else(|| tx.next_id());
    let issue = Issue {
        id: issue_id,
        node_id: node_id::entity(node_id::ISSUE, issue_id),
        repo_id: base.id,
        number: tx.table_mut::<Counters>().next_number(base.id),
        title: input.title,
        body: input.body,
        user_id: sender,
        state: IssueState::Open,
        state_reason: None,
        locked: false,
        active_lock_reason: None,
        label_ids: Vec::new(),
        assignee_ids: Vec::new(),
        closed_by: None,
        created_at: now,
        updated_at: now,
        closed_at: None,
        last_edited_at: None,
        pull: Some(Pull {
            id: pull_id,
            node_id: node_id::entity(node_id::PULL_REQUEST, pull_id),
            head: PullRef {
                repo_id: Some(head.id),
                ref_name: head_name.to_owned(),
                sha: head_oid.to_string(),
            },
            base: PullRef {
                repo_id: Some(base.id),
                ref_name: input.base,
                sha: base_oid.to_string(),
            },
            draft: input.draft,
            merged_at: None,
            merge_commit_sha: None,
            merged_by: None,
            mergeable: Some(git.mergeable(tx, base.id, base_oid, head_oid)?),
            maintainer_can_modify: input.maintainer_can_modify,
            requested_reviewers: Vec::new(),
            requested_teams: Vec::new(),
        }),
    };
    state::index(tx, issue.id, state::pull(&issue)?)?;
    let issue = tx.table_mut::<Issues>().insert(issue)?;
    events::emit(tx, git, &issue, sender, "opened", json!({}))?;
    Ok(issue)
}

fn ensure_unique(
    tx: &Tx,
    base: i64,
    base_name: &str,
    head: i64,
    head_name: &str,
    except: Option<i64>,
) -> Result<(), ApiError> {
    if tx.table::<Issues>().in_repo(base).any(|issue| {
        Some(issue.id) != except
            && issue.state == IssueState::Open
            && issue.pull.as_ref().is_some_and(|p| {
                p.head.repo_id == Some(head)
                    && p.head.ref_name == head_name
                    && p.base.ref_name == base_name
            })
    }) {
        return Err(validation("head", "custom"));
    }
    Ok(())
}

#[derive(Deserialize)]
struct Edit {
    title: Option<String>,
    #[serde(default, deserialize_with = "nullable_body")]
    body: BodyEdit,
    base: Option<String>,
    state: Option<String>,
    maintainer_can_modify: Option<bool>,
}

#[derive(Default)]
enum BodyEdit {
    #[default]
    Unchanged,
    Set(Option<String>),
}

fn nullable_body<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BodyEdit, D::Error> {
    Option::deserialize(d).map(BodyEdit::Set)
}

pub(crate) fn update(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let input: Edit = ctx.body()?;
    let mut issue = Issue::clone(read::issue(ctx)?);
    may_edit(ctx, &issue)?;
    let before = issue.state;
    let author = sender(ctx.actor())?;
    let now = ctx.now();
    let (tx, git) = ctx.tx_mut_and_git()?;
    let changed = edit(tx, git, &mut issue, input, author, now)?;
    if before != issue.state
        || !changed.is_empty()
        || tx
            .table::<Issues>()
            .get(issue.id)
            .is_some_and(|old| **old != issue)
    {
        issue.updated_at = now;
        tx.table_mut::<Issues>().replace(issue.clone())?;
    }
    if !changed.is_empty() {
        events::emit(
            tx,
            git,
            &issue,
            author,
            "edited",
            json!({"changes": changed}),
        )?;
    }
    if before != issue.state {
        let action = if issue.state == IssueState::Closed {
            "closed"
        } else {
            "reopened"
        };
        events::emit(tx, git, &issue, author, action, json!({}))?;
    }
    Reply::json(&render::detail(ctx.git(), ctx.tx(), ctx.urls(), &issue)?)
}

fn can_push(tx: &Tx, actor: &Actor, repo: &Repo) -> bool {
    match actor {
        Actor::User { user, .. } => repo_role(tx, user.id, repo) >= Some(RepoRole::Write),
        _ => true,
    }
}

fn may_edit(ctx: &Ctx<'_>, issue: &Issue) -> Result<(), ApiError> {
    let tx = ctx.tx();
    let actor = ctx.actor();
    if let Actor::User { user, .. } = actor
        && user.id == issue.user_id
    {
        return Ok(());
    }
    let head = issue
        .pull
        .as_ref()
        .and_then(|p| p.head.repo_id)
        .and_then(|id| tx.table::<Repos>().get(id));
    if can_push(tx, actor, ctx.repo()?) || head.is_some_and(|h| can_push(tx, actor, h)) {
        Ok(())
    } else {
        Err(ApiError::forbidden(
            "Must have write access to the repository or be the author",
        ))
    }
}

fn edit(
    tx: &Tx,
    git: &Git,
    issue: &mut Issue,
    input: Edit,
    actor: i64,
    now: Timestamp,
) -> Result<Map<String, Value>, ApiError> {
    let mut changes = Map::new();
    let p = issue.pull.as_mut().ok_or_else(ApiError::not_found)?;
    if let Some(title) = input.title.filter(|title| *title != issue.title) {
        if title.trim().is_empty() {
            return Err(validation("title", "missing_field"));
        }
        changes.insert("title".into(), json!({"from": issue.title}));
        issue.title = title;
    }
    if let BodyEdit::Set(body) = input.body
        && body != issue.body
    {
        changes.insert(
            "body".into(),
            json!({"from": issue.body.as_deref().unwrap_or_default()}),
        );
        issue.body = body;
        issue.last_edited_at = Some(now);
    }
    let target = input
        .state
        .as_deref()
        .map(|state| IssueState::from_rest(state).ok_or_else(|| validation("state", "invalid")))
        .transpose()?
        .unwrap_or(issue.state);
    let retarget = input.base.filter(|base| *base != p.base.ref_name);
    if target == IssueState::Open && (issue.state != target || retarget.is_some()) {
        let head_repo = p
            .head
            .repo_id
            .ok_or_else(|| validation("head", "invalid"))?;
        let name = retarget.as_deref().unwrap_or(&p.base.ref_name);
        if p.merged_at.is_some() || (head_repo == issue.repo_id && p.head.ref_name == name) {
            return Err(validation("state", "invalid"));
        }
        ensure_unique(
            tx,
            issue.repo_id,
            name,
            head_repo,
            &p.head.ref_name,
            Some(issue.id),
        )?;
        let base = state::branch(git, tx, issue.repo_id, name)
            .map_err(|_| validation("base", "invalid"))?;
        let head = state::branch(git, tx, head_repo, &p.head.ref_name)
            .map_err(|_| validation("head", "invalid"))?;
        if retarget.is_some() {
            changes.insert(
                "base".into(),
                json!({"ref": {"from": p.base.ref_name}, "sha": {"from": p.base.sha}}),
            );
            p.base.ref_name = name.to_owned();
        }
        p.base.sha = base.to_string();
        p.head.sha = head.to_string();
        p.mergeable = Some(git.mergeable(tx, issue.repo_id, base, head)?);
    } else if retarget.is_some() {
        return Err(validation("base", "invalid"));
    }
    if let Some(allow) = input.maintainer_can_modify {
        p.maintainer_can_modify = allow;
    }
    if target != issue.state {
        let closed = target == IssueState::Closed;
        issue.state = target;
        issue.closed_at = closed.then_some(now);
        issue.closed_by = closed.then_some(actor);
        if closed {
            p.mergeable = None;
        }
    }
    Ok(changes)
}

#[derive(Deserialize)]
struct Merge {
    commit_title: Option<String>,
    commit_message: Option<String>,
    sha: Option<String>,
    #[serde(rename = "merge_method")]
    method: Option<String>,
}

pub(crate) fn merge(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let input: Merge = ctx.body()?;
    let repo = ctx.repo()?;
    let issue = read::issue(ctx)?.clone();
    let p = state::pull(&issue)?;
    if input
        .method
        .as_deref()
        .is_some_and(|method| method != "merge")
    {
        return Err(ApiError::unprocessable(
            "Only the merge method is supported",
        ));
    }
    if !repo.allow_merge_commit
        || p.draft
        || issue.state == IssueState::Closed
        || p.head.repo_id.is_none()
    {
        return Err(ApiError::new(405, "Pull request cannot be merged"));
    }
    let repo_id = repo.id;
    let head_repo = p.head.repo_id.ok_or_else(ApiError::not_found)?;
    let live_head = state::branch(ctx.git(), ctx.tx(), head_repo, &p.head.ref_name)?;
    let sha = live_head.to_string();
    if input
        .sha
        .as_deref()
        .is_some_and(|wanted| !wanted.eq_ignore_ascii_case(&sha))
    {
        return Err(ApiError::conflict("The head SHA does not match"));
    }
    let actor = sender(ctx.actor())?;
    let title = input
        .commit_title
        .unwrap_or_else(|| format!("Merge pull request #{}", issue.number));
    let detail = input.commit_message.as_deref().unwrap_or(&issue.title);
    let message = format!("{title}\n\n{detail}");
    let (tx, git) = ctx.tx_mut_and_git()?;
    let result = git
        .merge(tx, repo_id, actor, &p.base.ref_name, &sha, &message)
        .map_err(|e| match e {
            GitError::Conflict { .. } | GitError::NothingToMerge => {
                ApiError::new(405, "Pull request cannot be merged")
            }
            other => other.into(),
        })?;
    Reply::json(
        &json!({"sha": result.commit, "merged": true, "message": "Pull Request successfully merged"}),
    )
}

pub(crate) fn sender(actor: &Actor) -> Result<i64, ApiError> {
    actor
        .account()
        .map(|account| account.id)
        .ok_or_else(ApiError::unauthorized)
}

fn validation(field: &str, code: &str) -> ApiError {
    ApiError::unprocessable("Validation Failed").with_error(FieldError {
        resource: "PullRequest".into(),
        field: field.into(),
        code: code.into(),
        message: None,
        value: None,
    })
}

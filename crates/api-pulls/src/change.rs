use std::sync::Arc;

use http::StatusCode;
use hubdouble_admin::SeedCtx;
use hubdouble_api_gitdata::branch_rules::{self, refusal};
use hubdouble_auth::{Actor, Target as AuthTarget, authorize, repo_role};
use hubdouble_git::{Git, GitError};
use hubdouble_model::{
    ApiError, FieldError, Issue, IssueState, Pull, PullRef, Repo, RepoRole, Timestamp, node_id,
};
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::{Accounts, Counters, Issues, Repos, Sequence, Tx, TxMut};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::timeline::{self, StateKind};
use crate::{CONTENTS_WRITE, events, mergeability, read, render, reopen, reviewing, state};

const MERGE_DOCS: &str = "https://docs.github.com/rest/pulls/pulls#merge-a-pull-request";

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
    let opened = open(
        tx,
        ctx.git,
        &repo,
        &head,
        input,
        (sender, ctx.clock.now()),
        id,
    )?;
    mergeability::settle(tx, ctx.git, opened.id, false)
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
            mergeable: None,
            maintainer_can_modify: input.maintainer_can_modify,
            requested_reviewers: Vec::new(),
            requested_teams: Vec::new(),
        }),
    };
    let issue = tx.table_mut::<Issues>().insert(issue)?;
    state::sync_refs(tx, git, &issue)?;
    events::emit(tx, git, &issue, sender, "opened", json!({}))?;
    mergeability::start(tx, git, &issue)?;
    let id = issue.id;
    tx.table::<Issues>()
        .get(id)
        .cloned()
        .ok_or_else(ApiError::not_found)
}

fn ensure_unique(
    tx: &Tx,
    base: i64,
    base_name: &str,
    head: i64,
    head_name: &str,
    except: Option<i64>,
) -> Result<(), ApiError> {
    if open_between(tx, base, base_name, head, head_name, except) {
        return Err(validation("head", "custom"));
    }
    Ok(())
}

pub(crate) fn open_between(
    tx: &Tx,
    base: i64,
    base_name: &str,
    head: i64,
    head_name: &str,
    except: Option<i64>,
) -> bool {
    tx.table::<Issues>().in_repo(base).any(|issue| {
        Some(issue.id) != except
            && issue.state == IssueState::Open
            && issue.pull.as_ref().is_some_and(|p| {
                p.head.repo_id == Some(head)
                    && p.head.ref_name == head_name
                    && p.base.ref_name == base_name
            })
    })
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
    let via_app = timeline::via_app(ctx.actor());
    let now = ctx.now();
    let (tx, git) = ctx.tx_mut_and_git()?;
    let edited = edit(tx, git, &mut issue, input, author, now)?;
    if before != issue.state
        || !edited.changes.is_empty()
        || tx
            .table::<Issues>()
            .get(issue.id)
            .is_some_and(|old| **old != issue)
    {
        issue.updated_at = now;
        tx.table_mut::<Issues>().replace(issue.clone())?;
        state::sync_refs(tx, git, &issue)?;
    }
    let who = (author, via_app);
    if let Some((from, to)) = edited.renamed {
        timeline::record(tx, &issue, who, StateKind::Renamed { from, to })?;
    }
    if !edited.changes.is_empty() {
        events::emit(
            tx,
            git,
            &issue,
            author,
            "edited",
            json!({"changes": edited.changes}),
        )?;
    }
    if before != issue.state {
        if issue.state == IssueState::Closed {
            timeline::record(tx, &issue, who, StateKind::Closed)?;
            events::emit(tx, git, &issue, author, "closed", json!({}))?;
        } else {
            reopen::reopened(tx, git, &issue, author, via_app)?;
        }
    }
    if edited.rebased {
        timeline::record(tx, &issue, who, StateKind::BaseRefChanged)?;
        mergeability::start(tx, git, &issue)?;
    }
    if let Some(refusal) = edited.refusal {
        return kept(&refusal);
    }
    let issue = read::issue(ctx)?;
    Reply::json(&render::detail(ctx.git(), ctx.tx(), ctx.urls(), issue)?)
}

/// GitHub's error answer for a refusal that keeps the writes made before it, so the handler
/// answers it without the rollback an `Err` makes.
///
/// # Errors
///
/// A serialization error.
pub fn kept(refusal: &ApiError) -> Result<Reply, ApiError> {
    let status = StatusCode::from_u16(refusal.status).unwrap_or(StatusCode::UNPROCESSABLE_ENTITY);
    Ok(Reply::json(&json!({
        "message": refusal.message,
        "errors": refusal.errors,
        "documentation_url": refusal.documentation_url,
        "status": refusal.status.to_string(),
    }))?
    .status(status))
}

struct Edited {
    changes: Map<String, Value>,
    renamed: Option<(String, String)>,
    rebased: bool,
    refusal: Option<ApiError>,
}

fn can_push(tx: &Tx, actor: &Actor, repo: &Repo) -> bool {
    match actor {
        Actor::User { user, .. } => repo_role(tx, user.id, repo) >= Some(RepoRole::Write),
        _ => true,
    }
}

fn may_edit(ctx: &Ctx<'_>, issue: &Issue) -> Result<(), ApiError> {
    may_change(ctx.tx(), ctx.actor(), ctx.repo()?, issue)
}

pub(crate) fn may_change(
    tx: &Tx,
    actor: &Actor,
    repo: &Repo,
    issue: &Issue,
) -> Result<(), ApiError> {
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
    if can_push(tx, actor, repo) || head.is_some_and(|h| can_push(tx, actor, h)) {
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
) -> Result<Edited, ApiError> {
    let mut edited = Edited {
        changes: Map::new(),
        renamed: None,
        rebased: false,
        refusal: None,
    };
    if issue.pull.is_none() {
        return Err(ApiError::not_found());
    }
    let title = input.title.filter(|title| *title != issue.title);
    if title.as_ref().is_some_and(|title| title.trim().is_empty()) {
        return Err(validation("title", "missing_field"));
    }
    let target = input
        .state
        .as_deref()
        .and_then(IssueState::from_rest)
        .unwrap_or(issue.state);
    if let BodyEdit::Set(body) = input.body
        && body != issue.body
    {
        edited.changes.insert(
            "body".into(),
            json!({"from": issue.body.as_deref().unwrap_or_default()}),
        );
        issue.body = body;
        issue.last_edited_at = Some(now);
    }
    if let Some(allow) = input.maintainer_can_modify {
        state::pull_mut(issue)?.maintainer_can_modify = allow;
    }
    if target == IssueState::Open && issue.state == IssueState::Closed {
        match reopen::reopen(tx, git, issue) {
            Ok(()) => {}
            Err(refused) if refused.status == 422 => edited.refusal = Some(refused),
            Err(other) => return Err(other),
        }
    }
    if edited.refusal.is_none() {
        if let Some(title) = title {
            edited
                .changes
                .insert("title".into(), json!({"from": issue.title}));
            let from = std::mem::replace(&mut issue.title, title);
            edited.renamed = Some((from, issue.title.clone()));
        }
        if target == IssueState::Closed && issue.state == IssueState::Open {
            issue.state = IssueState::Closed;
            issue.closed_at = Some(now);
            issue.closed_by = Some(actor);
        }
    }
    let base = &state::pull(issue)?.base.ref_name;
    if let Some(name) = input.base.filter(|name| name != base) {
        match retarget(tx, git, issue, name)? {
            Ok(from) => {
                edited.changes.insert("base".into(), from);
                edited.rebased = true;
            }
            Err(refused) => edited.refusal = Some(refused),
        }
    }
    Ok(edited)
}

fn retarget(
    tx: &Tx,
    git: &Git,
    issue: &mut Issue,
    name: String,
) -> Result<Result<Value, ApiError>, ApiError> {
    let (id, repo) = (issue.id, issue.repo_id);
    let open = issue.state == IssueState::Open;
    let p = state::pull_mut(issue)?;
    let refused = |message: String| {
        Ok(Err(ApiError::unprocessable("Validation Failed")
            .with_error(FieldError {
                resource: "PullRequest".into(),
                field: "base".into(),
                code: "invalid".into(),
                message: Some(message),
                value: None,
            })))
    };
    if !open {
        return refused("Cannot change the base branch of a closed pull request.".into());
    }
    let base = match state::branch(git, tx, repo, &name) {
        Ok(base) => base,
        Err(GitError::NotFound) => {
            return refused(format!("Proposed base branch '{name}' was not found"));
        }
        Err(other) => return Err(other.into()),
    };
    let Some(head_repo) = p.head.repo_id else {
        return Ok(Err(validation("head", "invalid")));
    };
    let head = match state::branch(git, tx, head_repo, &p.head.ref_name) {
        Ok(head) => head,
        Err(GitError::NotFound) => return Ok(Err(validation("head", "invalid"))),
        Err(other) => return Err(other.into()),
    };
    if git.compare(tx, repo, base, head)?.ahead_by == 0 {
        return refused(format!(
            "There are no new commits between base branch '{name}' and head branch '{}'",
            p.head.ref_name
        ));
    }
    if open_between(tx, repo, &name, head_repo, &p.head.ref_name, Some(id)) {
        return refused(format!(
            "A pull request already exists for base branch '{name}' and head branch '{}'",
            p.head.ref_name
        ));
    }
    let from = json!({"ref": {"from": p.base.ref_name}, "sha": {"from": p.base.sha}});
    p.base.ref_name = name;
    p.base.sha = base.to_string();
    p.head.sha = head.to_string();
    p.mergeable = None;
    Ok(Ok(from))
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
    let deletes = ctx
        .tx()
        .table::<Repos>()
        .get(head_repo)
        .is_some_and(|h| deletes_head(ctx.tx(), ctx.actor(), &issue, h));
    let who = (actor, timeline::via_app(ctx.actor()));
    let base_tip = state::branch(ctx.git(), ctx.tx(), repo_id, &p.base.ref_name)?;
    let merger = ctx.actor().clone();
    let (tx, git) = ctx.tx_mut_and_git()?;
    let result = git
        .merge(tx, repo_id, actor, &p.base.ref_name, &sha, &message)
        .map_err(|e| match e {
            GitError::Conflict { .. } | GitError::NothingToMerge => {
                ApiError::new(405, "Pull request cannot be merged")
            }
            other => other.into(),
        })?;
    let violations =
        reviewing::merge_violations(tx, git, &merger, &issue, (base_tip, result.commit))?;
    if !violations.is_empty() {
        return Err(refusal(405, &violations, MERGE_DOCS));
    }
    if deletes {
        delete_head(tx, git, &issue, who)?;
    }
    Reply::json(
        &json!({"sha": result.commit, "merged": true, "message": "Pull Request successfully merged"}),
    )
}

fn deletes_head(tx: &Tx, actor: &Actor, merged: &Issue, head: &Arc<Repo>) -> bool {
    let Some(p) = merged.pull.as_ref() else {
        return false;
    };
    let branch = p.head.ref_name.as_str();
    let protected = branch_rules::rules(tx, Some(actor), head, branch, false)
        .iter()
        .any(|rule| rule.rule_type == "deletion");
    let heads_another = tx
        .table::<Issues>()
        .pulls_with_head_repo(head.id)
        .any(|other| {
            other.id != merged.id
                && other.state == IssueState::Open
                && other
                    .pull
                    .as_ref()
                    .is_some_and(|o| o.head.ref_name == branch)
        });
    head.delete_branch_on_merge
        && branch != head.default_branch
        && !protected
        && !heads_another
        && authorize(tx, actor, &CONTENTS_WRITE, AuthTarget::Repo(head)).is_ok()
}

fn delete_head(
    tx: &mut TxMut<'_>,
    git: &Git,
    merged: &Issue,
    who: (i64, Option<i64>),
) -> Result<(), ApiError> {
    let p = state::pull(merged)?;
    let head_repo = p.head.repo_id.ok_or_else(ApiError::not_found)?;
    let name = format!("refs/heads/{}", p.head.ref_name);
    let Ok(tip) = git.resolve_ref(tx, head_repo, &name) else {
        return Ok(());
    };
    let dependents: Vec<_> = tx
        .table::<Issues>()
        .in_repo(head_repo)
        .filter(|issue| {
            head_repo == merged.repo_id
                && issue.id != merged.id
                && issue.pull.as_ref().is_some_and(|d| {
                    d.base.ref_name == p.head.ref_name && d.head.repo_id == Some(head_repo)
                })
        })
        .cloned()
        .collect();
    let mut retargeted = Vec::new();
    for dependent in dependents {
        let mut dependent = Issue::clone(&dependent);
        if let Ok(from) = retarget(tx, git, &mut dependent, p.base.ref_name.clone())? {
            dependent.updated_at = git.now();
            tx.table_mut::<Issues>().replace(dependent.clone())?;
            state::sync_refs(tx, git, &dependent)?;
            timeline::record(tx, &dependent, who, StateKind::AutomaticBaseChangeSucceeded)?;
            retargeted.push((dependent, from));
        } else {
            let kind = StateKind::AutomaticBaseChangeFailed;
            timeline::record_at(tx, &dependent, who, kind, git.now())?;
        }
    }
    git.update_ref(tx, head_repo, who.0, &name, Some(tip), None, true)?;
    for (dependent, from) in retargeted {
        timeline::record(tx, &dependent, who, StateKind::BaseRefChanged)?;
        events::emit(
            tx,
            git,
            &dependent,
            who.0,
            "edited",
            json!({"changes": {"base": from}}),
        )?;
    }
    Ok(())
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

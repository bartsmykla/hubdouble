use std::sync::Arc;

use http::StatusCode;
use hubdouble_auth::{Actor, repo_role};
use hubdouble_model::{
    Account, ApiError, Event, FieldError, Grant, Issue, IssueComment, Label, ReactionSubjectKind,
    Repo, RepoRole, Requirement, Scope,
};
use hubdouble_render::{self as r, ReactionRollup, Urls};
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::{Accounts, Apps, IssueComments, Labels, Reactions, Repos, Teams, Tx, TxMut};
use serde::Serialize;
use serde_json::{Map, Value};

pub(crate) const READ: Requirement = Requirement {
    any_of: &[Grant::read("issues"), Grant::read("pull_requests")],
    all_of: &[],
    scope: Scope::Repo,
    scopes: &[],
};

pub(crate) const WRITE: Requirement = Requirement {
    any_of: &[Grant::write("issues"), Grant::write("pull_requests")],
    all_of: &[],
    scope: Scope::Repo,
    scopes: &[],
};

pub(crate) const ISSUES_READ: Requirement = Requirement {
    any_of: &[Grant::read("issues")],
    all_of: &[],
    scope: Scope::Repo,
    scopes: &[],
};

pub(crate) const ISSUES_WRITE: Requirement = Requirement {
    any_of: &[Grant::write("issues")],
    all_of: &[],
    scope: Scope::Repo,
    scopes: &[],
};

pub(crate) const DEFAULT_LABEL_COLOR: &str = "ededed";

pub(crate) fn invalid(resource: &str, field: &str, code: &str) -> ApiError {
    ApiError::unprocessable("Validation Failed").with_error(FieldError {
        resource: resource.to_owned(),
        field: field.to_owned(),
        code: code.to_owned(),
        message: None,
        value: None,
    })
}

pub(crate) fn invalid_value(resource: &str, field: &str, value: &str) -> ApiError {
    let mut e = invalid(resource, field, "invalid");
    if let Some(error) = e.errors.first_mut() {
        error.value = Some(value.into());
    }
    e
}

pub(crate) fn created(body: &impl Serialize, location: &str) -> Result<Reply, ApiError> {
    Reply::json(body)?
        .status(StatusCode::CREATED)
        .header(http::header::LOCATION, location)
}

pub(crate) fn to_value(v: &impl Serialize) -> Result<Value, ApiError> {
    serde_json::to_value(v).map_err(|e| ApiError::internal(e.to_string()))
}

pub(crate) fn full_name(tx: &Tx, repo: &Repo) -> String {
    let owner = tx
        .table::<Accounts>()
        .get(repo.owner_id)
        .map_or("", |a| a.login.as_str());
    format!("{owner}/{}", repo.name)
}

pub(crate) fn user(tx: &Tx, urls: &Urls, id: i64) -> Option<r::SimpleUser> {
    tx.table::<Accounts>()
        .get(id)
        .map(|a| r::SimpleUser::new(urls, a))
}

pub(crate) use hubdouble_api_pulls::association;

pub(crate) fn integration(tx: &Tx, urls: &Urls, app_id: Option<i64>) -> Option<r::Integration> {
    let app = tx.table::<Apps>().get(app_id?)?;
    let owner = tx.table::<Accounts>().get(app.owner_id)?;
    Some(r::Integration::new(urls, app, owner))
}

pub(crate) fn label_view(urls: &Urls, full: &str, l: &Label) -> r::Label {
    r::Label::new(urls, l, full)
}

fn labels_of(tx: &Tx, urls: &Urls, full: &str, ids: &[i64]) -> Vec<r::Label> {
    let labels = tx.table::<Labels>();
    ids.iter()
        .filter_map(|id| labels.get(*id))
        .map(|l| label_view(urls, full, l))
        .collect()
}

fn rollup(tx: &Tx, url: String, kind: ReactionSubjectKind, subject: i64) -> ReactionRollup {
    ReactionRollup::new(
        url,
        tx.table::<Reactions>()
            .children(subject)
            .filter(|r| r.subject_kind == kind)
            .map(AsRef::as_ref),
    )
}

/// The REST and webhook form of an issue of `repo`, whose full name is `full`.
#[must_use]
pub fn issue_view(tx: &Tx, urls: &Urls, repo: &Repo, full: &str, i: &Issue) -> r::Issue {
    let assignees = i
        .assignee_ids
        .iter()
        .filter_map(|id| user(tx, urls, *id))
        .collect();
    let mut v = r::Issue::new(
        urls,
        i,
        full,
        user(tx, urls, i.user_id),
        labels_of(tx, urls, full, &i.label_ids),
        assignees,
    );
    v.comments = count(tx.table::<IssueComments>().children(i.id).count());
    v.closed_by = i.closed_by.and_then(|id| user(tx, urls, id));
    v.author_association = association(tx, repo, i.user_id);
    v.performed_via_github_app = integration(
        tx,
        urls,
        tx.table::<Accounts>().get(i.user_id).and_then(|a| a.app_id),
    );
    v.reactions = rollup(
        tx,
        std::mem::take(&mut v.reactions.url),
        ReactionSubjectKind::Issue,
        i.id,
    );
    v
}

/// The REST and webhook form of a comment on `issue`.
#[must_use]
pub fn comment_view(
    tx: &Tx,
    urls: &Urls,
    repo: &Repo,
    full: &str,
    issue: &Issue,
    c: &IssueComment,
) -> r::IssueComment {
    let mut v = r::IssueComment::new(
        urls,
        c,
        full,
        issue.number,
        issue.pull.is_some(),
        user(tx, urls, c.user_id),
    );
    v.author_association = association(tx, repo, c.user_id);
    v.performed_via_github_app = integration(tx, urls, c.via_app_id);
    v.reactions = rollup(
        tx,
        std::mem::take(&mut v.reactions.url),
        ReactionSubjectKind::IssueComment,
        c.id,
    );
    v
}

fn branch(
    tx: &Tx,
    urls: &Urls,
    side: &hubdouble_model::PullRef,
    fallback_owner: &Account,
) -> r::PullBranch {
    let repo = side.repo_id.and_then(|id| tx.table::<Repos>().get(id));
    let owner = repo
        .and_then(|rp| tx.table::<Accounts>().get(rp.owner_id))
        .map_or(fallback_owner, AsRef::as_ref);
    let rendered = repo.map(|rp| r::Repository::new(urls, rp, owner));
    r::PullBranch::new(urls, side, owner, rendered)
}

pub(crate) fn pull_view(
    tx: &Tx,
    urls: &Urls,
    repo: &Repo,
    full: &str,
    i: &Issue,
) -> Option<r::PullRequest> {
    let p = i.pull.as_ref()?;
    let accounts = tx.table::<Accounts>();
    let author = accounts.get(i.user_id)?;
    let mut v = r::PullRequest::new(
        urls,
        i,
        full,
        Some(r::SimpleUser::new(urls, author)),
        branch(tx, urls, &p.head, author),
        branch(tx, urls, &p.base, author),
    )?;
    v.labels = labels_of(tx, urls, full, &i.label_ids);
    v.assignees = i
        .assignee_ids
        .iter()
        .filter_map(|id| user(tx, urls, *id))
        .collect();
    v.assignee = v.assignees.first().cloned();
    v.requested_reviewers = p
        .requested_reviewers
        .iter()
        .filter_map(|id| user(tx, urls, *id))
        .collect();
    v.requested_teams = p
        .requested_teams
        .iter()
        .filter_map(|id| {
            let t = tx.table::<Teams>().get(*id)?;
            Some(r::TeamSimple::new(urls, t, accounts.get(t.org_id)?))
        })
        .collect();
    v.merged_by = p.merged_by.and_then(|id| user(tx, urls, id));
    v.comments = count(tx.table::<IssueComments>().children(i.id).count());
    v.author_association = association(tx, repo, i.user_id);
    v.mergeable_state = hubdouble_api_pulls::mergeable_state(tx, i);
    Some(v)
}

fn count(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

pub(crate) struct Who {
    pub(crate) account: Arc<Account>,
    pub(crate) via_app: Option<i64>,
    pub(crate) role: RepoRole,
}

impl Who {
    pub(crate) fn can(&self, role: RepoRole) -> bool {
        self.role >= role
    }
}

pub(crate) fn who(ctx: &Ctx<'_>, repo: &Repo) -> Result<Who, ApiError> {
    match ctx.actor() {
        Actor::Installation { app, bot, .. } => Ok(Who {
            account: bot.clone(),
            via_app: Some(app.id),
            role: RepoRole::Admin,
        }),
        Actor::User { user, .. } => Ok(Who {
            role: repo_role(ctx.tx(), user.id, repo).unwrap_or(RepoRole::Read),
            account: user.clone(),
            via_app: None,
        }),
        Actor::Anonymous | Actor::App { .. } => Err(ApiError::forbidden(
            "Resource not accessible by integration",
        )),
    }
}

pub(crate) fn payload_urls(tx: &Tx) -> Urls {
    Urls::new(tx.config(), "")
}

pub(crate) fn emit(
    tx: &mut TxMut<'_>,
    name: &'static str,
    action: &'static str,
    repo: &Repo,
    sender: i64,
    payload: Map<String, Value>,
) {
    hubdouble_hooks::emit(
        tx,
        Event {
            name,
            action: Some(action),
            repo_id: Some(repo.id),
            org_id: None,
            sender_id: sender,
            app_id: None,
            installation_id: None,
            payload,
        },
    );
}

pub(crate) fn emit_issue(
    tx: &mut TxMut<'_>,
    repo: &Repo,
    issue: &Issue,
    action: &'static str,
    sender: i64,
    extra: Vec<(&'static str, Value)>,
) -> Result<(), ApiError> {
    let urls = payload_urls(tx);
    let full = full_name(tx, repo);
    let mut p = Map::new();
    let name = if issue.pull.is_some() {
        p.insert("number".to_owned(), Value::from(issue.number));
        let pull = pull_view(tx, &urls, repo, &full, issue)
            .ok_or_else(|| ApiError::internal("the pull request has no author"))?;
        p.insert("pull_request".to_owned(), to_value(&pull)?);
        "pull_request"
    } else {
        p.insert(
            "issue".to_owned(),
            to_value(&issue_view(tx, &urls, repo, &full, issue))?,
        );
        "issues"
    };
    for (k, v) in extra {
        p.insert(k.to_owned(), v);
    }
    emit(tx, name, action, repo, sender, p);
    Ok(())
}

pub(crate) fn changed_from(field: &str, old: Value) -> Map<String, Value> {
    let mut from = Map::new();
    from.insert("from".to_owned(), old);
    let mut changes = Map::new();
    changes.insert(field.to_owned(), Value::Object(from));
    changes
}

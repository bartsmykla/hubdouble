use std::sync::Arc;

use hubdouble_git::Git;
use hubdouble_model::{
    ApiError, Issue, IssueState, Label, Repo, RepoRole, StateReason, Timestamp, node_id,
};
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::{Accounts, Counters, IssueComments, Issues, Labels, Tx, TxMut};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::cursor::{self, Keyed};
use crate::events::{self, What};
use crate::shared::{
    Who, changed_from, created, emit_issue, full_name, invalid, invalid_value, issue_view,
    label_view, payload_urls, to_value, user, who,
};

const LIST_DOCS: &str = "https://docs.github.com/v3/issues/#list-issues";
const REPOSITORY_ISSUES_DOCS: &str =
    "https://docs.github.com/rest/issues/issues#list-repository-issues";
const DEFAULT_TYPES: [&str; 3] = ["Task", "Bug", "Feature"];
const LOCAL_EPOCH: Timestamp = Timestamp(8 * 3600);

#[derive(Deserialize)]
#[serde(untagged)]
enum LabelName {
    Name(String),
    Object { name: String },
}

impl LabelName {
    fn into_name(self) -> String {
        match self {
            LabelName::Name(n) | LabelName::Object { name: n } => n,
        }
    }
}

fn label_names(v: Value) -> Result<Vec<String>, ApiError> {
    serde_json::from_value::<Vec<LabelName>>(v)
        .map(|names| names.into_iter().map(LabelName::into_name).collect())
        .map_err(|_| invalid("Issue", "labels", "invalid"))
}

fn issue_by_number(tx: &Tx, repo: &Repo, number: i64) -> Result<Arc<Issue>, ApiError> {
    tx.table::<Issues>()
        .by_number(repo.id, number)
        .cloned()
        .ok_or_else(ApiError::not_found)
}

#[derive(Deserialize)]
struct Lock {
    lock_reason: Option<String>,
}

const LOCK_REASONS: [&str; 4] = ["off-topic", "too heated", "resolved", "spam"];

pub(crate) fn lock(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let reason = if ctx.raw_body().is_empty() {
        None
    } else {
        ctx.body::<Lock>()?.lock_reason
    };
    if let Some(r) = reason.as_deref()
        && !LOCK_REASONS.contains(&r)
    {
        return Err(invalid_value("Issue", "lock_reason", r));
    }
    set_lock(ctx, reason, true)
}

pub(crate) fn unlock(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    set_lock(ctx, None, false)
}

fn set_lock(ctx: &mut Ctx<'_>, reason: Option<String>, locked: bool) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?.clone();
    let number = ctx.param_i64("issue_number")?;
    let who = who(ctx, &repo)?;
    if !who.can(RepoRole::Write) {
        return Err(ApiError::forbidden("Must have push access to repository."));
    }
    let now = ctx.now();
    let tx = ctx.tx_mut()?;
    let old = issue_by_number(tx, &repo, number)?;
    if old.locked == locked && old.active_lock_reason == reason {
        return Ok(Reply::empty());
    }
    let mut issue = Issue::clone(&old);
    issue.locked = locked;
    issue.active_lock_reason.clone_from(&reason);
    let issue = Arc::new(issue);
    tx.table_mut::<Issues>().replace(issue.clone())?;
    let (what, action) = if locked {
        (What::Locked(reason), "locked")
    } else {
        (What::Unlocked, "unlocked")
    };
    events::record(tx, &issue, who.account.id, who.via_app, what, now)?;
    emit_issue(tx, &repo, &issue, action, who.account.id, Vec::new())?;
    Ok(Reply::empty())
}

fn assignee_ids(tx: &Tx, repo: &Repo, logins: &[String]) -> Result<Vec<i64>, ApiError> {
    let mut ids = Vec::with_capacity(logins.len());
    for login in logins {
        let id = tx
            .table::<Accounts>()
            .by_login(login)
            .filter(|a| hubdouble_auth::repo_role(tx, a.id, repo).is_some())
            .map(|a| a.id)
            .ok_or_else(|| invalid("Issue", "assignees", "invalid"))?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

fn label_extra(
    tx: &Tx,
    repo: &Repo,
    label: &Label,
) -> Result<Vec<(&'static str, Value)>, ApiError> {
    let urls = payload_urls(tx);
    Ok(vec![(
        "label",
        to_value(&label_view(&urls, &full_name(tx, repo), label))?,
    )])
}

fn assignee_extra(tx: &Tx, id: i64) -> Result<Vec<(&'static str, Value)>, ApiError> {
    Ok(vec![(
        "assignee",
        to_value(&user(tx, &payload_urls(tx), id))?,
    )])
}

fn what_label(l: &Label, added: bool) -> What {
    let (name, color) = (l.name.clone(), l.color.clone());
    if added {
        What::Labeled { name, color }
    } else {
        What::Unlabeled { name, color }
    }
}

struct Change<'a> {
    repo: &'a Repo,
    who: &'a Who,
    now: Timestamp,
}

impl Change<'_> {
    fn state(
        &self,
        tx: &mut TxMut<'_>,
        git: &Git,
        issue: &Issue,
        to: IssueState,
    ) -> Result<(), ApiError> {
        let (what, action) = match to {
            IssueState::Closed => (What::Closed(issue.state_reason), "closed"),
            IssueState::Open => (What::Reopened, "reopened"),
        };
        let sender = self.who.account.id;
        if to == IssueState::Open && issue.pull.is_some() {
            return hubdouble_api_pulls::reopened(tx, git, issue, sender, self.who.via_app);
        }
        if issue.pull.is_some() {
            hubdouble_api_pulls::sync_refs(tx, git, issue)?;
        }
        events::record(tx, issue, sender, self.who.via_app, what, self.now)?;
        emit_issue(tx, self.repo, issue, action, sender, Vec::new())
    }

    fn labels(
        &self,
        tx: &mut TxMut<'_>,
        issue: &Issue,
        labels: &[i64],
        added: bool,
    ) -> Result<(), ApiError> {
        for id in labels {
            let Some(label) = tx.table::<Labels>().get(*id).cloned() else {
                continue;
            };
            let at = self.now;
            events::record(
                tx,
                issue,
                self.who.account.id,
                self.who.via_app,
                what_label(&label, added),
                at,
            )?;
            let extra = label_extra(tx, self.repo, &label)?;
            let action = if added { "labeled" } else { "unlabeled" };
            emit_issue(tx, self.repo, issue, action, self.who.account.id, extra)?;
        }
        Ok(())
    }

    fn assignees(
        &self,
        tx: &mut TxMut<'_>,
        issue: &Issue,
        ids: &[i64],
        added: bool,
    ) -> Result<(), ApiError> {
        for id in ids {
            let what = if added {
                What::Assigned(*id)
            } else {
                What::Unassigned(*id)
            };
            events::record(
                tx,
                issue,
                self.who.account.id,
                self.who.via_app,
                what,
                self.now,
            )?;
            let extra = assignee_extra(tx, *id)?;
            let action = if added { "assigned" } else { "unassigned" };
            emit_issue(tx, self.repo, issue, action, self.who.account.id, extra)?;
        }
        Ok(())
    }
}

fn view_reply(
    tx: &Tx,
    ctx_urls: &hubdouble_render::Urls,
    repo: &Repo,
    issue: &Issue,
) -> Result<Reply, ApiError> {
    Reply::json(&issue_view(tx, ctx_urls, repo, &full_name(tx, repo), issue))
}

pub(crate) fn list(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?.clone();
    let q = ctx.query();
    let refuse =
        |field: &str, value: &str| invalid_value("Issue", field, value).with_docs(LIST_DOCS);
    let state = match q.get("state").unwrap_or("open") {
        "open" => Some(IssueState::Open),
        "closed" => Some(IssueState::Closed),
        "all" => None,
        other => return Err(refuse("state", other)),
    };
    let tx = ctx.tx();
    let accounts = tx.table::<Accounts>();
    let any_milestone = match q.get("milestone") {
        None | Some("none") => false,
        Some("*") => true,
        Some(other) => return Err(refuse("milestone", other)),
    };
    let assignee = q.get("assignee");
    let assignee_id = match assignee {
        None | Some("none" | "*") => None,
        Some(login) => Some(
            accounts
                .by_login(login)
                .ok_or_else(|| refuse("assignee", login))?
                .id,
        ),
    };
    let any_type = match q.get("type") {
        None | Some("none") => false,
        Some(t) if t == "*" || DEFAULT_TYPES.iter().any(|d| d.eq_ignore_ascii_case(t)) => true,
        Some(other) => return Err(refuse("type", other)),
    };
    let since = match q.get("since") {
        Some("") => {
            return Err(ApiError::unprocessable("Invalid datetime for since").with_docs(LIST_DOCS));
        }
        _ => ctx.since(REPOSITORY_ISSUES_DOCS)?.map(|s| s.at),
    };
    let mut wanted_labels = Vec::new();
    let mut unknown = false;
    for name in q
        .get("labels")
        .into_iter()
        .flat_map(|l| l.split(','))
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        match tx.table::<Labels>().by_name(repo.id, name) {
            Some(l) => wanted_labels.push(l.id),
            None => unknown = true,
        }
    }
    let creator = q
        .get("creator")
        .map(|login| accounts.by_login(login).map(|a| a.id));
    let before_local_epoch = since.is_some_and(|s| s < LOCAL_EPOCH);
    let issues: Vec<Arc<Issue>> = if unknown || any_milestone || any_type || before_local_epoch {
        Vec::new()
    } else {
        tx.table::<Issues>()
            .in_repo(repo.id)
            .filter(|i| state.is_none_or(|s| i.state == s))
            .filter(|i| wanted_labels.iter().all(|l| i.label_ids.contains(l)))
            .filter(|i| since.is_none_or(|s| i.updated_at >= s))
            .filter(|i| creator.is_none_or(|c| c == Some(i.user_id)))
            .filter(|i| match (assignee, assignee_id) {
                (Some("none"), _) => i.assignee_ids.is_empty(),
                (Some("*"), _) => !i.assignee_ids.is_empty(),
                (_, Some(id)) => i.assignee_ids.contains(&id),
                _ => true,
            })
            .cloned()
            .collect()
    };
    let comments = tx.table::<IssueComments>();
    let ascending = q.get("direction") == Some("asc");
    let sort = q.get("sort").unwrap_or("created");
    let millis = |t: Timestamp| u64::try_from(t.unix()).unwrap_or(0).saturating_mul(1000);
    let mut keyed: Vec<Keyed<Arc<Issue>>> = issues
        .into_iter()
        .map(|i| Keyed {
            key: match sort {
                "updated" => millis(i.updated_at),
                "comments" => u64::try_from(comments.children(i.id).count()).unwrap_or(u64::MAX),
                _ => millis(i.created_at),
            },
            id: u64::try_from(i.id).unwrap_or(0),
            item: i,
        })
        .collect();
    keyed.sort_by_key(|e| (e.key, e.id));
    if !ascending {
        keyed.reverse();
    }
    let urls = ctx.urls();
    let full = full_name(tx, &repo);
    let base = format!("{}/repositories/{}/issues", urls.api, repo.id);
    cursor::page(ctx, &base, !ascending, &keyed, |i| {
        issue_view(tx, urls, &repo, &full, i)
    })
}

pub(crate) fn get(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?.clone();
    let issue = issue_by_number(ctx.tx(), &repo, ctx.param_i64("issue_number")?)?;
    Ok(view_reply(ctx.tx(), ctx.urls(), &repo, &issue)?.modified(issue.updated_at))
}

#[derive(Deserialize)]
struct NewIssue {
    title: Option<Value>,
    body: Option<String>,
    assignee: Option<String>,
    #[serde(default)]
    assignees: Vec<String>,
    labels: Option<Value>,
}

fn title_of(v: Option<&Value>) -> Result<Option<String>, ApiError> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.clone())),
        Some(Value::Number(n)) => Ok(Some(n.to_string())),
        Some(_) => Err(invalid("Issue", "title", "invalid")),
    }
}

pub(crate) fn create(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let input: NewIssue = ctx.body()?;
    let repo = ctx.repo()?.clone();
    if !repo.has_issues {
        return Err(ApiError::new(410, "Issues are disabled for this repo"));
    }
    let title = title_of(input.title.as_ref())?
        .ok_or_else(|| invalid("Issue", "title", "missing_field"))?;
    let who = who(ctx, &repo)?;
    let mut logins = input.assignees;
    logins.extend(input.assignee);
    let names = input
        .labels
        .map(label_names)
        .transpose()?
        .unwrap_or_default();
    let urls = ctx.urls().clone();
    let now = ctx.now();
    let tx = ctx.tx_mut()?;
    let (assignees, labels) = if who.can(RepoRole::Write) {
        let assignees = assignee_ids(tx, &repo, &logins)?;
        let labels = crate::labels::ensure(tx, &repo, who.account.id, &names)?;
        (assignees, labels.iter().map(|l| l.id).collect())
    } else {
        (Vec::new(), Vec::new())
    };
    let id = tx.next_id();
    let number = tx.table_mut::<Counters>().next_number(repo.id);
    let issue = tx.table_mut::<Issues>().insert(Issue {
        id,
        node_id: node_id::entity(node_id::ISSUE, id),
        repo_id: repo.id,
        number,
        title,
        body: input.body,
        user_id: who.account.id,
        state: IssueState::Open,
        state_reason: None,
        locked: false,
        active_lock_reason: None,
        label_ids: labels,
        assignee_ids: assignees,
        closed_by: None,
        created_at: now,
        updated_at: now,
        closed_at: None,
        last_edited_at: None,
        pull: None,
    })?;
    emit_issue(tx, &repo, &issue, "opened", who.account.id, Vec::new())?;
    let author = who.account.id;
    hubdouble_api_pulls::record_mentions(tx, &issue, author, (None, issue.body.as_deref()), now)?;
    let change = Change {
        repo: &repo,
        who: &who,
        now,
    };
    change.labels(tx, &issue, &issue.label_ids, true)?;
    change.assignees(tx, &issue, &issue.assignee_ids, true)?;
    let view = issue_view(tx, &urls, &repo, &full_name(tx, &repo), &issue);
    created(&view, &view.url)
}

fn diff(old: &[i64], new: &[i64]) -> (Vec<i64>, Vec<i64>) {
    let added = new.iter().filter(|id| !old.contains(id)).copied().collect();
    let removed = old.iter().filter(|id| !new.contains(id)).copied().collect();
    (added, removed)
}

struct Edit {
    title: Option<String>,
    #[expect(
        clippy::option_option,
        reason = "an absent body stays, a null body clears it"
    )]
    body: Option<Option<String>>,
    state: Option<IssueState>,
    reason: Option<StateReason>,
    labels: Option<Vec<String>>,
    assignees: Option<Vec<String>>,
}

fn rest_enum<T>(
    input: &Map<String, Value>,
    field: &str,
    parse: fn(&str) -> Option<T>,
) -> Result<Option<T>, ApiError> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => parse(s)
            .map(Some)
            .ok_or_else(|| invalid("Issue", field, "invalid")),
        Some(_) => Err(invalid("Issue", field, "invalid")),
    }
}

impl Edit {
    fn parse(input: &Map<String, Value>) -> Result<Edit, ApiError> {
        let body = match input.get("body") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(Value::String(s)) => Some(Some(s.clone())),
            Some(_) => return Err(invalid("Issue", "body", "invalid")),
        };
        let labels = match input.get("labels") {
            None | Some(Value::Null) => None,
            Some(v) => Some(label_names(v.clone())?),
        };
        let assignees = match (input.get("assignees"), input.get("assignee")) {
            (Some(v), _) if !v.is_null() => Some(
                serde_json::from_value(v.clone())
                    .map_err(|_| invalid("Issue", "assignees", "invalid"))?,
            ),
            (_, Some(Value::String(s))) => Some(vec![s.clone()]),
            (_, Some(Value::Null)) => Some(Vec::new()),
            _ => None,
        };
        Ok(Edit {
            title: title_of(input.get("title"))?,
            body,
            state: rest_enum(input, "state", IssueState::from_rest)?,
            reason: rest_enum(input, "state_reason", StateReason::from_rest)?,
            labels,
            assignees,
        })
    }
}

pub(crate) fn apply_state(
    issue: &mut Issue,
    to: Option<IssueState>,
    reason: Option<StateReason>,
    by: i64,
    now: Timestamp,
) {
    match to {
        Some(IssueState::Closed) => {
            issue.state = IssueState::Closed;
            issue.state_reason = Some(reason.unwrap_or(StateReason::Completed));
            issue.closed_at = Some(now);
            issue.closed_by = Some(by);
        }
        Some(IssueState::Open) => {
            issue.state = IssueState::Open;
            issue.state_reason = Some(StateReason::Reopened);
            issue.closed_at = None;
            issue.closed_by = None;
        }
        None => {
            if let Some(r) = reason.filter(|_| issue.state == IssueState::Closed) {
                issue.state_reason = Some(r);
            }
        }
    }
    if issue.pull.is_some() {
        issue.state_reason = None;
    }
}

type Ordered = (Map<String, Value>, Option<IssueState>, Option<ApiError>);

#[expect(
    clippy::option_option,
    reason = "an absent body stays, a null body clears it"
)]
fn in_order(
    tx: &Tx,
    git: &Git,
    issue: &mut Issue,
    (title, body): (Option<String>, Option<Option<String>>),
    state: Option<IssueState>,
) -> Result<Ordered, ApiError> {
    let mut changes = Map::new();
    if let Some(b) = body.filter(|b| *b != issue.body) {
        let from = std::mem::replace(&mut issue.body, b);
        changes.insert("body".to_owned(), Value::from(from.unwrap_or_default()));
    }
    let mut to = state.filter(|s| *s != issue.state);
    let mut refusal = None;
    if to == Some(IssueState::Open) && issue.pull.is_some() {
        match hubdouble_api_pulls::reopen(tx, git, issue) {
            Ok(()) => {}
            Err(refused) if refused.status == 422 => {
                to = None;
                refusal = Some(refused);
            }
            Err(other) => return Err(other),
        }
    }
    if let Some(t) = title.filter(|t| *t != issue.title && refusal.is_none()) {
        let from = std::mem::replace(&mut issue.title, t);
        changes.insert("title".to_owned(), Value::from(from));
    }
    Ok((changes, to, refusal))
}

pub(crate) fn update(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let Edit {
        title,
        body,
        state,
        reason,
        labels: names,
        assignees: logins,
    } = Edit::parse(&ctx.body()?)?;
    let repo = ctx.repo()?.clone();
    let number = ctx.param_i64("issue_number")?;
    let who = who(ctx, &repo)?;
    let urls = ctx.urls().clone();
    let now = ctx.now();
    let (tx, git) = ctx.tx_mut_and_git()?;
    let old = issue_by_number(tx, &repo, number)?;
    if !who.can(RepoRole::Triage) && old.user_id != who.account.id {
        return Err(ApiError::forbidden("Must have admin rights to Repository."));
    }
    let pushes = who.can(RepoRole::Write);
    let mut issue = Issue::clone(&old);
    let (changes, reopened_or_closed, refusal) =
        in_order(tx, git, &mut issue, (title, body), state)?;
    if !changes.is_empty() {
        issue.last_edited_at = Some(now);
    }
    apply_state(&mut issue, reopened_or_closed, reason, who.account.id, now);
    let (added_labels, removed_labels) = match names.filter(|_| pushes) {
        Some(names) => {
            let ids: Vec<i64> = crate::labels::ensure(tx, &repo, who.account.id, &names)?
                .iter()
                .map(|l| l.id)
                .collect();
            let d = diff(&old.label_ids, &ids);
            issue.label_ids = ids;
            d
        }
        None => (Vec::new(), Vec::new()),
    };
    let (added_assignees, removed_assignees) = match logins.filter(|_| pushes) {
        Some(logins) => {
            let ids = assignee_ids(tx, &repo, &logins)?;
            let d = diff(&old.assignee_ids, &ids);
            issue.assignee_ids = ids;
            d
        }
        None => (Vec::new(), Vec::new()),
    };
    if issue == *old {
        return match refusal {
            Some(refused) => hubdouble_api_pulls::kept(&refused),
            None => view_reply(tx, &urls, &repo, &old),
        };
    }
    issue.updated_at = now;
    let issue = Arc::new(issue);
    tx.table_mut::<Issues>().replace(issue.clone())?;
    let sender = who.account.id;
    if let Some(from) = changes.get("title").and_then(Value::as_str) {
        let what = What::Renamed {
            from: from.to_owned(),
            to: issue.title.clone(),
        };
        events::record(tx, &issue, sender, who.via_app, what, now)?;
    }
    if !changes.is_empty() {
        let mut wrapped = Map::new();
        for (k, v) in changes {
            wrapped.extend(changed_from(&k, v));
        }
        let extra = vec![("changes", Value::Object(wrapped))];
        emit_issue(tx, &repo, &issue, "edited", sender, extra)?;
    }
    if issue.body != old.body {
        hubdouble_api_pulls::record_mentions(
            tx,
            &issue,
            sender,
            (old.body.as_deref(), issue.body.as_deref()),
            now,
        )?;
    }
    let change = Change {
        repo: &repo,
        who: &who,
        now,
    };
    if let Some(to) = reopened_or_closed {
        change.state(tx, git, &issue, to)?;
    }
    change.labels(tx, &issue, &added_labels, true)?;
    change.labels(tx, &issue, &removed_labels, false)?;
    change.assignees(tx, &issue, &added_assignees, true)?;
    change.assignees(tx, &issue, &removed_assignees, false)?;
    match refusal {
        Some(refused) => hubdouble_api_pulls::kept(&refused),
        None => view_reply(tx, &urls, &repo, &issue),
    }
}

pub(crate) fn add_labels(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let input: Value = ctx.body()?;
    let names = match input {
        Value::Array(_) => label_names(input)?,
        Value::Object(mut o) => match o.remove("labels") {
            Some(Value::Array(a)) if a.is_empty() => {
                return Err(invalid("Issue", "labels", "invalid"));
            }
            Some(v) => label_names(v)?,
            None => return Err(invalid("Issue", "labels", "missing_field")),
        },
        _ => return Err(invalid("Issue", "labels", "invalid")),
    };
    let repo = ctx.repo()?.clone();
    let number = ctx.param_i64("issue_number")?;
    let who = who(ctx, &repo)?;
    if !who.can(RepoRole::Triage) {
        return Err(ApiError::forbidden("Must have triage access to repository"));
    }
    let urls = ctx.urls().clone();
    let now = ctx.now();
    let tx = ctx.tx_mut()?;
    let old = issue_by_number(tx, &repo, number)?;
    let labels = crate::labels::ensure(tx, &repo, who.account.id, &names)?;
    let added: Vec<i64> = labels
        .iter()
        .map(|l| l.id)
        .filter(|id| !old.label_ids.contains(id))
        .collect();
    let issue = if added.is_empty() {
        old
    } else {
        let mut issue = Issue::clone(&old);
        issue.label_ids.extend(&added);
        issue.updated_at = now;
        let issue = Arc::new(issue);
        tx.table_mut::<Issues>().replace(issue.clone())?;
        Change {
            repo: &repo,
            who: &who,
            now,
        }
        .labels(tx, &issue, &added, true)?;
        issue
    };
    let full = full_name(tx, &repo);
    let all = tx.table::<Labels>();
    let views: Vec<_> = issue
        .label_ids
        .iter()
        .filter_map(|id| all.get(*id))
        .map(|l| label_view(&urls, &full, l))
        .collect();
    Reply::json(&views)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_list_takes_names_and_name_objects() {
        let names = label_names(serde_json::json!(["bug", {"name": "agent"}])).unwrap();
        assert_eq!(names, ["bug", "agent"]);
        assert!(label_names(serde_json::json!([1])).is_err());
    }
}

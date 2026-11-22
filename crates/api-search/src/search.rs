use std::sync::Arc;

use hubdouble_auth::{Actor, Target, authorize};
use hubdouble_gql::{Args, Conn, Ctx, ErrorType, GqlError, Obj, Resolved, connection};
use hubdouble_model::{Grant, Issue, IssueState, Repo, Requirement, Scope};
use hubdouble_store::{Accounts, Issues, Labels, Repos, Tx};

use crate::query::{Is, Query, SortKey};

const fn read(grants: &'static [Grant]) -> Requirement {
    Requirement {
        any_of: grants,
        scope: Scope::Repo,
        scopes: &[],
    }
}

const PULLS: Requirement = read(&[Grant::read("pull_requests")]);
const ISSUES: Requirement = read(&[Grant::read("issues")]);

pub(crate) fn search(cx: &Ctx<'_>, _: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let kind = args.get::<String>("type")?.unwrap_or_default();
    if !matches!(kind.as_str(), "ISSUE" | "ISSUE_ADVANCED" | "ISSUE_HYBRID") {
        return Err(GqlError::new(
            ErrorType::NotImplemented,
            format!("search(type: {kind}) is not implemented"),
        ));
    }
    let text = args.get::<String>("query")?.unwrap_or_default();
    let found = run(cx.tx, cx.actor, &Query::parse(&text));
    connection(
        "SearchResultItemConnection",
        &found,
        args,
        |i| i.id.to_string(),
        |i| Resolved::Object {
            type_name: if i.pull.is_some() {
                "PullRequest"
            } else {
                "Issue"
            },
            value: Arc::<Issue>::clone(i),
        },
    )
}

pub(crate) fn nodes(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let conn = obj.get::<Conn>()?;
    Ok(Resolved::List(
        conn.edges.iter().map(|e| e.node.clone()).collect(),
    ))
}

pub(crate) fn issue_count(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(Resolved::Leaf(obj.get::<Conn>()?.total_count.into()))
}

pub(crate) fn run<'t>(tx: &'t Tx, actor: &Actor, q: &Query<'_>) -> Vec<&'t Arc<Issue>> {
    let accounts = tx.table::<Accounts>();
    let authors: Vec<i64> = q
        .authors
        .iter()
        .filter_map(|login| {
            let login = login
                .strip_prefix("app/")
                .map_or_else(|| (*login).to_owned(), |slug| format!("{slug}[bot]"));
            accounts.by_login(&login).map(|a| a.id)
        })
        .collect();
    if !q.authors.is_empty() && authors.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    for repo in repos(tx, q) {
        let pulls = authorize(tx, actor, &PULLS, Target::Repo(repo)).is_ok();
        let issues = authorize(tx, actor, &ISSUES, Target::Repo(repo)).is_ok();
        found.extend(tx.table::<Issues>().in_repo(repo.id).filter(|i| {
            (if i.pull.is_some() { pulls } else { issues })
                && (authors.is_empty() || authors.contains(&i.user_id))
                && matches(tx, q, i)
        }));
    }
    match q.sort {
        SortKey::Created => found.sort_unstable_by_key(|i| (i.created_at, i.id)),
        SortKey::Updated => found.sort_unstable_by_key(|i| (i.updated_at, i.id)),
    }
    if !q.ascending {
        found.reverse();
    }
    found
}

fn repos<'t>(tx: &'t Tx, q: &Query<'_>) -> Vec<&'t Arc<Repo>> {
    let accounts = tx.table::<Accounts>();
    let table = tx.table::<Repos>();
    let owners: Vec<i64> = q
        .owners
        .iter()
        .filter_map(|login| accounts.by_login(login).map(|a| a.id))
        .collect();
    let mut repos: Vec<&Arc<Repo>> = if !q.repos.is_empty() {
        q.repos
            .iter()
            .filter_map(|full| {
                let (owner, name) = full.split_once('/')?;
                table.by_name(accounts.by_login(owner)?.id, name)
            })
            .filter(|r| q.owners.is_empty() || owners.contains(&r.owner_id))
            .collect()
    } else if !q.owners.is_empty() {
        owners.iter().flat_map(|id| table.children(*id)).collect()
    } else {
        table.iter().collect()
    };
    repos.sort_unstable_by_key(|r| r.id);
    repos.dedup_by_key(|r| r.id);
    repos
}

fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len()
        && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

fn matches(tx: &Tx, q: &Query<'_>, i: &Issue) -> bool {
    let pull = i.pull.as_ref();
    let is = q.is.iter().all(|is| match is {
        Is::Pull => pull.is_some(),
        Is::Issue => pull.is_none(),
        Is::Open => i.state == IssueState::Open,
        Is::Closed => i.state == IssueState::Closed,
        Is::Merged => pull.is_some_and(|p| p.merged_at.is_some()),
        Is::Draft => pull.is_some_and(|p| p.draft),
    });
    let branches = (q.heads.is_empty() && q.bases.is_empty())
        || pull.is_some_and(|p| {
            (q.heads.is_empty()
                || q.heads
                    .iter()
                    .any(|h| starts_with_ignore_case(&p.head.ref_name, h)))
                && (q.bases.is_empty()
                    || q.bases
                        .iter()
                        .any(|b| p.base.ref_name.eq_ignore_ascii_case(b)))
        });
    is && branches && labelled(tx, q, i) && text(q, i)
}

fn labelled(tx: &Tx, q: &Query<'_>, i: &Issue) -> bool {
    if q.labels.is_empty() {
        return true;
    }
    let labels = tx.table::<Labels>();
    q.labels.iter().all(|group| {
        i.label_ids
            .iter()
            .filter_map(|id| labels.get(*id))
            .any(|label| {
                group
                    .iter()
                    .any(|name| label.name.eq_ignore_ascii_case(name))
            })
    })
}

fn text(q: &Query<'_>, i: &Issue) -> bool {
    if q.terms.is_empty() {
        return true;
    }
    let title = if q.in_title {
        i.title.to_lowercase()
    } else {
        String::new()
    };
    let body = match (&i.body, q.in_body) {
        (Some(body), true) => body.to_lowercase(),
        _ => String::new(),
    };
    q.terms
        .iter()
        .all(|t| title.contains(t.as_str()) || body.contains(t.as_str()))
}

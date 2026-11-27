use std::sync::Arc;

use hubdouble_auth::{Target as AuthTarget, authorize};
use hubdouble_gql::{
    Args, Ctx, ErrorType, GqlError, Int, Obj, Registry, Resolved, Target, connection,
};
use hubdouble_model::{
    AccountKind, ApiError, Grant, Issue, IssueComment, IssueState, Repo, Requirement, node_id,
};
use hubdouble_store::{Accounts, IssueComments, Issues, Repos};
use serde::{Deserialize, Serialize};

use crate::links::{IssueLinks, closing_references, pull_closing_issues};
use crate::shared::{ISSUES_READ, READ, full_name};

const PULLS_READ: Requirement = Requirement {
    any_of: &[Grant::read("pull_requests")],
    ..ISSUES_READ
};

fn closes(text: &str, (web, full_name, number): (&str, &str, i64), local: bool) -> bool {
    closing_references(text, web).any(|(qualifier, target)| {
        target == number
            && ((local && qualifier.is_empty()) || qualifier.eq_ignore_ascii_case(full_name))
    })
}

fn keyword_link(source: &Repo, pull: &Issue, issue_repo: i64, named: (&str, &str, i64)) -> bool {
    pull.pull
        .as_ref()
        .is_some_and(|p| p.base.ref_name == source.default_branch)
        && pull
            .body
            .as_deref()
            .is_some_and(|body| closes(body, named, source.id == issue_repo))
}

fn association(cx: &Ctx<'_>, repo_id: i64, user_id: i64) -> Result<Resolved, GqlError> {
    let repo =
        cx.tx.table::<Repos>().get(repo_id).ok_or_else(|| {
            GqlError::new(ErrorType::NotFound, "Could not resolve the repository.")
        })?;
    leaf(&hubdouble_api_pulls::association(cx.tx, repo, user_id))
}

fn leaf(v: &impl Serialize) -> Result<Resolved, GqlError> {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::Null) => Ok(Resolved::Null),
        Ok(v) => Ok(Resolved::Leaf(v)),
        Err(e) => Err(GqlError::new(ErrorType::Internal, e.to_string())),
    }
}

fn lock_reason(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let Some(reason) = obj.get::<Issue>()?.active_lock_reason.as_deref() else {
        return Ok(Resolved::Null);
    };
    let value = match reason {
        "off-topic" => "OFF_TOPIC",
        "resolved" => "RESOLVED",
        "spam" => "SPAM",
        "too heated" => "TOO_HEATED",
        _ => {
            return Err(GqlError::new(
                ErrorType::NotImplemented,
                format!("hubdouble does not implement the lock reason {reason}"),
            ));
        }
    };
    leaf(&value)
}

fn read_repo<'a>(
    ctx: &'a Ctx<'_>,
    repo_id: i64,
    requirement: &Requirement,
) -> Result<&'a Arc<Repo>, GqlError> {
    let repo = ctx
        .tx
        .table::<Repos>()
        .get(repo_id)
        .ok_or_else(ApiError::not_found)?;
    authorize(ctx.tx, ctx.actor, requirement, AuthTarget::Repo(repo))?;
    Ok(repo)
}

fn issue_object(i: &Arc<Issue>) -> Resolved {
    Resolved::Object {
        type_name: "Issue",
        value: Arc::<Issue>::clone(i),
    }
}

fn load_issue(ctx: &Ctx<'_>, id: i64) -> Result<Resolved, GqlError> {
    let issue = ctx
        .tx
        .table::<Issues>()
        .get(id)
        .filter(|i| i.pull.is_none())
        .ok_or_else(ApiError::not_found)?;
    read_repo(ctx, issue.repo_id, &ISSUES_READ)?;
    Ok(issue_object(issue))
}

fn load_comment(ctx: &Ctx<'_>, id: i64) -> Result<Resolved, GqlError> {
    let comment = ctx
        .tx
        .table::<IssueComments>()
        .get(id)
        .ok_or_else(ApiError::not_found)?;
    read_repo(ctx, comment.repo_id, &READ)?;
    Ok(Resolved::Object {
        type_name: "IssueComment",
        value: Arc::<IssueComment>::clone(comment),
    })
}

fn issue_of_comment<'a>(ctx: &'a Ctx<'_>, c: &IssueComment) -> Result<&'a Arc<Issue>, GqlError> {
    ctx.tx
        .table::<Issues>()
        .get(c.issue_id)
        .ok_or_else(|| GqlError::new(ErrorType::NotFound, "the comment's issue is gone"))
}

fn closed_by(ctx: &Ctx<'_>, obj: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let issue = obj.get::<Issue>()?;
    let include_closed = args.get::<bool>("includeClosedPrs")?.unwrap_or(false);
    let user_linked_only = args.get::<bool>("userLinkedOnly")?.unwrap_or(false);
    let exclude_user_linked = args.get::<bool>("excludeUserLinked")?.unwrap_or(false);
    let repo = read_repo(ctx, issue.repo_id, &PULLS_READ)?;
    let full = full_name(ctx.tx, repo);
    let named = (
        ctx.tx.config().web_url.as_str(),
        full.as_str(),
        issue.number,
    );
    let issues = ctx.tx.table::<Issues>();
    let mut pulls: Vec<&Arc<Issue>> = if user_linked_only {
        Vec::new()
    } else {
        ctx.tx
            .table::<Repos>()
            .iter()
            .filter(|source| source.id == repo.id || read_repo(ctx, source.id, &PULLS_READ).is_ok())
            .flat_map(|source| {
                issues
                    .in_repo(source.id)
                    .filter(move |i| keyword_link(source, i, repo.id, named))
            })
            .collect()
    };
    if !exclude_user_linked {
        pulls.extend(
            ctx.tx
                .table::<IssueLinks>()
                .children(issue.id)
                .filter_map(|l| issues.get(l.pull_issue_id))
                .filter(|p| read_repo(ctx, p.repo_id, &PULLS_READ).is_ok()),
        );
    }
    pulls.retain(|p| include_closed || p.state == IssueState::Open);
    pulls.sort_by_key(|p| p.id);
    pulls.dedup_by_key(|p| p.id);
    let repos = ctx.tx.table::<Repos>();
    pulls.retain(|pull| {
        repos.get(pull.repo_id).is_some_and(|source| {
            pull_closing_issues(ctx.tx, pull, source).any(|link| {
                link.issue.id == issue.id && link.matches(user_linked_only, exclude_user_linked)
            })
        })
    });
    connection(
        "PullRequestConnection",
        &pulls,
        args,
        |p| p.id.to_string(),
        |p| Resolved::Object {
            type_name: "PullRequest",
            value: Arc::<Issue>::clone(p),
        },
    )
}

fn closing_issues(ctx: &Ctx<'_>, obj: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let pull = obj.get::<Issue>()?;
    let repo = read_repo(ctx, pull.repo_id, &PULLS_READ)?;
    let user_linked_only = args.get::<bool>("userLinkedOnly")?.unwrap_or(false);
    let exclude_user_linked = args.get::<bool>("excludeUserLinked")?.unwrap_or(false);
    let mut references: Vec<_> = pull_closing_issues(ctx.tx, pull, repo)
        .filter(|link| link.matches(user_linked_only, exclude_user_linked))
        .map(|link| link.issue)
        .filter(|issue| read_repo(ctx, issue.repo_id, &ISSUES_READ).is_ok())
        .collect();
    if let Some(order) = args.get::<IssueOrder>("orderBy")? {
        order_issues(ctx, &mut references, &order);
    }
    connection(
        "IssueConnection",
        &references,
        args,
        |issue| issue.id.to_string(),
        |issue| issue_object(issue),
    )
}

#[derive(Deserialize)]
struct IssueOrder {
    field: IssueOrderField,
    direction: Direction,
}

#[derive(Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum IssueOrderField {
    CreatedAt,
    UpdatedAt,
    Comments,
}

#[derive(Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum Direction {
    Asc,
    Desc,
}

fn order_issues(ctx: &Ctx<'_>, issues: &mut [&Arc<Issue>], order: &IssueOrder) {
    match order.field {
        IssueOrderField::CreatedAt => {
            issues.sort_unstable_by_key(|issue| (issue.created_at, issue.id));
        }
        IssueOrderField::UpdatedAt => {
            issues.sort_unstable_by_key(|issue| (issue.updated_at, issue.id));
        }
        IssueOrderField::Comments => {
            let comments = ctx.tx.table::<IssueComments>();
            issues.sort_by_cached_key(|issue| (comments.children(issue.id).count(), issue.id));
        }
    }
    if matches!(order.direction, Direction::Desc) {
        issues.reverse();
    }
}

fn comment_author(ctx: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let c = obj.get::<IssueComment>()?;
    Ok(ctx
        .tx
        .table::<Accounts>()
        .get(c.user_id)
        .map_or(Resolved::Null, |a| Resolved::Object {
            type_name: match a.kind {
                AccountKind::User => "User",
                AccountKind::Bot => "Bot",
                AccountKind::Organization => "Organization",
            },
            value: Arc::<hubdouble_model::Account>::clone(a),
        }))
}

fn repository_issue(ctx: &Ctx<'_>, obj: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let repo = obj.get::<Repo>()?;
    read_repo(ctx, repo.id, &ISSUES_READ)?;
    let number = args
        .int("number")?
        .ok_or_else(|| GqlError::new(ErrorType::Unprocessable, "argument number is required"))?;
    let held = match number {
        Int::Fits(n) => ctx.tx.table::<Issues>().by_number(repo.id, n),
        Int::Past(_) => None,
    };
    held.filter(|i| i.pull.is_none())
        .map(issue_object)
        .ok_or_else(|| {
            GqlError::new(
                ErrorType::NotFound,
                format!("Could not resolve to an Issue with the number of {number}."),
            )
        })
}

pub(crate) fn register(gql: &mut Registry) {
    gql.object::<Issue>("Issue");
    gql.object::<IssueComment>("IssueComment");
    gql.node(node_id::ISSUE, "Issue", load_issue);
    gql.node(node_id::ISSUE_COMMENT, "IssueComment", load_comment);

    gql.field("Issue", "id", |_, o, _| leaf(&o.get::<Issue>()?.node_id));
    gql.field("Issue", "number", |_, o, _| leaf(&o.get::<Issue>()?.number));
    gql.field("Issue", "title", |_, o, _| leaf(&o.get::<Issue>()?.title));
    gql.field("Issue", "body", |_, o, _| {
        leaf(&o.get::<Issue>()?.body.clone().unwrap_or_default())
    });
    gql.field("Issue", "state", |_, o, _| leaf(&o.get::<Issue>()?.state));
    gql.field("Issue", "lastEditedAt", |_, o, _| {
        leaf(&o.get::<Issue>()?.last_edited_at)
    });
    gql.field("Issue", "repository", |cx, o, _| {
        cx.load(Target::RepoId(o.get::<Issue>()?.repo_id))
    });
    gql.field("Issue", "closedByPullRequestsReferences", closed_by);
    gql.field("PullRequest", "closingIssuesReferences", closing_issues);
    gql.field("Issue", "activeLockReason", lock_reason);
    gql.field("PullRequest", "activeLockReason", lock_reason);
    gql.field("Issue", "authorAssociation", |cx, o, _| {
        let issue = o.get::<Issue>()?;
        association(cx, issue.repo_id, issue.user_id)
    });
    gql.field("IssueComment", "authorAssociation", |cx, o, _| {
        let comment = o.get::<IssueComment>()?;
        association(cx, comment.repo_id, comment.user_id)
    });

    gql.field("IssueComment", "id", |_, o, _| {
        leaf(&o.get::<IssueComment>()?.node_id)
    });
    gql.field("IssueComment", "databaseId", |_, o, _| {
        leaf(&o.get::<IssueComment>()?.id)
    });
    gql.field("IssueComment", "body", |_, o, _| {
        leaf(&o.get::<IssueComment>()?.body)
    });
    gql.field("IssueComment", "createdAt", |_, o, _| {
        leaf(&o.get::<IssueComment>()?.created_at)
    });
    gql.field("IssueComment", "lastEditedAt", |_, o, _| {
        leaf(&o.get::<IssueComment>()?.last_edited_at)
    });
    gql.field("IssueComment", "author", comment_author);
    gql.field("IssueComment", "issue", |cx, o, _| {
        let issue = issue_of_comment(cx, o.get::<IssueComment>()?)?;
        let requirement = if issue.pull.is_some() {
            &PULLS_READ
        } else {
            &ISSUES_READ
        };
        read_repo(cx, issue.repo_id, requirement)?;
        Ok(issue_object(issue))
    });
    gql.field("IssueComment", "pullRequest", |cx, o, _| {
        let issue = issue_of_comment(cx, o.get::<IssueComment>()?)?;
        Ok(if issue.pull.is_some() {
            read_repo(cx, issue.repo_id, &PULLS_READ)?;
            Resolved::Object {
                type_name: "PullRequest",
                value: Arc::<Issue>::clone(issue),
            }
        } else {
            Resolved::Null
        })
    });

    gql.field("Repository", "issue", repository_issue);
}

#[cfg(test)]
mod tests {
    use super::closes;

    #[test]
    fn a_closing_keyword_links_the_issue_it_names() {
        let named = ("https://github.com", "acme/public", 12);
        assert!(closes("Fixes #12", named, true));
        assert!(closes("This resolves: acme/public#12.", named, false));
        assert!(closes("(closes #12)", ("", "ACME/Public", 12), true));
        assert!(!closes("Fixes #12", named, false));
        assert!(!closes("Fixes #123", named, true));
        assert!(!closes("See #12", named, true));
        assert!(!closes("fixes other/repo#12", named, true));
    }
}

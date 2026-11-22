use std::sync::Arc;

use hubdouble_gql::{Args, Ctx, ErrorType, GqlError, Obj, Registry, Resolved, connection};
use hubdouble_model::{AccountKind, Issue, IssueComment, IssueState, Repo, node_id};
use hubdouble_store::{Accounts, Issues, Repos};
use serde::Serialize;

use crate::shared::full_name;

const CLOSING_KEYWORDS: [&str; 9] = [
    "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
];

/// Whether `text` holds a closing keyword followed by `#number` or `owner/name#number`.
pub(crate) fn closes(text: &str, full_name: &str, number: i64) -> bool {
    let lower = text.to_ascii_lowercase();
    let full = full_name.to_ascii_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    words.windows(2).any(|pair| {
        let keyword = pair[0].trim_start_matches('(').trim_end_matches(':');
        if !CLOSING_KEYWORDS.contains(&keyword) {
            return false;
        }
        let target = pair[1].trim_end_matches(['.', ',', ';', ')', '!', '?']);
        let digits = target.strip_prefix('#').or_else(|| {
            target
                .strip_prefix(full.as_str())
                .and_then(|t| t.strip_prefix('#'))
        });
        digits.and_then(|d| d.parse::<i64>().ok()) == Some(number)
    })
}

fn leaf(v: &impl Serialize) -> Result<Resolved, GqlError> {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::Null) => Ok(Resolved::Null),
        Ok(v) => Ok(Resolved::Leaf(v)),
        Err(e) => Err(GqlError::new(ErrorType::Internal, e.to_string())),
    }
}

fn visible_repo(ctx: &Ctx<'_>, repo_id: i64) -> Option<Arc<Repo>> {
    ctx.tx
        .table::<Repos>()
        .get(repo_id)
        .filter(|r| hubdouble_auth::can_see(ctx.tx, ctx.actor, r))
        .cloned()
}

fn issue_object(i: &Arc<Issue>) -> Resolved {
    Resolved::Object {
        type_name: "Issue",
        value: Arc::<Issue>::clone(i),
    }
}

#[expect(clippy::unnecessary_wraps, reason = "a node loader returns a Result")]
fn load_issue(ctx: &Ctx<'_>, id: i64) -> Result<Resolved, GqlError> {
    Ok(ctx
        .tx
        .table::<Issues>()
        .get(id)
        .filter(|i| i.pull.is_none() && visible_repo(ctx, i.repo_id).is_some())
        .map_or(Resolved::Null, issue_object))
}

#[expect(clippy::unnecessary_wraps, reason = "a node loader returns a Result")]
fn load_comment(ctx: &Ctx<'_>, id: i64) -> Result<Resolved, GqlError> {
    Ok(ctx
        .tx
        .table::<hubdouble_store::IssueComments>()
        .get(id)
        .filter(|c| visible_repo(ctx, c.repo_id).is_some())
        .map_or(Resolved::Null, |c| Resolved::Object {
            type_name: "IssueComment",
            value: Arc::<IssueComment>::clone(c),
        }))
}

fn repository(ctx: &Ctx<'_>, repo_id: i64) -> Resolved {
    ctx.tx
        .table::<Repos>()
        .get(repo_id)
        .map_or(Resolved::Null, |r| Resolved::Object {
            type_name: "Repository",
            value: Arc::<Repo>::clone(r),
        })
}

fn issue_of_comment(ctx: &Ctx<'_>, c: &IssueComment) -> Result<Arc<Issue>, GqlError> {
    ctx.tx
        .table::<Issues>()
        .get(c.issue_id)
        .cloned()
        .ok_or_else(|| GqlError::new(ErrorType::NotFound, "the comment's issue is gone"))
}

fn closed_by(ctx: &Ctx<'_>, obj: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let issue = obj.get::<Issue>()?;
    let include_closed = args.get::<bool>("includeClosedPrs")?.unwrap_or(false);
    let user_linked_only = args.get::<bool>("userLinkedOnly")?.unwrap_or(false);
    let Some(repo) = ctx.tx.table::<Repos>().get(issue.repo_id) else {
        return Ok(Resolved::Null);
    };
    let full = full_name(ctx.tx, repo);
    let pulls: Vec<&Arc<Issue>> = if user_linked_only {
        Vec::new()
    } else {
        ctx.tx
            .table::<Issues>()
            .in_repo(repo.id)
            .filter(|i| {
                i.pull
                    .as_ref()
                    .is_some_and(|p| p.base.ref_name == repo.default_branch)
            })
            .filter(|i| include_closed || i.state == IssueState::Open)
            .filter(|i| {
                i.body
                    .as_deref()
                    .is_some_and(|b| closes(b, &full, issue.number))
            })
            .collect()
    };
    connection(
        "PullRequestConnection",
        &pulls,
        args,
        |p| p.number.to_string(),
        |p| Resolved::Object {
            type_name: "PullRequest",
            value: Arc::<Issue>::clone(p),
        },
    )
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
    let number: i64 = args
        .get("number")?
        .ok_or_else(|| GqlError::new(ErrorType::Unprocessable, "argument number is required"))?;
    ctx.tx
        .table::<Issues>()
        .by_number(repo.id, number)
        .filter(|i| i.pull.is_none())
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
        Ok(repository(cx, o.get::<Issue>()?.repo_id))
    });
    gql.field("Issue", "closedByPullRequestsReferences", closed_by);

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
        Ok(issue_object(&issue_of_comment(
            cx,
            o.get::<IssueComment>()?,
        )?))
    });
    gql.field("IssueComment", "pullRequest", |cx, o, _| {
        let issue = issue_of_comment(cx, o.get::<IssueComment>()?)?;
        Ok(if issue.pull.is_some() {
            Resolved::Object {
                type_name: "PullRequest",
                value: issue,
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
        assert!(closes("Fixes #12", "acme/public", 12));
        assert!(closes("This resolves: acme/public#12.", "acme/public", 12));
        assert!(closes("(closes #12)", "ACME/Public", 12));
        assert!(!closes("Fixes #123", "acme/public", 12));
        assert!(!closes("See #12", "acme/public", 12));
        assert!(!closes("fixes other/repo#12", "acme/public", 12));
    }
}

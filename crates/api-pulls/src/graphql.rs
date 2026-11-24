use std::sync::Arc;

use hubdouble_api_gitdata::branch_rules::MergeMethod;
use hubdouble_auth::{Target as AuthTarget, authorize};
use hubdouble_git::{CommitNode, GitError, RefNode};
use hubdouble_gql::{
    Args, Ctx, ErrorType, GqlError, MutCtx, Obj, Registry, Resolved, Target, connection,
};
use hubdouble_model::{ApiError, Issue, IssueState, Pull, Repo, node_id};
use hubdouble_store::{Issues, Labels, Repos};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    CONTENTS_WRITE, READ, WRITE, change, events, mergeability, merging, render, reviewing, state,
};

macro_rules! fields {
    ($r:ident, $($field:literal => |$ctx:ident, $issue:ident, $p:ident| $value:expr),* $(,)?) => {
        $( $r.field("PullRequest", $field, |$ctx, obj, _| {
            let $issue = obj.get::<Issue>()?;
            let $p = state::pull($issue)?;
            Ok(scalar($value))
        }); )*
    };
}

pub(crate) fn register(r: &mut Registry) {
    r.object::<Issue>("PullRequest");
    r.node(node_id::PULL_REQUEST, "PullRequest", by_id);
    r.object_resolved::<PullCommit>("PullRequestCommit");
    r.object::<DraftPayload>("MarkPullRequestReadyForReviewPayload");
    r.object::<DraftPayload>("ConvertPullRequestToDraftPayload");
    r.field("Repository", "pullRequest", by_number);
    r.field("Ref", "associatedPullRequests", associated);
    r.field("PullRequest", "repository", |cx, obj, _| {
        cx.load(Target::RepoId(obj.get::<Issue>()?.repo_id))
    });
    r.field("PullRequest", "baseRepository", |cx, obj, _| {
        repository(cx, state::pull(obj.get::<Issue>()?)?.base.repo_id)
    });
    r.field("PullRequest", "headRepository", |cx, obj, _| {
        repository(cx, state::pull(obj.get::<Issue>()?)?.head.repo_id)
    });
    r.field("PullRequest", "baseRef", base_ref);
    r.field("PullRequest", "commits", commits);
    r.field("PullRequest", "mergeCommit", merge_commit);
    r.field("PullRequestCommit", "commit", |_, obj, _| {
        Ok(commit_object(obj.get::<PullCommit>()?.node.clone()))
    });
    fields!(r,
        "id" => |_cx, _issue, p| &p.node_id,
        "number" => |_cx, issue, _p| issue.number,
        "title" => |_cx, issue, _p| &issue.title,
        "body" => |_cx, issue, _p| issue.body.as_deref().unwrap_or_default(),
        "isDraft" => |_cx, _issue, p| p.draft,
        "state" => |_cx, issue, p| pull_state(issue, p),
        "baseRefName" => |_cx, _issue, p| &p.base.ref_name,
        "baseRefOid" => |_cx, _issue, p| &p.base.sha,
        "headRefName" => |_cx, _issue, p| &p.head.ref_name,
        "headRefOid" => |_cx, _issue, p| &p.head.sha,
        "updatedAt" => |_cx, issue, _p| issue.updated_at,
        "mergedAt" => |_cx, _issue, p| p.merged_at,
        "mergeable" => |cx, issue, p| {
            mergeability::read(cx.read_jobs, cx.tx, issue);
            match p.mergeable { Some(true) => "MERGEABLE", Some(false) => "CONFLICTING", None => "UNKNOWN" }
        },
        "mergeStateStatus" => |cx, issue, _p| {
            mergeability::read(cx.read_jobs, cx.tx, issue);
            crate::mergeable_state(cx.tx, issue).to_ascii_uppercase()
        },
        "authorAssociation" => |cx, issue, _p| association(cx, issue)?,
        "reviewDecision" => |cx, issue, p| reviewing::review_decision(cx.tx, issue, p),
        "url" => |cx, issue, _p| format!("{}/{}/pull/{}", cx.urls.web, render::full_name(cx.tx, issue.repo_id)?, issue.number),
    );
    r.object::<DraftPayload>("ReopenPullRequestPayload");
    r.object::<DraftPayload>("MergePullRequestPayload");
    for kind in [
        "MarkPullRequestReadyForReviewPayload",
        "ConvertPullRequestToDraftPayload",
        "ReopenPullRequestPayload",
        "MergePullRequestPayload",
    ] {
        r.field(kind, "pullRequest", |_, obj, _| {
            Ok(pull_object(&obj.get::<DraftPayload>()?.issue))
        });
    }
    r.mutation("markPullRequestReadyForReview", |cx, args| {
        draft(cx, args, false)
    });
    r.mutation("convertPullRequestToDraft", |cx, args| {
        draft(cx, args, true)
    });
    r.mutation("reopenPullRequest", reopen);
    r.mutation("mergePullRequest", merge);
}

fn scalar(value: impl Serialize) -> Resolved {
    let value = json!(value);
    if value.is_null() {
        Resolved::Null
    } else {
        Resolved::Leaf(value)
    }
}

fn pull_object(issue: &Arc<Issue>) -> Resolved {
    Resolved::Object {
        type_name: "PullRequest",
        value: issue.clone(),
    }
}

fn pull_state(issue: &Issue, p: &Pull) -> &'static str {
    if p.merged_at.is_some() {
        "MERGED"
    } else if issue.state == IssueState::Closed {
        "CLOSED"
    } else {
        "OPEN"
    }
}

fn check_read(cx: &Ctx<'_>, id: i64) -> Result<(), GqlError> {
    cx.load(Target::RepoId(id))?;
    let repo = cx
        .tx
        .table::<Repos>()
        .get(id)
        .ok_or_else(ApiError::not_found)?;
    Ok(authorize(cx.tx, cx.actor, &READ, AuthTarget::Repo(repo))?)
}

fn by_id(cx: &Ctx<'_>, id: i64) -> Result<Resolved, GqlError> {
    let issue = cx
        .tx
        .table::<Issues>()
        .by_alt_id(id)
        .filter(|i| i.pull.is_some())
        .ok_or_else(ApiError::not_found)?;
    check_read(cx, issue.repo_id)?;
    Ok(pull_object(issue))
}

fn by_number(cx: &Ctx<'_>, obj: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let repo = obj.get::<Repo>()?;
    check_read(cx, repo.id)?;
    let number = required(args, "number")?;
    Ok(pull_object(state::numbered(cx.tx, repo.id, number)?))
}

fn repository(cx: &Ctx<'_>, id: Option<i64>) -> Result<Resolved, GqlError> {
    let Some(id) = id else {
        return Ok(Resolved::Null);
    };
    match cx.load(Target::RepoId(id)) {
        Err(err) if err.kind == ErrorType::NotFound => Ok(Resolved::Null),
        result => result,
    }
}

#[derive(Deserialize)]
struct Order {
    field: String,
    direction: String,
}

fn associated(cx: &Ctx<'_>, obj: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let reference = obj.get::<RefNode>()?;
    let states = args.get::<Vec<String>>("states")?;
    let base = args.get::<String>("baseRefName")?;
    let head = args.get::<String>("headRefName")?;
    let labels = args.get::<Vec<String>>("labels")?;
    let order = args.get::<Order>("orderBy")?;
    let name = reference
        .name
        .strip_prefix("refs/heads/")
        .unwrap_or(&reference.name);
    let mut pulls = Vec::new();
    for i in cx
        .tx
        .table::<Issues>()
        .pulls_with_head_repo(reference.repo_id)
    {
        let p = state::pull(i)?;
        if p.head.ref_name == name
            && states.as_ref().is_none_or(|states| {
                states.is_empty() || states.iter().any(|s| s == pull_state(i, p))
            })
            && base.as_ref().is_none_or(|base| p.base.ref_name == *base)
            && head.as_ref().is_none_or(|head| p.head.ref_name == *head)
            && labels.as_ref().is_none_or(|labels| {
                labels.iter().all(|label| {
                    i.label_ids
                        .iter()
                        .filter_map(|id| cx.tx.table::<Labels>().get(*id))
                        .any(|row| row.name.eq_ignore_ascii_case(label))
                })
            })
            && check_read(cx, i.repo_id).is_ok()
        {
            pulls.push(i);
        }
    }
    let by_update = order.as_ref().is_some_and(|o| o.field == "UPDATED_AT");
    let ascending = order.is_some_and(|o| o.direction == "ASC");
    pulls.sort_unstable_by(|a, b| {
        let stamp = |i: &Issue| {
            if by_update {
                i.updated_at
            } else {
                i.created_at
            }
        };
        let order = (stamp(a), a.id).cmp(&(stamp(b), b.id));
        if ascending { order } else { order.reverse() }
    });
    connection(
        "PullRequestConnection",
        &pulls,
        args,
        |i| i.id.to_string(),
        |i| pull_object(i),
    )
}

fn association(cx: &Ctx<'_>, issue: &Issue) -> Result<&'static str, GqlError> {
    let repo = cx
        .tx
        .table::<Repos>()
        .get(issue.repo_id)
        .ok_or_else(ApiError::not_found)?;
    Ok(crate::association(cx.tx, repo, issue.user_id))
}

struct PullCommit {
    node: Arc<CommitNode>,
}

fn commit_object(value: Arc<CommitNode>) -> Resolved {
    Resolved::Object {
        type_name: "Commit",
        value,
    }
}

fn base_ref(cx: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let base = &state::pull(obj.get::<Issue>()?)?.base;
    let Some(repo_id) = base.repo_id else {
        return Ok(Resolved::Null);
    };
    Ok(
        match state::branch(cx.git, cx.tx, repo_id, &base.ref_name) {
            Ok(target) => Resolved::Object {
                type_name: "Ref",
                value: Arc::new(RefNode {
                    repo_id,
                    name: format!("refs/heads/{}", base.ref_name),
                    prefix: "refs/heads/".to_owned(),
                    target,
                }),
            },
            Err(GitError::NotFound) => Resolved::Null,
            Err(e) => return Err(ApiError::from(e).into()),
        },
    )
}

fn merge_commit(cx: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let issue = obj.get::<Issue>()?;
    let p = state::pull(issue)?;
    let Some(sha) = p
        .merge_commit_sha
        .as_deref()
        .filter(|_| p.merged_at.is_some())
    else {
        return Ok(Resolved::Null);
    };
    let node = state::commit_node(
        cx.git,
        cx.tx,
        issue.repo_id,
        sha.parse().map_err(ApiError::from)?,
    )
    .map_err(ApiError::from)?;
    Ok(commit_object(Arc::new(node)))
}

fn commits(cx: &Ctx<'_>, obj: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let i = obj.get::<Issue>()?;
    let diff = state::compare(cx.git, cx.tx, i)?;
    connection(
        "PullRequestCommitConnection",
        &diff.commits,
        args,
        |(id, _)| id.to_string(),
        |(id, commit)| {
            let node = Arc::new(CommitNode {
                repo_id: i.repo_id,
                id: *id,
                commit: commit.clone(),
            });
            Resolved::Object {
                type_name: "PullRequestCommit",
                value: Arc::new(PullCommit { node }),
            }
        },
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DraftInput {
    pull_request_id: String,
    client_mutation_id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DraftPayload {
    #[serde(skip)]
    issue: Arc<Issue>,
    client_mutation_id: Option<String>,
}

fn load_pull(cx: &MutCtx<'_>, id: &str) -> Result<Arc<Issue>, GqlError> {
    let Resolved::Object {
        type_name: "PullRequest",
        value,
    } = cx.load(Target::Node(id))?
    else {
        return Err(ApiError::not_found().into());
    };
    Ok(value
        .downcast::<Issue>()
        .map_err(|_| ApiError::not_found())?)
}

fn reopen(cx: &mut MutCtx<'_>, args: &Args) -> Result<Resolved, GqlError> {
    let input: DraftInput = required(args, "input")?;
    let mut issue = load_pull(cx, &input.pull_request_id)?;
    let repo = cx
        .tx
        .table::<Repos>()
        .get(issue.repo_id)
        .ok_or_else(ApiError::not_found)?
        .clone();
    cx.authorize(&WRITE, &repo)?;
    change::may_change(&cx.tx, cx.actor, &repo, &issue)?;
    if issue.state == IssueState::Closed {
        let mut updated = Issue::clone(&issue);
        crate::reopen(&cx.tx, cx.git, &mut updated).map_err(|e| {
            if e.status == 422 {
                GqlError::new(ErrorType::Unprocessable, "Could not open the pull request.")
            } else {
                e.into()
            }
        })?;
        updated.updated_at = cx.clock.now();
        issue = Arc::new(updated);
        cx.tx
            .table_mut::<Issues>()
            .replace(issue.clone())
            .map_err(ApiError::from)?;
        crate::reopened(
            &mut cx.tx,
            cx.git,
            &issue,
            change::sender(cx.actor)?,
            crate::timeline::via_app(cx.actor),
        )?;
    }
    Ok(Resolved::Object {
        type_name: "ReopenPullRequestPayload",
        value: Arc::new(DraftPayload {
            issue,
            client_mutation_id: input.client_mutation_id,
        }),
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergeInput {
    pull_request_id: String,
    commit_headline: Option<String>,
    commit_body: Option<String>,
    expected_head_oid: Option<String>,
    merge_method: Option<String>,
    author_email: Option<String>,
    client_mutation_id: Option<String>,
}

fn merge(cx: &mut MutCtx<'_>, args: &Args) -> Result<Resolved, GqlError> {
    let input: MergeInput = required(args, "input")?;
    let issue = load_pull(cx, &input.pull_request_id)?;
    let repo = cx
        .tx
        .table::<Repos>()
        .get(issue.repo_id)
        .ok_or_else(ApiError::not_found)?
        .clone();
    cx.authorize(&CONTENTS_WRITE, &repo)?;
    let method = match input.merge_method.as_deref() {
        None | Some("MERGE") => MergeMethod::Merge,
        Some("SQUASH") => MergeMethod::Squash,
        Some("REBASE") => MergeMethod::Rebase,
        Some(other) => {
            return Err(GqlError::new(
                ErrorType::Unprocessable,
                format!("Invalid merge method {other}"),
            ));
        }
    };
    if let Some(email) = &input.author_email
        && !merging::owns_email(&cx.tx, change::sender(cx.actor)?, email)
    {
        return Err(GqlError::new(
            ErrorType::Unprocessable,
            "Invalid email address",
        ));
    }
    change::merge_pull(
        &mut cx.tx,
        cx.git,
        cx.actor,
        (&repo, &issue),
        change::MergeOptions {
            method,
            title: input.commit_headline,
            message: input.commit_body,
            sha: input.expected_head_oid,
            author_email: input.author_email,
        },
    )?;
    let issue = cx
        .tx
        .table::<Issues>()
        .get(issue.id)
        .ok_or_else(ApiError::not_found)?
        .clone();
    Ok(Resolved::Object {
        type_name: "MergePullRequestPayload",
        value: Arc::new(DraftPayload {
            issue,
            client_mutation_id: input.client_mutation_id,
        }),
    })
}

fn draft(cx: &mut MutCtx<'_>, args: &Args, draft: bool) -> Result<Resolved, GqlError> {
    let input: DraftInput = required(args, "input")?;
    let mut issue = load_pull(cx, &input.pull_request_id)?;
    let repo = cx
        .tx
        .table::<Repos>()
        .get(issue.repo_id)
        .ok_or_else(ApiError::not_found)?;
    cx.authorize(&CONTENTS_WRITE, repo)?;
    cx.authorize(&WRITE, repo)?;
    let p = state::pull(&issue)?;
    if issue.state == IssueState::Closed {
        return Err(ApiError::unprocessable("The pull request is closed").into());
    }
    let (type_name, action) = if draft {
        ("ConvertPullRequestToDraftPayload", "converted_to_draft")
    } else {
        ("MarkPullRequestReadyForReviewPayload", "ready_for_review")
    };
    if p.draft != draft {
        let mut updated = Issue::clone(&issue);
        updated.pull.as_mut().ok_or_else(ApiError::not_found)?.draft = draft;
        updated.updated_at = cx.clock.now();
        issue = Arc::new(updated);
        cx.tx
            .table_mut::<Issues>()
            .replace(issue.clone())
            .map_err(ApiError::from)?;
        events::emit(
            &mut cx.tx,
            cx.git,
            &issue,
            change::sender(cx.actor)?,
            action,
            json!({}),
        )?;
    }
    Ok(Resolved::Object {
        type_name,
        value: Arc::new(DraftPayload {
            issue,
            client_mutation_id: input.client_mutation_id,
        }),
    })
}

fn required<T: serde::de::DeserializeOwned>(args: &Args, name: &str) -> Result<T, GqlError> {
    args.get(name)?
        .ok_or_else(|| GqlError::new(ErrorType::Unprocessable, format!("Missing {name}")))
}

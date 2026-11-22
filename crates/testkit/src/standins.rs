//! Test-only GraphQL stand-ins a track test opts into with [`crate::Builder::register`], so a
//! query can reach the track's fields before the track that owns the parent field lands. None
//! of them is in `register_all`.

use std::sync::Arc;

use hubdouble_admin::Registrar;
use hubdouble_git::{BlobNode, CommitNode, GitError, ObjectData, ObjectId, TagNode, TreeNode};
use hubdouble_gql::{Args, Ctx, ErrorType, GqlError, Obj, Resolved, Target};
use hubdouble_model::{Issue, Repo};
use hubdouble_store::Issues;

/// `Query.repository(owner:, name:)`.
pub fn query_repository(r: &mut Registrar<'_>) {
    r.gql.field("Query", "repository", repository);
}

/// `Repository.pullRequest(number:)`.
pub fn repository_pull_request(r: &mut Registrar<'_>) {
    r.gql.field("Repository", "pullRequest", pull_request);
}

/// `Repository.issue(number:)`.
pub fn repository_issue(r: &mut Registrar<'_>) {
    r.gql.field("Repository", "issue", issue);
}

/// `Repository.object(expression:, oid:)`.
pub fn repository_object(r: &mut Registrar<'_>) {
    r.gql.field("Repository", "object", object);
}

fn required<T: serde::de::DeserializeOwned>(args: &Args, name: &str) -> Result<T, GqlError> {
    args.get(name)?.ok_or_else(|| {
        GqlError::new(
            ErrorType::Unprocessable,
            format!("argument {name} is required"),
        )
    })
}

/// # Errors
///
/// `NOT_FOUND` for a repository the actor cannot see, as [`Ctx::load`] answers.
pub fn repository(ctx: &Ctx<'_>, _: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let owner: String = required(args, "owner")?;
    let name: String = required(args, "name")?;
    ctx.load(Target::Repo {
        owner: &owner,
        name: &name,
    })
}

fn numbered(ctx: &Ctx<'_>, obj: &Obj, args: &Args, pull: bool) -> Result<Resolved, GqlError> {
    let repo = obj.get::<Repo>()?;
    let number: i64 = required(args, "number")?;
    let type_name = if pull { "PullRequest" } else { "Issue" };
    match ctx.tx.table::<Issues>().by_number(repo.id, number) {
        Some(i) if i.pull.is_some() == pull => Ok(Resolved::Object {
            type_name,
            value: Arc::<Issue>::clone(i),
        }),
        _ => Err(GqlError::new(
            ErrorType::NotFound,
            format!("Could not resolve to a {type_name} with the number of {number}."),
        )),
    }
}

/// # Errors
///
/// `NOT_FOUND` when the number is free or belongs to an issue.
pub fn pull_request(ctx: &Ctx<'_>, obj: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    numbered(ctx, obj, args, true)
}

/// # Errors
///
/// `NOT_FOUND` when the number is free or belongs to a pull request.
pub fn issue(ctx: &Ctx<'_>, obj: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    numbered(ctx, obj, args, false)
}

/// `null` when nothing matches, as GitHub answers.
///
/// # Errors
///
/// `UNPROCESSABLE` for a bad object ID or no argument, `HUBDOUBLE_NOT_IMPLEMENTED` while the git
/// store is a stub.
pub fn object(ctx: &Ctx<'_>, obj: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let repo = obj.get::<Repo>()?;
    let found = if let Some(oid) = args.get::<String>("oid")? {
        let id: ObjectId = oid
            .parse()
            .map_err(|e: GitError| GqlError::new(ErrorType::Unprocessable, e.to_string()))?;
        ctx.git.object(ctx.tx, repo.id, id)
    } else {
        let expression: String = required(args, "expression")?;
        ctx.git.expression(ctx.tx, repo.id, &expression)
    };
    let object = match found {
        Ok(o) => o,
        Err(GitError::NotFound) => return Ok(Resolved::Null),
        Err(GitError::NotImplemented(what)) => {
            return Err(GqlError::new(ErrorType::NotImplemented, what));
        }
        Err(e) => return Err(GqlError::new(ErrorType::Unprocessable, e.to_string())),
    };
    let repo_id = repo.id;
    let id = object.id;
    Ok(match object.data {
        ObjectData::Blob(data) => Resolved::Object {
            type_name: "Blob",
            value: Arc::new(BlobNode { repo_id, id, data }),
        },
        ObjectData::Tree(entries) => Resolved::Object {
            type_name: "Tree",
            value: Arc::new(TreeNode {
                repo_id,
                id,
                entries,
            }),
        },
        ObjectData::Commit(commit) => Resolved::Object {
            type_name: "Commit",
            value: Arc::new(CommitNode {
                repo_id,
                id,
                commit,
            }),
        },
        ObjectData::Tag(tag) => Resolved::Object {
            type_name: "Tag",
            value: Arc::new(TagNode { repo_id, id, tag }),
        },
    })
}
